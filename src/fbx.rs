//! FBX → AIR。
//!
//! 用 ufbx 做解析，但**换算与归一化全部在这里完成**，不让消费方猜：
//! - 轴向/单位：`LoadOpts.target_axes` 指定 +X right / +Y up / +Z front、
//!   `target_unit_meters = 1.0`，由 ufbx 在构建 `*_to_world` 矩阵时一次性算好；
//!   原始单位与 up 轴仍记录进 `AirSource`，用于解释"模型看起来大了 100 倍"这类问题。
//! - 顶点：必须按「角点」拆分而不是按 ufbx 顶点号。同一个位号在 UV 接缝/硬边处
//!   会对应不同 UV 与法线，按位号复用会造成贴图扭曲；这里以 `(pos, nrm, uv)`
//!   三元组去重，并保留「输出顶点 → ufbx 位号」映射，供蒙皮权重回填。
//! - 三角化：逐 face 调 `triangulate_face_vec`，绝不拿 `num_indices` 直接拼——
//!   四边面/多边形会算错索引数；`reversed_winding` 时交换 2/3 号角点保证 CCW。
//! - UV：FBX 原点在左下，引擎在左上，统一 `v' = 1 - v`。
//! - 骨骼：ufbx 的权重按位号聚集（`deformer.vertices[v]` 给出区间），必须借上面的
//!   映射搬到拆分后的顶点上，再取权重最大的 4 个并归一化。

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::air::*;
use crate::util;

