"""生成「超过旧宿主单帧上限」的大资产夹具：贴图由随机噪声构成，压不下去。

为什么单独做：0.2.0 的瓶颈是 `host/fs.read` 一次只能带回约 720 KiB（1 MiB 帧 / base64
4:3）。`big.glb`（26k 三角形）只有约 590 KB，撞不到这条线；要覆盖「分片读」，夹具必须
真的超过它。用随机噪声 PNG 撑体积是最省事的办法：zlib 压缩率接近 1，尺寸可控且不
依赖几何复杂度（几何越复杂，解析越慢，测的就不是 IO 了）。

产出（都在 .tmp-ws/）：
- `huge_1m.glb` ≈ 1.25 MiB：刚过 720 KiB，验证「旧上限之上、2 MiB 之下」
- `huge_3m.glb` ≈ 3.2 MiB：超过 2 MiB，验证多片（≥6 次读）与分片写
"""

from __future__ import annotations

import json
import os
import struct
import zlib
from pathlib import Path

GRID = 16  # 16×16 网格 = 512 个三角形，几何本身很小，体积全由贴图贡献


def noise_png(width: int, height: int, seed: int) -> bytes:
    """随机噪声 PNG。level=0 直接存储，压缩率≈1，体积可精确预估。"""
    rng = _Rng(seed)
    raw = bytearray()
    row_bytes = width * 3
    noise = rng.bytes(height * row_bytes)
    for y in range(height):
        raw.append(0)  # filter type 0
        start = y * row_bytes
        raw += noise[start : start + row_bytes]

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(raw), 0))
        + chunk(b"IEND", b"")
    )


class _Rng:
    """deterministic 伪随机：夹具必须每次生成完全一致，否则哈希断言会飘。"""

    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFF

    def bytes(self, count: int) -> bytes:
        out = bytearray()
        while len(out) < count:
            # xorshift32
            x = self.state or 0x9E3779B9
            x ^= (x << 13) & 0xFFFFFFFF
            x ^= x >> 17
            x ^= (x << 5) & 0xFFFFFFFF
            self.state = x & 0xFFFFFFFF
            out += struct.pack("<I", self.state)
        return bytes(out[:count])


def build_glb(tex_width: int, tex_height: int, seed: int) -> tuple[bytes, bytes]:
    positions: list[float] = []
    normals: list[float] = []
    uvs: list[float] = []
    indices: list[int] = []
    for j in range(GRID + 1):
        for i in range(GRID + 1):
            x = -1.0 + 2.0 * i / GRID
            y = -1.0 + 2.0 * j / GRID
            positions += [x, y, 0.0]
            normals += [0.0, 0.0, 1.0]
            uvs += [i / GRID, 1.0 - j / GRID]
    row = GRID + 1
    for j in range(GRID):
        for i in range(GRID):
            a = j * row + i
            b = a + 1
            c = a + row
            d = c + 1
            indices += [a, b, d, a, d, c]

    image = noise_png(tex_width, tex_height, seed)

    blobs: list[bytes] = []
    views: list[dict] = []

    def add_view(payload: bytes, target: int | None) -> int:
        while sum(len(b) for b in blobs) % 4:
            blobs.append(b"\x00")
        offset = sum(len(b) for b in blobs)
        blobs.append(payload)
        view = {"buffer": 0, "byteOffset": offset, "byteLength": len(payload)}
        if target is not None:
            view["target"] = target
        views.append(view)
        return len(views) - 1

    pos_view = add_view(struct.pack(f"<{len(positions)}f", *positions), 34962)
    nrm_view = add_view(struct.pack(f"<{len(normals)}f", *normals), 34962)
    uv_view = add_view(struct.pack(f"<{len(uvs)}f", *uvs), 34962)
    idx_view = add_view(struct.pack(f"<{len(indices)}H", *indices), 34963)
    img_view = add_view(image, None)

    count = (GRID + 1) * (GRID + 1)
    doc = {
        "asset": {"version": "2.0", "generator": "asset3d-huge-fixture"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Board", "mesh": 0}],
        "meshes": [
            {
                "name": "BoardMesh",
                "primitives": [
                    {
                        "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
                        "indices": 3,
                        "material": 0,
                        "mode": 4,
                    }
                ],
            }
        ],
        "materials": [
            {
                "name": "Noise",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                    "baseColorTexture": {"index": 0},
                    "metallicFactor": 0.0,
                    "roughnessFactor": 0.8,
                },
                "doubleSided": True,
            }
        ],
        "textures": [{"source": 0, "sampler": 0}],
        "samplers": [{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}],
        "images": [{"name": "noise", "bufferView": img_view, "mimeType": "image/png"}],
        "accessors": [
            {"bufferView": pos_view, "componentType": 5126, "count": count, "type": "VEC3",
             "min": [-1.0, -1.0, 0.0], "max": [1.0, 1.0, 0.0]},
            {"bufferView": nrm_view, "componentType": 5126, "count": count, "type": "VEC3"},
            {"bufferView": uv_view, "componentType": 5126, "count": count, "type": "VEC2"},
            {"bufferView": idx_view, "componentType": 5123, "count": len(indices), "type": "SCALAR"},
        ],
        "bufferViews": views,
        "buffers": [{"byteLength": sum(len(b) for b in blobs)}],
    }

    binary = b"".join(blobs)
    json_bytes = json.dumps(doc, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    json_bytes += b" " * ((4 - len(json_bytes) % 4) % 4)
    bin_bytes = binary + b"\x00" * ((4 - len(binary) % 4) % 4)
    total = 12 + 8 + len(json_bytes) + 8 + len(bin_bytes)
    glb = (
        struct.pack("<III", 0x46546C67, 2, total)
        + struct.pack("<II", len(json_bytes), 0x4E4F534A)
        + json_bytes
        + struct.pack("<II", len(bin_bytes), 0x004E4942)
        + bin_bytes
    )
    return glb, image


def main() -> None:
    out = Path(__file__).resolve().parent.parent / ".tmp-ws"
    out.mkdir(parents=True, exist_ok=True)
    # 贴图尺寸按目标体积反推：PNG ≈ w*h*3 + 行过滤字节 + 少量 zlib 头。
    specs = [
        ("huge_1m.glb", 600, 600, 0x51ED2701),
        ("huge_3m.glb", 1000, 1000, 0x7F4A7C15),
    ]
    for name, width, height, seed in specs:
        glb, image = build_glb(width, height, seed)
        target = out / name
        target.write_bytes(glb)
        digest = __import__("hashlib").sha256(image).hexdigest()
        print(
            f"wrote {target} ({target.stat().st_size} 字节, 贴图 {len(image)} 字节, "
            f"png-sha256={digest[:16]})"
        )


if __name__ == "__main__":
    main()
