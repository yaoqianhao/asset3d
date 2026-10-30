//! 资产 → 可画的三角形集合（M-UI-3 第二刀 + 第三刀材质贴图）。
//!
//! AIR 的顶点留在 **mesh 局部空间**（见 `src/gltf.rs` 头注释第 2 条），世界坐标要按
//! `AirNode` 的 TRS 沿父链累乘。视口只关心「一组三角形 + 一个包围盒」，所以这里把
//! 节点变换直接烘进顶点，运行时不再维护场景图——省掉一层可能写错的递归。
//!
//! 第三刀把「材质」拆成**分区**（[`ScenePart`]）：同一 mesh 按 `material_parts` 切成
//! 若干段索引区间，每段带一份 baseColor 与（可选的）解码后的贴图。渲染侧按分区换
//! bind group，一个 draw call 一段——比「逐顶点材质索引 + 材质缓冲」简单，也不需要
//! 在着色器里做分支。
//!
//! 与宿主插件侧的差别：插件不允许碰磁盘（外部资源经 `host/fs.read` 取），视口进程是
//! 用户自己双击起来的独立进程，没有宿主闸门可借，因此这里直接读文件；`gltf` 的外部
//! `uri` 也只允许落在资产同目录下，越界一律当读不到（缺失贴图不该让看图失败）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use glam::{Mat3, Mat4, Quat, Vec3};

use crate::air::{AirAnimation, AirDoc, AirMaterial, AirNode, AirTexture, MaterialPart};

/// 贴图长边上限（4096）。GPU 的 `max_texture_dimension_2d` 保底值就是 4096，
/// 同时把 RGBA8 展开面积压到 64 Mi 像素（256 MB）。
const MAX_TEXTURE_EDGE: u32 = 4096;

/// 贴图像素上限（64 Mi 像素 = 4096×4096 的 RGBA）。
///
/// 它是**降采样的触发线，不是解码的否决线**：超过这条线就按比例缩到
/// [`MAX_TEXTURE_EDGE`] 以内再上传。早先这里是「超了就 `Err`」，而 Tripo 那批资产的
/// `baseColorFactor` 是 (1,1,1,1)、颜色全在 8192×8192 的 baseColorTexture 里，
/// 一丢就等于整块白模——省下 200 MB 显存换来「看不见资产」，不划算。
const MAX_TEXTURE_PIXELS: u64 = (MAX_TEXTURE_EDGE as u64) * (MAX_TEXTURE_EDGE as u64);

/// GPU 顶点：位置 + 法线 + UV + 4 个关节槽位。与 `shader.wgsl` 的 `VsIn`
/// 逐字段对应，步长 56 字节（`f32 × 12 + u16 × 4`）。
///
/// 蒙皮与非蒙皮共用一份布局，靠 `weights` 是否全零区分：全零的顶点位置/法线
/// 已经是**世界空间**（合并时烘进了节点矩阵），非零的留在 **mesh 局部空间**，
/// 由着色器用关节矩阵算到世界。这样一份顶点流能同时装蒙皮与非蒙皮网格。
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    /// 关节索引，指向 `Scene::joint_matrices` 的全局槽位。
    pub joints: [u16; 4],
    /// 关节权重，和为 1；全零表示「这个顶点不蒙皮」。
    pub weights: [f32; 4],
}

/// 解码后的贴图：RGBA8 紧密排列，行序自上而下（与 wgpu 的纹理坐标 v=0 对齐）。
pub struct TexImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// 原图尺寸；`Some` 表示这份像素是降采样后的版本（原图超过 [`MAX_TEXTURE_EDGE`]）。
    /// 拿来写 `notes`，免得用户以为贴图被裁坏或根本没加载。
    pub downscaled_from: Option<(u32, u32)>,
}

/// 一份材质。`key` 用于渲染侧给 bind group 去重（同一材质被多个 mesh 引用时，
/// 不该把同一张 4K 贴图重复上传 N 次）。
pub struct Material {
    pub key: usize,
    pub name: String,
    pub base_color: [f32; 4],
    pub texture: Option<Arc<TexImage>>,
}

/// 一段同材质的索引区间。`index_start` 是**全局索引缓冲**里的偏移，不是 mesh 内偏移。
pub struct ScenePart {
    pub index_start: u32,
    pub index_count: u32,
    pub material: Material,
}

impl Material {
    /// 没有材质声明时的兜底：与第一刀着色器里的冷灰蓝一致，观感不会跳。
    fn fallback(key: usize) -> Self {
        Self {
            key,
            name: String::new(),
            base_color: [0.62, 0.68, 0.78, 1.0],
            texture: None,
        }
    }
}

/// 一份可画的场景：合并后的顶点/索引 + 按材质切好的分区 + 取景用的包围球。
pub struct Scene {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub parts: Vec<ScenePart>,
    /// 蒙皮矩阵：`jointWorld × inverseBindMatrix`，列主序。顶点里的 `joints[i]`
    /// 就是这个数组的下标；非蒙皮场景为空。
    pub joint_matrices: Vec<[f32; 16]>,
    /// 参与蒙皮的顶点数（日志用：区分「资产有骨骼」与「资产真的在用骨骼」）。
    pub skinned_vertex_count: usize,
    /// 包围盒中心（世界空间）。
    pub center: Vec3,
    /// 包围球半径，用来定相机距离与远近裁剪面。
    pub radius: f32,
    pub mesh_count: usize,
    pub triangle_count: usize,
    /// 成功解码的贴图张数（日志用）。
    pub texture_count: usize,
    /// 非致命问题（贴图解码失败 / 过大）：不影响开窗，但要说清为什么是纯色。
    pub notes: Vec<String>,
    /// 可播放骨架与动画。资产既没有骨骼也没有动画时为 `None`——此时视口退化成
    /// 纯静态看图，时间轴与骨骼线框都不该出现。
    pub rig: Option<Rig>,
}

/// 解析一个资产文件。返回的 `Scene` 可以直接塞进 GPU 缓冲。
pub fn load(path: &Path) -> Result<Scene, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("读取文件失败：{err}"))?;
    if bytes.is_empty() {
        return Err("文件是空的".to_string());
    }
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let hint = path.to_string_lossy().to_string();
    let container = match ext.as_str() {
        "glb" => "glb",
        "gltf" => "gltf",
        "fbx" => "fbx",
        other => {
            return Err(format!(
                "不认识的扩展名 `.{other}`：视口只支持 .glb / .gltf / .fbx"
            ))
        }
    };
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut doc = AirDoc::new(container, &hint);
    match ext.as_str() {
        "glb" | "gltf" => {
            let mut fetch = |uri: &str| read_sibling(&dir, uri);
            let fetch_ref: &mut dyn FnMut(&str) -> Option<Vec<u8>> = &mut fetch;
            // embed_textures = true：视口拿不到宿主的 `host/fs.read`，GLB 的图像又在
            // 二进制 chunk 里（没有单独文件可读），只能让解析器顺手把它 base64 出来。
            // 代价是解析期内存翻倍，比起「贴图一律丢」这笔账划算。
            // want_animations = true：第四刀之前这里是 `false`——那时视口不看时间轴，
            // 解出来的轨道没人消费。从这一刀起播放头要驱动关节矩阵，没有轨道就没得播。
            crate::gltf::import(&mut doc, &bytes, &hint, true, true, fetch_ref)?;
        }
        _ => crate::fbx::import(&mut doc, &bytes, &hint, true, true)?,
    }
    merge(&doc, &dir)
}

