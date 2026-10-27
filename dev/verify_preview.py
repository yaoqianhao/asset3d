"""预览分片落盘实测：只管「宿主 1 MiB 单帧」这条硬顶，用真实夹具压。

为什么单独一支脚本：`dev/harness.py` 的预览用例只有小立方体（2 KB 载荷），
根本碰不到分片路径。这里用 26450 三角形的大网格（JSON 载荷 ~1.2 MB）跑真链路：
写盘不超帧、分片能拼回完整 JSON、贴图仍在、三角预算降级可生效。

用法：
    python dev/verify_preview.py
"""

from __future__ import annotations

import json
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WS = ROOT / ".tmp-ws"
FRAME_LIMIT = 1024 * 1024
RESERVE = 64 * 1024
EXE = ROOT / "dist" / "asset3d.exe"


def budget(path: str) -> int:
    """与 `rpc::payload_budget` 同一算式：帧上限 - 余量 - 路径/编码/字段开销。"""
    return (FRAME_LIMIT - RESERVE) - (len(path) + len("utf8") + 256)


def node_ok() -> bool:
    return shutil.which("node") is not None


def shard_names(text: str) -> list[str]:
    names = []
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("- ") and ".shard." in line and line.endswith("）"):
            names.append(line[2:].split("（")[0])
    return names


def assemble(names: list[str]) -> tuple[dict, list[int]]:
    """按序号合并分片：每个文件是 `push({"i":N,"t":"<JSON 片段>"})`。"""
    parts: dict[int, str] = {}
    sizes = []
    for name in names:
        text = (WS / name).read_text(encoding="utf-8")
        sizes.append(len(text.encode("utf-8")))
        body = text[len('window.__AIR_SHARDS__.push({"i":'):]
        index_text, rest = body.split(',"t":', 1)
        literal = rest.rsplit("});", 1)[0]
        parts[int(index_text)] = json.loads(literal)
    joined = "".join(parts[key] for key in sorted(parts))
    return json.loads(joined), sizes


