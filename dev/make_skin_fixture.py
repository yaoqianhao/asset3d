"""生成带蒙皮 + 动画的 GLB 夹具（skin.glb）。

为什么单独做：缺陷⑥说的是 `estimate_binary_bytes` 漏算蒙皮/动画的头部开销，
而上一批夹具全是纯静态网格——没有骨骼、没有动画，这个估算就算错了也测不出来。
这个夹具故意带上 2 个关节、每关节一份逆绑定矩阵、一条旋转动画，
好让「估算值 vs 真实落盘体积」的对比真的能压到那条缺陷上。

同时它也是缺陷②的对照面：preview 必须**不**载入动画，而 export 必须**照常**带上。
"""

import json
import math
import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WS = ROOT / ".tmp-ws"


def pack(target: list[bytearray], blobs: list[bytes], data: bytes, align: int = 4) -> None:
    pad = (align - len(data) % align) % align
    target.append(data + b"\x00" * pad)


def main() -> None:
    WS.mkdir(parents=True, exist_ok=True)
    blobs: list[bytes] = []
    views: list[dict] = []

    def add(data: bytes, target: int | None = None) -> int:
        offset = sum(len(b) for b in blobs)
        blobs.append(data)
        view = {"buffer": 0, "byteOffset": offset, "byteLength": len(data)}
        if target is not None:
            view["target"] = target
        views.append(view)
        return len(views) - 1

    # 一条 4 顶点、2 三角形的细长面片：变形看得见，体积又小。
    positions = [(0.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.4, 0.0, 0.0), (0.4, 1.0, 0.0)]
    normals = [(0.0, 0.0, 1.0)] * 4
    uvs = [(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)]
    indices = [0, 1, 2, 2, 1, 3]
    # 上半截交给关节 2，下半截交给关节 1，weights 归一
    joints = [(0, 1, 0, 0)] * 4
    weights = [(0.0, 1.0, 0.0, 0.0), (1.0, 0.0, 0.0, 0.0), (0.0, 1.0, 0.0, 0.0), (1.0, 0.0, 0.0, 0.0)]

    pos_view = add(b"".join(struct.pack("<3f", *p) for p in positions), 34962)
    nrm_view = add(b"".join(struct.pack("<3f", *n) for n in normals), 34962)
    uv_view = add(b"".join(struct.pack("<2f", *t) for t in uvs), 34962)
    idx_view = add(b"".join(struct.pack("<H", i) for i in indices), 34963)
    jnt_view = add(b"".join(struct.pack("<4B", *j) for j in joints), 34962)
    wgt_view = add(b"".join(struct.pack("<4f", *w) for w in weights), 34962)

    # 逆绑定矩阵：关节 1 原点、关节 2 在 y=0.5，所以第二个 IBM 需要 -0.5 的平移。
    ibm = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, -0.5, 0.0, 1.0,
    ]
    ibm_view = add(b"".join(struct.pack("<16f", *ibm[i * 16:(i + 1) * 16]) for i in range(2)))

    # 一条绕 Z 转 90° 的关键帧动画：时间 0 / 1 秒，值是两个四元数。
    angles = [0.0, math.pi / 2]
    times = [0.0, 1.0]
    time_view = add(b"".join(struct.pack("<f", t) for t in times))
    quats = []
    for angle in angles:
        quats += [0.0, 0.0, math.sin(angle / 2), math.cos(angle / 2)]
    quat_view = add(b"".join(struct.pack("<4f", *quats[i * 4:(i + 1) * 4]) for i in range(2)))

    doc = {
        "asset": {"version": "2.0", "generator": "asset3d-skin-fixture"},
        "scene": 0,
        "scenes": [{"nodes": [0, 1]}],
        # 刻意给承载网格的节点挂上非单位 TRS：顶点若被错误地烘成世界坐标，
        # 导出结果就会带上这段位移/旋转，harness 用「顶点仍在 0..0.4 × 0..1 内」抓住它。
        "nodes": [
            {
                "name": "SkinnedMesh",
                "mesh": 0,
                "skin": 0,
                "translation": [0.25, 0.0, -0.5],
                "rotation": [0.0, 0.25881904510252074, 0.0, 0.9659258262890683],
            },
            {"name": "Root", "children": [2]},
            {"name": "Spine", "translation": [0.0, 0.5, 0.0]},
        ],
        "meshes": [{
            "name": "Strip",
            "primitives": [{
                "attributes": {
                    "POSITION": 0,
                    "NORMAL": 1,
                    "TEXCOORD_0": 2,
                    "JOINTS_0": 4,
                    "WEIGHTS_0": 5,
                },
                "indices": 3,
                "material": 0,
                "mode": 4,
            }],
        }],
        "materials": [{
            "name": "Plain",
            "pbrMetallicRoughness": {
                "baseColorFactor": [0.9, 0.6, 0.2, 1.0],
                "metallicFactor": 0.0,
                "roughnessFactor": 0.8,
            },
            "doubleSided": True,
        }],
        "skins": [{"name": "Rig", "joints": [1, 2], "inverseBindMatrices": 6, "skeleton": 1}],
        "animations": [{
            "name": "Wave",
            "samplers": [{"input": 7, "output": 8, "interpolation": "LINEAR"}],
            "channels": [{"sampler": 0, "target": {"node": 2, "path": "rotation"}}],
        }],
        "accessors": [
            {"bufferView": pos_view, "componentType": 5126, "count": 4, "type": "VEC3",
             "min": [-0.0, 0.0, 0.0], "max": [0.4, 1.0, 0.0]},
            {"bufferView": nrm_view, "componentType": 5126, "count": 4, "type": "VEC3"},
            {"bufferView": uv_view, "componentType": 5126, "count": 4, "type": "VEC2"},
            {"bufferView": idx_view, "componentType": 5123, "count": len(indices), "type": "SCALAR"},
            {"bufferView": jnt_view, "componentType": 5121, "count": 4, "type": "VEC4"},
            {"bufferView": wgt_view, "componentType": 5126, "count": 4, "type": "VEC4"},
            {"bufferView": ibm_view, "componentType": 5126, "count": 2, "type": "MAT4"},
            {"bufferView": time_view, "componentType": 5126, "count": 2, "type": "SCALAR",
             "min": [0.0], "max": [1.0]},
            {"bufferView": quat_view, "componentType": 5126, "count": 2, "type": "VEC4"},
        ],
        "bufferViews": views,
        "buffers": [{"byteLength": sum(len(b) for b in blobs)}],
    }

    json_bytes = json.dumps(doc, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    json_bytes += b" " * ((4 - len(json_bytes) % 4) % 4)
    binary = b"".join(blobs)
    binary += b"\x00" * ((4 - len(binary) % 4) % 4)
    total = 12 + 8 + len(json_bytes) + 8 + len(binary)
    glb = (
        struct.pack("<III", 0x46546C67, 2, total)
        + struct.pack("<II", len(json_bytes), 0x4E4F534A)
        + json_bytes
        + struct.pack("<II", len(binary), 0x004E4942)
        + binary
    )
    target = WS / "skin.glb"
    target.write_bytes(glb)
    print(
        f"wrote {target} ({target.stat().st_size} bytes, 2 关节 + 1 条动画 + 节点 TRS)"
    )


if __name__ == "__main__":
    main()
