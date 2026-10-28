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

/// 关键帧之间怎么插值。
pub const INTERPOLATION_LINEAR: &str = "linear";
/// 保持前一帧到最后（glTF 的 `STEP`）。
pub const INTERPOLATION_STEP: &str = "step";

fn default_interpolation() -> String {
    INTERPOLATION_LINEAR.to_string()
}

fn is_linear_interpolation(value: &String) -> bool {
    value.is_empty() || value == INTERPOLATION_LINEAR
}

/// 一条轨道只驱动一个属性：位移/缩放是 Vec3，旋转是四元数（统一按 4 分量存）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirTrack {
    /// 目标节点下标。
    pub node: u32,
    /// `translation` / `rotation` / `scale`。
    pub property: String,
    /// 关键帧之间的插值方式：`linear`（默认，省略不写）/ `step`。
    ///
    /// glTF 的 `STEP` 原样带进来；`CUBICSPLINE` 在导入期已丢弃切线，降级成
    /// `linear`（切线没了按三次插只会更歪，线性至少是保守近似）。
    #[serde(default = "default_interpolation", skip_serializing_if = "is_linear_interpolation")]
    pub interpolation: String,
    pub times: Vec<f32>,
    /// 每个时间点一个值：位移/缩放取前 3 个分量，旋转取 4 个（`x,y,z,w`）。
    pub values: Vec<[f32; 4]>,
}

impl Default for AirTrack {
    fn default() -> Self {
        Self {
            node: 0,
            property: String::new(),
            interpolation: default_interpolation(),
            times: Vec::new(),
            values: Vec::new(),
        }
    }
}

/// `AirTrack.property` 的枚举视图。AIR 里仍存字符串（格式稳定），
/// 采样与播放走枚举，避免逐帧比字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackProperty {
    Translation,
    Rotation,
    Scale,
}

impl TrackProperty {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "translation" => Some(Self::Translation),
            "rotation" => Some(Self::Rotation),
            "scale" => Some(Self::Scale),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Translation => "translation",
            Self::Rotation => "rotation",
            Self::Scale => "scale",
        }
    }

    /// 有效分量数：旋转 4，位移/缩放 3（第 4 个分量恒 0，占位对齐）。
    pub fn components(self) -> usize {
        match self {
            Self::Rotation => 4,
            _ => 3,
        }
    }
}

/// 一次采样的结果：驱动哪个节点、哪个属性、值是什么。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackSample {
    pub node: u32,
    pub property: TrackProperty,
    pub value: [f32; 4],
}

impl AirTrack {
    pub fn property_kind(&self) -> Option<TrackProperty> {
        TrackProperty::parse(&self.property)
    }

    pub fn is_step(&self) -> bool {
        self.interpolation == INTERPOLATION_STEP
    }

    /// 轨道末端时间（时间轴为空时为 0）。
    pub fn end_time(&self) -> f32 {
        self.times.last().copied().filter(|t| t.is_finite()).unwrap_or(0.0)
    }

    /// 有效关键帧数：`times` / `values` 取较短的一侧（长度不符时按能用的部分算，
    /// 校验会把这种数据标成阻断级）。
    pub fn keyframe_count(&self) -> usize {
        self.times.len().min(self.values.len())
    }

    /// 在 `time` 处采样。
    ///
    /// 约定：
    /// - 时间轴为空 → `None`（没有姿态可给）；
    /// - 只有 1 帧、或 `time` 落在首帧之前 / 末帧之后 → 取端点值（hold，不循环；
    ///   循环由 `AirAnimation::wrap_time` 负责）；
    /// - `linear` 按属性插值：位移/缩放线性，旋转 slerp；
    /// - `step` 保持前一帧；
    /// - `time` 为 NaN/Inf 时取首帧（宁可静止也不要 NaN 扩散到整棵骨架）。
    pub fn sample_at(&self, time: f32) -> Option<[f32; 4]> {
        let n = self.keyframe_count();
        if n == 0 {
            return None;
        }
        // NaN 无法比较大小，任何分区/夹取都会得到未定义摆位，只能退回首帧；
        // ±∞ 则是有序的，交给下面的端点分支处理（-∞ 首帧、+∞ 末帧）。
        if time.is_nan() {
            return Some(self.values[0]);
        }
        if n == 1 || time <= self.times[0] {
            return Some(self.values[0]);
        }
        if time >= self.times[n - 1] {
            return Some(self.values[n - 1]);
        }
        // `partition_point` 给出第一个 > time 的下标，减一即左端点。
        let left = self.times[..n]
            .partition_point(|t| *t <= time)
            .saturating_sub(1)
            .min(n - 2);
        let (t0, t1) = (self.times[left], self.times[left + 1]);
        let (v0, v1) = (self.values[left], self.values[left + 1]);
        if self.is_step() {
            return Some(v0);
        }
        if !(t1 > t0) {
            // 时间轴重复（两个关键帧同一时刻）：取后一个，避免除零。
            return Some(v1);
        }
        let alpha = ((time - t0) / (t1 - t0)).clamp(0.0, 1.0);
        if self.property_kind() == Some(TrackProperty::Rotation) {
            Some(crate::util::slerp_quat(v0, v1, alpha))
        } else {
            Some([
                v0[0] + (v1[0] - v0[0]) * alpha,
                v0[1] + (v1[1] - v0[1]) * alpha,
                v0[2] + (v1[2] - v0[2]) * alpha,
                0.0,
            ])
        }
    }
}

