//! VRM 专有内容的识别与降级说明。
//!
//! `.vrm` 不是新容器：它就是一份 **GLB**（VRM 0.x 与 VRM 1.0 都是）。
//! 因此导入链路完全复用 [`crate::gltf::import`]，本模块只做两件事：
//!
//! 1. **认出来**——从 glTF JSON 的 `extensions` / `extensionsUsed` 里读出
//!    `VRM`（0.x）或 `VRMC_vrm`（1.0），顺带收集 meta、humanoid 骨映射、
//!    MToon 材质、表情、spring bone 这些 VRM 独有的语义。
//! 2. **说清楚降级**——AIR 表达的是「几何 + 材质 + 骨骼 + 动画」，没有 toon
//!    光照模型、没有描边、没有表情（blendShape 已经作为 morph target 之外的
//!    普通网格顶点导入，不做表情合成）、没有物理摆裙。这些内容一律按普通
//!    glTF 语义解析，绝不因为「扩展不认识」或「骨名不是 glTF 标准名」而失败。
//!
//! 设计口径：**未知扩展只记录、不阻断**。VRM 文件里常见 `KHR_*` 之外的第三方
//! 扩展（`VRMC_springBone`、`VRMC_materials_mtoon`、`VRM` 自己……），逐个当
//! 错误报会让「能画出来的资产」被拒之门外，所以识别结果全部走 `issues`。

use serde_json::Value;

use crate::air::AirDoc;

/// VRM 0.x 的扩展名（写在 `extensions.VRM`）。
pub const EXT_VRM_0: &str = "VRM";
/// VRM 1.0 的扩展名（写在 `extensions.VRMC_vrm`）。
pub const EXT_VRM_1: &str = "VRMC_vrm";

/// VRM 扩展族：出现在 `extensionsUsed` 里就值得在报告里点名。
const VRM_FAMILY: [&str; 5] = [
    EXT_VRM_0,
    EXT_VRM_1,
    "VRMC_materials_mtoon",
    "VRMC_springBone",
    "VRMC_vrm_animation",
];

/// 规范版本。判定依据是**扩展名**（`VRM` ⇒ 0.x、`VRMC_vrm` ⇒ 1.0），
/// `specVersion` 只用于展示——个别导出器会把 0.x 写成 `0.0`、把 1.0 写成
/// `1.0`，也有写成 `0.99` 的预览版，按字符串判会误伤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VrmSpec {
    V0,
    V1,
}

impl VrmSpec {
    pub fn label(self) -> &'static str {
        match self {
            VrmSpec::V0 => "VRM 0.x",
            VrmSpec::V1 => "VRM 1.0",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            VrmSpec::V0 => EXT_VRM_0,
            VrmSpec::V1 => EXT_VRM_1,
        }
    }
}

/// 一根 humanoid 骨在场景图里的落点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VrmBone {
    /// VRM 规范里的骨名（`hips` / `leftUpperArm` / `spine`……）。
    pub bone: String,
    /// 对应 `nodes` 下标。VRM 0.x / 1.0 都是这个含义，只是 JSON 结构不同。
    pub node: u32,
}

/// 一次 VRM 识别的结果。字段刻意全部是「计数 + 少量名字」，便于直接拼进
/// 人类可读摘要；完整映射表留在 [`VrmInfo::bones`] 供需要时取用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VrmInfo {
    pub spec: VrmSpec,
    /// 文件里写的 `specVersion`（可能为空）。
    pub spec_version: String,
    /// 头像名（0.x 的 `meta.title` / 1.0 的 `meta.name`）。
    pub name: String,
    pub authors: Vec<String>,
    /// 授权说明（0.x 的 `licenseName` / 1.0 的 `licenseUrl`）。
    pub license: String,
    /// `extensionsUsed` 全量，保持文件顺序。
    pub extensions_used: Vec<String>,
    /// 其中属于 VRM 族的扩展。
    pub vrm_extensions: Vec<String>,
    /// 既不是 VRM 族、也不是本插件处理范围内的其它扩展。
    pub other_extensions: Vec<String>,
    /// humanoid 骨映射（有序，按文件里的顺序）。
    pub bones: Vec<VrmBone>,
    /// 使用 MToon 着色的材质名。
    pub mtoon_materials: Vec<String>,
    /// 表情数量（0.x `blendShapeGroups` / 1.0 `expressions`）。
    pub expression_count: usize,
    /// spring bone 链数量（0.x `boneGroups` / 1.0 `VRMC_springBone.springs`）。
    pub spring_chains: usize,
    /// 是否声明了 lookAt（第一人称视线）。
    pub look_at: bool,
    /// 内置缩略图（VRM 里是一张贴图的索引）；`None` 表示没声明。
    pub thumbnail: Option<u32>,
}