/// 一条 FBX 导入的入口。
///
/// `hint` 是源文件在工作区内的相对路径，ufbx 用它解析贴图的相对路径并写进
/// `Texture::relative_filename`；`embed_textures` 决定是否把贴图字节塞进 AIR。
pub fn import(
    doc: &mut AirDoc,
    bytes: &[u8],
    hint: &str,
    want_animations: bool,
    embed_textures: bool,
) -> Result<(), String> {
    let scene = ufbx::load_memory(bytes, load_opts(hint))
        .map_err(|err| format!("FBX 解析失败：{:?} {}", err.type_, err.description))?;

    // ---- 源信息：单位/轴/创建器，出问题时最先要看的三个字段 ----
    doc.source.bytes = bytes.len() as u64;
    doc.source.native_unit_meters = Some(scene.settings.original_unit_meters as f32);
    doc.source.native_up_axis = Some(axis_name(scene.settings.original_axis_up));
    let creator = scene.metadata.creator.to_string();
    if !creator.is_empty() {
        doc.source.creator = Some(creator);
    }
    if !scene.metadata.ascii {
        doc.source.container = "fbx".to_string();
    }
    for warning in &scene.metadata.warnings {
        doc.note(format!("ufbx 警告：{}", warning.description));
    }

    // ---- 节点表 ----
    let mut index_of: HashMap<u32, u32> = HashMap::with_capacity(scene.nodes.len());
    for (i, node) in scene.nodes.iter().enumerate() {
        index_of.insert(node.element.element_id, i as u32);
    }

    let mut paths: Vec<String> = vec![String::new(); scene.nodes.len()];
    for (i, node) in scene.nodes.iter().enumerate() {
        let parent_path = node
            .parent
            .as_ref()
            .and_then(|parent| index_of.get(&parent.element.element_id).copied())
            .and_then(|parent| paths.get(parent as usize).cloned())
            .unwrap_or_default();
        let name = node.element.name.to_string();
        let name = if name.is_empty() {
            format!("node{i}")
        } else {
            name
        };
        paths[i] = if parent_path.is_empty() {
            name
        } else {
            format!("{parent_path}/{name}")
        };
    }
    doc.nodes = scene
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let parent = node
                .parent
                .as_ref()
                .and_then(|p| index_of.get(&p.element.element_id).copied());
            // 局部变换用 node_to_parent（已含轴向/单位换算）；根节点没有父，
            // 直接用它自己的世界矩阵，避免丢根上的缩放。
            let local = util::mat4_from_ufbx(if parent.is_some() {
                &node.node_to_parent
            } else {
                &node.node_to_world
            });
            let (position, rotation, scale) = util::decompose(&local);
            AirNode {
                index: i as u32,
                name: node.element.name.to_string(),
                path: paths[i].clone(),
                parent,
                position,
                rotation,
                scale,
                mesh: None,
                skin: None,
            }
        })
        .collect();

    // ---- 材质与贴图（全量抽出：即使没有被网格引用，也保留，便于诊断）----
    let mut material_id_to_index: HashMap<u32, u32> = HashMap::with_capacity(scene.materials.len());
    let mut textures: Vec<AirTexture> = Vec::new();
    let mut texture_slot_of_element: HashMap<u32, u32> = HashMap::new();
    for (i, material) in scene.materials.iter().enumerate() {
        material_id_to_index.insert(material.element.element_id, i as u32);
        doc.materials.push(extract_material(
            material,
            i as u32,
            &mut textures,
            &mut texture_slot_of_element,
            embed_textures,
        ));
    }
    doc.textures = textures;

    // ---- 网格：逐节点抽取（一个 mesh 被多个节点实例化时各抽一份）----
    let mut skin_slots: HashMap<u32, u32> = HashMap::new();
    for (node_index, node) in scene.nodes.iter().enumerate() {
        let Some(mesh_ref) = node.mesh.as_ref() else {
            continue;
        };
        let mesh: &ufbx::Mesh = mesh_ref;
        let (air_mesh, skin_ref, skin_stats) = extract_mesh(
            mesh,
            node,
            &paths[node_index],
            &doc.materials,
            &material_id_to_index,
            &index_of,
        );
        // 蒙皮降级不能静默：超过 4 权重被截断、以及没有任何有效权重的顶点都要写进 issues
        let mesh_name = air_mesh.name.clone();
        if skin_stats.truncated > 0 {
            doc.note(format!(
                "网格 `{mesh_name}` 有 {} 个顶点受超过 4 个骨骼影响，已截断为权重最大的 4 个（其余权重被丢弃）",
                skin_stats.truncated
            ));
        }
        if skin_stats.orphan > 0 {
            doc.note(format!(
                "网格 `{mesh_name}` 有 {} 个顶点没有任何有效蒙皮权重，导出后这些顶点会塌到骨骼原点",
                skin_stats.orphan
            ));
        }
        let mesh_index = doc.meshes.len() as u32;
        let skin_index = skin_ref.and_then(|(skin_element, skin)| {
            // 同一个 deformer 被多个网格共用时只建一条骨骼表
            if let Some(&existing) = skin_slots.get(&skin_element) {
                return Some(existing);
            }
            let index = doc.skins.len() as u32;
            doc.skins.push(skin);
            skin_slots.insert(skin_element, index);
            Some(index)
        });
        doc.meshes.push(AirMesh {
            skin: skin_index,
            ..air_mesh
        });
        if let Some(node) = doc.nodes.get_mut(node_index) {
            node.mesh = Some(mesh_index);
            node.skin = skin_index;
        }
    }

    if want_animations {
        extract_animations(doc, &scene, &index_of);
    }

    // 注意：不在这里调 `doc.finalize()`。收尾（统计 + 一致性校验）只由公共入口
    // `tools::load` 统一执行一次，否则 `issues` 会整组重复。
    Ok(())
}

fn load_opts(hint: &str) -> ufbx::LoadOpts<'_> {
    ufbx::LoadOpts {
        target_axes: ufbx::CoordinateAxes {
            right: ufbx::CoordinateAxis::PositiveX,
            up: ufbx::CoordinateAxis::PositiveY,
            front: ufbx::CoordinateAxis::PositiveZ,
        },
        target_unit_meters: 1.0,
        // 外部贴图一律不经 ufbx 直接读盘：插件只有宿主闸门这一条合法 IO 通路。
        load_external_files: false,
        ignore_missing_external_files: true,
        evaluate_skinning: false,
        use_blender_pbr_material: true,
        // 缺法线时让 ufbx 生成，避免下游拿到零向量导致的纯黑面
        generate_missing_normals: true,
        filename: ufbx::StringOpt::Ref(hint),
        ..Default::default()
    }
}

fn axis_name(axis: ufbx::CoordinateAxis) -> String {
    match axis {
        ufbx::CoordinateAxis::PositiveX => "+X".to_string(),
        ufbx::CoordinateAxis::NegativeX => "-X".to_string(),
        ufbx::CoordinateAxis::PositiveY => "+Y".to_string(),
        ufbx::CoordinateAxis::NegativeY => "-Y".to_string(),
        ufbx::CoordinateAxis::PositiveZ => "+Z".to_string(),
        ufbx::CoordinateAxis::NegativeZ => "-Z".to_string(),
        // 未知轴（部分导出器不写 GlobalSettings）按 +Y up 处理，并在 note 里说明
        ufbx::CoordinateAxis::Unknown => "+Y".to_string(),
    }
}

