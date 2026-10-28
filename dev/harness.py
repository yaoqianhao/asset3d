"""asset3d 插件的宿主侧联调台：按 NDJSON 帧协议驱动 dist/asset3d.exe。

为什么自己写：宿主真实的插件通道在 Electron 主进程里，跑一次要开 IDE；
这个脚本只实现插件用到的 6 个 host/* 方法，但**照抄宿主的硬约束**，让问题在本地
就撞出来，而不是等上线被杀进程：

- 单帧 1 MiB（`crates/plugin/src/loader.rs: FRAME_LIMIT_BYTES`）
- `host/fs.read` 单次读窗口 512 KiB，靠 `offset` 续读，回包带 `offset`/`size`/`eof`
  （`crates/core/src/plugin_host.rs: MAX_READ_WINDOW_BYTES`）
- `host/fs.write` 支持 `append` / `offset`，分片写的回包带写后 `size`
- 单次工具调用最多 200 次宿主调用，超了回 6109（`QUOTA_PER_CALL`）

用法：
    python dev/harness.py            # 分片读写宿主（0.3.0 的目标环境）
    python dev/harness.py --legacy   # 模拟 0.2.0 时代的旧宿主：fs.read 无 offset、
                                     # fs.write 无 append/offset、回包没有 size/eof
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
# 与宿主 MAX_READ_WINDOW_BYTES / QUOTA_PER_CALL 对齐
MAX_READ_WINDOW = 512 * 1024
QUOTA_PER_CALL = 200
SENSITIVE = {".env", ".git", ".ssh", "id_rsa"}
# 0.2.0 的单帧读取上限：1 MiB 帧按 base64 4:3 回推
LEGACY_READ_BYTES = (FRAME_LIMIT - SHARD_RESERVE) // 4 * 3


class HostApiError(Exception):
    """带宿主错误码的失败：插件靠 code + message 判断「旧宿主不支持 offset」。"""

    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class Harness:
    def __init__(self, exe: Path, workspace: Path, legacy: bool = False) -> None:
        self.workspace = workspace.resolve()
        self.legacy = legacy
        self.calls_used = 0
        self.max_calls_seen = 0
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
        # 记下每次 fs.read 的分片边界：验证「真的分片读了」而不是碰巧一次读完
        self.reads: list[tuple[str, int, int]] = []
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
            # 宿主按行读帧，超过 1 MiB 直接判协议违规：这里如实记下来
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
                except HostApiError as exc:
                    self._send(
                        {
                            "jsonrpc": "2.0",
                            "id": frame.get("id"),
                            "error": {"code": exc.code, "message": exc.message},
                        }
                    )
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

    def request(self, method: str, params: dict, timeout: float = 180.0) -> dict:
        with self.lock:
            self.next_id += 1
            rid = self.next_id
            # 宿主在每次工具调用开始时把配额计数归零（begin_call）
            if method == "plugin/tool-invoke":
                self.calls_used = 0
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
        with self.lock:
            if self.calls_used >= QUOTA_PER_CALL:
                raise HostApiError(
                    6109, f"单次工具调用的宿主调用次数已达上限（{QUOTA_PER_CALL} 次）"
                )
            self.calls_used += 1
            self.max_calls_seen = max(self.max_calls_seen, self.calls_used)
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
            return self._fs_read(rel, params)
        if method == "host/fs.write":
            return self._fs_write(rel, params)
        raise NotImplementedError(method)

    def _fs_read(self, rel: str, params: dict) -> dict:
        # 旧宿主：参数表里没有 offset，多传就是「未知参数」
        if self.legacy and "offset" in params:
            raise HostApiError(-32602, "host/fs.read: unknown parameter `offset`")
        target = self._abs(rel)
        encoding = params.get("encoding", "utf8")
        raw = target.read_bytes()
        size = len(raw)
        offset = int(params.get("offset") or 0)
        if offset > size:
            raise HostApiError(1003, f"offset {offset} 超出文件长度 {size}：{rel}")
        want = int(params.get("maxBytes") or (size or 1))
        if not self.legacy:
            # 关键：宿主会把 maxBytes 夹到 512 KiB，插件必须按回包 bytes 推进
            want = min(want, MAX_READ_WINDOW)
        chunk = raw[offset : offset + want]
        content = (
            base64_encode(chunk) if encoding == "base64" else chunk.decode("utf-8", "replace")
        )
        self.reads.append((rel, offset, len(chunk)))
        reply = {
            "path": rel,
            "content": content,
            "encoding": encoding,
            "bytes": len(chunk),
            "truncated": offset + len(chunk) < size,
        }
        if not self.legacy:
            reply.update(
                {"offset": offset, "size": size, "eof": offset + len(chunk) >= size}
            )
        return reply

    def _fs_write(self, rel: str, params: dict) -> dict:
        if self.legacy and ("append" in params or "offset" in params):
            raise HostApiError(
                -32602, "host/fs.write: unknown parameter `append` / `offset`"
            )
        target = self._abs(rel)
        target.parent.mkdir(parents=True, exist_ok=True)
        existed = target.exists()
        data = (
            __import__("base64").b64decode(params["content"])
            if params.get("encoding") == "base64"
            else str(params.get("content", "")).encode("utf-8")
        )
        before = target.stat().st_size if existed else 0
        append = bool(params.get("append"))
        at = params.get("offset")
        if append:
            start = before
        elif at is not None:
            start = int(at)
        else:
            start = None

        if start is None:
            # 缺省语义：整文件替换（宿主走临时文件 + rename，这里等价写字节）
            target.write_bytes(data)
            offset, size, padded = 0, len(data), 0
        else:
            padded = max(0, start - before)
            mode = "r+b" if existed else "w+b"
            with open(target, mode) as fh:
                if padded:
                    fh.truncate(start)  # 与宿主 set_len 补零一致
                fh.seek(start)
                fh.write(data)
                fh.flush()
            offset, size = start, max(start + len(data), before)
        self.writes.append(rel)
        reply = {"path": rel, "created": not existed, "bytes": len(data)}
        if not self.legacy:
            # 旧宿主回包没有 size：插件靠它判断「append 是否真的生效」
            reply.update({"offset": offset, "size": size, "padded": padded})
        return reply

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


def glb_image_bytes(path: Path) -> bytes:
    """从 GLB 里取出 image 的原始字节：用来验证分片读写没有悄悄改动内容。"""
    blob = path.read_bytes()
    total = int.from_bytes(blob[8:12], "little")
    assert total == len(blob), f"{path} 的 GLB 长度字段对不上"
    json_len = int.from_bytes(blob[12:16], "little")
    doc = json.loads(blob[20 : 20 + json_len].decode("utf-8"))
    bin_start = 20 + json_len + 8
    binary = blob[bin_start:]
    view = doc["bufferViews"][doc["images"][0]["bufferView"]]
    start = view.get("byteOffset", 0)
    return binary[start : start + view["byteLength"]]


def prepare_fixtures() -> None:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import make_big_fixture
    import make_fbx_fixture
    import make_fixture
    import make_glb_fixture
    import make_huge_fixture
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
    # 超大夹具（1.04 MiB / 2.87 MiB）：0.2.0 的 720 KiB 上限正好卡在它们之间。
    make_huge_fixture.main()


def run_paged(failures: list[str]) -> None:
    harness = Harness(EXE, WS)
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

        run_huge(harness, failures)

        # 越界保护：超过 96 MiB 的资产要在插件侧拦下，并给出可操作诊断，
        # 而不是把 200 次宿主调用额度刷爆后由宿主判违规终止进程。
        over = WS / "over_limit.bin"
        with open(over, "wb") as fh:
            fh.truncate(97 * 1024 * 1024)
        ok, text = harness.tool("inspect_asset", {"path": "over_limit.bin"})
        print(f"[{'ok  ' if not ok else 'fail'}] 97 MiB 资产被插件拦下：{text.splitlines()[0][:120]}")
        if ok:
            failures.append("97 MiB 资产没有被拦截")
        elif "超过单次导入上限" not in text:
            failures.append(f"超限诊断缺少可操作说明：{text[:200]}")

        if harness.violations:
            failures.extend(harness.violations)
        else:
            print(f"[ok  ] 全程无超限帧（上限 {FRAME_LIMIT} 字节）")
        print(
            f"[info] 分片读宿主：进度 {len(harness.progress)} 条，写盘 {len(harness.writes)} 次，"
            f"单次工具调用最多用了 {harness.max_calls_seen} / {QUOTA_PER_CALL} 次宿主调用"
        )
    finally:
        harness.close()


def run_huge(harness: Harness, failures: list[str]) -> None:
    """超过旧上限（720 KiB）与超过 2 MiB 两个量级的端到端验证。

    关注点不是「能跑通」，而是三件具体的事：
    1. 读：真的按 offset 分片读（每片 ≤ 512 KiB），没有靠放大帧蒙混；
    2. 写：超过 1 MiB 的 AIR JSON / 预览载荷被分片写下去，且内容逐字节没变；
    3. 上限：96 MiB 之内的资产不会被宿主判违规，也不会被插件提前吓退。
    """
    for stem, floor in (("huge_1m", 720 * 1024), ("huge_3m", 2 * 1024 * 1024)):
        source = WS / f"{stem}.glb"
        size = source.stat().st_size
        if size <= floor:
            failures.append(f"{source.name} 只有 {size} 字节，没超过 {floor}，测不到目标场景")
            continue

        before_reads = len(harness.reads)
        ok, text = harness.tool("inspect_asset", {"path": source.name})
        reads = [item for item in harness.reads[before_reads:] if item[0] == source.name]
        chunks = [item[2] for item in reads]
        head = text.splitlines()[0] if text else ""
        print(
            f"[{'ok  ' if ok else 'fail'}] inspect_asset ({source.name} {size} 字节): "
            f"读 {len(reads)} 片，最大 {max(chunks) if chunks else 0} 字节 | {head[:80]}"
        )
        if not ok:
            failures.append(f"inspect_asset ({source.name}) -> {text}")
            continue
        if len(reads) < 2:
            failures.append(f"{source.name} 超过 720 KiB 却只读了 {len(reads)} 次，没有走分片")
        if chunks and max(chunks) > MAX_READ_WINDOW:
            failures.append(f"{source.name} 单片 {max(chunks)} 超过宿主窗口 {MAX_READ_WINDOW}")
        if sum(chunks) != size:
            failures.append(f"{source.name} 分片合计 {sum(chunks)} 与文件长度 {size} 不符")

        # 导出必须内嵌贴图，否则 AIR 只有几百 KB，写侧的分片路径根本不会触发
        # out 会被规范成 .air.json / .air.bin：显式给出 .air.json 才能落到可预期的文件名
        out_base = f"{stem}_embed.air.json"
        ok, text = harness.tool(
            "export_asset",
            {
                "path": source.name,
                "format": "both",
                "embedTextures": True,
                "out": out_base,
            },
        )
        head = text.splitlines()[0] if text else ""
        print(f"[{'ok  ' if ok else 'fail'}] export_asset ({source.name}): {head[:110]}")
        if not ok:
            failures.append(f"export_asset ({source.name}) -> {text}")
            continue

        json_path = WS / out_base
        bin_path = WS / out_base.replace(".air.json", ".air.bin")
        if not json_path.exists() or not bin_path.exists():
            failures.append(f"{source.name} 导出缺文件：{json_path.name} / {bin_path.name}")
            continue
        json_size = json_path.stat().st_size
        bin_size = bin_path.stat().st_size
        if json_size <= FRAME_LIMIT:
            failures.append(
                f"{json_path.name} 只有 {json_size} 字节，没超过单帧 1 MiB，写侧分片没被测到"
            )
        if bin_path.read_bytes()[:4] != b"AIRB":
            failures.append(f"{bin_path.name} magic 不是 AIRB")

        # 逐字节核对贴图：分片读 + 分片写都可能悄悄截断，只有比对哈希能发现
        import hashlib

        expected = hashlib.sha256(glb_image_bytes(source)).hexdigest()
        air = json.loads(json_path.read_text(encoding="utf-8"))
        textures = air.get("textures") or []
        payload = textures[0].get("data_base64") if textures else None
        if not payload:
            failures.append(f"{json_path.name} 里贴图没内嵌")
        else:
            got = hashlib.sha256(__import__("base64").b64decode(payload)).hexdigest()
            if got != expected:
                failures.append(f"{source.name} 贴图哈希不一致：{got[:16]} != {expected[:16]}")
            else:
                print(
                    f"[ok  ] {source.name} 贴图逐字节一致（sha256 {got[:16]}…），"
                    f".air.json {json_size} 字节 / .air.bin {bin_size} 字节"
                )

        # 预览：贴图内嵌 + HTML 分片，必须多片且每片都在预算内
        out_html = f"{stem}.preview.html"
        before_writes = len(harness.writes)
        ok, text = harness.tool(
            "preview_asset", {"path": source.name, "out": out_html}
        )
        head = text.splitlines()[0] if text else ""
        print(f"[{'ok  ' if ok else 'fail'}] preview_asset ({source.name}): {head[:110]}")
        if not ok:
            failures.append(f"preview_asset ({source.name}) -> {text}")
            continue
        shards = sorted(WS.glob(f"{stem}.preview.shard.*.js"))
        sizes = [item.stat().st_size for item in shards]
        total = sum(sizes)
        biggest = max(sizes) if sizes else 0
        if not shards:
            failures.append(f"{source.name} 预览没有产出分片")
        elif biggest >= FRAME_LIMIT - SHARD_RESERVE:
            failures.append(f"{source.name} 预览单片 {biggest} 超出预算 {FRAME_LIMIT - SHARD_RESERVE}")
        elif total <= FRAME_LIMIT:
            failures.append(f"{source.name} 预览载荷 {total} 没超过 1 MiB，分片路径没覆盖")
        else:
            print(
                f"[ok  ] {source.name} 预览 {len(shards)} 片共 {total} 字节，单片最大 {biggest}"
                f"（写盘 {len(harness.writes) - before_writes} 次）"
            )
        if not (WS / out_html).exists():
            failures.append(f"{source.name} 预览缺 HTML 骨架")


def run_legacy(failures: list[str]) -> None:
    """旧宿主回归：不能因为新能力就要求所有人先升级宿主。

    分片能力缺失时必须退回整文件路径；只有「整文件也装不下」的资产才报错，
    而且报错要直接说清「宿主太旧 + 上限是多少 + 该怎么办」。
    """
    harness = Harness(EXE, WS, legacy=True)
    try:
        ok = harness.hello().get("result") is not None
        if not ok:
            failures.append("旧宿主下握手失败")
            return

        for name, args in [
            ("inspect_asset", {"path": "cube.gltf"}),
            ("preview_asset", {"path": "cube.gltf"}),
            ("inspect_asset", {"path": "big.glb"}),
        ]:
            ok, text = harness.tool(name, args)
            print(f"[{'ok  ' if ok else 'fail'}] 旧宿主 {name}: {text.splitlines()[0][:110]}")
            if not ok:
                failures.append(f"旧宿主 {name} 不该失败 -> {text}")

        # 590 KB 的 big.glb 超过 512 KiB 窗口，旧宿主只能整帧读；它仍应成功，
        # 这正是「降级而不是拒绝服务」的证据。
        big = WS / "big.glb"
        if big.stat().st_size > LEGACY_READ_BYTES:
            failures.append("big.glb 已超过旧宿主上限，无法作为降级成功用例")

        ok, text = harness.tool("inspect_asset", {"path": "huge_3m.glb"})
        print(f"[{'ok  ' if not ok else 'fail'}] 旧宿主读 2.87 MiB 资产被拦：{text.splitlines()[0][:120]}")
        if ok:
            failures.append("旧宿主竟然读成功了大资产，说明没走 is_unsupported 判定")
        elif "宿主版本过旧" not in text or "host/fs.read" not in text:
            failures.append(f"大资产在旧宿主上的诊断不可操作：{text[:200]}")

        # 写侧降级：导出内嵌贴图的 AIR 必然超帧，旧宿主不支持 append 就该失败，
        # 而且不能留下半截文件（0 字节文件也算「写了」，会误导后续读取）。
        target = WS / "legacy_big.air.json"
        ok, text = harness.tool(
            "export_asset",
            {"path": "big.glb", "format": "json", "embedTextures": True, "out": "legacy_big"},
        )
        print(f"[{'ok  ' if not ok else 'fail'}] 旧宿主分片写被拦：{text.splitlines()[0][:120]}")
        if ok:
            failures.append("旧宿主不支持 append 却导出成功，分片写降级逻辑有问题")
        elif "宿主版本过旧" not in text or "host/fs.write" not in text:
            failures.append(f"分片写在旧宿主上的诊断不可操作：{text[:200]}")
        if target.exists() and target.stat().st_size > 0:
            failures.append(f"分片写失败后留下了半截文件：{target.name} {target.stat().st_size} 字节")
        else:
            print("[ok  ] 分片写失败未留下半截文件")

        if harness.violations:
            failures.extend(harness.violations)
    finally:
        harness.close()


def main() -> int:
    if not EXE.exists():
        print(f"缺少 {EXE}，先跑 cargo build --release 并把产物复制到 dist/")
        return 2
    WS.mkdir(parents=True, exist_ok=True)
    prepare_fixtures()
    print(f"[info] 宿主模式：{'旧宿主（无 offset/append）' if LEGACY_ONLY else '分片读写'}")

    failures: list[str] = []
    if LEGACY_ONLY:
        run_legacy(failures)
    else:
        run_paged(failures)
        run_legacy(failures)

    print()
    if failures:
        print(f"FAILED {len(failures)} 项：")
        for item in failures:
            print(f"  - {item}")
        return 1
    print("全部通过")
    return 0


if __name__ == "__main__":
    LEGACY_ONLY = "--legacy" in sys.argv
    sys.exit(main())
