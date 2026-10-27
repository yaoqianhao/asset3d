"""asset3d 插件的宿主侧联调台：按 NDJSON 帧协议驱动 dist/asset3d.exe。

为什么自己写：宿主真实的插件通道在 Electron 主进程里，跑一次要开 IDE；
这个脚本只实现插件用到的 6 个 host/* 方法，外加**真实的 1 MiB 单帧上限**，
让「帧预算」这类问题在本地就能撞出来，而不是等上线被 6106 杀掉进程。

用法：
    python dev/harness.py            # 跑默认用例
    python dev/harness.py --big      # 额外验证「超大资产」的拒绝路径
"""

from __future__ import annotations

import json
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXE = ROOT / "dist" / "asset3d.exe"
WS = ROOT / ".tmp-ws"
FRAME_LIMIT = 1024 * 1024
# 单帧留给路径/字段名/编码的固定余量，与 src/rpc.rs 的 FRAME_RESERVE_BYTES 一致
SHARD_RESERVE = 64 * 1024
SENSITIVE = {".env", ".git", ".ssh", "id_rsa"}


class Harness:
    def __init__(self, exe: Path, workspace: Path) -> None:
        self.workspace = workspace.resolve()
        self.proc = subprocess.Popen(
            [str(exe)],
            cwd=str(exe.parent),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self.next_id = 100
        self.pending: dict[int, dict] = {}
        self.lock = threading.Lock()
        self.progress: list[str] = []
        self.violations: list[str] = []
        self.writes: list[str] = []
        threading.Thread(target=self._pump_stdout, daemon=True).start()
        threading.Thread(target=self._pump_stderr, daemon=True).start()

    # ---- 传输 -------------------------------------------------------------
    def _send(self, frame: dict) -> None:
        line = json.dumps(frame, ensure_ascii=False) + "\n"
        if len(line.encode("utf-8")) > FRAME_LIMIT:
            self.violations.append(f"发出超限帧 {len(line)} 字节")
        self.proc.stdin.write(line)
        self.proc.stdin.flush()

    def _pump_stdout(self) -> None:
        for raw in self.proc.stdout:
            if len(raw.encode("utf-8")) > FRAME_LIMIT:
                self.violations.append(f"插件回包超限 {len(raw)} 字节")
                self.proc.kill()
                return
            try:
                frame = json.loads(raw)
            except json.JSONDecodeError:
                self.violations.append(f"非法帧：{raw[:120]!r}")
                continue
            if "method" in frame:
                # 插件 → 宿主的能力请求，就地应答
                try:
                    result = self._host_call(frame["method"], frame.get("params") or {})
                    self._send({"jsonrpc": "2.0", "id": frame.get("id"), "result": result})
                except Exception as exc:  # noqa: BLE001 - 联调台，错误要原样回给插件
                    self._send(
                        {
                            "jsonrpc": "2.0",
                            "id": frame.get("id"),
                            "error": {"code": 2001, "message": str(exc)},
                        }
                    )
            else:
                with self.lock:
                    self.pending[frame.get("id")] = frame

    def _pump_stderr(self) -> None:
        for raw in self.proc.stderr:
            print(f"  [plugin] {raw.rstrip()}")

    def request(self, method: str, params: dict, timeout: float = 120.0) -> dict:
        with self.lock:
            self.next_id += 1
            rid = self.next_id
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        waited = 0.0
        step = 0.05
        while waited < timeout:
            with self.lock:
                if rid in self.pending:
                    return self.pending.pop(rid)
            threading.Event().wait(step)
            waited += step
        raise TimeoutError(f"{method} 超时（{timeout}s）")

    # ---- 宿主能力 ---------------------------------------------------------
    def _abs(self, rel: str) -> Path:
        target = (self.workspace / rel).resolve()
        if target != self.workspace and self.workspace not in target.parents:
            raise PermissionError(f"路径越出工作区：{rel}")
        if any(part in SENSITIVE for part in target.parts):
            raise PermissionError(f"敏感路径：{rel}")
        return target

    def _host_call(self, method: str, params: dict):
        if method == "host/progress":
            self.progress.append(params.get("message", ""))
            return {"ok": True}
        rel = params.get("path", "")
        if method == "host/fs.stat":
            target = self._abs(rel)
            st = target.stat()
            return {"path": rel, "size": st.st_size, "dir": target.is_dir()}
        if method == "host/fs.list":
            target = self._abs(rel) if rel else self.workspace
            entries = []
            for item in sorted(target.iterdir()):
                if item.name in SENSITIVE:
                    continue
                entries.append(
                    {"name": item.name, "dir": item.is_dir(),
                     "size": 0 if item.is_dir() else item.stat().st_size}
                )
            return {"entries": entries}
        if method == "host/fs.read":
            target = self._abs(rel)
            encoding = params.get("encoding", "utf8")
            max_bytes = int(params.get("maxBytes") or 96 * 1024 * 1024)
            raw = target.read_bytes()
            keep = raw[:max_bytes]
            content = (
                base64_encode(keep) if encoding == "base64" else keep.decode("utf-8", "replace")
            )
            return {
                "path": rel,
                "content": content,
                "encoding": encoding,
                "bytes": len(keep),
                "truncated": len(raw) > max_bytes,
            }
        if method == "host/fs.write":
            target = self._abs(rel)
            target.parent.mkdir(parents=True, exist_ok=True)
            created = not target.exists()
            if params.get("encoding") == "base64":
                import base64

                target.write_bytes(base64.b64decode(params["content"]))
            else:
                target.write_text(params["content"], encoding="utf-8")
            self.writes.append(rel)
            return {"path": rel, "created": created, "bytes": target.stat().st_size}
        raise NotImplementedError(method)

    # ---- 用例 -------------------------------------------------------------
    def hello(self) -> dict:
        """握手。方法名与宿主一致：`plugin/handshake`（老宿主还有 `plugin/initialize`）。"""
        return self.request(
            "plugin/handshake",
            {
                "pluginApi": {"major": 1, "minor": 0},
                "host": {"name": "asset3d-harness"},
                "plugin": {"id": "asset3d"},
                "workspace": {"root": str(self.workspace), "name": self.workspace.name},
            },
        )

    def tool(self, name: str, arguments: dict) -> tuple[bool, str]:
        reply = self.request(
            "plugin/tool-invoke", {"tool": name, "arguments": arguments}
        )
        if "error" in reply:
            return False, f"{reply['error'].get('code')}: {reply['error'].get('message')}"
        result = reply.get("result")
        return True, result if isinstance(result, str) else json.dumps(result, ensure_ascii=False)

    def close(self) -> None:
        try:
            self.proc.stdin.close()
        except Exception:  # noqa: BLE001
            pass
        self.proc.wait(timeout=10)


def base64_encode(data: bytes) -> str:
    import base64

    return base64.b64encode(data).decode()


def summarise(air: dict) -> dict:
    """把 AIR 折成可直接比较的形状摘要：几何量、层级、材质/贴图数量。

    GLB 与 glTF 是同一份几何的两种容器，摘要不同就说明容器解析走了岔路
    （比如 BIN chunk 偏移算错、补齐字节被当成几何）。
    """
    meshes = air.get("meshes") or []
    return {
        "nodes": len(air.get("nodes") or []),
        "meshes": len(meshes),
        "positions": sum(len(m.get("positions") or []) for m in meshes),
        "indices": sum(len(m.get("indices") or []) for m in meshes),
        "uvs": sum(len(m.get("uvs") or []) for m in meshes),
        "normals": sum(len(m.get("normals") or []) for m in meshes),
        "materials": len(air.get("materials") or []),
        "textures": len(air.get("textures") or []),
        "skins": len(air.get("skins") or []),
        "animations": len(air.get("animations") or []),
    }


def main() -> int:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import make_big_fixture
    import make_fbx_fixture
    import make_fixture
    import make_glb_fixture
    import make_skin_fixture

    make_fixture.main()
    make_fbx_fixture.main()
    # GLB 容器（magic + JSON chunk + BIN chunk）是独立分支，必须有真二进制夹具；
    # 它复用 .gltf 的几何，因此两者的解析结果可以逐项对比。
    make_glb_fixture.main()
    # 蒙皮夹具带 2 关节、每关节 IBM、1 条动画，且承载网格的节点带非单位 TRS：
    # 少了它，「IBM/动画头部是否被估算进去」「顶点是否被错误烘成世界坐标」都没法验。
    make_skin_fixture.main()
    # 大夹具（26450 三角形）必须先于断言生成：预览分片是本次要盯的核心。
    make_big_fixture.main()

    if not EXE.exists():
        print(f"缺少 {EXE}，先跑 cargo build --release 并复制到 dist/")
        return 2

    harness = Harness(EXE, WS)
    failures: list[str] = []
    try:
        hello = harness.hello()
        plugin = hello.get("result") or {}
        reported = {t["name"] for t in plugin.get("tools", [])}
        manifest = json.loads((ROOT / "dist" / "plugin.json").read_text(encoding="utf-8"))
        declared = {t["name"] for t in manifest["tools"]}
        if reported != declared:
            failures.append(f"握手上报与清单不一致：{reported ^ declared}")
        else:
            print(f"[ok  ] 握手 {len(reported)} 个工具，与 plugin.json 一致")

        cases = [
            ("inspect_asset", {"path": "cube.gltf"}),
            ("export_asset", {"path": "cube.gltf", "format": "both"}),
            ("inspect_asset", {"path": "quad.fbx"}),
            ("export_asset", {"path": "quad.fbx", "format": "both"}),
            ("preview_asset", {"path": "cube.gltf"}),
            ("list_assets", {"dir": ""}),
            ("export_engine_loader", {"out": "src/air_loader.rs", "overwrite": True}),
        ]
        for name, args in cases:
            ok, text = harness.tool(name, args)
            head = text.splitlines()[0] if text else ""
            print(f"[{'ok  ' if ok else 'fail'}] {name}: {head[:110]}")
            if not ok:
                failures.append(f"{name} -> {text}")

        # 再验一次 AIR 里的顶点确实是局部空间：Pivot 的位移不该烘进顶点
        air = json.loads((WS / "cube.air.json").read_text(encoding="utf-8"))
        pivot = next(n for n in air["nodes"] if n["name"] == "Pivot")
        cube = next(n for n in air["nodes"] if n["name"] == "Cube")
        ymax = max(point[1] for point in air["meshes"][0]["positions"])
        if abs(pivot["position"][1] - 0.5) > 1e-5:
            failures.append(f"Pivot 位移丢失：{pivot['position']}")
        if abs(ymax - 0.5) > 1e-4:
            failures.append(f"顶点疑似被烘焙成世界坐标，y_max={ymax}")
        if cube.get("parent") != pivot["index"]:
            failures.append(f"Cube 的父节点不是 Pivot：{cube}")
        if not failures:
            print("[ok  ] 顶点保持 mesh 局部空间，TRS 由节点链承担")

        # FBX 链路单独验：四边形必须被三角化成 2 个三角形，且 UV 已翻成左上原点
        fbx = json.loads((WS / "quad.air.json").read_text(encoding="utf-8"))
        mesh = fbx["meshes"][0]
        if len(mesh["positions"]) != 4 or len(mesh["indices"]) != 6:
            failures.append(
                f"FBX 三角化异常：顶点 {len(mesh['positions'])}，索引 {len(mesh['indices'])}"
            )
        if mesh["uvs"] and abs(mesh["uvs"][0][1] - 0.0) > 1e-5:
            failures.append(f"FBX UV 未翻转：uv0={mesh['uvs'][0]}")
        if not failures:
            print(f"[ok  ] FBX 三角化 {len(mesh['indices']) // 3} 三角形，UV 已翻转")

        # GLB 链路单独验：二进制容器解析出的 AIR 必须与同源 .gltf 逐项一致，
        # 且 .air.bin 必须是 AIRB v1（引擎侧 load_air 靠 magic + 版本判型）。
        gltf_air = json.loads((WS / "cube.air.json").read_text(encoding="utf-8"))
        gltf_counts = summarise(gltf_air)
        for name, args in [
            ("inspect_asset", {"path": "cube.glb"}),
            ("export_asset", {"path": "cube.glb", "format": "both"}),
            ("preview_asset", {"path": "cube.glb"}),
        ]:
            ok, text = harness.tool(name, args)
            head = text.splitlines()[0] if text else ""
            print(f"[{'ok  ' if ok else 'fail'}] {name} (.glb): {head[:110]}")
            if not ok:
                failures.append(f"{name} (.glb) -> {text}")
        if not failures:
            glb_air = json.loads((WS / "cube.air.json").read_text(encoding="utf-8"))
            if glb_air["source"].get("container") != "glb":
                failures.append(f"GLB 容器没被识别：{glb_air['source']}")
            if summarise(glb_air) != gltf_counts:
                failures.append(f"GLB 与 glTF 解析结果不一致：{summarise(glb_air)} vs {gltf_counts}")
            bin_path = WS / "cube.air.bin"
            if not bin_path.exists():
                failures.append("GLB 导出没有产出 cube.air.bin")
            else:
                blob = bin_path.read_bytes()
                if blob[:4] != b"AIRB":
                    failures.append(f".air.bin magic 不对：{blob[:4]!r}")
                else:
                    print(
                        f"[ok  ] GLB 与 glTF 同源一致 {gltf_counts}，"
                        f".air.bin {len(blob)} 字节 magic=AIRB"
                    )

        # 蒙皮链路：关节/IBM/动画三项都要在，且顶点必须留在 mesh 局部空间——
        # 承载网格的节点挂了 translation/rotation，烘进顶点的话 y_max 就不再是 1.0。
        for name, args in [
            ("inspect_asset", {"path": "skin.glb"}),
            ("export_asset", {"path": "skin.glb", "format": "both"}),
        ]:
            ok, text = harness.tool(name, args)
            print(f"[{'ok  ' if ok else 'fail'}] {name} (.skin): {text.splitlines()[0][:110]}")
            if not ok:
                failures.append(f"{name} (skin.glb) -> {text}")
        skin = json.loads((WS / "skin.air.json").read_text(encoding="utf-8"))
        skin_mesh = skin["meshes"][0]
        xs = [p[0] for p in skin_mesh["positions"]]
        ys = [p[1] for p in skin_mesh["positions"]]
        rig = (skin.get("skins") or [None])[0]
        if rig is None:
            failures.append(f"蒙皮没被识别：skins={skin.get('skins')}")
        else:
            if len(rig.get("joints") or []) != 2:
                failures.append(f"关节数不对：{rig.get('joints')}")
            if not rig.get("inverse_bind_matrices"):
                failures.append(f"每关节的 IBM 缺失：{rig}")
        if len(skin.get("animations") or []) != 1:
            failures.append(f"export 必须带动画：animations={skin.get('animations')}")
        if not skin_mesh.get("weights") or not skin_mesh.get("joints"):
            failures.append("蒙皮权重/关节索引没进 AIR")
        if max(ys) > 1.0 + 1e-4 or min(ys) < -1e-4 or max(xs) > 0.4 + 1e-4:
            failures.append(
                f"蒙皮网格顶点疑似被节点 TRS 烘焙：x∈[{min(xs):.3f},{max(xs):.3f}] "
                f"y∈[{min(ys):.3f},{max(ys):.3f}]"
            )
        skinned_node = next((n for n in skin["nodes"] if n["name"] == "SkinnedMesh"), None)
        if not skinned_node or not skinned_node.get("position"):
            failures.append(f"节点 TRS 丢失：{skinned_node}")
        elif abs(skinned_node["position"][0] - 0.25) > 1e-5:
            failures.append(f"节点位移不对：{skinned_node['position']}")
        else:
            print(
                f"[ok  ] 蒙皮 {len(rig['joints'])} 关节 / IBM {len(rig['inverse_bind_matrices'])} 份 / "
                f"动画 {len(skin['animations'])} 段，顶点仍在局部空间"
            )

        # 预览必须不载入动画：同样的资产，preview 的说明里要明确写「未载入」。
        ok, text = harness.tool("preview_asset", {"path": "skin.glb"})
        print(f"[{'ok  ' if ok else 'fail'}] preview_asset (.skin): {text.splitlines()[0][:110]}")
        if not ok:
            failures.append(f"preview_asset (skin.glb) -> {text}")
        elif "未载入" not in text:
            failures.append(f"preview 说明里没有「动画未载入」：{text[:200]}")

        # 超 1 MiB 预览：单帧装不下，必须切成多片，且每片都要低于宿主 1 MiB 硬顶。
        # harness 自身会记录超限帧，这里再对落盘文件独立复核一遍（双保险）。
        started = time.time()
        ok, text = harness.tool(
            "preview_asset", {"path": "big.glb", "out": "big_shard.preview.html"}
        )
        print(f"[{'ok  ' if ok else 'fail'}] preview_asset (.big): {text.splitlines()[0][:110]}")
        if not ok:
            failures.append(f"preview_asset (big.glb) -> {text}")
        else:
            shards = [
                item
                for item in WS.glob("big_shard.preview.shard.*.js")
                if item.stat().st_mtime >= started - 1.0
            ]
            sizes = [item.stat().st_size for item in shards]
            total = sum(sizes)
            biggest = max(sizes) if sizes else 0
            if len(shards) < 2:
                failures.append(f"载荷超 1 MiB 却没分片：{len(shards)} 片")
            elif total <= FRAME_LIMIT:
                failures.append(f"分片总量没超过 1 MiB（{total}），夹具不够大，测不到分片路径")
            elif biggest >= FRAME_LIMIT - SHARD_RESERVE:
                failures.append(
                    f"单片 {biggest} 字节超出预算上限 {FRAME_LIMIT - SHARD_RESERVE}"
                )
            elif not (WS / "big_shard.preview.html").exists():
                failures.append("分片写了却没有 HTML 骨架")
            else:
                print(
                    f"[ok  ] 预览分片 {len(shards)} 片，共 {total} 字节（>1 MiB），"
                    f"单片最大 {biggest} < {FRAME_LIMIT - SHARD_RESERVE}"
                )

        # 预算保护：写一个必然超帧的文件，要求插件给出「宿主原语不支持分片」而不是被杀
        big = WS / "big.bin"
        big.write_bytes(b"\x00" * (2 * 1024 * 1024))
        ok, text = harness.tool("preview_asset", {"path": "big.bin"})
        print(f"[info] 超限路径：{text.splitlines()[0][:150]}")

        if harness.violations:
            failures.extend(harness.violations)
        else:
            print(f"[ok  ] 全程无超限帧（上限 {FRAME_LIMIT} 字节）")
        print(f"[info] 进度日志 {len(harness.progress)} 条，写盘 {len(harness.writes)} 次")
    finally:
        harness.close()

    if failures:
        print("\n失败项：")
        for item in failures:
            print(f" - {item}")
        return 1
    print("\n全部用例通过")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