/// 把 `AirDoc` 的所有网格按节点世界矩阵合并成一份顶点流，并按材质切分区。
fn merge(doc: &AirDoc, dir: &Path) -> Result<Scene, String> {
    // 世界矩阵按需缓存：扁平资产（mesh 挂在根上）一次就够，深骨架也只算一遍。
    let mut cache: Vec<Option<Mat4>> = vec![None; doc.nodes.len()];
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut parts: Vec<ScenePart> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut mesh_count = 0usize;
    // 贴图按文档内索引缓存：同一张贴图被 20 个材质引用时只解码一次。
    let mut textures: HashMap<usize, Option<Arc<TexImage>>> = HashMap::new();
    let mut texture_count = 0usize;
    // FBX 的 UV 原点在左下，glTF 在左上；图像行序都是自上而下，所以前者要翻 v。
    // 这个判断读的是 AIR 的 `units.uv_origin`，不是格式名——将来换解析器也不会错。
    let flip_v = doc.units.uv_origin == "bottom-left";

    // 蒙皮矩阵表：`jointWorld × inverseBindMatrix`，全文档平铺成一个数组，
    // 顶点里的关节索引是全局槽位（`skin_offsets[skin] + local`）。这样一份
    // storage buffer 就能装下所有蒙皮网格，不必每个 skin 一个 bind group。
    let (joint_matrices, skin_offsets) = build_joint_matrices(doc, &mut cache, &mut notes);
    let joint_total = joint_matrices.len() as u32;
    let mut skinned_vertex_count = 0usize;
    // 取景用的包围盒按**世界空间**累计：蒙皮顶点自身留在局部空间（着色器才算得
    // 出世界位置），但 bind pose 的世界位置就是节点矩阵作用后的结果，用它定相机
    // 距离与裁剪面既正确又稳定。
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    let mut missing_skin_notes = 0usize;

    for node in &doc.nodes {
        let Some(mesh_index) = node.mesh else { continue };
        let Some(mesh) = doc.meshes.get(mesh_index as usize) else {
            continue;
        };
        if mesh.positions.is_empty() || mesh.indices.is_empty() {
            continue;
        }
        let world = world_matrix(doc, node.index as usize, &mut cache, 0);
        // 法线要用逆转置：含非均匀缩放的节点上，直接乘世界矩阵会把法线拧歪。
        let normal_matrix = {
            let linear = Mat3::from_mat4(world);
            let inv = linear.inverse();
            if inv.is_finite() {
                inv.transpose()
            } else {
                Mat3::IDENTITY
            }
        };
        let base = vertices.len() as u32;
        // ---- 这个网格能不能蒙皮 ----
        // 条件：节点挂了 skin、skin 存在、mesh 的 joints/weights 与顶点数一一对应。
        // 任一不满足就退回「烘世界矩阵」的老路——宁可不蒙皮，也不要画出塌成一团
        // 的模型（缺权重时整块面会被拉到原点）。
        let skin_base = node.skin.and_then(|skin_index| {
            let usable = doc.skins.get(skin_index as usize).is_some()
                && mesh.joints.len() == mesh.positions.len()
                && mesh.weights.len() == mesh.positions.len();
            if !usable && missing_skin_notes < 3 {
                missing_skin_notes += 1;
                notes.push(format!(
                    "网格「{}」声明了蒙皮但关节/权重与顶点数不匹配：按静态网格绘制",
                    mesh.name
                ));
            }
            usable.then(|| skin_offsets.get(skin_index as usize).copied().unwrap_or(0))
        });
        for (index, position) in mesh.positions.iter().enumerate() {
            let local = Vec3::from_array(*position);
            let world_pos = world.transform_point3(local);
            let normal = mesh
                .normals
                .get(index)
                .map(|normal| normal_matrix * Vec3::from_array(*normal))
                .filter(|normal| normal.length_squared() > 1e-12)
                .map(Vec3::normalize)
                // 没有法线就别猜：朝上会让模型整体发暗，取「背离中心」更接近剪影感。
                .unwrap_or_else(|| {
                    let away = world_pos - Vec3::ZERO;
                    if away.length_squared() > 1e-12 {
                        away.normalize()
                    } else {
                        Vec3::Y
                    }
                });
            // UV 缺失（几何有但没 TEXCOORD_0）时给 (0,0)：采样退化成「取左上角一个像素」，
            // 比留 NaN 让整块面变黑可控。
            let mut uv = mesh.uvs.get(index).copied().unwrap_or([0.0, 0.0]);
            if flip_v {
                uv[1] = 1.0 - uv[1];
            }

            // ---- 权重清洗 ----
            // 导出器常留脏数据：和为 0、NaN、负值、索引越界。NaN / 负值按槽位丢弃
            // 后重新归一化；只有「和为 0」或「有越界槽位」才让整个顶点退回静态
            // 路径（robust access 会给 0 矩阵，顶点直接塌到原点，宁可不动）。
            let mut joints = [0u16; 4];
            let mut weights = [0.0f32; 4];
            let mut skinned = false;
            if let Some(skin_base) = skin_base {
                let raw_weights = mesh.weights[index];
                let sum: f32 = raw_weights
                    .iter()
                    .filter(|weight| weight.is_finite() && **weight > 0.0)
                    .sum();
                if sum > 1e-6 {
                    let raw_joints = mesh.joints[index];
                    let mut valid = true;
                    for slot in 0..4 {
                        let global = skin_base as u32 + raw_joints[slot] as u32;
                        if raw_weights[slot] > 0.0 && global >= joint_total {
                            valid = false;
                        }
                        weights[slot] = if raw_weights[slot].is_finite() && raw_weights[slot] > 0.0 {
                            raw_weights[slot] / sum
                        } else {
                            0.0
                        };
                        joints[slot] = global.min(u16::MAX as u32) as u16;
                    }
                    skinned = valid;
                }
            }
            if skinned {
                skinned_vertex_count += 1;
            } else {
                weights = [0.0; 4];
                joints = [0; 4];
            }

            // 取景包围盒要用「绑定姿态下真正画出来的位置」：非蒙皮顶点就是
            // world_pos，蒙皮顶点得把关节矩阵算一遍（着色器里做的事，这里为
            // 了定相机距离先算一次）。不算的话，带骨骼位移的资产会被按未骨
            // 变换的位置取景，模型跑到画面外。
            let bound_pos = if skinned {
                let mut posed = Vec3::ZERO;
                for slot in 0..4 {
                    if weights[slot] > 0.0 {
                        let matrix = Mat4::from_cols_array(&joint_matrices[joints[slot] as usize]);
                        posed += matrix.transform_point3(local) * weights[slot];
                    }
                }
                posed
            } else {
                world_pos
            };
            min = min.min(bound_pos);
            max = max.max(bound_pos);
            vertices.push(Vertex {
                // 蒙皮顶点留在 mesh 局部空间：着色器用关节矩阵把它算到世界，
                // 变形才发生在 GPU 上。非蒙皮顶点直接把世界位置烘进去。
                pos: if skinned {
                    local.to_array()
                } else {
                    world_pos.to_array()
                },
                // 蒙皮时法线也留在局部空间，由关节矩阵的 3×3 部分变换。
                normal: if skinned {
                    mesh.normals.get(index).copied().unwrap_or([0.0, 1.0, 0.0])
                } else {
                    normal.to_array()
                },
                uv,
                joints,
                weights,
            });
        }

        // 索引可能指向不存在的顶点（畸形资产）：逐条过一遍，宁缺勿崩。
        let limit = mesh.positions.len() as u32;
        let mesh_triangles = (mesh.indices.len() / 3) as u32;
        let mut covered = vec![false; mesh_triangles as usize];
        // 不用 `mut`：闭包体不捕获可变环境，只通过参数改外部集合。
        let push_part = |range: std::ops::Range<u32>,
                             material: Material,
                             indices: &mut Vec<u32>,
                             parts: &mut Vec<ScenePart>| {
            let start = indices.len() as u32;
            for triangle in range {
                let at = triangle as usize * 3;
                let Some(triangle_indices) = mesh.indices.get(at..at + 3) else {
                    continue;
                };
                if triangle_indices.iter().all(|index| *index < limit) {
                    indices.extend(triangle_indices.iter().map(|index| base + *index));
                }
            }
            let count = indices.len() as u32 - start;
            if count > 0 {
                parts.push(ScenePart {
                    index_start: start,
                    index_count: count,
                    material,
                });
            }
        };

        // AIR 保证 `material_parts` 按三角形顺序排列（gltf 逐 primitive、fbx 逐材质游程）。
        for part in &mesh.material_parts {
            let start = part.triangle_offset.min(mesh_triangles);
            let end = start.saturating_add(part.triangle_count).min(mesh_triangles);
            if start >= end {
                continue;
            }
            covered[start as usize..end as usize].iter_mut().for_each(|flag| *flag = true);
            let material = material_of(doc, dir, part, &mut textures, &mut notes, &mut texture_count);
            push_part(start..end, material, &mut indices, &mut parts);
        }

        // 分区没覆盖到的三角形（缺材质段 / 覆盖不全的畸形资产）用兜底材质补上，
        // 否则那些面会凭空消失——比「颜色不对」难查得多。
        let mut cursor = 0u32;
        while cursor < mesh_triangles {
            if covered[cursor as usize] {
                cursor += 1;
                continue;
            }
            let start = cursor;
            while cursor < mesh_triangles && !covered[cursor as usize] {
                cursor += 1;
            }
            push_part(
                start..cursor,
                Material::fallback(usize::MAX),
                &mut indices,
                &mut parts,
            );
        }
        mesh_count += 1;
    }

    if vertices.is_empty() || indices.is_empty() {
        return Err(doc
            .issues
            .first()
            .cloned()
            .unwrap_or_else(|| "资产里没有可显示的网格（可能只有骨骼 / 相机 / 灯光）".to_string()));
    }

    if !min.is_finite() || !max.is_finite() {
        return Err("顶点坐标里没有有限值（资产可能全是 NaN）".to_string());
    }
    let center = (min + max) * 0.5;
    // 退化资产（单个面片）半径会是 0：给个下限，否则相机距离除零。
    let radius = ((max - min).length() * 0.5).max(1e-3);

    // 骨架视图复用上面算好的绑定姿态矩阵：绑定姿态只是「时间轴上的一条特例」，
    // 单独再算一遍既有第二份真相，也可能与包围盒口径漂开。
    let rig = Rig::new(doc, &skin_offsets, &joint_matrices);

    Ok(Scene {
        triangle_count: indices.len() / 3,
        vertices,
        indices,
        parts,
        joint_matrices,
        skinned_vertex_count,
        center,
        radius,
        mesh_count,
        texture_count,
        notes,
        rig,
    })
}