fn extract_material(
    mat: &ufbx::Material,
    index: u32,
    textures: &mut Vec<AirTexture>,
    slot_of_element: &mut HashMap<u32, u32>,
    embed_textures: bool,
) -> AirMaterial {
    let pbr = &mat.pbr;
    // base_factor 在经典 Phong 材质下可能只带 1 个分量（只有 x 有值），
    // 当 vec4 用会得到 (1,0,0) 这种假红，因此仅三分量以上才采信。
    let mut base = if pbr.base_factor.has_value && pbr.base_factor.value_components >= 3 {
        pbr.base_factor.value_vec4
    } else if pbr.base_color.has_value {
        pbr.base_color.value_vec4
    } else {
        ufbx::Vec4 {
            x: 0.8,
            y: 0.8,
            z: 0.8,
            w: 1.0,
        }
    };
    if base.w <= 0.0 {
        base.w = 1.0;
    }
    let metallic = map_scalar(&pbr.metalness);
    let roughness = map_scalar(&pbr.roughness);

    let mut base_color_texture = None;
    for slot in mat.textures.iter() {
        let tex: &ufbx::Texture = &slot.texture;
        let element_id = tex.element.element_id;
        let slot_name = if slot.material_prop.is_empty() {
            slot.shader_prop.to_string()
        } else {
            slot.material_prop.to_string()
        };
        let slot_index = match slot_of_element.get(&element_id).copied() {
            Some(existing) => existing,
            None => {
                let index = textures.len() as u32;
                textures.push(build_texture(index, tex, &slot_name, embed_textures));
                slot_of_element.insert(element_id, index);
                index
            }
        };
        // 只在没选过时按优先级挑 base color，避免后者覆盖前者
        if base_color_texture.is_none() && is_base_color_slot(&slot_name) {
            base_color_texture = Some(slot_index);
        }
    }

    AirMaterial {
        index,
        name: mat.element.name.to_string(),
        base_color: [base.x as f32, base.y as f32, base.z as f32, base.w as f32],
        metallic: metallic.clamp(0.0, 1.0),
        roughness: roughness.clamp(0.0, 1.0),
        base_color_texture,
        double_sided: default_double_sided(mat),
    }
}

/// FBX 没有统一的「双面」开关，但 ufbx 会把各导出器的写法归一到 features 上。
fn default_double_sided(mat: &ufbx::Material) -> bool {
    mat.features.double_sided.enabled
}

fn map_scalar(map: &ufbx::MaterialMap) -> f32 {
    if map.has_value {
        map.value_vec4.x as f32
    } else {
        0.0
    }
}

fn is_base_color_slot(slot: &str) -> bool {
    let lower = slot.to_ascii_lowercase();
    ["diffusecolor", "basecolor", "base_color", "albedo", "diffuse"]
        .iter()
        .any(|want| lower.contains(want))
}

fn build_texture(
    index: u32,
    tex: &ufbx::Texture,
    slot: &str,
    embed_textures: bool,
) -> AirTexture {
    let embedded = !tex.content.is_empty();
    let raw: &[u8] = &tex.content;
    let (width, height, mime) = util::probe_image_size(raw)
        .map(|(w, h, mime)| (Some(w), Some(h), mime.to_string()))
        .unwrap_or((None, None, String::new()));
    let name = {
        let filename = tex.filename.to_string();
        if filename.is_empty() {
            format!("texture_{index}")
        } else {
            filename
        }
    };
    let source_path = {
        let relative = tex.relative_filename.to_string();
        let filename = tex.filename.to_string();
        if !relative.is_empty() {
            Some(relative)
        } else if !filename.is_empty() {
            Some(filename)
        } else {
            None
        }
    };
    AirTexture {
        index,
        name,
        slot: slot.to_string(),
        mime_type: mime,
        width,
        height,
        bytes: raw.len() as u64,
        embedded,
        data_base64: if embed_textures && embedded {
            Some(BASE64.encode(raw))
        } else {
            None
        },
        source_path: if embedded { None } else { source_path },
    }
}

