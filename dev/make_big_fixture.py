"""生成「大三角 + 带贴图」的 GLB 夹具：115×115 网格 = 26450 个三角形。

为什么用 GLB 而不是 .gltf：宿主 `host/fs.read` 只回 720 KB 原始字节
（1 MiB 帧 / base64 4:3），.gltf 的 data URI 会把二进制膨胀 4/3，稍大一点就读不进来。
GLB 的 BIN chunk 是裸字节，同样几何能塞进限制内，正好用来验证预览分片。

夹具特征（专门压这几条缺陷）：
- 单网格 26450 三角形：JSON 载荷远超 1 MiB，预览必须分片才能落盘（缺陷①）
- 单个超大网格：`maxTriangles` 小于它时只能截断，不能整网格丢光（缺陷④）
- 内嵌 PNG 贴图 + UV：贴图链路可用（缺陷③⑤）
"""

import math
import struct
import zlib
from pathlib import Path

W = 115
H = 115

POSITIONS = []
NORMALS = []
UVS = []
INDICES = []


def png(width: int, height: int) -> bytes:
    """64×64 棋盘 PNG，用来确认贴图真的贴上了而不是纯色。"""
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # filter type 0
        for x in range(width):
            light = ((x // 8) + (y // 8)) % 2 == 0
            raw += bytes((235, 235, 235) if light else (40, 90, 170))

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def height(x: float, y: float) -> float:
    return 0.12 * math.sin(x * 6.0) * math.cos(y * 6.0)


for j in range(H + 1):
    for i in range(W + 1):
        x = -1.0 + 2.0 * i / W
        y = -1.0 + 2.0 * j / H
        z = height(x, y)
        POSITIONS += [x, y, z]
        dx = 0.12 * 6.0 * math.cos(x * 6.0) * math.cos(y * 6.0)
        dy = -0.12 * 6.0 * math.sin(x * 6.0) * math.sin(y * 6.0)
        length = math.sqrt(dx * dx + dy * dy + 1.0)
        NORMALS += [-dx / length, -dy / length, 1.0 / length]
        UVS += [i / W, 1.0 - j / H]

row = W + 1
for j in range(H):
    for i in range(W):
        a = j * row + i
        b = a + 1
        c = a + row
        d = c + 1
        INDICES += [a, b, d, a, d, c]


def main() -> None:
    out = Path(__file__).resolve().parent.parent / ".tmp-ws"
    out.mkdir(parents=True, exist_ok=True)

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

    pos_view = add_view(struct.pack(f"<{len(POSITIONS)}f", *POSITIONS), 34962)
    nrm_view = add_view(struct.pack(f"<{len(NORMALS)}f", *NORMALS), 34962)
    uv_view = add_view(struct.pack(f"<{len(UVS)}f", *UVS), 34962)
    idx_view = add_view(struct.pack(f"<{len(INDICES)}H", *INDICES), 34963)
    img_view = add_view(png(64, 64), None)

    vertex_count = (W + 1) * (H + 1)
    accessors = [
        {"bufferView": pos_view, "componentType": 5126, "count": vertex_count, "type": "VEC3",
         "min": [-1.0, -1.0, -0.12], "max": [1.0, 1.0, 0.12]},
        {"bufferView": nrm_view, "componentType": 5126, "count": vertex_count, "type": "VEC3"},
        {"bufferView": uv_view, "componentType": 5126, "count": vertex_count, "type": "VEC2"},
        {"bufferView": idx_view, "componentType": 5123, "count": len(INDICES), "type": "SCALAR"},
    ]

    binary = b"".join(blobs)
    doc = {
        "asset": {"version": "2.0", "generator": "asset3d-big-fixture"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Terrain", "mesh": 0}],
        "meshes": [{
            "name": "TerrainMesh",
            "primitives": [{
                "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
                "indices": 3,
                "material": 0,
                "mode": 4,
            }],
        }],
        "materials": [{
            "name": "Checker",
            "pbrMetallicRoughness": {
                "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                "baseColorTexture": {"index": 0},
                "metallicFactor": 0.0,
                "roughnessFactor": 0.75,
            },
            "doubleSided": True,
        }],
        "textures": [{"source": 0, "sampler": 0}],
        "samplers": [{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}],
        "images": [{"name": "checker", "bufferView": img_view, "mimeType": "image/png"}],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{"byteLength": len(binary)}],
    }

    import json

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

    # 无贴图版本：同一份几何、材质不带贴图。缺陷①要求「无贴图场景同样要分片」，
    # 只有跑一遍这个变体才能确认分片逻辑不是靠贴图分支撑起来的。
    plain = json.loads(json_bytes.decode("utf-8"))
    plain["materials"][0]["pbrMetallicRoughness"].pop("baseColorTexture", None)
    for key in ("textures", "images", "samplers"):
        plain.pop(key, None)
    plain_json = json.dumps(plain, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    plain_json += b" " * ((4 - len(plain_json) % 4) % 4)
    plain_total = 12 + 8 + len(plain_json) + 8 + len(bin_bytes)
    plain_glb = (
        struct.pack("<III", 0x46546C67, 2, plain_total)
        + struct.pack("<II", len(plain_json), 0x4E4F534A)
        + plain_json
        + struct.pack("<II", len(bin_bytes), 0x004E4942)
        + bin_bytes
    )
    plain_target = out / "big_plain.glb"
    plain_target.write_bytes(plain_glb)
    print(f"wrote {plain_target} ({plain_target.stat().st_size} bytes, 无贴图)")

    target = out / "big.glb"
    target.write_bytes(glb)
    triangles = len(INDICES) // 3
    print(
        f"wrote {target} ({target.stat().st_size} bytes, vertices={vertex_count}, "
        f"triangles={triangles}, bin={len(binary)})"
    )


if __name__ == "__main__":
    main()