/// 把文档里所有 skin 展开成一张平铺的关节矩阵表。
///
/// 返回 `(矩阵表, 每个 skin 在表里的起始槽位)`。矩阵按列主序存 `[f32; 16]`，
/// 与 WGSL 的 `mat4x4<f32>` 内存布局一致，可以直接 `cast_slice` 上传。
///
/// 口径：`jointWorld × inverseBindMatrix`。顶点是 **mesh 局部坐标**，乘上它就
/// 落到世界空间——这是 glTF skinning 的标准式，蒙皮网格自身的节点变换不参与
/// （它已经含在关节世界矩阵里了）。
fn build_joint_matrices(
    doc: &AirDoc,
    cache: &mut Vec<Option<Mat4>>,
    notes: &mut Vec<String>,
) -> (Vec<[f32; 16]>, Vec<u32>) {
    let mut matrices: Vec<[f32; 16]> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(doc.skins.len());
    for (index, skin) in doc.skins.iter().enumerate() {
        offsets.push(matrices.len() as u32);
        let count = skin.joints.len();
        if skin.inverse_bind_matrices.len() != count && notes.len() < 12 {
            // 缺逆绑定矩阵不等于不能显示：按单位矩阵补，模型会「略微不对」但能看。
            notes.push(format!(
                "蒙皮 #{index}：关节数 {count} 与逆绑定矩阵数 {} 不一致，缺失按单位矩阵补齐",
                skin.inverse_bind_matrices.len()
            ));
        }
        for slot in 0..count {
            let inverse_bind = skin
                .inverse_bind_matrices
                .get(slot)
                .copied()
                .map(|matrix: [f32; 16]| Mat4::from_cols_array(&matrix))
                .filter(|matrix| matrix.is_finite())
                .unwrap_or(Mat4::IDENTITY);
            let joint = skin.joints[slot] as usize;
            let world = if joint < doc.nodes.len() {
                world_matrix(doc, joint, cache, 0)
            } else {
                if notes.len() < 12 {
                    notes.push(format!(
                        "蒙皮 #{index}：槽位 {slot} 指向不存在的节点 {joint}，按单位矩阵处理"
                    ));
                }
                Mat4::IDENTITY
            };
            matrices.push((world * inverse_bind).to_cols_array());
        }
    }
    (matrices, offsets)
}

/// 一个 skin 在骨架视图里的样子：关节节点 + 逆绑定矩阵 + 全局槽位起点。
struct RigSkin {
    /// 关节的**节点下标**，顺序与矩阵槽位一一对应。
    nodes: Vec<u32>,
    inverse_bind: Vec<Mat4>,
    /// 该 skin 的第一根骨头在全局矩阵表里的槽位。
    offset: u32,
}

/// 可播放的骨架：绑定姿态快照 + 动画轨道 + 逆绑定矩阵。
///
/// 为什么不在 `AirDoc` 上直接播：`AirDoc` 是「文件里的样子」，一旦被动画改写，
/// 重载 / 切动画 / scrub 回 0 都拿不到原始姿态了。所以这里留一份 `bind_nodes`
/// 做底图，每帧从它恢复再叠加动画——scrub 是幂等的，来回拖不会累积漂移。
pub struct Rig {
    bind_nodes: Vec<AirNode>,
    /// 求值用的 TRS 副本，逐帧改写。
    nodes: Vec<AirNode>,
    /// 世界矩阵缓存，逐帧清空。`None` = 本帧没算过。
    cache: Vec<Option<Mat4>>,
    animations: Vec<AirAnimation>,
    skins: Vec<RigSkin>,
    /// 平铺的 `jointWorld × inverseBindMatrix`，直接喂给 GPU 的 storage buffer。
    joint_matrices: Vec<[f32; 16]>,
    /// 每根关节的世界位置（骨骼线框的端点）。
    joint_points: Vec<Vec3>,
    /// 骨骼父子对，元素是矩阵槽位。父不在同一 skin 里时不出线——跨 skin 连
    /// 一根线在画面上就是「凭空多出一根棍子」。
    bone_lines: Vec<[u16; 2]>,
}

impl Rig {
    fn new(doc: &AirDoc, skin_offsets: &[u32], bind: &[[f32; 16]]) -> Option<Self> {
        // 只有骨骼没有动画也要留：骨骼线框是「这个资产到底有没有绑对」的
        // 第一眼判据；只有动画没有骨骼（纯节点动画）同样要留，时间轴得能播。
        if doc.skins.is_empty() && doc.animations.is_empty() {
            return None;
        }
        let mut skins = Vec::with_capacity(doc.skins.len());
        let mut bone_lines: Vec<[u16; 2]> = Vec::new();
        for (index, skin) in doc.skins.iter().enumerate() {
            let mut slot_of: HashMap<u32, u32> = HashMap::with_capacity(skin.joints.len());
            for (slot, node) in skin.joints.iter().enumerate() {
                // 同一关节重复出现时以第一个槽位为准：重复槽位本身就是畸形数据，
                // 后面那条线连到哪儿都是猜。
                slot_of.entry(*node).or_insert(slot as u32);
            }
            let offset = skin_offsets.get(index).copied().unwrap_or(0);
            for (slot, node) in skin.joints.iter().enumerate() {
                let parent = doc
                    .nodes
                    .get(*node as usize)
                    .and_then(|node| node.parent)
                    .and_then(|parent| slot_of.get(&parent).copied());
                if let Some(parent_slot) = parent {
                    let from = offset.saturating_add(parent_slot).min(u16::MAX as u32) as u16;
                    let to = offset.saturating_add(slot as u32).min(u16::MAX as u32) as u16;
                    bone_lines.push([from, to]);
                }
            }
            // 逆绑定矩阵口径与 `build_joint_matrices` 保持一致（缺失/非有限按单位阵）。
            let inverse_bind = (0..skin.joints.len())
                .map(|slot| {
                    skin.inverse_bind_matrices
                        .get(slot)
                        .copied()
                        .map(|matrix| Mat4::from_cols_array(&matrix))
                        .filter(|matrix| matrix.is_finite())
                        .unwrap_or(Mat4::IDENTITY)
                })
                .collect();
            skins.push(RigSkin {
                nodes: skin.joints.clone(),
                inverse_bind,
                offset,
            });
        }
        let mut rig = Self {
            bind_nodes: doc.nodes.clone(),
            nodes: doc.nodes.clone(),
            cache: vec![None; doc.nodes.len()],
            animations: doc.animations.clone(),
            skins,
            joint_matrices: bind.to_vec(),
            joint_points: Vec::new(),
            bone_lines,
        };
        // 先摆一次绑定姿态：把 `joint_points` 填上，也让「不播动画」时矩阵表与
        // `Scene::joint_matrices` 完全一致（同一份输入、同一套算式）。
        rig.pose_at(None, 0.0);
        Some(rig)
    }