def main() -> int:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import harness
    import make_big_fixture
    import make_fixture
    import make_skin_fixture

    make_fixture.main()
    make_big_fixture.main()
    make_skin_fixture.main()
    if not EXE.exists():
        print(f"缺少 {EXE}，先 cargo build --release 并复制到 dist/")
        return 2

    failures: list[str] = []
    h = harness.Harness(EXE, WS)
    try:
        h.hello()

        # --- 大网格 + 带贴图：必须分片、每片都过闸门、拼回来数据完整 ---
        ok, text_of_big = h.tool("preview_asset", {"path": "big.glb"})
        text = text_of_big
        print(f"[{'ok  ' if ok else 'fail'}] preview big.glb: {text.splitlines()[0][:150]}")
        if not ok:
            failures.append(f"preview big.glb 失败：{text}")
        else:
            names = shard_names(text)
            payload, sizes = assemble(names) if names else ({}, [])
            if len(names) < 2:
                failures.append(f"载荷远超 1 MiB 却没分片：{names}")
            for name, size in zip(names, sizes):
                limit = budget(name)
                if size >= limit:
                    failures.append(f"{name} 有 {size} 字节，越过预算 {limit}")
            if sizes:
                print(
                    f"[info] 分片 {len(names)} 个，最大 {max(sizes)} 字节，"
                    f"预算 {min(budget(n) for n in names)} 字节"
                )
            triangles = sum(len(m.get("indices") or []) for m in payload.get("meshes") or []) // 3
            if triangles < 24000:
                failures.append(f"拼回的几何只有 {triangles} 个三角形，夹具没被完整读入")
            else:
                print(f"[ok  ] 分片拼回完整 JSON：{triangles} 三角形")
            textures = payload.get("textures") or []
            if not textures or not textures[0].get("data_base64"):
                failures.append("拼回的载荷里贴图缺失")
            else:
                print(f"[ok  ] 贴图仍内嵌：{textures[0].get('name')} {len(textures[0]['data_base64'])} 字节 base64")
            if payload.get("animations"):
                failures.append(f"预览不该载入动画轨道：{len(payload['animations'])} 条")
            else:
                print("[ok  ] 预览载荷无动画轨道（不烘焙动画）")

            html = (WS / "big.preview.html").read_text(encoding="utf-8")
            html_bytes = len(html.encode("utf-8"))
            if html_bytes >= budget("big.preview.html"):
                failures.append(f"HTML 骨架 {html_bytes} 字节越过预算")
            for name in names:
                if f'<script src="{name}"></script>' not in html:
                    failures.append(f"HTML 没有按序 include {name}")
            if "__AIR_SHARD_LIST__" in html or "__AIR_SHARD_META__" in html:
                failures.append("HTML 里还有未替换的占位符")
            for token in ("failPage", "sniffMime", "__AIR_SHARDS__"):
                if token not in html:
                    failures.append(f"viewer 缺少 {token}（缺片容错 / mime 魔数回退）")
            print(
                f"[ok  ] HTML 骨架 {html_bytes} 字节，按序 include {len(names)} 片，"
                f"顶部列出分片清单"
            )

        # --- 蒙皮/动画：⑥ 估算要盖住真实落盘体积；② 预览不载入动画 ---
        ok, skin_inspect = h.tool("inspect_asset", {"path": "skin.glb"})
        if not ok:
            failures.append(f"inspect skin.glb 失败：{skin_inspect}")
        else:
            match = re.search(r"AIRB 约 ([\d.]+) KB", skin_inspect)
            ok2, skin_export = h.tool("export_asset", {"path": "skin.glb", "format": "bin"})
            if not match or not ok2:
                failures.append(f"拿不到 AIRB 估算或导出失败：{skin_export[:120]}")
            else:
                estimate = int(float(match.group(1)) * 1024)
                actual = (WS / "skin.air.bin").stat().st_size
                if estimate < actual:
                    failures.append(f"AIRB 估算 {estimate} 字节 < 实际 {actual} 字节（导出会撞预算）")
                else:
                    print(f"[ok  ] AIRB 估算 {estimate} 字节 ≥ 实际 {actual} 字节（含 2 关节 IBM + 1 条动画）")
            lens = re.search(r"统计：节点 (\d+) / 网格 (\d+) / 顶点 (\d+) / 三角形 (\d+)", skin_export)
            if not lens:
                failures.append("导出回执里没有统计行")
            elif "骨骼 1" not in skin_export or "动画 1" not in skin_export:
                failures.append("导出丢了蒙皮/动画（export 侧必须保留）")
            else:
                print("[ok  ] export 侧保留蒙皮与动画轨道")

        ok, skin_preview = h.tool("preview_asset", {"path": "skin.glb"})
        if not ok:
            failures.append(f"preview skin.glb 失败：{skin_preview}")
        else:
            skin_payload, _ = assemble(shard_names(skin_preview))
            if skin_payload.get("animations"):
                failures.append("带骨骼资产进预览时动画轨道没被剔除（load animations=false 未生效）")
            else:
                print("[ok  ] 带骨骼资产的预览载荷同样无动画轨道")
            if not skin_payload.get("skins"):
                failures.append("预览丢了蒙皮结构（关节/IBM 应随 AIR 一起走）")

        # --- 无贴图的大网格：分片不能靠贴图撑起来 ---
        ok, plain_text = h.tool("preview_asset", {"path": "big_plain.glb"})
        print(f"[{'ok  ' if ok else 'fail'}] preview big_plain.glb（无贴图）: {plain_text.splitlines()[0][:150]}")
        if not ok:
            failures.append(f"无贴图场景预览失败：{plain_text}")
        else:
            plain_names = shard_names(plain_text)
            plain_payload, plain_sizes = assemble(plain_names)
            plain_triangles = (
                sum(len(m.get("indices") or []) for m in plain_payload.get("meshes") or []) // 3
            )
            if plain_triangles < 24000:
                failures.append(f"无贴图载荷只有 {plain_triangles} 三角形")
            for name, size in zip(plain_names, plain_sizes):
                if size >= budget(name):
                    failures.append(f"{name} 有 {size} 字节，越过预算 {budget(name)}")
            if plain_payload.get("textures"):
                failures.append("无贴图夹具却拼出了贴图")
            print(
                f"[ok  ] 无贴图场景分 {len(plain_names)} 片，拼回 {plain_triangles} 三角形，"
                f"最大 {max(plain_sizes)} 字节 < {min(budget(n) for n in plain_names)}"
            )

        # --- 完整预览留一份副本，给 viewer 侧的合并/容错用例用 ---
        # 不复用同名目录：后面的 maxTriangles 用例会覆盖 big.preview.*，直接删目录
        # 又属于破坏性操作，所以每轮开一个新目录。
        full_dir = WS / f"full-{int(time.time())}"
        full_dir.mkdir(parents=True, exist_ok=True)
        for name in shard_names(text_of_big):
            shutil.copy2(WS / name, full_dir / name)
        shutil.copy2(WS / "big.preview.html", full_dir / "big.preview.html")
        if node_ok():
            result = subprocess.run(
                ["node", str(ROOT / "dev" / "verify_viewer.js"), str(full_dir / "big.preview.html")],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            print(result.stdout.strip())
            if result.returncode != 0:
                failures.append("viewer 分片合并/容错/mime 回退实测失败（详见上方输出）")
            if result.stderr.strip():
                print(result.stderr.strip()[:600])
        else:
            failures.append("缺少 node，无法实测 viewer 侧分片合并与 mime 回退")

        # --- 单网格超预算：截断而不是丢光，也不撞写盘上限 ---
        ok, text = h.tool("preview_asset", {"path": "big.glb", "maxTriangles": 1000})
        print(f"[{'ok  ' if ok else 'fail'}] preview big.glb maxTriangles=1000: {text.splitlines()[0][:130]}")
        if not ok:
            failures.append(f"三角预算降级失败：{text}")
        else:
            if "截断" not in text:
                failures.append("maxTriangles 未生效：没有截断记录")
            payload, _ = assemble(shard_names(text))
            tri = sum(len(m.get("indices") or []) for m in payload.get("meshes") or []) // 3
            if tri != 1000:
                failures.append(f"截断后应为 1000 个三角形，实际 {tri}")
            else:
                print("[ok  ] 单网格降级生效：26450 → 1000 三角形后仍能落盘")

        # --- 小资产（单分片路径）也要正常 ---
        ok, text = h.tool("preview_asset", {"path": "cube.gltf"})
        print(f"[{'ok  ' if ok else 'fail'}] preview cube.gltf: {text.splitlines()[0][:140]}")
        if not ok:
            failures.append(f"小资产预览失败：{text}")
        else:
            names = shard_names(text)
            payload, _ = assemble(names)
            if not payload.get("textures"):
                failures.append("cube 夹具的贴图没进载荷")
            else:
                print(f"[ok  ] 小资产单片路径正常（{len(names)} 片，贴图在）")

        if h.violations:
            failures.extend(h.violations)
        else:
            print(f"[ok  ] 全程无超限帧（上限 {FRAME_LIMIT} 字节）")
    finally:
        h.close()

    if failures:
        print("\n失败项：")
        for item in failures:
            print(f" - {item}")
        return 1
    print("\n预览分片实测通过")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