fn extract_mesh(
    mesh: &ufbx::Mesh,
    node: &ufbx::Node,
    node_path: &str,
    materials: &[AirMaterial],
    material_id_to_index: &HashMap<u32, u32>,
    index_of: &HashMap<u32, u32>,
) -> (AirMesh, Option<(u32, AirSkin)>, SkinStats) {
    // 顶点要落在「节点局部空间」：引擎会用 AirNode 的 TRS 沿父链累乘得到世界矩阵。
    // geometry_to_world 里同时含节点变换与单位/轴向换算，除掉节点自身世界矩阵后
    // 剩下的几何→节点矩阵才是网格应有的坐标空间。
    let geo_to_node = {
        let node_to_world = util::mat4_from_ufbx(&node.node_to_world);
        let geo_to_world = util::mat4_from_ufbx(&node.geometry_to_world);
        util::mat4_mul(&util::mat4_invert(&node_to_world), &geo_to_world)
    };
    let has_normal = mesh.vertex_normal.exists;
    let has_uv = mesh.vertex_uv.exists;

    let corner_count = mesh.num_indices;
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(corner_count);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(corner_count);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(corner_count);
    let mut corner_to_vertex: Vec<u32> = vec![0; corner_count];
    let mut vertex_source: Vec<u32> = Vec::with_capacity(corner_count);
    let mut dedup: HashMap<(u32, u32, u32), u32> = HashMap::new();

    let mut aabb_min = [f32::INFINITY; 3];
    let mut aabb_max = [f32::NEG_INFINITY; 3];

    for (corner, slot) in corner_to_vertex.iter_mut().enumerate() {
        let pos_ix = mesh.vertex_indices.get(corner).copied().unwrap_or(0);
        let nrm_ix = if has_normal {
            mesh.vertex_normal.indices.get(corner).copied().unwrap_or(0)
        } else {
            0
        };
        let uv_ix = if has_uv {
            mesh.vertex_uv.indices.get(corner).copied().unwrap_or(0)
        } else {
            0
        };
        let key = (pos_ix, nrm_ix, uv_ix);
        if let Some(&existing) = dedup.get(&key) {
            *slot = existing;
            continue;
        }

        let local = mesh
            .vertex_position
            .values
            .get(pos_ix as usize)
            .copied()
            .unwrap_or(ufbx::Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            });
        // geo_to_node：位置落在节点局部空间，轴向/单位换算由 AirNode 的 TRS 承担
        let p = util::transform_point(
            &geo_to_node,
            [local.x as f32, local.y as f32, local.z as f32],
        );
        for axis in 0..3 {
            aabb_min[axis] = aabb_min[axis].min(p[axis]);
            aabb_max[axis] = aabb_max[axis].max(p[axis]);
        }

        let local_normal = if has_normal {
            mesh.vertex_normal
                .values
                .get(nrm_ix as usize)
                .copied()
                .unwrap_or(ufbx::Vec3 {
                    x: 0.0,
                    y: 1.0,
                    z: 0.0,
                })
        } else {
            ufbx::Vec3 {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            }
        };
        // 法线同样走 geo→node 矩阵（位置同理落在节点局部空间），
        // 且必须用余子式变换，直接乘矩阵在有非均匀缩放时会歪
        let n = util::transform_normal(
            &geo_to_node,
            [
                local_normal.x as f32,
                local_normal.y as f32,
                local_normal.z as f32,
            ],
        );

        let (u, v) = if has_uv {
            let uv = mesh
                .vertex_uv
                .values
                .get(uv_ix as usize)
                .copied()
                .unwrap_or(ufbx::Vec2 { x: 0.0, y: 0.0 });
            (uv.x as f32, 1.0 - uv.y as f32)
        } else {
            (0.0, 0.0)
        };

        let output = positions.len() as u32;
        vertex_source.push(pos_ix);
        positions.push(p);
        normals.push(n);
        uvs.push([u, v]);
        dedup.insert(key, output);
        *slot = output;
    }

    if !aabb_min.iter().all(|v| v.is_finite()) || !aabb_max.iter().all(|v| v.is_finite()) {
        aabb_min = [0.0; 3];
        aabb_max = [0.0; 3];
    }

    // ---- 索引：逐 face 三角化 ----
    let mut indices: Vec<u32> = Vec::with_capacity(mesh.num_triangles * 3);
    let mut material_parts: Vec<MaterialPart> = Vec::new();
    let mut face_tris: Vec<u32> = Vec::new();
    let mut part_material: Option<Option<u32>> = None;

    for face_index in 0..mesh.num_faces {
        let face = mesh.faces[face_index];
        ufbx::triangulate_face_vec(&mut face_tris, mesh, face);
        if face_tris.is_empty() {
            continue;
        }
        if mesh.reversed_winding {
            flip_winding(&mut face_tris);
        }

        let raw_material = mesh.face_material.get(face_index).copied().unwrap_or(0);
        let global_material = resolve_material(mesh, raw_material, material_id_to_index);
        let material_name = global_material
            .and_then(|idx| materials.get(idx as usize))
            .map(|m| m.name.clone())
            .unwrap_or_default();

        let triangle_count = (face_tris.len() / 3) as u32;
        let triangle_offset = (indices.len() / 3) as u32;
        // triangulate_face_vec 给的是角点号，必须经 corner_to_vertex 转成顶点号
        indices.extend(map_corners(&face_tris, &corner_to_vertex));

        // 连续同材质的 face 合并成一个分区，减少 draw call 分段
        if part_material == Some(global_material) {
            if let Some(last) = material_parts.last_mut() {
                last.triangle_count += triangle_count;
            }
        } else {
            material_parts.push(MaterialPart {
                material: global_material,
                material_name: material_name.clone(),
                triangle_offset,
                triangle_count,
            });
            part_material = Some(global_material);
        }
    }

    let (skin_ref, joints, weights, skin_stats) =
        extract_skinning(mesh, node, &vertex_source, index_of);

    (
        AirMesh {
            name: mesh.element.name.to_string(),
            node: node_path.to_string(),
            positions,
            normals,
            uvs,
            indices,
            material_parts,
            aabb_min,
            aabb_max,
            joints,
            weights,
            skin: None,
            source_vertex_count: mesh.num_vertices as u32,
            source_face_count: mesh.num_faces as u32,
            source_triangle_count: mesh.num_triangles as u32,
        },
        skin_ref,
        skin_stats,
    )
}