    /// 在 `time` 处求值并写回矩阵表，返回被写入的轨道条数。
    ///
    /// `animation` 为 `None`（或下标失效）时退化成绑定姿态——这正是「暂停在 0
    /// 秒」该有的样子，不需要另开一条 reset 路径。
    pub fn pose_at(&mut self, animation: Option<usize>, time: f32) -> usize {
        let written = {
            let Self {
                bind_nodes,
                nodes,
                animations,
                ..
            } = self;
            nodes.clone_from(bind_nodes);
            match animation.and_then(|index| animations.get(index)) {
                Some(anim) => anim.apply_at(time, nodes),
                None => 0,
            }
        };
        let Self {
            nodes,
            cache,
            skins,
            joint_matrices,
            joint_points,
            ..
        } = self;
        for slot in cache.iter_mut() {
            *slot = None;
        }
        joint_points.clear();
        joint_points.resize(joint_matrices.len(), Vec3::ZERO);
        for skin in skins.iter() {
            for (slot, node) in skin.nodes.iter().enumerate() {
                let world = node_world(nodes, *node as usize, cache, 0);
                let inverse_bind = skin
                    .inverse_bind
                    .get(slot)
                    .copied()
                    .unwrap_or(Mat4::IDENTITY);
                let at = skin.offset as usize + slot;
                if let Some(entry) = joint_matrices.get_mut(at) {
                    *entry = (world * inverse_bind).to_cols_array();
                }
                if let Some(point) = joint_points.get_mut(at) {
                    // 蒙皮矩阵的平移列是「顶点被推到哪」，骨骼端点要的是关节
                    // 自己在世界里的位置，所以取 `world` 而不是上面的乘积。
                    *point = world.w_axis.truncate();
                }
            }
        }
        written
    }

    pub fn joint_matrices(&self) -> &[[f32; 16]] {
        &self.joint_matrices
    }

    pub fn joint_points(&self) -> &[Vec3] {
        &self.joint_points
    }

    pub fn bone_lines(&self) -> &[[u16; 2]] {
        &self.bone_lines
    }

    pub fn animation_count(&self) -> usize {
        self.animations.len()
    }

    pub fn animation_name(&self, index: usize) -> Option<&str> {
        self.animations.get(index).map(|anim| anim.name.as_str())
    }

    /// 动画时长（`effective_duration`：导入器没填 `duration` 时用轨道末端兜底）。
    pub fn duration(&self, index: usize) -> f32 {
        self.animations
            .get(index)
            .map(AirAnimation::effective_duration)
            .unwrap_or(0.0)
    }

    /// 选一条动画：指定名字优先（忽略大小写，兼容 `Take 001` / `take 001`），
    /// 名字对不上或没指定时退回第一条。`None` 表示这个资产根本没有动画。
    pub fn pick_animation(&self, hint: Option<&str>) -> Option<usize> {
        if self.animations.is_empty() {
            return None;
        }
        let hint = hint.map(str::trim).filter(|hint| !hint.is_empty());
        if let Some(hint) = hint {
            if let Some(index) = self
                .animations
                .iter()
                .position(|anim| anim.name.eq_ignore_ascii_case(hint))
            {
                return Some(index);
            }
        }
        Some(0)
    }

    /// 名字提示是否真的命中了（没命中时调用方该提示一句，而不是假装播的是你要的）。
    pub fn animation_name_matches(&self, index: usize, hint: Option<&str>) -> bool {
        let Some(hint) = hint.map(str::trim).filter(|hint| !hint.is_empty()) else {
            return true;
        };
        self.animation_name(index)
            .is_some_and(|name| name.eq_ignore_ascii_case(hint))
    }

    /// 日志用的一句话概览：`3 条动画（Idle 2.5s / Run 1.2s / …）`。
    pub fn animation_summary(&self) -> String {
        let mut parts: Vec<String> = self
            .animations
            .iter()
            .take(6)
            .map(|anim| {
                let name = if anim.name.is_empty() {
                    "(未命名)"
                } else {
                    anim.name.as_str()
                };
                format!("{name} {:.2}s", anim.effective_duration())
            })
            .collect();
        if self.animations.len() > parts.len() {
            parts.push(format!("…共 {} 条", self.animations.len()));
        }
        format!(
            "{} 条动画（{}）",
            self.animations.len(),
            parts.join(" / ")
        )
    }
}

/// 推进播放头，返回 `(新时间, 是否还在播)`。
///
/// 抽成纯函数是为了能脱开窗口测：播放循环里最容易错的就是「跨过末尾那一帧」
/// 到底该 wrap 还是该停，而这类边界在 GUI 里几乎无法复现。
pub fn advance_playhead(time: f32, dt: f32, duration: f32, looping: bool) -> (f32, bool) {
    if !duration.is_finite() || duration <= 0.0 {
        return (0.0, false);
    }
    // 单帧最多推进 0.1s：窗口拖动、断点会让 `dt` 变得很大，按真实间隔推进
    // 就会「一恢复就跳好几秒」。
    let time = time + dt.clamp(0.0, 0.1);
    if time < duration {
        return (time, true);
    }
    if looping {
        // `rem_euclid` 而不是 `-=`：一帧跨过多个循环长度时也不会丢掉余数。
        (time.rem_euclid(duration), true)
    } else {
        (duration, false)
    }
}

/// 把 AIR 的材质段翻译成渲染侧材质：baseColor + 解码后的 baseColor 贴图。
///
/// 失败一律降级为兜底材质并记一条 `notes`：贴图坏了不至于让人看不到模型。
fn material_of(
    doc: &AirDoc,
    dir: &Path,
    part: &MaterialPart,
    cache: &mut HashMap<usize, Option<Arc<TexImage>>>,
    notes: &mut Vec<String>,
    decoded: &mut usize,
) -> Material {
    let Some(index) = part.material else {
        return Material::fallback(usize::MAX);
    };
    let Some(material) = doc.materials.get(index as usize) else {
        return Material::fallback(usize::MAX);
    };
    let texture = material
        .base_color_texture
        .and_then(|texture_index| texture_image(doc, dir, texture_index, cache, notes, decoded));
    Material {
        key: index as usize,
        name: material.name.clone(),
        base_color: normalize_base_color(material),
        texture,
    }
}

/// AIR 的 `base_color` 是 glTF 的 linear factor / FBX 的 DiffuseColor。
/// 前者是线性值可直接用，后者是 sRGB 显示值——两者混在一起没法统一，这里只做
/// 「非法值兜底」，不做色彩空间转换（M-UI-3 的目标是看清形状与花纹，不是色准）。
fn normalize_base_color(material: &AirMaterial) -> [f32; 4] {
    let mut color = material.base_color;
    for channel in color.iter_mut() {
        if !channel.is_finite() {
            *channel = 1.0;
        }
        *channel = channel.clamp(0.0, 1.0);
    }
    // alpha 未做透明排序，先当不透明处理：半透明资产按 1.0 画，至少不会出现
    // 「黑洞」或前后遮挡乱序。
    color[3] = 1.0;
    color
}

/// 取一张贴图并解码。优先用内嵌的 base64（GLB / FBX 内嵌），其次按 `source_path`
/// 到资产同目录找（glTF 的外部 `.png`）。
fn texture_image(
    doc: &AirDoc,
    dir: &Path,
    index: u32,
    cache: &mut HashMap<usize, Option<Arc<TexImage>>>,
    notes: &mut Vec<String>,
    decoded: &mut usize,
) -> Option<Arc<TexImage>> {
    if let Some(hit) = cache.get(&(index as usize)) {
        return hit.clone();
    }
    let texture = doc.textures.get(index as usize);
    let image = texture.and_then(|texture| {
        let raw = read_texture_bytes(dir, texture)?;
        match decode(&raw, texture) {
            Ok(image) => {
                // 降采样是「看得到但缩过」，必须写清楚实际尺寸：不然用户只会觉得贴图糊，
                // 或者反过来以为资产里就是 4K 贴图。
                if let Some((original_w, original_h)) = image.downscaled_from {
                    if notes.len() < 12 {
                        notes.push(format!(
                            "贴图 `{}` {original_w}×{original_h} 超出 {edge}×{edge} 上限，已降采样到 {}×{} 后上传",
                            texture.name,
                            image.width,
                            image.height,
                            edge = MAX_TEXTURE_EDGE,
                        ));
                    }
                }
                *decoded += 1;
                Some(Arc::new(image))
            }
            Err(reason) => {
                notes.push(format!("贴图 `{}` {reason}", texture.name));
                None
            }
        }
    });
    cache.insert(index as usize, image.clone());
    image
}

