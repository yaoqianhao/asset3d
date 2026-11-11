"""生成 VRM 夹具：一个 VRM 1.0（`.vrm`）、一个 VRM 0.x（存成 `.glb`）。

为什么两个都要：

  * `.vrm` 扩展名走的是「按名字认」的分支，但如果只测它，就无法证明
    「VRM 存成 `.glb` 时也认得出」——不少导出器（Blender 的 VRM 插件、
    UniVRM 的某些批处理）产出的文件名与扩展名就是 `.glb`，只按扩展名判会漏；
  * 两种规范的元信息位置完全不同：1.0 是 `extensions.VRMC_vrm` + 对象形式的
    `humanBones` + 材质扩展 `VRMC_materials_mtoon`；0.x 是 `extensions.VRM` +
    数组形式的 `humanBones` + 顶层 `materialProperties[]`。同一份解析代码要
    在两种写法上都拿到人头名/骨映射/MToon 名字，只有各给一个夹具才验得出来。

几何直接复用 `make_fixture.py` 的立方体（同源），只往 JSON 里塞 VRM 扩展，
所以 VRM 与普通 GLB 的差异**只有扩展块**——解析结果里非 VRM 字段应当逐项相同，
差异只出现在 `vrm` 摘要与 `issues` 上。

用法：
    python dev/make_vrm_fixture.py
"""

from __future__ import annotations

import base64
import json
import sys
from pathlib import Path


def load_cube() -> tuple[dict, bytes]:
    """取 `cube.gltf` 的 JSON 与 BIN 负载（data URI 拆出来）。"""
    dev_dir = Path(__file__).resolve().parent
    out = dev_dir.parent / ".tmp-ws"
    out.mkdir(parents=True, exist_ok=True)

    sys.path.insert(0, str(dev_dir))
    import make_fixture

    make_fixture.main()

    doc = json.loads((out / "cube.gltf").read_text(encoding="utf-8"))
    uri = doc["buffers"][0].pop("uri")
    assert uri.startswith("data:"), uri[:32]
    payload = base64.b64decode(uri.split(",", 1)[1])
    assert len(payload) == doc["buffers"][0]["byteLength"], (
        len(payload),
        doc["buffers"][0]["byteLength"],
    )
    return doc, payload


def with_vrm1(doc: dict) -> dict:
    """VRM 1.0：`VRMC_vrm` 扩展 + 材质级 `VRMC_materials_mtoon`。"""
    doc = json.loads(json.dumps(doc))
    doc["extensionsUsed"] = ["VRMC_vrm", "VRMC_materials_mtoon", "KHR_materials_unlit"]
    doc["extensions"] = {
        "VRMC_vrm": {
            "specVersion": "1.0",
            "meta": {
                "name": "Alicia",
                "authors": ["夹具作者"],
                "licenseUrl": "https://vrm.dev/licenses/1.0/",
            },
            # 1.0 的 humanBones 是对象；节点下标指向夹具里真实存在的两级节点。
            "humanoid": {
                "humanBones": {
                    "hips": {"node": 0},
                    "spine": {"node": 1},
                }
            },
            "expressions": {
                "preset": {"happy": {}, "blink": {}},
                "custom": {"smirk": {}},
            },
            "lookAt": {"type": "bone"},
            "springBone": {
                "springs": [{"name": "hair"}, {"name": "skirt"}],
            },
        }
    }
    doc["materials"][0].setdefault("extensions", {})["VRMC_materials_mtoon"] = {
        "specVersion": "1.0",
        "shadingToony": 0.9,
        "outlineWidthMode": "worldCoordinates",
    }
    return doc


def with_vrm0(doc: dict) -> dict:
    """VRM 0.x：`VRM` 扩展，骨映射是数组、MToon 在顶层 `materialProperties`。"""
    doc = json.loads(json.dumps(doc))
    doc["extensionsUsed"] = ["VRM"]
    doc["extensions"] = {
        "VRM": {
            "specVersion": "0.0",
            "meta": {
                "title": "AliciaZero",
                "author": "零号作者",
            },
            # 0.x 的 humanBones 是数组，元素形如 { bone, node }。
            "humanoid": {
                "humanBones": [
                    {"bone": "hips", "node": 0},
                    {"bone": "spine", "node": 1},
                ]
            },
            "materialProperties": [
                {"name": "Body", "shader": "VRM/MToon"},
                {"name": "Plain", "shader": "Standard"},
            ],
            "blendShapeMaster": {"blendShapeGroups": [{}, {}, {}]},
            "secondaryAnimation": {"boneGroups": [{}]},
            "firstPerson": {"firstPersonBone": 1},
        }
    }
    return doc


def main() -> None:
    dev_dir = Path(__file__).resolve().parent
    out = dev_dir.parent / ".tmp-ws"

    sys.path.insert(0, str(dev_dir))
    from make_glb_fixture import pack_glb

    base, payload = load_cube()

    v1 = pack_glb(with_vrm1(base), payload)
    target1 = out / "alicia.vrm"
    target1.write_bytes(v1)
    doc1 = json.loads(v1[20 : 20 + int.from_bytes(v1[12:16], "little")].decode("utf-8"))
    body1 = doc1["extensions"]["VRMC_vrm"]
    assert body1["specVersion"] == "1.0"
    assert body1["meta"]["name"] == "Alicia"

    v0 = pack_glb(with_vrm0(base), payload)
    target0 = out / "alicia_v0.glb"  # 刻意用 .glb：验证「不靠扩展名也认得出 VRM」
    target0.write_bytes(v0)
    doc0 = json.loads(v0[20 : 20 + int.from_bytes(v0[12:16], "little")].decode("utf-8"))
    assert "VRM" in doc0["extensions"]
    assert "VRMC_vrm" not in doc0.get("extensions", {})

    print(f"wrote {target1} ({len(v1)} bytes, VRM 1.0, name={body1['meta']['name']})")
    print(f"wrote {target0} ({len(v0)} bytes, VRM 0.x 存成 .glb)")


if __name__ == "__main__":
    main()