impl VrmInfo {
    /// 识别 glTF JSON 里的 VRM 内容；不是 VRM 就返回 `None`。
    ///
    /// 判据优先级：`extensions` 里真有 `VRM` / `VRMC_vrm` 对象 → 认；
    /// 只有 `extensionsUsed` 里列了名字（文件被裁掉了扩展体）→ 也认，
    /// 但 meta / humanoid 都是空的，摘要里会说明「仅声明未见实现」。
    pub fn detect(json: &Value) -> Option<Self> {
        let used = string_array(json.pointer("/extensionsUsed"));
        let extensions = json.get("extensions");

        let vrm0 = extensions.and_then(|value| value.get(EXT_VRM_0));
        let vrm1 = extensions.and_then(|value| value.get(EXT_VRM_1));

        // 1.0 优先：同一个文件不可能同时是两种规范，但万一被工具串过，1.0 的信息更全。
        let (spec, body) = match (vrm1, vrm0) {
            (Some(body), _) => (VrmSpec::V1, Some(body)),
            (None, Some(body)) => (VrmSpec::V0, Some(body)),
            (None, None) => {
                // 只声明没实现：仍然当成 VRM（否则用户看到「就是个普通 glb」无从排查）。
                if used.iter().any(|name| name == EXT_VRM_1) {
                    (VrmSpec::V1, None)
                } else if used.iter().any(|name| name == EXT_VRM_0) {
                    (VrmSpec::V0, None)
                } else {
                    return None;
                }
            }
        };

        let mut info = VrmInfo {
            spec,
            spec_version: String::new(),
            name: String::new(),
            authors: Vec::new(),
            license: String::new(),
            vrm_extensions: used
                .iter()
                .filter(|name| VRM_FAMILY.contains(&name.as_str()))
                .cloned()
                .collect(),
            other_extensions: used
                .iter()
                .filter(|name| !VRM_FAMILY.contains(&name.as_str()))
                .cloned()
                .collect(),
            extensions_used: used,
            bones: Vec::new(),
            mtoon_materials: Vec::new(),
            expression_count: 0,
            spring_chains: 0,
            look_at: false,
            thumbnail: None,
        };

        let Some(body) = body else {
            return Some(info);
        };

        info.spec_version = body
            .get("specVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // ---- meta ----
        if let Some(meta) = body.get("meta") {
            info.name = meta
                .get("name")
                .or_else(|| meta.get("title"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // 0.x 是单个 `author` 字符串，1.0 是 `authors` 数组；两种都收。
            if let Some(author) = meta.get("author").and_then(Value::as_str) {
                if !author.trim().is_empty() {
                    info.authors.push(author.trim().to_string());
                }
            }
            for author in string_array(meta.get("authors")) {
                if !info.authors.contains(&author) {
                    info.authors.push(author);
                }
            }
            info.license = meta
                .get("licenseUrl")
                .or_else(|| meta.get("licenseName"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            info.thumbnail = meta
                .get("thumbnailImage")
                .or_else(|| meta.get("texture"))
                .and_then(Value::as_u64)
                .map(|index| index as u32);
        }

        // ---- humanoid ----
        info.bones = match spec {
            // 0.x：`humanBones: [{ bone: "hips", node: 0 }, …]`
            VrmSpec::V0 => body
                .pointer("/humanoid/humanBones")
                .and_then(Value::as_array)
                .map(|bones| {
                    bones
                        .iter()
                        .filter_map(|entry| {
                            let bone = entry.get("bone").and_then(Value::as_str)?;
                            let node = entry.get("node").and_then(Value::as_u64)?;
                            Some(VrmBone {
                                bone: bone.to_string(),
                                node: node as u32,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            // 1.0：`humanBones: { hips: { node: 0 }, … }`
            VrmSpec::V1 => body
                .pointer("/humanoid/humanBones")
                .and_then(Value::as_object)
                .map(|bones| {
                    bones
                        .iter()
                        .filter_map(|(bone, entry)| {
                            let node = entry.get("node").and_then(Value::as_u64)?;
                            Some(VrmBone {
                                bone: bone.clone(),
                                node: node as u32,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };

        // ---- MToon 材质 ----
        match spec {
            // 0.x：MToon 参数在顶层 `materialProperties[]`，`shader` 以 `VRM/MToon` 结尾。
            VrmSpec::V0 => {
                let props = body
                    .get("materialProperties")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                for (index, prop) in props.iter().enumerate() {
                    let shader = prop.get("shader").and_then(Value::as_str).unwrap_or_default();
                    if !shader.to_ascii_lowercase().contains("mtoon") {
                        continue;
                    }
                    let name = prop
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("materialProperties[{index}]"));
                    info.mtoon_materials.push(name);
                }
            }
            // 1.0：MToon 挂在 glTF 材质自己的扩展上。
            VrmSpec::V1 => {
                let materials = json
                    .get("materials")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                for (index, material) in materials.iter().enumerate() {
                    let Some(_) = material.pointer("/extensions/VRMC_materials_mtoon") else {
                        continue;
                    };
                    let name = material
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("materials[{index}]"));
                    info.mtoon_materials.push(name);
                }
            }
        }

        // ---- 表情 / spring bone / lookAt ----
        match spec {
            VrmSpec::V0 => {
                info.expression_count = body
                    .pointer("/blendShapeMaster/blendShapeGroups")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                info.spring_chains = body
                    .pointer("/secondaryAnimation/boneGroups")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                info.look_at = body.get("firstPerson").is_some();
            }
            VrmSpec::V1 => {
                // 1.0 的 preset / custom 都是对象；两者都没有时视为无表情。
                let preset = body
                    .pointer("/expressions/preset")
                    .and_then(Value::as_object)
                    .map(serde_json::Map::len)
                    .unwrap_or(0);
                let custom = body
                    .pointer("/expressions/custom")
                    .and_then(Value::as_object)
                    .map(serde_json::Map::len)
                    .unwrap_or(0);
                info.expression_count = preset + custom;
                // spring bone 在 1.0 是独立扩展 `VRMC_springBone`。
                info.spring_chains = json
                    .pointer("/extensions/VRMC_springBone/springs")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                info.look_at = body.get("lookAt").is_some();
            }
        }

        Some(info)
    }

    /// 一行式摘要（`inspect_asset` 的 `VRM` 行）。
    pub fn summary_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(self.spec.label().to_string());
        if !self.spec_version.is_empty() {
            parts.push(format!("spec {}", self.spec_version));
        }
        if !self.name.is_empty() {
            parts.push(self.name.clone());
        }
        if !self.authors.is_empty() {
            parts.push(format!("作者 {}", self.authors.join("、")));
        }
        parts.push(format!("humanoid 骨 {}", self.bones.len()));
        parts.push(format!("MToon 材质 {}", self.mtoon_materials.len()));
        if self.expression_count > 0 {
            parts.push(format!("表情 {}", self.expression_count));
        }
        if self.spring_chains > 0 {
            parts.push(format!("spring bone 链 {}", self.spring_chains));
        }
        if self.look_at {
            parts.push("lookAt".to_string());
        }
        if let Some(texture) = self.thumbnail {
            parts.push(format!("缩略图 贴图 #{texture}"));
        }
        if !self.license.is_empty() {
            parts.push(format!("授权 {}", self.license));
        }
        parts.join("｜")
    }

    /// 降级说明：讲清楚「哪些 VRM 语义在 AIR 里没有对应物」。
    ///
    /// 这些话不是免责声明，而是排查依据——用户看到头像比原版「平淡/没有描边」，
    /// 需要立刻知道这是设计取舍，而不是解析器坏了。
    pub fn degrade_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.bones.is_empty() {
            lines.push(format!(
                "{} 未提供 humanoid 骨映射（扩展体缺失或文件被裁剪）：骨骼按 glTF 节点原样导入，重定向需自行按节点名匹配",
                self.spec.label()
            ));
        } else {
            lines.push(format!(
                "humanoid 骨映射 {} 根：AIR 不保存 humanoid / T-pose 语义，骨骼按 glTF 节点与蒙皮原样导入（节点名与骨名一一对应，重定向按名字匹配）",
                self.bones.len()
            ));
        }
        if !self.mtoon_materials.is_empty() {
            lines.push(format!(
                "MToon 材质 {} 个（{}）：按 glTF PBR 降级解析，toon 渐变/明暗分界/描边/光照模型不进 AIR，渲染色调会比原版更接近物理材质",
                self.mtoon_materials.len(),
                self.mtoon_materials
                    .iter()
                    .take(6)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("、")
            ));
        }
        if self.expression_count > 0 {
            lines.push(format!(
                "表情 {} 个：morph target 只作为网格变形数据导入，AIR 不保存表情名与权重控制，播放侧无法按名驱动",
                self.expression_count
            ));
        }
        if self.spring_chains > 0 {
            lines.push(format!(
                "spring bone 链 {} 条：属于 VRM 运行时物理，AIR 未定义物理节点，摆裙/头发需引擎侧自行实现",
                self.spring_chains
            ));
        }
        if self.look_at {
            lines.push(
                "lookAt（第一人称视线）为 VRM 专有绑定，AIR 未导入，需要引擎侧自行实现"
                    .to_string(),
            );
        }
        if !self.other_extensions.is_empty() {
            lines.push(format!(
                "另有 {} 个非 VRM 扩展按未知处理（不影响导入）：{}",
                self.other_extensions.len(),
                self.other_extensions.join("、")
            ));
        }
        lines
    }

    /// 把识别结果写进 [`AirDoc`]：摘要行挂到 `vrm`，降级说明进 `issues`。
    ///
    /// **只进 issues、不进 errors**：VRM 专有内容的缺失是能力边界，不是数据损坏，
    /// 把它当阻断级问题会让所有 VRM 资产都无法导出 AIR。
    pub fn apply(&self, doc: &mut AirDoc) {
        doc.vrm = Some(self.clone());
        doc.note(format!("VRM 内容：{}", self.summary_line()));
        for line in self.degrade_lines() {
            doc.note(line);
        }
    }
}

/// 从 `Value` 里取字符串数组，非字符串元素静默跳过（坏字段不值得让导入失败）。
fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// VRM 0.x：meta 用 `title`/`author`，humanoid 是数组，MToon 在 `materialProperties`。
    #[test]
    fn detects_vrm0_meta_humanoid_and_mtoon() {
        let json = json!({
            "asset": { "version": "2.0", "generator": "VRoid" },
            "extensionsUsed": ["VRM", "KHR_materials_unlit"],
            "extensions": {
                "VRM": {
                    "specVersion": "0.0",
                    "meta": { "title": "Alicia", "author": "Someone", "texture": 3 },
                    "humanoid": { "humanBones": [
                        { "bone": "hips", "node": 0 },
                        { "bone": "leftUpperArm", "node": 1 }
                    ]},
                    "blendShapeMaster": { "blendShapeGroups": [{}, {}, {}] },
                    "secondaryAnimation": { "boneGroups": [{}] },
                    "firstPerson": { "lookAt": {} },
                    "materialProperties": [
                        { "name": "Face", "shader": "VRM/MToon" },
                        { "name": "Body", "shader": "VRM/MToon" },
                        { "name": "Other", "shader": "Standard" }
                    ]
                }
            }
        });
        let info = VrmInfo::detect(&json).expect("应识别为 VRM");
        assert_eq!(info.spec, VrmSpec::V0);
        assert_eq!(info.spec_version, "0.0");
        assert_eq!(info.name, "Alicia");
        assert_eq!(info.authors, vec!["Someone".to_string()]);
        assert_eq!(info.bones.len(), 2);
        assert_eq!(info.bones[1].bone, "leftUpperArm");
        assert_eq!(info.bones[1].node, 1);
        assert_eq!(info.mtoon_materials, vec!["Face".to_string(), "Body".to_string()]);
        assert_eq!(info.expression_count, 3);
        assert_eq!(info.spring_chains, 1);
        assert!(info.look_at);
        assert_eq!(info.thumbnail, Some(3));
        assert_eq!(info.vrm_extensions, vec!["VRM".to_string()]);
        assert_eq!(info.other_extensions, vec!["KHR_materials_unlit".to_string()]);
    }

    /// VRM 1.0：meta 用 `name`/`authors`，humanoid 是对象，MToon 挂在材质扩展上，
    /// spring bone 是独立扩展。
    #[test]
    fn detects_vrm1_humanoid_mtoon_and_springs() {
        let json = json!({
            "asset": { "version": "2.0" },
            "extensionsUsed": ["VRMC_vrm", "VRMC_materials_mtoon", "VRMC_springBone"],
            "extensions": {
                "VRMC_vrm": {
                    "specVersion": "1.0",
                    "meta": { "name": "Robot", "authors": ["A", "B"], "licenseUrl": "https://x" },
                    "humanoid": { "humanBones": {
                        "hips": { "node": 2 },
                        "spine": { "node": 3 }
                    }},
                    "expressions": { "preset": { "happy": {} }, "custom": { "blink2": {} } },
                    "lookAt": { "type": "bone" }
                },
                "VRMC_springBone": { "springs": [{}, {}] }
            },
            "materials": [
                { "name": "Hair", "extensions": { "VRMC_materials_mtoon": {} } },
                { "name": "Skin" }
            ]
        });
        let info = VrmInfo::detect(&json).expect("应识别为 VRM 1.0");
        assert_eq!(info.spec, VrmSpec::V1);
        assert_eq!(info.spec_version, "1.0");
        assert_eq!(info.name, "Robot");
        assert_eq!(info.authors, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(info.license, "https://x");
        assert_eq!(info.bones.len(), 2, "humanBones 对象应逐键解出");
        assert_eq!(info.mtoon_materials, vec!["Hair".to_string()]);
        assert_eq!(info.expression_count, 2);
        assert_eq!(info.spring_chains, 2);
        assert!(info.look_at);
        assert_eq!(info.vrm_extensions.len(), 3);
        assert!(info.other_extensions.is_empty());
    }

    /// 只有 `extensionsUsed` 里点了名、没有扩展体：仍按 VRM 认，但骨映射为空，
    /// 摘要要明确说出「扩展体缺失」而不是让用户以为这是个普通 GLB。
    #[test]
    fn declared_without_body_is_still_vrm_but_explains() {
        let json = json!({
            "asset": { "version": "2.0" },
            "extensionsUsed": ["VRMC_vrm"]
        });
        let info = VrmInfo::detect(&json).expect("声明了也应识别");
        assert_eq!(info.spec, VrmSpec::V1);
        assert_eq!(info.spec_version, "");
        assert!(info.bones.is_empty());
        let lines = info.degrade_lines();
        assert!(
            lines[0].contains("未提供 humanoid 骨映射"),
            "{}",
            lines[0]
        );
    }

    /// 普通 glTF / GLB 不能被误判成 VRM。
    #[test]
    fn plain_gltf_is_not_vrm() {
        let json = json!({
            "asset": { "version": "2.0" },
            "extensionsUsed": ["KHR_materials_unlit"],
            "extensions": { "KHR_materials_unlit": {} }
        });
        assert!(VrmInfo::detect(&json).is_none());
        assert!(
            VrmInfo::detect(&json!({ "asset": { "version": "2.0" } })).is_none()
        );
    }

    /// 降级说明必须逐项点名，且**不许**变成阻断级问题（否则 VRM 一律导不出）。
    #[test]
    fn apply_only_reports_issues_never_errors() {
        let json = json!({
            "extensionsUsed": ["VRM"],
            "extensions": {
                "VRM": {
                    "specVersion": "0.0",
                    "meta": { "title": "T", "author": "A" },
                    "humanoid": { "humanBones": [{ "bone": "hips", "node": 0 }] },
                    "materialProperties": [{ "name": "Face", "shader": "VRM/MToon" }],
                    "blendShapeMaster": { "blendShapeGroups": [{}] },
                    "secondaryAnimation": { "boneGroups": [{}] }
                }
            }
        });
        let info = VrmInfo::detect(&json).unwrap();
        let mut doc = AirDoc::new("glb", "a.vrm");
        info.apply(&mut doc);
        assert!(doc.errors.is_empty(), "VRM 专有内容不得阻断导出");
        assert!(doc.vrm.is_some());
        let joined = doc.issues.join("\n");
        assert!(joined.contains("VRM 0.x"), "缺少版本：{joined}");
        assert!(joined.contains("MToon"), "缺少 MToon 说明：{joined}");
        assert!(joined.contains("表情"), "缺少表情说明：{joined}");
        assert!(joined.contains("spring bone"), "缺少物理说明：{joined}");
    }

    /// 摘要行不能因为字段全空就崩掉；最小输入也要有版本标签。
    #[test]
    fn summary_line_survives_empty_body() {
        let info = VrmInfo::detect(&json!({ "extensionsUsed": ["VRM"] })).unwrap();
        let line = info.summary_line();
        assert!(line.contains("VRM 0.x"));
        assert!(line.contains("humanoid 骨 0"));
    }
}