/// 翻转每个三角形的绕序（1↔2 号角点互换）。
///
/// `reversed_winding` 是 ufbx 对「左手系 / 镜像变换」网格给出的修正标记：
/// 几何数据不动，只把三角形顶点顺序倒过来，否则整块面朝里、开背面剔除就消失。
fn flip_winding(face_tris: &mut [u32]) {
    for triangle in 0..face_tris.len() / 3 {
        face_tris.swap(triangle * 3 + 1, triangle * 3 + 2);
    }
}

/// 把 `triangulate_face_vec` 给出的「角点号」翻译成输出顶点号。
///
/// 这一步必须做：ufbx 的角点号是按 `num_indices` 展开的（位置/法线/UV 各有一套
/// 索引，同一位置的角点可能有多个），而 AIR 的索引指向去重后的顶点。
/// 越界的角点号按 0 兜底——坏数据宁可退化成一根退化三角形，也不该 panic。
fn map_corners(face_tris: &[u32], corner_to_vertex: &[u32]) -> Vec<u32> {
    face_tris
        .iter()
        .map(|&corner| corner_to_vertex.get(corner as usize).copied().unwrap_or(0))
        .collect()
}

/// 单顶点蒙皮打包结果。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct SlotPacking {
    joints: [u16; 4],
    weights: [f32; 4],
    /// 因超过 4 槽位被丢弃的关节数（>0 即「该顶点被截断」）。
    dropped: usize,
}

/// 把一个顶点的全部（关节槽位, 权重）收进固定的 4 槽位。
///
/// 三件事按顺序做，顺序不能换：
/// 1. 按权重降序——保留的应当是最重要的 4 根骨；
/// 2. 截断到 4——着色器只有 4 槽；
/// 3. 重新归一化——截断后 sum 不再为 1，不归一化就是「模型被压扁」。
fn pack_skin_slots(acc: &mut Vec<(u16, f32)>) -> SlotPacking {
    acc.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let dropped = acc.len().saturating_sub(4);
    acc.truncate(4);
    let sum: f32 = acc.iter().map(|(_, w)| *w).sum();
    let mut packed = SlotPacking::default();
    if sum > 1e-6 {
        for (i, (slot, weight)) in acc.iter().enumerate() {
            packed.joints[i] = *slot;
            packed.weights[i] = weight / sum;
        }
    }
    packed.dropped = dropped;
    packed
}