impl AirAnimation {
    /// 真实时长：`duration` 为 0 或落后于轨道末端时用轨道末端兜底。
    ///
    /// 导入器本应算准 `duration`，但手写 AIR / 被裁剪的动画经常只留字段不填，
    /// 播放端直接按 `duration` 循环会把尾巴截掉。
    pub fn effective_duration(&self) -> f32 {
        let tail = self
            .tracks
            .iter()
            .map(AirTrack::end_time)
            .fold(0.0f32, f32::max);
        self.duration.max(tail)
    }

    /// 循环取模：把任意时间折到 `0..duration`。时长为 0 时恒返回 0。
    pub fn wrap_time(&self, time: f32) -> f32 {
        let duration = self.effective_duration();
        if !(duration > 0.0) || !time.is_finite() {
            return 0.0;
        }
        let wrapped = time % duration;
        if wrapped < 0.0 {
            wrapped + duration
        } else {
            wrapped
        }
    }

    /// 关键帧总数（各轨道求和）。
    pub fn keyframe_count(&self) -> usize {
        self.tracks.iter().map(AirTrack::keyframe_count).sum()
    }

    /// 被驱动的节点数（去重）。视口可以据此判断「这条动画能动多少骨骼」。
    pub fn driven_node_count(&self) -> usize {
        let mut nodes = self
            .tracks
            .iter()
            .filter(|track| track.property_kind().is_some() && track.keyframe_count() > 0)
            .map(|track| track.node)
            .collect::<Vec<_>>();
        nodes.sort_unstable();
        nodes.dedup();
        nodes.len()
    }

    /// 采样全部轨道。未知属性（不是 TRS 三者）直接跳过。
    pub fn sample_at(&self, time: f32) -> Vec<TrackSample> {
        let mut out = Vec::with_capacity(self.tracks.len());
        for track in &self.tracks {
            let Some(property) = track.property_kind() else {
                continue;
            };
            let Some(value) = track.sample_at(time) else {
                continue;
            };
            out.push(TrackSample {
                node: track.node,
                property,
                value,
            });
        }
        out
    }

