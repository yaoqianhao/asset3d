//! AIR（Asset Intermediate Representation）：插件对外的**归一化资产结构**。
//!
//! 这份结构的字段与引擎内核的 `AssetEntry`（`crates/r3e-asset`）逐个对齐，
//! 目的不是"看起来像"，而是**反序列化即可用**：
//! - 坐标：右手系、+Y up、+Z front、单位米（FBX 的轴向与单位在导入期就换算了）；
//! - 三角形绕序：CCW；
//! - UV：原点在左上，`v' = 1 - v`（FBX/GLB 都是左下原点，导入期统一翻转过）；
//! - 顶点法线恒存在、单位长度；权重恒归一化且每顶点最多 4 个关节。
//!
//! 因此 `positions` / `normals` / `uvs` / `indices` 是**可直接灌进顶点缓冲**的
//! 平铺数组，不需要消费方再做去重、三角化或坐标变换。

use serde::{Deserialize, Serialize};

/// AIR 结构版本。字段增删必须升版本：消费方据此拒绝未知布局。
pub const AIR_VERSION: u32 = 1;

/// 一次导入的完整产出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirDoc {
    /// 固定为 `"air"`：文件被改名后仍能自证格式。
    pub format: String,
    pub air_version: u32,
    pub generator: String,
    pub source: AirSource,
    pub units: AirUnits,
    #[serde(default)]
    pub nodes: Vec<AirNode>,
    #[serde(default)]
    pub meshes: Vec<AirMesh>,
    #[serde(default)]
    pub materials: Vec<AirMaterial>,
    #[serde(default)]
    pub textures: Vec<AirTexture>,
    #[serde(default)]
    pub skins: Vec<AirSkin>,
    #[serde(default)]
    pub animations: Vec<AirAnimation>,
    #[serde(default)]
    pub stats: AirStats,
    /// 导入期发现但不足以失败的问题（缺 UV、权重超 4、贴图丢失……）。
    #[serde(default)]
    pub issues: Vec<String>,
    /// 阻断级问题：消费方按这份数据渲染必然出错（索引越界、法线/UV/权重数量与顶点
    /// 不符、权重和不为 1）。与 `issues` 分开，导出工具据此拒写盘。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirSource {
    /// 工作区相对路径。
    pub path: String,
    /// 容器格式：`fbx` / `glb` / `gltf`。
    pub container: String,
    pub bytes: u64,
    /// FBX 的原生单位（仅 FBX 有；glTF 恒为米）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_unit_meters: Option<f32>,
    /// FBX 头部声明的轴系（如 `Y` / `Z`），glTF 恒为 `Y`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_up_axis: Option<String>,
    /// FBX 的创建器字符串（`FBX SDK 2020` / `Blender 4.2`……），用于诊断轴系问题。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
}

/// 归一化后的坐标与单位约定，随数据一起走，避免消费方猜。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirUnits {
    pub meters_per_unit: f32,
    pub up_axis: String,
    pub front_axis: String,
    pub right_axis: String,
    pub uv_origin: String,
    pub winding: String,
}

impl Default for AirUnits {
    fn default() -> Self {
        Self {
            meters_per_unit: 1.0,
            up_axis: "+Y".to_string(),
            front_axis: "+Z".to_string(),
            right_axis: "+X".to_string(),
            uv_origin: "top-left".to_string(),
            winding: "ccw".to_string(),
        }
    }
}

/// 场景图节点。索引即数组下标，`parent` / `mesh` / `skin` 都引用这些下标。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirNode {
    pub index: u32,
    pub name: String,
    /// 从根拼下来的路径（`Root/Hips/Spine`），跨文件唯一，比名字可靠。
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    pub position: [f32; 3],
    /// 四元数 `(x, y, z, w)`。
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    /// 该节点挂的网格下标。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesh: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skin: Option<u32>,
}