fn resolve_material(
    mesh: &ufbx::Mesh,
    raw_material: u32,
    material_id_to_index: &HashMap<u32, u32>,
) -> Option<u32> {
    if mesh.materials.is_empty() {
        return None;
    }
    let index = raw_material as usize;
    if index >= mesh.materials.len() {
        return Some(0);
    }
    let material: &ufbx::Material = &mesh.materials[index];
    material_id_to_index
        .get(&material.element.element_id)
        .copied()
        .or(Some(raw_material))
}

/// 蒙皮降级统计，由调用方写入 `doc.issues`，避免静默压权。
#[derive(Debug, Clone, Copy, Default)]
struct SkinStats {
    /// 受超过 4 个骨骼影响的顶点数（权重被截断为最大的 4 个）。
    truncated: usize,
    /// 没有任何有效蒙皮权重的顶点数。
    orphan: usize,
}

type SkinResult = (
    Option<(u32, AirSkin)>,
    Vec<[u16; 4]>,
    Vec<[f32; 4]>,
    SkinStats,
);

/// 抽取蒙皮。返回 `((deformer_element_id, 骨骼表), 每顶点关节, 每顶点权重, 降级统计)`。
fn extract_skinning(
    mesh: &ufbx::Mesh,
    node: &ufbx::Node,
    vertex_source: &[u32],
    index_of: &HashMap<u32, u32>,
) -> SkinResult {
    let Some(deformer) = mesh.skin_deformers.first() else {
        return (None, Vec::new(), Vec::new(), SkinStats::default());
    };

    // 顶点在节点局部空间，IBM 要把局部顶点映射到骨骼绑定空间，因此需要节点自身的
    // 世界矩阵（AIR 里节点局部 → 世界正是由 AirNode 的 TRS 承担的）。
    let node_world = util::mat4_from_ufbx(&node.node_to_world);

    // cluster 下标 → 关节槽位；骨骼节点不在场景树里的 cluster 直接丢弃
    let mut slot_of_cluster: Vec<Option<u16>> = vec![None; deformer.clusters.len()];
    let mut joints: Vec<u32> = Vec::new();
    let mut inverse_bind: Vec<[f32; 16]> = Vec::new();
    for (cluster_index, cluster) in deformer.clusters.iter().enumerate() {
        let Some(bone) = cluster.bone_node.as_ref() else {
            continue;
        };
        let Some(&node_index) = index_of.get(&bone.element.element_id) else {
            continue;
        };
        if joints.len() >= u16::MAX as usize {
            break;
        }
        slot_of_cluster[cluster_index] = Some(joints.len() as u16);
        joints.push(node_index);
        // 顶点留在节点局部空间，而 `bind_to_world` 是绑定姿态下骨骼的世界矩阵：
        // 要让 `joint_world * IBM` 把局部顶点送到世界，IBM = invert(bind_to_world) * node_world。
        inverse_bind.push(util::mat4_mul(
            &util::mat4_invert(&util::mat4_from_ufbx(&cluster.bind_to_world)),
            &node_world,
        ));
    }
    if joints.is_empty() {
        return (None, Vec::new(), Vec::new(), SkinStats::default());
    }

    let mut joints_out: Vec<[u16; 4]> = Vec::with_capacity(vertex_source.len());
    let mut weights_out: Vec<[f32; 4]> = Vec::with_capacity(vertex_source.len());
    let mut truncated = 0usize;
    let mut orphan = 0usize;

    for &source in vertex_source {
        let mut acc: Vec<(u16, f32)> = Vec::new();
        // deformer.vertices[v] 给出该位号的权重区间 [begin, begin + num_weights)
        if let Some(range) = deformer.vertices.get(source as usize) {
            let begin = range.weight_begin as usize;
            let end = begin + range.num_weights as usize;
            for k in begin..end.min(deformer.weights.len()) {
                let weight = deformer.weights[k];
                let Some(Some(slot)) = slot_of_cluster.get(weight.cluster_index as usize) else {
                    continue;
                };
                let value = weight.weight as f32;
                if value > 0.0 {
                    acc.push((*slot, value));
                }
            }
        }
        if acc.is_empty() {
            orphan += 1;
        }
        // 权重大的优先保留（4 个槽位），再归一化，保证着色器里 sum(weight)=1
        let packed = pack_skin_slots(&mut acc);
        if packed.dropped > 0 {
            truncated += 1;
        }
        joints_out.push(packed.joints);
        weights_out.push(packed.weights);
    }

    let name = if deformer.element.name.is_empty() {
        mesh.element.name.to_string()
    } else {
        deformer.element.name.to_string()
    };
    // 降级统计随结果一起带出去：`validate()` 只能看到「权重和≠1 的顶点数」，
    // 看不出「被截断」与「孤儿顶点」这两个语义事实，所以由调用方写进 doc.issues。
    let skin = AirSkin {
        index: 0,
        name,
        joints,
        inverse_bind_matrices: inverse_bind,
    };
    (
        Some((deformer.element.element_id, skin)),
        joints_out,
        weights_out,
        SkinStats { truncated, orphan },
    )
}