    /// 把 `time` 处的姿态写回节点 TRS，返回实际写入的轨道条数。
    ///
    /// 节点下标越界、未知属性、空时间轴都静默跳过——播放端不该因为一条坏轨道
    /// 丢掉整段动画；真正的数据错误由 `AirDoc::validate` 在导入期报出来。
    pub fn apply_at(&self, time: f32, nodes: &mut [AirNode]) -> usize {
        let mut written = 0;
        for track in &self.tracks {
            let Some(property) = track.property_kind() else {
                continue;
            };
            let Some(value) = track.sample_at(time) else {
                continue;
            };
            let Some(node) = nodes.get_mut(track.node as usize) else {
                continue;
            };
            match property {
                TrackProperty::Translation => node.position = [value[0], value[1], value[2]],
                TrackProperty::Rotation => node.rotation = crate::util::normalize_quat(value),
                TrackProperty::Scale => node.scale = [value[0], value[1], value[2]],
            }
            written += 1;
        }
        written
    }
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
        // 动画：轨道长度、节点下标、时间轴单调性出错时播放端要么驱动错骨骼、
        // 要么整条轨道静默消失，所以在导入期就分级报出来，而不是等视口播出一段
        // 「看着不对」的动画再去猜。
        for (ai, clip) in self.animations.iter().enumerate() {
            let label = format!(
                "动画 #{} `{}`",
                ai,
                if clip.name.is_empty() { "<未命名>" } else { &clip.name }
            );
            if clip.tracks.is_empty() {
                issues.push(format!("{label} 没有任何轨道（不会被播放）"));
            }
            for (ti, track) in clip.tracks.iter().enumerate() {
                let track_label = format!(
                    "{label} 轨道 #{ti}（{}）",
                    if track.property.is_empty() {
                        "<空属性>"
                    } else {
                        &track.property
                    }
                );
                if track.property_kind().is_none() {
                    errors.push(format!(
                        "{track_label} 的属性既不是 `translation` / `rotation` / `scale`，播放端无法应用"
                    ));
                }
                if !self.nodes.is_empty() && track.node as usize >= self.nodes.len() {
                    errors.push(format!(
                        "{track_label} 目标节点下标 {} 越界（节点数 {}）",
                        track.node,
                        self.nodes.len()
                    ));
                }
                if track.times.len() != track.values.len() {
                    errors.push(format!(
                        "{track_label} 时间点数 {} ≠ 关键值数 {}（多出来的那一半无法配对）",
                        track.times.len(),
                        track.values.len()
                    ));
                }
                if track.keyframe_count() < 2 {
                    issues.push(format!(
                        "{track_label} 只有 {} 个关键帧（常量轨道，播起来等于不生效）",
                        track.keyframe_count()
                    ));
                }
                let mut previous = f32::NEG_INFINITY;
                for (ki, time) in track.times.iter().enumerate() {
                    if !time.is_finite() {
                        errors.push(format!(
                            "{track_label} 第 {ki} 个时间点不是有限数（{time}），采样会扩散出 NaN"
                        ));
                        break;
                    }
                    if *time < previous {
                        errors.push(format!(
                            "{track_label} 时间轴非单调：第 {ki} 个时间点 {time} 小于前一个 {previous}，采样会往回跳"
                        ));
                        break;
                    }
                    previous = *time;
                }
                if !track.interpolation.is_empty()
                    && !matches!(
                        track.interpolation.as_str(),
                        INTERPOLATION_LINEAR | INTERPOLATION_STEP
                    )
                {
                    issues.push(format!(
                        "{track_label} 的插值方式 `{}` 不认识，按 linear 处理",
                        track.interpolation
                    ));
                }
                if track.property_kind() == Some(TrackProperty::Rotation) {
                    let bad = track
                        .values
                        .iter()
                        .filter(|value| {
                            let len2 = value.iter().map(|c| c * c).sum::<f32>();
                            len2.is_finite() && (len2.sqrt() - 1.0).abs() > 1e-2
                        })
                        .count();
                    if bad > 0 {
                        issues.push(format!(
                            "{track_label} 有 {bad} 个旋转关键帧未归一化（采样时按归一化处理）"
                        ));
                    }
                }
            }
            let tail = clip.tracks.iter().map(AirTrack::end_time).fold(0.0f32, f32::max);
            if !(clip.duration > 0.0) && tail > 0.0 {
                issues.push(format!(
                    "{label} 的 duration 为 {}，循环时按轨道末端 {tail:.3}s 兜底",
                    clip.duration
                ));
            } else if clip.duration + 1e-4 < tail {
                issues.push(format!(
                    "{label} 的 duration {:.3}s 短于轨道末端 {tail:.3}s，尾部会被截掉（按末端兜底）",
                    clip.duration
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
                let step_tracks = clip.tracks.iter().filter(|track| track.is_step()).count();
                let _ = writeln!(
                    out,
                    "  [{}] {}｜{:.3}s（有效 {:.3}s），{} 条轨道 / {} 个关键帧 / 驱动 {} 个节点{}",
                    i,
                    if clip.name.is_empty() { "<未命名>" } else { &clip.name },
                    clip.duration,
                    clip.effective_duration(),
                    clip.tracks.len(),
                    clip.keyframe_count(),
                    clip.driven_node_count(),
                    if step_tracks > 0 {
                        format!("，{step_tracks} 条 STEP")
                    } else {
                        String::new()
                    }
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

    // -----------------------------------------------------------------------
    // 动画采样（M-UI-4 第一刀）
    // -----------------------------------------------------------------------

    fn track(property: &str, times: Vec<f32>, values: Vec<[f32; 4]>) -> AirTrack {
        AirTrack {
            node: 0,
            property: property.to_string(),
            interpolation: INTERPOLATION_LINEAR.to_string(),
            times,
            values,
        }
    }

    /// 0→10 沿 X，1s 到 2s 保持：三段各测一处的经典轨道。
    fn ramp_track() -> AirTrack {
        track(
            "translation",
            vec![0.0, 1.0, 2.0],
            vec![
                [0.0, 0.0, 0.0, 0.0],
                [10.0, 0.0, 0.0, 0.0],
                [10.0, 0.0, 0.0, 0.0],
            ],
        )
    }

    #[test]
    fn linear_sample_lerps_and_holds_endpoints() {
        let t = ramp_track();
        assert_eq!(t.sample_at(0.0).unwrap(), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(t.sample_at(1.0).unwrap(), [10.0, 0.0, 0.0, 0.0]);
        let mid = t.sample_at(0.5).unwrap();
        assert!((mid[0] - 5.0).abs() < 1e-5, "{mid:?}");
        // 越界不循环：两端保持，循环交给 wrap_time。
        assert_eq!(t.sample_at(-99.0).unwrap(), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(t.sample_at(99.0).unwrap(), [10.0, 0.0, 0.0, 0.0]);
        // 落在两段之间的保持段里：不许外推回 20。
        let held = t.sample_at(1.5).unwrap();
        assert!((held[0] - 10.0).abs() < 1e-5, "{held:?}");
    }

    #[test]
    fn NaN_time_falls_back_to_first_keyframe() {
        let t = ramp_track();
        assert_eq!(t.sample_at(f32::NAN).unwrap(), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(t.sample_at(f32::INFINITY).unwrap(), [10.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn step_track_holds_previous_keyframe() {
        let mut t = ramp_track();
        t.interpolation = INTERPOLATION_STEP.to_string();
        assert!(t.is_step());
        assert_eq!(t.sample_at(0.999).unwrap(), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(t.sample_at(1.0).unwrap(), [10.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn single_keyframe_track_is_a_constant_pose() {
        let t = track(
            "scale",
            vec![0.0],
            vec![[2.0, 2.0, 2.0, 0.0]],
        );
        assert_eq!(t.sample_at(123.0).unwrap(), [2.0, 2.0, 2.0, 0.0]);
        let empty = track("scale", vec![], vec![]);
        assert!(empty.sample_at(0.0).is_none());
    }

    #[test]
    fn rotation_sample_uses_slerp_and_shortest_arc() {
        let quarter = std::f32::consts::FRAC_PI_4;
        let a = [0.0, 0.0, 0.0, 1.0];
        // 绕 Y 轴 90°
        let b = [0.0, quarter.sin(), 0.0, quarter.cos()];
        let t = track("rotation", vec![0.0, 1.0], vec![a, b]);
        let mid = t.sample_at(0.5).unwrap();
        // 45° 对应 (0, sin22.5°, 0, cos22.5°) ≈ (0, 0.38268, 0, 0.92388)
        assert!((mid[1] - 0.38268).abs() < 1e-3, "中点不是 45°：{mid:?}");
        assert!((mid[3] - 0.92388).abs() < 1e-3, "{mid:?}");

        // q 与 -q 是同一旋转：取反后必须走同一段弧，而不是绕远路。
        let neg = track("rotation", vec![0.0, 1.0], vec![a, [-b[0], -b[1], -b[2], -b[3]]]);
        let mid_neg = neg.sample_at(0.5).unwrap();
        assert!(
            (mid_neg[1] - mid[1]).abs() < 1e-3 && (mid_neg[3] - mid[3]).abs() < 1e-3,
            "{mid_neg:?}"
        );
    }

    #[test]
    fn duplicated_timestamps_do_not_divide_by_zero() {
        let t = track(
            "translation",
            vec![0.0, 1.0, 1.0, 2.0],
            vec![
                [0.0, 0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                [2.0, 0.0, 0.0, 0.0],
                [3.0, 0.0, 0.0, 0.0],
            ],
        );
        let value = t.sample_at(1.0).unwrap();
        assert!(value[0].is_finite(), "{value:?}");
    }

    #[test]
    fn effective_duration_falls_back_to_track_tail() {
        let clip = AirAnimation {
            name: "walk".to_string(),
            duration: 0.0,
            tracks: vec![ramp_track()],
        };
        assert!((clip.effective_duration() - 2.0).abs() < 1e-6);
        // duration 写得比轨道短时按末端兜底；写得长则以 duration 为准。
        let short = AirAnimation {
            duration: 0.5,
            ..clip.clone()
        };
        assert!((short.effective_duration() - 2.0).abs() < 1e-6);
        let long = AirAnimation {
            duration: 5.0,
            ..clip.clone()
        };
        assert!((long.effective_duration() - 5.0).abs() < 1e-6);
    }

    #[test]
    fn wrap_time_is_periodic_including_negative_time() {
        let clip = AirAnimation {
            name: "walk".to_string(),
            duration: 2.0,
            tracks: vec![ramp_track()],
        };
        assert!((clip.wrap_time(2.5) - 0.5).abs() < 1e-6);
        assert!((clip.wrap_time(-0.5) - 1.5).abs() < 1e-6);
        assert!((clip.wrap_time(0.0)).abs() < 1e-6);
        // 零时长不该出现 NaN/负值。
        let still = AirAnimation::default();
        assert_eq!(still.wrap_time(3.0), 0.0);
    }

    #[test]
    fn apply_at_writes_all_three_trs_channels() {
        let clip = AirAnimation {
            name: "pose".to_string(),
            duration: 1.0,
            tracks: vec![
                track(
                    "translation",
                    vec![0.0, 1.0],
                    vec![[0.0, 0.0, 0.0, 0.0], [2.0, 4.0, 6.0, 0.0]],
                ),
                track(
                    "scale",
                    vec![0.0, 1.0],
                    vec![[1.0, 1.0, 1.0, 0.0], [3.0, 3.0, 3.0, 0.0]],
                ),
                track(
                    "rotation",
                    vec![0.0, 1.0],
                    vec![[0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0, 3.0]],
                ),
            ],
        };
        let mut nodes = vec![AirNode::default()];
        assert_eq!(clip.apply_at(0.5, &mut nodes), 3);
        assert!((nodes[0].position[1] - 2.0).abs() < 1e-5);
        assert!((nodes[0].scale[2] - 2.0).abs() < 1e-5);
        // 未归一化的旋转关键帧写回时必须归一化，否则后续矩阵会带缩放。
        let len2: f32 = nodes[0].rotation.iter().map(|c| c * c).sum();
        assert!((len2.sqrt() - 1.0).abs() < 1e-5, "{:?}", nodes[0].rotation);
        // 越界节点被跳过，不 panic。
        let mut empty = Vec::new();
        assert_eq!(clip.apply_at(0.5, &mut empty), 0);
    }

    #[test]
    fn animation_validation_separates_errors_from_issues() {
        let mut doc = AirDoc::new("a.air", "a.air");
        doc.nodes.push(AirNode::default());
        doc.animations.push(AirAnimation {
            name: "bad".to_string(),
            duration: 0.0,
            tracks: vec![
                // 节点越界 + 属性不认识：两条都是阻断级。
                track("bogus", vec![0.0, 1.0], vec![[0.0; 4], [1.0; 4]]),
                // 时间点比关键值多：配对不上，阻断。
                track("translation", vec![0.0, 0.5, 1.0], vec![[0.0; 4], [1.0; 4]]),
                // 时间轴回退：采样会往回跳，阻断。
                track(
                    "translation",
                    vec![0.0, 2.0, 1.0],
                    vec![[0.0; 4], [1.0; 4], [2.0; 4]],
                ),
            ],
        });
        doc.animations[0].tracks[0].node = 7;
        doc.validate();
        let report = doc.error_report();
        assert!(report.contains("越界"), "{report}");
        assert!(report.contains("无法应用"), "{report}");
        assert!(report.contains("时间点数 3 ≠ 关键值数 2"), "{report}");
        assert!(report.contains("时间轴非单调"), "{report}");
        // duration 没填只提示，不阻断。
        assert!(doc.issues.iter().any(|issue| issue.contains("兜底")), "{:?}", doc.issues);
    }

    #[test]
    fn linear_tracks_do_not_serialize_interpolation_field() {
        let value = serde_json::to_value(ramp_track()).expect("serialize");
        assert!(value.get("interpolation").is_none(), "{value}");
        let mut step = ramp_track();
        step.interpolation = INTERPOLATION_STEP.to_string();
        let value = serde_json::to_value(step).expect("serialize");
        assert_eq!(value["interpolation"], "step");
        // 反序列化时缺字段按 linear 补齐：老 AIR 文件不会因此报错。
        let raw = serde_json::json!({
            "node": 0,
            "property": "translation",
            "times": [0.0, 1.0],
            "values": [[0.0, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]]
        });
        let parsed: AirTrack = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(parsed.interpolation, INTERPOLATION_LINEAR);
        assert!(!parsed.is_step());
    }
}