/// 贴图原始编码字节（PNG/JPEG 原样，不解码）。
fn read_texture_bytes(dir: &Path, texture: &AirTexture) -> Option<Vec<u8>> {
    if let Some(data) = &texture.data_base64 {
        if let Ok(raw) = BASE64.decode(data) {
            if !raw.is_empty() {
                return Some(raw);
            }
        }
    }
    // 外部贴图：`source_path` 来自文件，和 `read_sibling` 同一套越界规则。
    let relative = texture.source_path.as_deref()?;
    read_sibling(dir, relative)
}

/// 解码成 RGBA8。只认 png / jpeg —— 与 Cargo 特性一致，其它格式直接说清楚。
fn decode(raw: &[u8], texture: &AirTexture) -> Result<TexImage, String> {
    let format = image::guess_format(raw)
        .map_err(|_| "不是可识别的图片数据（既不是 PNG 也不是 JPEG）".to_string())?;
    if !matches!(
        format,
        image::ImageFormat::Png | image::ImageFormat::Jpeg
    ) {
        return Err(format!("格式 {format:?} 不在视口支持范围内（只有 PNG / JPEG）"));
    }
    let image = image::load_from_memory_with_format(raw, format)
        .map_err(|err| format!("解码失败：{err}"))?;
    let mut rgba = image.to_rgba8();
    let (mut width, mut height) = rgba.dimensions();
    let mut downscaled_from = None;
    if u64::from(width) * u64::from(height) > MAX_TEXTURE_PIXELS {
        // 降采样而不是丢弃：贴图是 baseColor 的唯一信息来源，丢了模型就是纯白。
        let (target_w, target_h) = fit_within(width, height, MAX_TEXTURE_EDGE);
        // 采样核用 `Triangle`（双线性）：8K→4K 上 `Lanczos3` 要多跑数秒，而视口是
        // sRGB 直读、不做色彩管理，高阶核的锐度收益抵不过「导入一张图卡住十几秒」。
        rgba = image::imageops::resize(
            &rgba,
            target_w,
            target_h,
            image::imageops::FilterType::Triangle,
        );
        downscaled_from = Some((width, height));
        width = target_w;
        height = target_h;
    }
    let _ = texture.width;
    // 兜底断言：等比缩放后面积必然落在上限内，真越界了说明 `fit_within` 写错了，
    // 与其把 4K 以上的纹理甩给 GPU 触发校验错，不如在这里就暴露出来。
    debug_assert!(u64::from(width) * u64::from(height) <= MAX_TEXTURE_PIXELS);
    Ok(TexImage {
        width,
        height,
        rgba: rgba.into_raw(),
        downscaled_from,
    })
}

/// 等比缩放到最长边不超过 `edge`，两边至少 1 像素。
///
/// 用「四舍五入」而不是向下取整：短边向下取整会让 8191×4097 这类尺寸的比例明显走样。
/// 取整最多让短边多 0.5 像素，面积仍 ≤ `edge²`（最长边被 `min` 钉死）。
fn fit_within(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let longest = width.max(height).max(1);
    if longest <= edge {
        return (width.max(1), height.max(1));
    }
    let scale = |value: u32| {
        let scaled = (u64::from(value) * u64::from(edge) + u64::from(longest) / 2)
            / u64::from(longest);
        (scaled.max(1).min(u64::from(edge))) as u32
    };
    (scale(width), scale(height))
}

/// 节点世界矩阵 = 父链累乘。`depth` 只是坏 `parent`（自环 / 越界）的保险丝。
fn world_matrix(doc: &AirDoc, index: usize, cache: &mut Vec<Option<Mat4>>, depth: u32) -> Mat4 {
    node_world(&doc.nodes, index, cache, depth)
}

/// [`world_matrix`] 的节点切片版本：动画播放要在**逐帧变动的 TRS 副本**上重算世界
/// 矩阵，不能每次都回到 `AirDoc`（那里的 TRS 是绑定姿态）。
fn node_world(nodes: &[AirNode], index: usize, cache: &mut Vec<Option<Mat4>>, depth: u32) -> Mat4 {
    if let Some(Some(cached)) = cache.get(index) {
        return *cached;
    }
    let Some(node) = nodes.get(index) else {
        return Mat4::IDENTITY;
    };
    let local = Mat4::from_scale_rotation_translation(
        Vec3::from_array(node.scale),
        Quat::from_xyzw(
            node.rotation[0],
            node.rotation[1],
            node.rotation[2],
            node.rotation[3],
        )
        .normalize(),
        Vec3::from_array(node.position),
    );
    let world = match node.parent {
        // 自环直接当根：真实资产里偶有导出器写坏 parent。
        Some(parent) if depth < 64 && parent as usize != index => {
            node_world(nodes, parent as usize, cache, depth + 1) * local
        }
        _ => local,
    };
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(world);
    }
    world
}

/// 读 `gltf` 的外部 `uri`（`buffers[].uri` / `images[].uri`）。
///
/// 只允许落在资产同目录下：`uri` 来自文件，不该被当成往任意路径读文件的口子。
fn read_sibling(dir: &Path, uri: &str) -> Option<Vec<u8>> {
    if uri.is_empty() || uri.contains("://") || uri.starts_with("data:") {
        return None;
    }
    let decoded = percent_decode(uri);
    if decoded.is_empty() {
        return None;
    }
    let candidate = {
        let raw = PathBuf::from(&decoded);
        if raw.is_absolute() {
            raw
        } else {
            dir.join(raw)
        }
    };
    let canonical = candidate.canonicalize().ok()?;
    let root = dir.canonicalize().ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }
    std::fs::read(canonical).ok()
}

/// 空场景的「参考地板」：1 米一格的地面网格 + 原点处的三色坐标轴。
///
/// 为什么要有它：双击启动、或 `close` 掉模型之后，窗口只剩一块纯色——用户会以为
/// 程序没起来。地板 + 坐标轴给人尺度感（模型多大、原点在哪），也让「窗口是活的」
/// 一眼可见。几何只有线段，用不到索引，顶点按 `LineList` 两个一组排列。
///
/// 颜色借 `normal` 字段承载：网格线走自己的着色器，不参与光照，这个字段对它是
/// 「RGB 颜色」而不是法线，省一个顶点布局。UV 同理填 0：线段不采样。
pub fn reference_grid() -> Scene {
    /// 地面半宽（米）：20×20 的方格够放下一个房间大的资产。
    const HALF: i32 = 10;
    const GRID: [f32; 3] = [0.26, 0.28, 0.32];
    const GRID_MAJOR: [f32; 3] = [0.36, 0.38, 0.43];
    const AXIS_X: [f32; 3] = [0.72, 0.26, 0.28];
    const AXIS_Z: [f32; 3] = [0.26, 0.36, 0.72];

    let mut vertices: Vec<Vertex> = Vec::new();
    let mut push = |from: [f32; 3], to: [f32; 3], color: [f32; 3]| {
        vertices.push(Vertex {
            pos: from,
            normal: color,
            uv: [0.0, 0.0],
            joints: [0; 4],
            weights: [0.0; 4],
        });
        vertices.push(Vertex {
            pos: to,
            normal: color,
            uv: [0.0, 0.0],
            joints: [0; 4],
            weights: [0.0; 4],
        });
    };

    let span = HALF as f32;
    for step in -HALF..=HALF {
        let at = step as f32;
        // 每 5 格一条亮线：远看有刻度，近看不过于密。
        let color = if step % 5 == 0 { GRID_MAJOR } else { GRID };
        // 与 X 轴重合的那条让给坐标轴，否则红色会被灰线覆盖。
        if step != 0 {
            push([-span, 0.0, at], [span, 0.0, at], color);
        }
        push([at, 0.0, -span], [at, 0.0, span], color);
    }
    // 坐标轴：X 红、Z 蓝，长度与地面同宽。Y 轴不画——正上方看下去是一条点线，
    // 只会让画面多一处噪点。
    push([-span, 0.0, 0.0], [span, 0.0, 0.0], AXIS_X);
    push([0.0, 0.0, -span], [0.0, 0.0, span], AXIS_Z);

    Scene {
        vertices,
        indices: Vec::new(),
        parts: Vec::new(),
        // 地板不蒙皮：关节表空，顶点权重全零，着色器走静态路径。
        joint_matrices: Vec::new(),
        skinned_vertex_count: 0,
        center: Vec3::ZERO,
        radius: span,
        mesh_count: 0,
        triangle_count: 0,
        texture_count: 0,
        notes: Vec::new(),
        // 地板没有骨骼也没有动画。
        rig: None,
    }
}