fn extract_animations(doc: &mut AirDoc, scene: &ufbx::Scene, index_of: &HashMap<u32, u32>) {
    if scene.anim_stacks.is_empty() {
        return;
    }
    for stack in scene.anim_stacks.iter() {
        let name = stack.element.name.to_string();
        if stack.layers.is_empty() {
            continue;
        }
        let opts = ufbx::BakeOpts {
            resample_rate: 30.0,
            trim_start_time: true,
            key_reduction_enabled: true,
            key_reduction_threshold: 1e-4,
            ..Default::default()
        };
        let baked = match ufbx::bake_anim(scene, &stack.anim, opts) {
            Ok(baked) => baked,
            Err(err) => {
                doc.note(format!(
                    "动画栈 `{name}` 烘焙失败：{:?} {}",
                    err.type_, err.description
                ));
                continue;
            }
        };
        let mut clip = AirAnimation {
            name: if name.is_empty() {
                "clip".to_string()
            } else {
                name
            },
            duration: 0.0,
            tracks: Vec::new(),
        };
        for baked_node in baked.nodes.iter() {
            let Some(&node_index) = index_of.get(&baked_node.element_id) else {
                continue;
            };
            push_track(
                &mut clip,
                node_index,
                "translation",
                baked_node.translation_keys.as_ref(),
            );
            push_rotation_track(&mut clip, node_index, baked_node.rotation_keys.as_ref());
            push_track(
                &mut clip,
                node_index,
                "scale",
                baked_node.scale_keys.as_ref(),
            );
        }
        if clip.tracks.is_empty() {
            continue;
        }
        clip.duration = clip
            .tracks
            .iter()
            .filter_map(|track| track.times.last().copied())
            .fold(0.0f32, f32::max);
        doc.animations.push(clip);
    }
}

fn push_track(
    clip: &mut AirAnimation,
    node: u32,
    property: &str,
    keys: &[ufbx::BakedVec3],
) {
    // 只有 1 个关键帧等于常量，播放时没有意义，直接省掉（体积能小一个量级）
    if keys.len() < 2 {
        return;
    }
    let mut times = Vec::with_capacity(keys.len());
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        times.push(key.time as f32);
        values.push([key.value.x as f32, key.value.y as f32, key.value.z as f32, 0.0]);
    }
    clip.tracks.push(AirTrack {
        node,
        property: property.to_string(),
        // FBX 的关键帧已按 30fps 烘焙成密集线性帧，插值方式固定 linear。
        interpolation: crate::air::INTERPOLATION_LINEAR.to_string(),
        times,
        values,
    });
}

