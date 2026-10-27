"""生成一个最小 glTF 立方体夹具：带法线、UV、索引、内嵌贴图与两级节点层级。

单文件（buffer / image 都用 data URI），方便直接丢进工作区联调。
"""

import base64
import json
import struct
import zlib
from pathlib import Path

# 立方体：8 个顶点 × (position 3 + normal 3 + uv 2) = 32 个 float，24 个索引
POS_NRM = [
    # 前 (+Z)
    (-0.5, -0.5, 0.5, 0.0, 0.0, 1.0), (0.5, -0.5, 0.5, 0.0, 0.0, 1.0),
    (0.5, 0.5, 0.5, 0.0, 0.0, 1.0), (-0.5, 0.5, 0.5, 0.0, 0.0, 1.0),
    # 后 (-Z)
    (0.5, -0.5, -0.5, 0.0, 0.0, -1.0), (-0.5, -0.5, -0.5, 0.0, 0.0, -1.0),
    (-0.5, 0.5, -0.5, 0.0, 0.0, -1.0), (0.5, 0.5, -0.5, 0.0, 0.0, -1.0),
]
UVS = [(0.0, 1.0), (1.0, 1.0), (1.0, 0.0), (0.0, 0.0)] * 2
INDICES = [
    0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7, 1, 4, 7, 1, 7, 2,
    5, 0, 3, 5, 3, 6, 3, 2, 7, 3, 7, 6, 5, 4, 1, 5, 1, 0,
]


def png(width: int, height: int, rgb: tuple[int, int, int]) -> bytes:
    """不引 Pillow：手搓一张纯色 PNG（足够验证贴图链路）。"""
    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    out = Path(__file__).resolve().parent.parent / ".tmp-ws"
    out.mkdir(parents=True, exist_ok=True)

    positions, normals, uvs = [], [], []
    for i, (px, py, pz, nx, ny, nz) in enumerate(POS_NRM):
        positions += [px, py, pz]
        normals += [nx, ny, nz]
        uvs += list(UVS[i])

    blobs, views, accessors = [], [], []

    def add_view(payload: bytes, target: int) -> int:
        while len(b"".join(blobs)) % 4:
            blobs.append(b"\x00")
        offset = sum(len(b) for b in blobs)
        blobs.append(payload)
        views.append({"buffer": 0, "byteOffset": offset, "byteLength": len(payload), "target": target})
        return len(views) - 1

    def add_accessor(view: int, kind: str, count: int, components: int) -> int:
        accessors.append(
            {
                "bufferView": view,
                "componentType": 5126 if kind == "f" else 5123,
                "count": count,
                "type": {1: "SCALAR", 2: "VEC2", 3: "VEC3"}[components],
                "min": [-0.5] * components if kind == "f" and components == 3 else None,
                "max": [0.5] * components if kind == "f" and components == 3 else None,
            }
        )
        acc = accessors[-1]
        accessors[-1] = {k: v for k, v in acc.items() if v is not None}
        return len(accessors) - 1

    pos_acc = add_accessor(add_view(struct.pack(f"<{len(positions)}f", *positions), 34962), "f", 8, 3)
    nrm_acc = add_accessor(add_view(struct.pack(f"<{len(normals)}f", *normals), 34962), "f", 8, 3)
    uv_acc = add_accessor(add_view(struct.pack(f"<{len(uvs)}f", *uvs), 34962), "f", 8, 2)
    idx_acc = add_accessor(add_view(struct.pack(f"<{len(INDICES)}H", *INDICES), 34963), "h", len(INDICES), 1)
    image_view = add_view(png(8, 8, (0x33, 0x99, 0xDD)), 0)

    buffer = b"".join(blobs)
    doc = {
        "asset": {"version": "2.0", "generator": "asset3d-fixture"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        # 两级层级：Pivot 带平移，Cube 带旋转缩放——用来验证 TRS 而不是烘焙世界坐标
        "nodes": [
            {"name": "Pivot", "children": [1], "translation": [0.0, 0.5, 0.0]},
            {"name": "Cube", "mesh": 0, "rotation": [0.0, 0.3826834, 0.0, 0.9238795],
             "scale": [1.0, 1.0, 1.0]},
        ],
        "meshes": [
            {
                "name": "CubeMesh",
                "primitives": [
                    {
                        "attributes": {"POSITION": pos_acc, "NORMAL": nrm_acc, "TEXCOORD_0": uv_acc},
                        "indices": idx_acc,
                        "material": 0,
                        "mode": 4,
                    }
                ],
            }
        ],
        "materials": [
            {
                "name": "Checker",
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
        "images": [{"name": "checker", "bufferView": image_view, "mimeType": "image/png"}],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [
            {
                "byteLength": len(buffer),
                "uri": "data:application/octet-stream;base64," + base64.b64encode(buffer).decode(),
            }
        ],
    }

    target = out / "cube.gltf"
    target.write_text(json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"wrote {target} ({target.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