/// `%20` 这类转义：glTF 的外部 `uri` 按 URI 编码，直接当路径用会找不到文件。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

#[cfg(test)]
mod tests {
    // 视口场景层单测（M-UI-3 第三刀：材质贴图）。
    //
    // 只测纯 CPU 部分：分区切分、UV 轴翻转、贴图解码与降级。GPU 侧（bind group
    // 去重、采样器）靠实机截图验证，不在这层断言。

    use super::*;
    use crate::air::{AirAnimation, AirMesh, AirNode, AirSkin, AirTrack};

    /// 一个 quad：4 顶点 2 三角形，UV 四角给极值，方便断言翻转。
    fn quad_mesh(parts: Vec<MaterialPart>, indices: Vec<u32>) -> AirMesh {
        AirMesh {
            name: "quad".to_string(),
            node: "n0".to_string(),
            positions: vec![
                [-1.0, 0.0, -1.0],
                [1.0, 0.0, -1.0],
                [1.0, 0.0, 1.0],
                [-1.0, 0.0, 1.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.25], [1.0, 0.75], [0.0, 1.0]],
            indices,
            material_parts: parts,
            ..Default::default()
        }
    }

    fn doc_with(mesh: AirMesh) -> AirDoc {
        let mut doc = AirDoc::new("t.glb", "t.glb");
        doc.meshes.push(mesh);
        doc.nodes.push(AirNode {
            name: "n0".to_string(),
            mesh: Some(0),
            // 单位变换要显式写：`AirNode` 的 Default 是全 0，直接堆出来的节点
            // 会把所有顶点缩到原点（merge 里会被「无有限值」拒掉）。
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
            ..Default::default()
        });
        doc
    }

    fn material(name: &str, color: [f32; 4]) -> AirMaterial {
        AirMaterial {
            name: name.to_string(),
            base_color: color,
            ..Default::default()
        }
    }

    /// 两个材质段必须切出两个分区，索引区间首尾相接且各自 3 个索引。
    #[test]
    fn mesh_splits_into_one_part_per_material() {
        let mesh = quad_mesh(
            vec![
                MaterialPart {
                    material: Some(0),
                    material_name: "red".to_string(),
                    triangle_offset: 0,
                    triangle_count: 1,
                },
                MaterialPart {
                    material: Some(1),
                    material_name: "blue".to_string(),
                    triangle_offset: 1,
                    triangle_count: 1,
                },
            ],
            vec![0, 1, 2, 0, 2, 3],
        );
        let mut doc = doc_with(mesh);
        doc.materials = vec![
            material("red", [1.0, 0.0, 0.0, 1.0]),
            material("blue", [0.0, 0.0, 1.0, 1.0]),
        ];

        let scene = merge(&doc, Path::new(".")).expect("merge");
        assert_eq!(scene.parts.len(), 2);
        assert_eq!((scene.parts[0].index_start, scene.parts[0].index_count), (0, 3));
        assert_eq!((scene.parts[1].index_start, scene.parts[1].index_count), (3, 3));
        assert_eq!(scene.parts[0].material.name, "red");
        assert_eq!(scene.parts[0].material.base_color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(scene.parts[1].material.name, "blue");
        // 没有贴图声明 → 不该凭空生成 texture，渲染侧走 baseColor 分支。
        assert!(scene.parts.iter().all(|part| part.material.texture.is_none()));
        assert_eq!(scene.texture_count, 0);
        assert_eq!(scene.triangle_count, 2);
    }

    /// FBX 的 UV 原点在左下；`units.uv_origin` 说 bottom-left 时 v 必须翻。
    #[test]
    fn uv_v_is_flipped_for_bottom_left_origin() {
        let mesh = quad_mesh(
            vec![MaterialPart {
                material: Some(0),
                material_name: "m".to_string(),
                triangle_offset: 0,
                triangle_count: 2,
            }],
            vec![0, 1, 2, 0, 2, 3],
        );
        let mut doc = doc_with(mesh);
        doc.materials = vec![material("m", [1.0, 1.0, 1.0, 1.0])];
        doc.units.uv_origin = "bottom-left".to_string();

        let scene = merge(&doc, Path::new(".")).expect("merge");
        assert_eq!(scene.vertices[1].uv, [1.0, 0.75], "0.25 要翻成 0.75");
        assert_eq!(scene.vertices[2].uv, [1.0, 0.25]);

        // 反过来（glTF 的 top-left）原样保留。
        doc.units.uv_origin = "top-left".to_string();
        let same = merge(&doc, Path::new(".")).expect("merge");
        assert_eq!(same.vertices[1].uv, [1.0, 0.25]);
    }

    /// 索引指到顶点数之外的三角形要整块丢掉：宁缺勿崩。
    #[test]
    fn out_of_range_indices_are_dropped() {
        let mesh = quad_mesh(
            vec![MaterialPart {
                material: Some(0),
                material_name: "m".to_string(),
                triangle_offset: 0,
                triangle_count: 2,
            }],
            // 第二个三角形引用了不存在的顶点 99。
            vec![0, 1, 2, 0, 99, 3],
        );
        let mut doc = doc_with(mesh);
        doc.materials = vec![material("m", [1.0, 1.0, 1.0, 1.0])];

        let scene = merge(&doc, Path::new(".")).expect("merge");
        assert_eq!(scene.triangle_count, 1);
        assert_eq!(scene.parts.len(), 1);
        assert_eq!(scene.parts[0].index_count, 3);
    }

    /// 材质段没盖到的三角形用兜底材质补上，不能凭空消失。
    #[test]
    fn uncovered_triangles_fall_back_instead_of_vanishing() {
        let mesh = quad_mesh(
            vec![MaterialPart {
                material: Some(0),
                material_name: "m".to_string(),
                triangle_offset: 0,
                triangle_count: 1,
            }],
            vec![0, 1, 2, 0, 2, 3],
        );
        let mut doc = doc_with(mesh);
        doc.materials = vec![material("m", [1.0, 1.0, 1.0, 1.0])];

        let scene = merge(&doc, Path::new(".")).expect("merge");
        assert_eq!(scene.parts.len(), 2, "第二段是兜底");
        assert_eq!(scene.parts[1].material.key, usize::MAX);
        assert!(scene.parts[1].material.name.is_empty());
        assert_eq!(scene.triangle_count, 2);
    }

    /// 贴图文件不存在时不报错、不 panic，只是没有贴图。
    #[test]
    fn missing_texture_degrades_to_solid_color() {
        let mesh = quad_mesh(
            vec![MaterialPart {
                material: Some(0),
                material_name: "m".to_string(),
                triangle_offset: 0,
                triangle_count: 2,
            }],
            vec![0, 1, 2, 0, 2, 3],
        );
        let mut doc = doc_with(mesh);
        doc.materials = vec![AirMaterial {
            name: "m".to_string(),
            base_color: [0.9, 0.4, 0.1, 1.0],
            base_color_texture: Some(0),
            ..Default::default()
        }];
        doc.textures = vec![AirTexture {
            name: "nope.png".to_string(),
            source_path: Some("nope.png".to_string()),
            ..Default::default()
        }];

        let scene = merge(&doc, Path::new(".")).expect("merge");
        assert!(scene.parts[0].material.texture.is_none());
        assert_eq!(scene.texture_count, 0);
        assert_eq!(scene.parts[0].material.base_color, [0.9, 0.4, 0.1, 1.0]);
    }

    /// baseColor 的非法值要收敛：NaN → 1.0，越界 clamp，alpha 先当不透明。
    #[test]
    fn base_color_is_sanitized() {
        let color = normalize_base_color(&AirMaterial {
            base_color: [f32::NAN, 2.0, -1.0, 0.2],
            ..Default::default()
        });
        assert_eq!(color, [1.0, 1.0, 0.0, 1.0]);
    }

    /// 同目录内的相对 uri 可读，越界 / 远程 / data: 一律拒。
    #[test]
    fn only_sibling_uris_are_read() {
        assert!(read_sibling(Path::new("."), "http://example.com/a.png").is_none());
        assert!(read_sibling(Path::new("."), "data:image/png;base64,AAAA").is_none());
        assert!(read_sibling(Path::new("."), "").is_none());
    }

    /// `%20` 这类转义要还原，坏转义原样保留。
    #[test]
    fn percent_decoding_handles_escapes() {
        assert_eq!(percent_decode("a%20b.png"), "a b.png");
        assert_eq!(percent_decode("100%.png"), "100%.png");
        assert_eq!(percent_decode("a%zz.png"), "a%zz.png");
    }

    /// 只认 PNG / JPEG：其它格式给明确原因，让 `notes` 能说清为什么是纯色。
    #[test]
    fn unsupported_image_formats_are_rejected() {
        let mut bmp = vec![0u8; 64];
        bmp[0] = b'B';
        bmp[1] = b'M';
        // `TexImage` 没派生 Debug（rgba 太吵），所以不用 `expect_err`。
        let reason = match decode(&bmp, &AirTexture::default()) {
            Ok(_) => panic!("BMP 不该被接受"),
            Err(reason) => reason,
        };
        assert!(reason.contains("PNG"), "{reason}");
    }

    /// 超限贴图必须降采样后**继续上传**：Tripo 那批资产的 `baseColorFactor` 是
    /// (1,1,1,1)，贴图一丢就是纯白模型——降采样只是分辨率打折，不是「看不见」。
    ///
    /// debug 下要编解码 64 Mi 像素，跑几分钟；默认只在 release / `--ignored` 时跑。
    #[test]
    #[ignore = "debug 下编解码 64 Mi 像素太慢，用 cargo test --release -- --ignored"]
    fn oversized_texture_is_downscaled_instead_of_dropped() {
        use image::ImageEncoder as _;

        let (source_w, source_h) = (4096u32, 4097u32);
        assert!(
            u64::from(source_w) * u64::from(source_h) > MAX_TEXTURE_PIXELS,
            "这条测试必须越过像素上限才有意义"
        );
        let pixel = [12u8, 34, 56, 255];
        let raw: Vec<u8> = pixel
            .iter()
            .copied()
            .cycle()
            .take((source_w * source_h * 4) as usize)
            .collect();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&raw, source_w, source_h, image::ExtendedColorType::Rgba8)
            .expect("编码 PNG");

        let image = decode(&png, &AirTexture::default()).expect("超限只该降采样，不该报错");
        assert_eq!(image.width.max(image.height), MAX_TEXTURE_EDGE);
        assert!(
            u64::from(image.width) * u64::from(image.height) <= MAX_TEXTURE_PIXELS,
            "降采样后仍在像素上限之外：{}×{}",
            image.width,
            image.height
        );
        assert_eq!(image.downscaled_from, Some((source_w, source_h)));
        assert_eq!(
            image.rgba.len() as u64,
            u64::from(image.width) * u64::from(image.height) * 4
        );
        // 抽样确认颜色还在：全白正是缺陷的表征，这里必须不是白色。
        let probe = &image.rgba[(image.width as usize * 2000 + 2000) * 4..][..4];
        assert_eq!(probe, pixel, "降采样后像素被清成白色");
    }

