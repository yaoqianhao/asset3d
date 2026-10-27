"""生成一个真正的二进制 GLB 夹具，并与 `make_fixture.py` 的 `.gltf` 保持同源。

为什么要有它：前面的 `cube.gltf` 是 JSON + data URI，走的 `parse_container`
的**非** glTF 分支；GLB 容器（magic / 版本 / chunk 头 / 4 字节对齐 / BIN chunk
承载几何与内嵌 PNG）是另一条分支，之前没有任何夹具覆盖它。

做法：
  1. 复用 `make_fixture.py` 生成 `cube.gltf`（立方体 + 法线 + UV + 索引 +
     内嵌 PNG + 两级节点层级），保证两个夹具的几何逐字节同源；
  2. 把 `buffers[0].uri` 的 data URI 拆出来塞进 BIN chunk，其余 JSON 原样保留；
  3. 按 GLB 2.0 容器规范拼装：12 字节头 + JSON chunk + BIN chunk，两段都补齐
     到 4 字节（JSON 补空格 0x20，BIN 补 0x00——补位字节必须是合法的，
     否则解析器会把填充当成 JSON 尾部垃圾或越界的多余几何）；
  4. 回读自检：头、chunk 长度、`bufferViews` 是否都落在 BIN chunk 内、贴图
     bufferView 里是不是真的 PNG —— 不做自检的夹具，坏了只会让下游用例
     变成一个看不懂的断言失败。

用法：
    python dev/make_glb_fixture.py
"""

from __future__ import annotations

import base64
import json
import struct
import sys
from pathlib import Path

GLB_MAGIC = 0x46546C67  # "glTF" 小端
CHUNK_JSON = 0x4E4F534A  # "JSON"
CHUNK_BIN = 0x004E4942  # "BIN\0"
PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


def pad(data: bytes, fill: bytes) -> bytes:
    return data + fill * ((4 - len(data) % 4) % 4)


def pack_glb(doc: dict, bin_payload: bytes) -> bytes:
    """把「已去掉 buffer.uri」的 glTF JSON 与二进制负载打成 GLB。"""
    json_bytes = pad(json.dumps(doc, ensure_ascii=False, separators=(",", ":")).encode("utf-8"), b" ")
    bin_bytes = pad(bin_payload, b"\x00")
    total = 12 + 8 + len(json_bytes) + 8 + len(bin_bytes)
    out = bytearray()
    out += struct.pack("<III", GLB_MAGIC, 2, total)
    out += struct.pack("<II", len(json_bytes), CHUNK_JSON) + json_bytes
    out += struct.pack("<II", len(bin_bytes), CHUNK_BIN) + bin_bytes
    return bytes(out)


def verify(bytes_: bytes, expect_bin: int) -> dict:
    """回读自检：坏了就在这里炸，而不是让下游断言变成谜语。"""
    magic, version, total = struct.unpack("<III", bytes_[:12])
    assert magic == GLB_MAGIC, f"magic 不对：{magic:#x}"
    assert version == 2, f"版本不对：{version}"
    assert total == len(bytes_), f"头部长度 {total} 与文件长度 {len(bytes_)} 不一致"

    offset = 12
    chunks: list[tuple[int, bytes]] = []
    while offset + 8 <= total:
        length, kind = struct.unpack("<II", bytes_[offset : offset + 8])
        start = offset + 8
        chunks.append((kind, bytes_[start : start + length]))
        offset = start + length + ((4 - length % 4) % 4)
    assert [c[0] for c in chunks] == [CHUNK_JSON, CHUNK_BIN], [hex(c[0]) for c in chunks]
    assert len(chunks[1][1]) >= expect_bin, (len(chunks[1][1]), expect_bin)

    doc = json.loads(chunks[0][1].decode("utf-8"))  # 尾部空格是无害填充
    bin_chunk = chunks[1][1]
    for i, view in enumerate(doc["bufferViews"]):
        end = view.get("byteOffset", 0) + view["byteLength"]
        assert end <= len(bin_chunk), f"bufferView[{i}] 越出 BIN chunk：{end} > {len(bin_chunk)}"
    png_view = doc["bufferViews"][doc["images"][0]["bufferView"]]
    head = bin_chunk[png_view.get("byteOffset", 0) :][: len(PNG_MAGIC)]
    assert head == PNG_MAGIC, f"贴图 bufferView 不是 PNG：{head!r}"
    assert "uri" not in doc["buffers"][0], "buffers[0] 仍带 uri，几何没搬进 BIN chunk"
    return doc


def main() -> None:
    dev_dir = Path(__file__).resolve().parent
    out = dev_dir.parent / ".tmp-ws"
    out.mkdir(parents=True, exist_ok=True)

    sys.path.insert(0, str(dev_dir))
    import make_fixture

    make_fixture.main()  # 产出 .tmp-ws/cube.gltf，两边几何同源

    gltf_path = out / "cube.gltf"
    doc = json.loads(gltf_path.read_text(encoding="utf-8"))
    uri = doc["buffers"][0].pop("uri")
    assert uri.startswith("data:"), uri[:32]
    payload = base64.b64decode(uri.split(",", 1)[1])
    assert len(payload) == doc["buffers"][0]["byteLength"], (
        len(payload),
        doc["buffers"][0]["byteLength"],
    )

    glb = pack_glb(doc, payload)
    target = out / "cube.glb"
    target.write_bytes(glb)

    checked = verify(glb, len(payload))
    prim = checked["meshes"][0]["primitives"][0]
    print(
        f"wrote {target} ({len(glb)} bytes, bin={len(payload)}, "
        f"accessors={len(checked['accessors'])}, materials={len(checked['materials'])}, "
        f"attrs={sorted(prim['attributes'])})"
    )
    print(f"header magic={GLB_MAGIC:#x} version=2 length={len(glb)}")


if __name__ == "__main__":
    main()