/// 一个网格：顶点属性平铺存放，`material_parts` 划分三角形区段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirMesh {
    pub name: String,
    /// 挂载它的节点路径（空表示场景根）。
    pub node: String,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    /// 三角形索引，CCW。长度恒为 3 的倍数。
    pub indices: Vec<u32>,
    pub material_parts: Vec<MaterialPart>,
    pub aabb_min: [f32; 3],
    pub aabb_max: [f32; 3],
    /// 每顶点 4 个关节槽位（指向 `skins[i].joints`），无法则空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joints: Vec<[u16; 4]>,
    /// 每顶点 4 个权重，与 `joints` 同步且和为 1。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weights: Vec<[f32; 4]>,
    /// 引用的骨骼表下标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skin: Option<u32>,
    /// 源文件里的顶点/面数（与输出不同：按角点拆分后顶点数通常更多）。
    pub source_vertex_count: u32,
    pub source_face_count: u32,
    pub source_triangle_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaterialPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub material: Option<u32>,
    pub material_name: String,
    /// 以三角形为单位。
    pub triangle_offset: u32,
    pub triangle_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirMaterial {
    pub index: u32,
    pub name: String,
    /// 线性空间的 RGBA（不是 sRGB）。
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_color_texture: Option<u32>,
    #[serde(default)]
    pub double_sided: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirTexture {
    pub index: u32,
    pub name: String,
    /// 素材来源：贴图槽名（`DiffuseColor` / `baseColorTexture`）或源文件路径。
    pub slot: String,
    /// 嗅探出的 MIME（`image/png`）；嗅探失败留空。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// 原始编码后的字节数（不是解码后的像素量）。
    pub bytes: u64,
    /// 是否内嵌在源文件里（否则是外部贴图，可能已丢）。
    pub embedded: bool,
    /// 贴图是否随 AIR 一起携带（`embed_textures` 为真时才有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    /// 外部贴图的原始路径（未内嵌时给出线索，便于人工找回）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirSkin {
    pub index: u32,
    pub name: String,
    /// 关节 = 场景节点下标，顺序即 `joints` 槽位顺序。
    pub joints: Vec<u32>,
    /// 逆绑定矩阵（列主序 4x4），与 `joints` 一一对应。
    ///
    /// 语义是 **节点局部空间 → 骨骼绑定空间**：顶点在 AIR 里保持所属节点的局部空间
    /// （节点变换由 `AirNode` 的 TRS 沿父链累乘），因此蒙皮矩阵 = `joint_world * inverse_bind`，
    /// 与 glTF 的 `inverseBindMatrices` 语义一致。
    pub inverse_bind_matrices: Vec<[f32; 16]>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirAnimation {
    pub name: String,
    pub duration: f32,
    pub tracks: Vec<AirTrack>,
}

/// 一条轨道只驱动一个属性：位移/缩放是 Vec3，旋转是四元数（统一按 4 分量存）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirTrack {
    /// 目标节点下标。
    pub node: u32,
    /// `translation` / `rotation` / `scale`。
    pub property: String,
    pub times: Vec<f32>,
    /// 每个时间点一个值：位移/缩放取前 3 个分量，旋转取 4 个（`x,y,z,w`）。
    pub values: Vec<[f32; 4]>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirStats {
    pub node_count: u32,
    pub mesh_count: u32,
    pub vertex_count: u64,
    pub triangle_count: u64,
    pub material_count: u32,
    pub texture_count: u32,
    pub skin_count: u32,
    pub joint_count: u32,
    pub animation_count: u32,
    /// 几何数据的估算内存占用（顶点属性 + 索引），用于判断能不能一次灌进 GPU。
    pub geometry_bytes: u64,
}

impl AirDoc {
    pub fn new(container: &str, path: &str) -> Self {
        Self {
            format: "air".to_string(),
            air_version: AIR_VERSION,
            generator: format!("ai-ide asset3d {}", env!("CARGO_PKG_VERSION")),
            source: AirSource {
                path: path.to_string(),
                container: container.to_string(),
                bytes: 0,
                native_unit_meters: None,
                native_up_axis: None,
                creator: None,
            },
            units: AirUnits::default(),
            nodes: Vec::new(),
            meshes: Vec::new(),
            materials: Vec::new(),
            textures: Vec::new(),
            skins: Vec::new(),
            animations: Vec::new(),
            stats: AirStats::default(),
            issues: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.issues.push(message.into());
    }

    /// 记录一条阻断级问题：数据本身就能让消费方渲染出错，导出前会据此拒写盘。
    pub fn fault(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// 阻断级问题清单（可直接拼进工具报错正文）。
    pub fn error_report(&self) -> String {
        self.errors
            .iter()
            .map(|error| format!("  - {error}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 收尾：算统计、跑一致性校验。导入器只管填数据，校验集中在这里。
    pub fn finalize(&mut self) {
        self.recompute_stats();
        self.validate();
    }

    fn recompute_stats(&mut self) {
        let mut stats = AirStats {
            node_count: self.nodes.len() as u32,
            mesh_count: self.meshes.len() as u32,
            material_count: self.materials.len() as u32,
            texture_count: self.textures.len() as u32,
            skin_count: self.skins.len() as u32,
            animation_count: self.animations.len() as u32,
            ..Default::default()
        };
        for mesh in &self.meshes {
            stats.vertex_count += mesh.positions.len() as u64;
            stats.triangle_count += (mesh.indices.len() / 3) as u64;
            stats.geometry_bytes += (mesh.positions.len() * 12
                + mesh.normals.len() * 12
                + mesh.uvs.len() * 8
                + mesh.indices.len() * 4
                + mesh.joints.len() * 8
                + mesh.weights.len() * 16) as u64;
        }
        for skin in &self.skins {
            stats.joint_count += skin.joints.len() as u32;
        }
        self.stats = stats;
    }

    /// 一致性校验，分两级：
    ///
    /// - **error**（`self.errors`）：数据自相矛盾，引擎照着画必然出错——索引越界、
    ///   法线/UV/权重数量与顶点数不符、权重和不为 1。导出工具据此拒写盘。
    /// - **issue**（`self.issues`）：能渲染但结果可疑（索引数非 3 倍数、空网格、
    ///   贴图尺寸未知、完全没有网格……），只提示不阻断。
    fn validate(&mut self) {
        let mut errors: Vec<String> = Vec::new();
        let mut issues: Vec<String> = Vec::new();
        for (mi, mesh) in self.meshes.iter().enumerate() {
            if mesh.indices.len() % 3 != 0 {
                issues.push(format!(
                    "网格 #{} `{}` 的索引数 {} 不是 3 的倍数（末尾 {} 个索引已无处成面，文件可能被截断）",
                    mi,
                    mesh.name,
                    mesh.indices.len(),
                    mesh.indices.len() % 3
                ));
            }
            let max_index = mesh.indices.iter().copied().max().unwrap_or(0);
            if !mesh.indices.is_empty() && max_index as usize >= mesh.positions.len() {
                errors.push(format!(
                    "网格 #{} `{}` 索引越界：最大索引 {} ≥ 顶点数 {}",
                    mi,
                    mesh.name,
                    max_index,
                    mesh.positions.len()
                ));
            }
            if mesh.normals.len() != mesh.positions.len() {
                errors.push(format!(
                    "网格 #{} `{}` 法线数 {} ≠ 顶点数 {}（法线按顶点索引取，数量不符会读到错位的法线）",
                    mi,
                    mesh.name,
                    mesh.normals.len(),
                    mesh.positions.len()
                ));
            }
            if mesh.uvs.len() != mesh.positions.len() {
                errors.push(format!(
                    "网格 #{} `{}` UV 数 {} ≠ 顶点数 {}（引擎按 uv0 逐顶点取）",
                    mi,
                    mesh.name,
                    mesh.uvs.len(),
                    mesh.positions.len()
                ));
            }
            if !mesh.weights.is_empty() {
                if mesh.weights.len() != mesh.positions.len() {
                    errors.push(format!(
                        "网格 #{} `{}` 权重数 {} ≠ 顶点数 {}",
                        mi,
                        mesh.name,
                        mesh.weights.len(),
                        mesh.positions.len()
                    ));
                }
                let bad = mesh
                    .weights
                    .iter()
                    .filter(|w| (w.iter().sum::<f32>() - 1.0).abs() > 1e-3)
                    .count();
                if bad > 0 {
                    errors.push(format!(
                        "网格 #{} `{}` 有 {bad} 个顶点权重和不为 1（蒙皮会缩水/鼓包，导出前必须归一）",
                        mi, mesh.name
                    ));
                }
            }
            if mesh.positions.is_empty() {
                issues.push(format!(
                    "网格 #{} `{}` 没有顶点（空网格或被裁剪）",
                    mi, mesh.name
                ));
            }
        }
        for (ti, texture) in self.textures.iter().enumerate() {
            if texture.width.is_none() && !texture.bytes.eq(&0) {
                issues.push(format!(
                    "贴图 #{} `{}` 未能解出尺寸（{}），可能不是 PNG/JPEG 或已损坏",
                    ti, texture.name, texture.mime_type
                ));
            }
        }
        if self.meshes.is_empty() {
            issues.push("文件里没有任何网格：可能是纯骨骼/摄像机场景，或几何未导出".to_string());
        }
        self.errors = errors;
        self.issues.extend(issues);
    }

    /// 给人看的摘要（`inspect_asset` 的正文）。
    pub fn summary_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "AIR 资产报告");
        let _ = writeln!(
            out,
            "  源文件    {}（{}，{}）",
            self.source.path,
            self.source.container.to_ascii_uppercase(),
            crate::util::format_bytes(self.source.bytes)
        );
        if let Some(unit) = self.source.native_unit_meters {
            let _ = writeln!(
                out,
                "  原生单位  {:.4} m/unit{}",
                unit,
                self.source
                    .native_up_axis
                    .as_ref()
                    .map(|axis| format!("，up 轴 {axis}"))
                    .unwrap_or_default()
            );
        }
        if let Some(creator) = &self.source.creator {
            let _ = writeln!(out, "  创建器    {creator}");
        }
        let s = &self.stats;
        let _ = writeln!(
            out,
            "  统计      节点 {} / 网格 {} / 顶点 {} / 三角形 {} / 材质 {} / 贴图 {} / 骨骼 {} / 动画 {}",
            s.node_count,
            s.mesh_count,
            s.vertex_count,
            s.triangle_count,
            s.material_count,
            s.texture_count,
            s.skin_count,
            s.animation_count
        );
        let _ = writeln!(
            out,
            "  几何内存  {}（顶点属性 + 索引）",
            crate::util::format_bytes(s.geometry_bytes)
        );

        if !self.meshes.is_empty() {
            let _ = writeln!(out, "\n网格：");
            for (i, mesh) in self.meshes.iter().enumerate() {
                let size = [
                    mesh.aabb_max[0] - mesh.aabb_min[0],
                    mesh.aabb_max[1] - mesh.aabb_min[1],
                    mesh.aabb_max[2] - mesh.aabb_min[2],
                ];
                let _ = writeln!(
                    out,
                    "  [{}] {} ← {}｜顶点 {}，三角形 {}，材质分区 {}，包围盒 {:.3}×{:.3}×{:.3} m",
                    i,
                    if mesh.name.is_empty() { "<未命名>" } else { &mesh.name },
                    if mesh.node.is_empty() { "<根>" } else { &mesh.node },
                    mesh.positions.len(),
                    mesh.indices.len() / 3,
                    mesh.material_parts.len(),
                    size[0],
                    size[1],
                    size[2]
                );
            }
        }
        if !self.materials.is_empty() {
            let _ = writeln!(out, "\n材质：");
            for (i, material) in self.materials.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "  [{}] {}｜base {:.2}/{:.2}/{:.2}/{:.2}，metallic {:.2}，roughness {:.2}{}",
                    i,
                    material.name,
                    material.base_color[0],
                    material.base_color[1],
                    material.base_color[2],
                    material.base_color[3],
                    material.metallic,
                    material.roughness,
                    material
                        .base_color_texture
                        .map(|t| format!("，baseColor 贴图 #{t}"))
                        .unwrap_or_default()
                );
            }
        }
        if !self.textures.is_empty() {
            let _ = writeln!(out, "\n贴图：");
            for (i, texture) in self.textures.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "  [{}] {}｜{}｜{}×{}｜{}｜{}",
                    i,
                    if texture.name.is_empty() { "<未命名>" } else { &texture.name },
                    if texture.mime_type.is_empty() { "?" } else { &texture.mime_type },
                    texture
                        .width
                        .map(|w| w.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    texture
                        .height
                        .map(|h| h.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    crate::util::format_bytes(texture.bytes),
                    if texture.embedded { "内嵌" } else { "外部引用" }
                );
            }
        }
        if !self.animations.is_empty() {
            let _ = writeln!(out, "\n动画：");
            for (i, clip) in self.animations.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "  [{}] {}｜{:.3}s，{} 条轨道",
                    i,
                    if clip.name.is_empty() { "<未命名>" } else { &clip.name },
                    clip.duration,
                    clip.tracks.len()
                );
            }
        }
        if !self.errors.is_empty() {
            let _ = writeln!(
                out,
                "\n阻断级问题（{}，导出会被拒绝）：",
                self.errors.len()
            );
            for error in &self.errors {
                let _ = writeln!(out, "  ! {error}");
            }
        }
        if !self.issues.is_empty() {
            let _ = writeln!(out, "\n注意（{}）：", self.issues.len());
            for issue in &self.issues {
                let _ = writeln!(out, "  - {issue}");
            }
        }
        out
    }

    /// 导出体积预估：JSON 是文本，浮点约占 10 字符/分量，粗估够用于提示。
    pub fn estimated_json_bytes(&self) -> u64 {
        let floats = self.stats.vertex_count * 8
            + self.stats.triangle_count * 3
            + self
                .animations
                .iter()
                .map(|clip| {
                    clip.tracks
                        .iter()
                        .map(|track| (track.times.len() + track.values.len() * 4) as u64 * 11)
                        .sum::<u64>()
                })
                .sum::<u64>();
        floats * 11 / 2 + 4096
    }
}

/// 从 AIR 里重建每顶点世界/局部矩阵无关的辅助数据：`mesh` 包围盒与中心点，
/// 供预览相机自动取景用（避免把镜头对在空处）。
pub fn bounds_of(meshes: &[AirMesh]) -> Option<([f32; 3], [f32; 3])> {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for mesh in meshes {
        if mesh.positions.is_empty() {
            continue;
        }
        any = true;
        for axis in 0..3 {
            min[axis] = min[axis].min(mesh.aabb_min[axis]);
            max[axis] = max[axis].max(mesh.aabb_max[axis]);
        }
    }
    if any {
        Some((min, max))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个顶点数正常、但索引/属性可单独调坏的网格。
    fn mesh_with(index_max: u32, normals: usize, uvs: usize) -> AirMesh {
        AirMesh {
            name: "probe".to_string(),
            positions: vec![[0.0, 0.0, 0.0]; 4],
            normals: vec![[0.0, 1.0, 0.0]; normals],
            uvs: vec![[0.0, 0.0]; uvs],
            indices: vec![0, 1, index_max],
            ..Default::default()
        }
    }

    fn validated(mesh: AirMesh) -> AirDoc {
        let mut doc = AirDoc::new("cube.gltf", "cube.gltf");
        doc.meshes.push(mesh);
        doc.validate();
        doc
    }

    #[test]
    fn out_of_range_index_is_blocking_error() {
        let doc = validated(mesh_with(9, 4, 4));
        assert!(doc.has_errors(), "索引越界必须是 error 级");
        assert!(doc.error_report().contains("索引越界"));
    }

    #[test]
    fn attribute_count_mismatch_is_blocking_error() {
        // 法线少一条、UV 多一条：都不是「能画只是可疑」，必须阻断导出。
        let doc = validated(mesh_with(2, 3, 5));
        let report = doc.error_report();
        assert!(doc.has_errors());
        assert!(report.contains("法线数 3 ≠ 顶点数 4"), "{report}");
        assert!(report.contains("UV 数 5 ≠ 顶点数 4"), "{report}");
    }

    #[test]
    fn unnormalized_weights_are_blocking_error() {
        let mut mesh = mesh_with(2, 4, 4);
        mesh.joints = vec![[0, 1, 0, 0]; 4];
        mesh.weights = vec![[0.5, 0.4, 0.0, 0.0]; 4];
        let doc = validated(mesh);
        assert!(doc.has_errors());
        assert!(doc.error_report().contains("权重和不为 1"));
    }

    #[test]
    fn ragged_index_count_stays_issue_not_error() {
        // 索引数不是 3 的倍数只影响末尾几个索引，仍可渲染，属 issue 级。
        let mut mesh = mesh_with(2, 4, 4);
        mesh.indices = vec![0, 1, 2, 0];
        let doc = validated(mesh);
        assert!(!doc.has_errors(), "不该升级为阻断");
        assert!(doc.issues.iter().any(|issue| issue.contains("不是 3 的倍数")));
    }
}