    /// 等比缩放：长边钉在 4096，短边按比例四舍五入；没超限的原样返回。
    #[test]
    fn fit_within_keeps_aspect_ratio() {
        assert_eq!(fit_within(8192, 8192, 4096), (4096, 4096));
        assert_eq!(fit_within(8000, 4000, 4096), (4096, 2048));
        assert_eq!(fit_within(1000, 500, 4096), (1000, 500));
        assert_eq!(fit_within(0, 0, 4096), (1, 1));
        // 极端长条：短边至少要留 1 像素，塌成 0 会让 wgpu 直接拒绝建纹理。
        assert_eq!(fit_within(100_000, 3, 4096), (4096, 1));
    }

    /// 真实 Tripo 资产回归（默认跳过，本机手动跑）：
    ///
    /// ```text
    /// ASSET3D_REAL_GLB=D:\share\sword\ironsword.glb `
    ///   cargo test --release real_glb -- --ignored --nocapture
    /// ```
    ///
    /// 走完整链路：读文件 → glTF 解析 → AIR（内嵌贴图）→ `merge` → 解码 8192×8192
    /// baseColor → 降采样 → 绑材质。像素抽样是「有没有变白模」的直接判据。
    #[test]
    #[ignore = "需要真实 8K 资产（ASSET3D_REAL_GLB），默认跳过"]
    fn real_glb_base_color_texture_is_bound_after_downscale() {
        let path = std::env::var("ASSET3D_REAL_GLB").expect("设置 ASSET3D_REAL_GLB 指向 .glb");
        // `set VAR=... && ...` 会把尾随空格带进来，路径被拼成 `.glb `，扩展名判定就废了。
        let path = path.trim().to_string();
        let scene = load(Path::new(&path)).expect("加载真实资产");
        let texture = scene.parts[0]
            .material
            .texture
            .as_ref()
            .expect("baseColor 贴图必须解码成功并绑上材质");
        assert!(
            texture.width.max(texture.height) <= MAX_TEXTURE_EDGE,
            "上传尺寸 {}×{} 超出长边上限",
            texture.width,
            texture.height
        );
        assert!(texture.downscaled_from.is_some(), "8192×8192 原图必须走降采样");

        // 5×5 均匀抽样：真贴图必然有大量非白像素；整片纯白就是缺陷复现。
        let (width, height) = (texture.width as usize, texture.height as usize);
        let mut samples = Vec::new();
        for gy in 0..5 {
            for gx in 0..5 {
                let x = width * (gx * 2 + 1) / 10;
                let y = height * (gy * 2 + 1) / 10;
                let at = (y * width + x) * 4;
                samples.push([texture.rgba[at], texture.rgba[at + 1], texture.rgba[at + 2]]);
            }
        }
        let colored = samples.iter().filter(|px| px.iter().any(|c| *c < 240)).count();
        println!(
            "{}: 原图 {:?} → {}×{}，notes={:?}，抽样非白 {}/{}，样本 {:?}",
            path,
            texture.downscaled_from,
            width,
            height,
            scene.notes,
            colored,
            samples.len(),
            &samples[..5]
        );
        assert!(colored >= 20, "抽样点几乎全白，贴图没真正绑上：{samples:?}");
        assert!(
            scene
                .notes
                .iter()
                .any(|note| note.contains("已降采样到")),
            "notes 必须写清实际降采样尺寸：{:?}",
            scene.notes
        );
    }

    // ---- M-UI-4 第二刀：蒙皮 ----
    use glam::Mat4;

    /// 构造一份「一个蒙皮网格 + 一根关节」的文档。关节是根节点，世界矩阵就是
    /// 它的 TRS；`inverse_bind` 由调用方给，用来模拟 bind pose / 任意姿态。
    fn skinned_doc(
        joint_translation: glam::Vec3,
        inverse_bind: Mat4,
        joints: Vec<[u16; 4]>,
        weights: Vec<[f32; 4]>,
    ) -> AirDoc {
        let mut mesh = quad_mesh(vec![], vec![0, 1, 2, 0, 2, 3]);
        mesh.joints = joints;
        mesh.weights = weights;
        let mut doc = doc_with(mesh);
        doc.nodes[0].skin = Some(0);
        doc.nodes.push(AirNode {
            name: "joint".to_string(),
            index: 1,
            position: joint_translation.to_array(),
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
            ..Default::default()
        });
        doc.skins.push(AirSkin {
            index: 0,
            name: "skin".to_string(),
            joints: vec![1],
            inverse_bind_matrices: vec![inverse_bind.to_cols_array()],
        });
        doc
    }

    /// 顶点权重全 1、关节指向平移 (2,0,0) 的根节点：矩阵第 4 列必须是平移，
    /// 顶点留在局部空间（着色器才算得出世界位置），权重和为 1。
    #[test]
    fn skin_matrix_bakes_joint_world_times_inverse_bind() {
        let joints = vec![[0u16, 0, 0, 0]; 4];
        let weights = vec![[1.0f32, 0.0, 0.0, 0.0]; 4];
        let doc = skinned_doc(glam::Vec3::new(2.0, 0.0, 0.0), Mat4::IDENTITY, joints, weights);
        let scene = merge(&doc, Path::new(".")).expect("蒙皮场景应能加载");

        assert_eq!(scene.joint_matrices.len(), 1);
        let matrix = Mat4::from_cols_array(&scene.joint_matrices[0]);
        assert!((matrix.w_axis.x - 2.0).abs() < 1e-6, "{matrix:?}");
        assert_eq!(scene.skinned_vertex_count, 4);
        // 蒙皮顶点必须是局部坐标：quad 的四角是 ±1，不能被烘到世界。
        assert!((scene.vertices[0].pos[0] + 1.0).abs() < 1e-6);
        assert_eq!(scene.vertices[0].joints[0], 0);
        let sum: f32 = scene.vertices[0].weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        // 包络按世界位置算：平移 2 之后中心 x 应在 2 附近。
        assert!((scene.center.x - 2.0).abs() < 1e-6, "{:?}", scene.center);
    }