fn push_rotation_track(clip: &mut AirAnimation, node: u32, keys: &[ufbx::BakedQuat]) {
    if keys.len() < 2 {
        return;
    }
    let mut times = Vec::with_capacity(keys.len());
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        times.push(key.time as f32);
        values.push(util::normalize_quat([
            key.value.x as f32,
            key.value.y as f32,
            key.value.z as f32,
            key.value.w as f32,
        ]));
    }
    clip.tracks.push(AirTrack {
        node,
        property: "rotation".to_string(),
        interpolation: crate::air::INTERPOLATION_LINEAR.to_string(),
        times,
        values,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四边形扇形三角化后是 2 个三角形，且角点号必须翻译成顶点号。
    /// 用「角点号 ≠ 顶点号」的映射，才能测出漏翻译的写法。
    #[test]
    fn quad_triangulation_maps_corners_to_vertices() {
        // ufbx 对 4 角点面的扇形切分结果：0,1,2 与 0,2,3
        let face_tris = vec![0u32, 1, 2, 0, 2, 3];
        // 去重后：角点 0→7、1→3、2→5、3→9（顶点号与角点号刻意不同）
        let corner_to_vertex = vec![7u32, 3, 5, 9];
        let indices = map_corners(&face_tris, &corner_to_vertex);
        assert_eq!(indices, vec![7, 3, 5, 7, 5, 9]);
        assert_eq!(indices.len(), 6, "四面形应当正好切出 2 个三角形");
        // 两个三角形共用的那条对角线必须是同一对顶点（7-5 与 5-7 首尾相反），
        // 否则说明扇形切分被改写成了别的拓扑。
        assert_eq!((indices[2], indices[3]), (5, 7));
    }

    #[test]
    fn reversed_winding_flips_each_triangle_only() {
        let mut face_tris = vec![0u32, 1, 2, 3, 4, 5];
        flip_winding(&mut face_tris);
        assert_eq!(face_tris, vec![0, 2, 1, 3, 5, 4], "只换 1↔2，顶点 0 不动");
        // 再翻回去必须回到原样：翻转是自逆操作，翻两次等于没翻
        flip_winding(&mut face_tris);
        assert_eq!(face_tris, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn out_of_range_corner_falls_back_to_zero() {
        // 损坏文件的角点号可能越界：退化三角形可以，panic 不行
        let indices = map_corners(&[0u32, 99, 2], &[4u32, 6]);
        assert_eq!(indices, vec![4, 0, 0]);
    }

    /// 超过 4 根骨：保留权重最大的 4 根、其余丢弃，并归一化到 sum=1。
    /// 不归一化在引擎里就是「模型被压缩/膨胀」，这是最典型的蒙皮错误。
    #[test]
    fn skin_weights_truncate_to_four_and_normalize() {
        let mut acc = vec![(0u16, 0.60f32), (1, 0.20), (2, 0.10), (3, 0.05), (4, 0.03), (5, 0.02)];
        let packed = pack_skin_slots(&mut acc);
        assert_eq!(packed.dropped, 2, "6 根骨要丢掉 2 根");
        assert_eq!(packed.joints, [0, 1, 2, 3], "按权重降序保留最重要的 4 根");
        let sum: f32 = packed.weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "截断后必须重新归一化，实际 sum={sum}");
        assert!((packed.weights[0] - 0.6 / 0.95).abs() < 1e-5);
    }

    /// 输入顺序打乱也必须按权重挑，不能按「谁知道先 push 谁」挑。
    #[test]
    fn skin_weights_pick_by_weight_not_by_order() {
        let mut acc = vec![(0u16, 0.01f32), (1, 0.02), (2, 0.03), (3, 0.04), (4, 0.9)];
        let packed = pack_skin_slots(&mut acc);
        assert_eq!(packed.joints[0], 4, "0.9 的那根骨必须占首槽");
        assert_eq!(packed.dropped, 1, "最小的 0.01 应该被丢");
        assert_eq!(packed.joints, [4, 3, 2, 1], "余下按降序占槽，末槽是 0.02 那根");
    }

    /// 没有被任何骨骼影响的顶点：4 个槽位全 0、权重全 0（不是 NaN、不是 1）。
    /// 这种顶点在 `extract_skinning` 里计为 orphan，只能靠统计发现。
    #[test]
    fn vertex_without_weights_produces_zero_slots() {
        let mut empty: Vec<(u16, f32)> = Vec::new();
        let packed = pack_skin_slots(&mut empty);
        assert_eq!(packed.joints, [0u16; 4]);
        assert_eq!(packed.weights, [0.0f32; 4]);
        assert_eq!(packed.dropped, 0);
    }

    /// 权重和为 0（全 0 权重被过滤后剩的是 0）不能除零产生 NaN。
    #[test]
    fn zero_weight_sum_never_emits_nan() {
        let mut acc = vec![(0u16, 0.0f32), (1, 0.0)];
        let packed = pack_skin_slots(&mut acc);
        assert!(packed.weights.iter().all(|w| w.is_finite()), "权重出现 NaN 会让整块蒙皮消失");
        assert_eq!(packed.weights, [0.0f32; 4]);
    }
}