    /// bind pose 的判据：`inverse_bind = inverse(jointWorld)` 时矩阵退回单位阵，
    /// 顶点算出来必须还是原地——这正是「不播放动画时模型不变形」的数学保证。
    #[test]
    fn bind_pose_degenerates_to_identity() {
        let joint_world = Mat4::from_translation(glam::Vec3::new(2.0, 0.0, 0.0));
        let joints = vec![[0u16, 0, 0, 0]; 4];
        let weights = vec![[1.0f32, 0.0, 0.0, 0.0]; 4];
        let doc = skinned_doc(
            glam::Vec3::new(2.0, 0.0, 0.0),
            joint_world.inverse(),
            joints,
            weights,
        );
        let scene = merge(&doc, Path::new(".")).expect("蒙皮场景应能加载");
        let matrix = Mat4::from_cols_array(&scene.joint_matrices[0]);
        for (actual, expected) in matrix.to_cols_array().iter().zip(
            Mat4::IDENTITY
                .to_cols_array()
                .iter(),
        ) {
            assert!((actual - expected).abs() < 1e-6, "{matrix:?}");
        }
    }

    /// 脏权重按槽位清洗：NaN 与负值置 0，剩下的重新归一化；全零才退回静态。
    /// 「清洗」和「回退」是两条不同的路径——能救的顶点不该被整体放弃。
    #[test]
    fn dirty_weights_are_scrubbed_not_dropped() {
        let joints = vec![[0u16, 0, 0, 0]; 4];
        let mut weights = vec![[0.0f32; 4]; 4];
        // NaN 与 -1 都不该参与；0.5 归一化后是 1.0。
        weights[1] = [f32::NAN, 0.5, -1.0, 0.0];
        let doc = skinned_doc(glam::Vec3::ZERO, Mat4::IDENTITY, joints, weights);
        let scene = merge(&doc, Path::new(".")).expect("脏权重不该挡住加载");

        assert_eq!(scene.skinned_vertex_count, 1);
        let scrubbed = scene.vertices[1].weights;
        assert_eq!(scrubbed[0], 0.0);
        assert_eq!(scrubbed[1], 1.0);
        assert_eq!(scrubbed[2], 0.0);
        let sum: f32 = scrubbed.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        // 其余顶点权重本来就是 0：它们必须走静态路径，位置是世界空间。
        assert_eq!(scene.vertices[0].weights, [0.0; 4]);
    }

    /// 越界关节索引同样退回静态：宁可这幅面不蒙皮，也不要整块塌掉。
    #[test]
    fn out_of_range_joint_index_falls_back_to_static() {
        let joints = vec![[7u16, 0, 0, 0]; 4];
        let weights = vec![[1.0f32, 0.0, 0.0, 0.0]; 4];
        let doc = skinned_doc(glam::Vec3::ZERO, Mat4::IDENTITY, joints, weights);
        let scene = merge(&doc, Path::new(".")).expect("越界索引不该挡住加载");
        assert_eq!(scene.skinned_vertex_count, 0);
        assert!(scene.vertices.iter().all(|v| v.joints == [0; 4]));
    }

    // ---- M-UI-4 第三刀：时间轴 ----

    /// 一份「关节沿 X 平移 0→4、时长 1s」的文档。
    fn animated_doc() -> AirDoc {
        let joints = vec![[0u16, 0, 0, 0]; 4];
        let weights = vec![[1.0f32, 0.0, 0.0, 0.0]; 4];
        let mut doc = skinned_doc(glam::Vec3::ZERO, Mat4::IDENTITY, joints, weights);
        doc.animations.push(AirAnimation {
            name: "Walk".to_string(),
            duration: 1.0,
            tracks: vec![AirTrack {
                node: 1,
                property: "translation".to_string(),
                interpolation: "linear".to_string(),
                times: vec![0.0, 1.0],
                values: vec![[0.0, 0.0, 0.0, 0.0], [4.0, 0.0, 0.0, 0.0]],
            }],
        });
        doc
    }

    /// 第三刀的核心断言：同一个 `Rig` 在不同时间求值，关节矩阵必须跟着动——
    /// 这就是「动画真的驱动了 GPU 骨骼矩阵」的最小证据。
    #[test]
    fn rig_pose_drives_joint_matrices_over_time() {
        let doc = animated_doc();
        let mut scene = merge(&doc, Path::new(".")).expect("带动画的蒙皮场景应能加载");
        let mut rig = scene.rig.take().expect("有蒙皮就该有 rig");

        assert_eq!(rig.animation_count(), 1);
        assert!((rig.duration(0) - 1.0).abs() < 1e-6);
        // 名字大小写不敏感；对不上时退回第一条，而不是「什么都不播」。
        assert_eq!(rig.pick_animation(Some("walk")), Some(0));
        assert_eq!(rig.pick_animation(Some("Nope")), Some(0));
        assert!(!rig.animation_name_matches(0, Some("Nope")));
        assert!(rig.animation_name_matches(0, Some("WALK")));

        rig.pose_at(Some(0), 0.0);
        assert!(Mat4::from_cols_array(&rig.joint_matrices()[0])
            .w_axis
            .x
            .abs()
            < 1e-6);
        // 半程走一半：线性插值给 2，容差留够（轨道采样内部按 f32 累加）。
        rig.pose_at(Some(0), 0.5);
        assert!((Mat4::from_cols_array(&rig.joint_matrices()[0]).w_axis.x - 2.0).abs() < 1e-3);
        rig.pose_at(Some(0), 1.0);
        assert!((Mat4::from_cols_array(&rig.joint_matrices()[0]).w_axis.x - 4.0).abs() < 1e-6);
        // 骨骼端点跟的是关节自身世界位置，不是蒙皮矩阵的平移列。
        assert!((rig.joint_points()[0].x - 4.0).abs() < 1e-6);

        // 退回绑定姿态：矩阵必须干净地回到单位阵，不能残留上一次的姿态。
        rig.pose_at(None, 0.0);
        let back = Mat4::from_cols_array(&rig.joint_matrices()[0]);
        for (actual, expected) in back
            .to_cols_array()
            .iter()
            .zip(Mat4::IDENTITY.to_cols_array().iter())
        {
            assert!((actual - expected).abs() < 1e-6, "{back:?}");
        }
        assert!(rig.joint_points()[0].length() < 1e-6);
    }

    /// 播放头推进：末尾那一帧是 wrap 还是停，决定「循环开关」到底有没有用。
    #[test]
    fn playhead_wraps_or_stops_at_the_end() {
        // 正常推进：一帧 50ms。
        assert_eq!(advance_playhead(0.0, 0.05, 1.0, true), (0.05, true));
        // 断点/拖动窗口会让 `dt` 很大：按 0.1s 截断，不许一次跳好几秒。
        let (time, playing) = advance_playhead(0.0, 5.0, 1.0, true);
        assert!((time - 0.1).abs() < 1e-6, "{time}");
        assert!(playing);
        // 跨过末尾 + 循环：余数保留（0.45 + 0.1 = 0.55 → 0.05）。
        let (time, playing) = advance_playhead(0.45, 0.1, 0.5, true);
        assert!((time - 0.05).abs() < 1e-6, "{time}");
        assert!(playing);
        // 跨过末尾 + 不循环：停在末尾。
        let (time, playing) = advance_playhead(0.45, 0.1, 0.5, false);
        assert!((time - 0.5).abs() < 1e-6, "{time}");
        assert!(!playing);
        // 一帧跨过多个循环长度（时长比截断步长还短）：余数仍然合法。
        let (time, playing) = advance_playhead(0.02, 0.1, 0.03, true);
        assert!((0.0..0.03).contains(&time), "{time}");
        assert!(playing);
        // 没有可播的动画：时间归零且不播，避免空转重绘。
        assert_eq!(advance_playhead(0.3, 0.016, 0.0, true), (0.0, false));
        assert_eq!(advance_playhead(0.3, 0.016, f32::NAN, true), (0.0, false));
        // 负 `dt`（时间回拨）不推进，也不许把播放头拉成负数。
        assert_eq!(advance_playhead(0.2, -1.0, 1.0, true), (0.2, true));
    }
}
