//! GLB / glTF → AIR。
//!
//! 与 FBX 导入的两处关键差异，写在这里免得消费方踩坑：
//!
//! 1. **UV 不翻转**：glTF 的 UV 原点本来就在左上，与 AIR 一致；FBX 才是左下。
//!    两边都翻一次就成镜像贴图了。
//! 2. **顶点留在 mesh 局部空间**：glTF 的 POSITION 本来就是 mesh 局部空间，AIR 也按
//!    节点局部空间存顶点（节点变换由 `AirNode` 的 TRS 沿父链累乘；FBX 那边烘的是
//!    `geometry_to_world` 除掉节点世界矩阵后剩下的 geo→node 矩阵）。因此这里不预烘
//!    世界坐标，`inverseBindMatrices` 也保持文件原值直接拷贝。同一个 mesh 被多个节点
//!    实例化时，各生成一份 AirMesh。
//!
//! 另外两点容易忘：`CUBICSPLINE` 的 sampler 每个关键帧输出 3 个值（入切线/值/出
//! 切线），必须取中间那一个，否则动画会以三倍速乱跳；`JOINTS_0` 即使声明为
//! normalized 也不能按归一化浮点读，否则关节索引全毁。

use std::collections::{HashMap, HashSet};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::Value;

use crate::air::*;
use crate::util;

/// 外部资源（`buffers[].uri` / `images[].uri`）的读取入口。插件不允许自己碰磁盘，
/// 一律由宿主闸门提供；返回 `None` 表示读不到（缺失贴图不该让导入失败）。
pub type FetchFn<'a> = &'a mut dyn FnMut(&str) -> Option<Vec<u8>>;

pub fn import(
    doc: &mut AirDoc,
    bytes: &[u8],
    hint: &str,
    want_animations: bool,
    embed_textures: bool,
    fetch: FetchFn<'_>,
) -> Result<(), String> {
    let (json, bin, container) = parse_container(bytes)?;
    doc.source.bytes = bytes.len() as u64;
    doc.source.container = container.to_string();
    doc.source.native_up_axis = Some("+Y".to_string());
    doc.source.native_unit_meters = Some(1.0);
    if let Some(asset) = json.get("asset") {
        let generator = asset
            .get("generator")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !generator.is_empty() {
            doc.source.creator = Some(generator.to_string());
        }
        let version = asset
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !version.starts_with('2') {
            doc.note(format!(
                "glTF 版本为 `{version}`，本插件按 2.0 解析，结果可能不准"
            ));
        }
    }

    let g = Gltf {
        json: &json,
        notes: std::cell::RefCell::new(Vec::new()),
    };
    let buffers = g.load_buffers(bin, hint, fetch, doc);
    let base_dir = parent_dir(hint);

    // ---- 场景图 ----
    let nodes = g.array("nodes");
    let parent_of = g.parent_map(nodes);
    doc.nodes = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            // 顶点留在节点局部空间（见模块文档第 2 条），这里只需解出本节点的 TRS，
            // 不再预计算世界矩阵。
            let local = g.local_matrix(node);
            let (position, rotation, scale) = util::decompose(&local);
            let name = node
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("node{i}"));
            AirNode {
                index: i as u32,
                name: name.clone(),
                path: build_path(i, &nodes, &parent_of),
                parent: parent_of.get(i).copied().flatten().map(|p| p as u32),
                position,
                rotation,
                scale,
                mesh: None,
                skin: None,
            }
        })
        .collect();

    // ---- 材质与贴图 ----
    let images = g.array("images");
    let mut texture_of_image: HashMap<u32, u32> = HashMap::new();
    for (image_index, image) in images.iter().enumerate() {
        let uri = image.get("uri").and_then(Value::as_str);
        let mime_hint = image.get("mimeType").and_then(Value::as_str);
        let raw: Option<Vec<u8>> = if image.get("bufferView").is_some() {
            match g.accessor_bytes(image.get("bufferView").and_then(Value::as_u64), &buffers) {
                Ok(data) => Some(data.to_vec()),
                Err(err) => {
                    doc.note(format!("内嵌贴图 #{image_index} 读取失败：{err}"));
                    None
                }
            }
        } else if let Some(uri) = uri {
            if let Some(payload) = uri.strip_prefix("data:") {
                decode_data_uri(payload)
            } else {
                let path = if base_dir.is_empty() {
                    uri.to_string()
                } else {
                    format!("{base_dir}/{uri}")
                };
                match fetch(&path) {
                    Some(data) => Some(data),
                    None => {
                        doc.note(format!(
                            "外部贴图 `{path}` 无法读取（不在工作区、或已被删除）"
                        ));
                        None
                    }
                }
            }
        } else {
            None
        };

        let name = image
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| uri.map(str::to_string))
            .unwrap_or_else(|| format!("image_{image_index}"));
        let (width, height, probed_mime) = raw
            .as_deref()
            .and_then(util::probe_image_size)
            .map(|(w, h, mime)| (Some(w), Some(h), mime.to_string()))
            .unwrap_or((None, None, String::new()));
        let index = doc.textures.len() as u32;
        doc.textures.push(AirTexture {
            index,
            name,
            slot: String::new(),
            mime_type: mime_hint.map(str::to_string).unwrap_or(probed_mime),
            width,
            height,
            bytes: raw.as_ref().map(|data| data.len() as u64).unwrap_or(0),
            embedded: true,
            data_base64: if embed_textures {
                raw.as_deref().map(|data| BASE64.encode(data))
            } else {
                None
            },
            source_path: uri
                .filter(|uri| !uri.starts_with("data:"))
                .map(str::to_string),
        });
        texture_of_image.insert(image_index as u32, index);
    }

    for (material_index, material) in g.array("materials").iter().enumerate() {
        let built = build_material(
            &g,
            material,
            material_index as u32,
            &texture_of_image,
            &mut doc.textures,
        );
        doc.materials.push(built);
    }

    // ---- 网格 ----
    let meshes = g.array("meshes");
    // glTF 的 `nodes` 是素材池，真正会被画出来的是 `scenes[scene].nodes` 里能走到的子树。
    // 只导场景子树：否则清单一堆引擎永远不会画的网格，用户会误判资产体积来源。
    let reachable = scene_nodes(&g, nodes, doc);
    let mut skipped_nodes = 0usize;
    let mut skin_slot: HashMap<(u32, u32), u32> = HashMap::new();
    for (node_index, node) in nodes.iter().enumerate() {
        if let Some(set) = &reachable {
            if !set.contains(&node_index) {
                if node.get("mesh").is_some() {
                    skipped_nodes += 1;
                }
                continue;
            }
        }
        let Some(mesh_index) = node.get("mesh").and_then(Value::as_u64) else {
            continue;
        };
        let Some(mesh_json) = meshes.get(mesh_index as usize) else {
            doc.note(format!("节点 #{node_index} 引用了不存在的 mesh #{mesh_index}"));
            continue;
        };
        let node_path = doc.nodes[node_index].path.clone();

        let skin_ref = node.get("skin").and_then(Value::as_u64);
        let mut air_skin: Option<(Option<u32>, Option<u32>)> = None;
        if let Some(skin_index) = skin_ref {
            let key = (skin_index as u32, node_index as u32);
            if let Some(&existing) = skin_slot.get(&key) {
                air_skin = Some((Some(existing), Some(existing)));
            } else if let Some(skin_json) = g.array("skins").get(skin_index as usize) {
                let built = build_skin(
                    skin_json,
                    skin_index as u32,
                    &g,
                    &buffers,
                    doc.nodes.len(),
                );
                let index = doc.skins.len() as u32;
                doc.skins.push(built);
                skin_slot.insert(key, index);
                air_skin = Some((Some(index), Some(index)));
            }
        }
        let (skin_for_mesh, skin_for_node) = air_skin.unwrap_or((None, None));

        let primitives = mesh_json
            .get("primitives")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mesh_name = mesh_json
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("mesh{mesh_index}"));

        // 一个 mesh 的多个 primitive 合并成一个 AirMesh：引擎按 material_parts
        // 划分区段，拆成多个 AssetEntry 反而丧失实例化关系。
        let mut merged = AirMesh {
            name: mesh_name.clone(),
            node: node_path.clone(),
            ..Default::default()
        };
        merged.aabb_min = [f32::INFINITY; 3];
        merged.aabb_max = [f32::NEG_INFINITY; 3];
        let mut skinned = false;

        for (prim_index, primitive) in primitives.iter().enumerate() {
            let attributes = primitive.get("attributes").cloned().unwrap_or(Value::Null);
            let Some(position_value) = attributes.get("POSITION") else {
                doc.note(format!(
                    "`{mesh_name}` 的 primitive #{prim_index} 没有 POSITION，已跳过"
                ));
                continue;
            };
            let position_index = position_value.as_u64().unwrap_or(0);
            let positions_local = match g.read_vec(&buffers, position_index, 3, true) {
                Ok(values) => values,
                Err(err) => {
                    doc.note(format!("`{mesh_name}` 的位置数据读取失败：{err}"));
                    continue;
                }
            };
            let normal_index = attributes.get("NORMAL").and_then(Value::as_u64);
            let normals_local = normal_index
                .and_then(|index| g.read_vec(&buffers, index, 3, true).ok());
            let uv_index = attributes.get("TEXCOORD_0").and_then(Value::as_u64);
            let uvs_local =
                uv_index.and_then(|index| g.read_vec(&buffers, index, 2, true).ok());

            let base = merged.positions.len() as u32;
            // 顶点保持 mesh 局部空间：节点变换由 AirNode 的 TRS 表达，引擎按父链累乘。
            // 若这里提前烘焙成世界坐标，节点变换会被应用两次（同一 mesh 也无法实例化）。
            for (i, p) in positions_local.iter().enumerate() {
                let local_p = [p[0], p[1], p[2]];
                for axis in 0..3 {
                    merged.aabb_min[axis] = merged.aabb_min[axis].min(local_p[axis]);
                    merged.aabb_max[axis] = merged.aabb_max[axis].max(local_p[axis]);
                }
                merged.positions.push(local_p);
                let n = normals_local
                    .as_ref()
                    .and_then(|list| list.get(i).map(|n| [n[0], n[1], n[2]]))
                    .unwrap_or([0.0, 1.0, 0.0]);

                merged.normals.push(n);
                // glTF UV 原点在左上，与 AIR 一致，不翻转
                let uv = uvs_local
                    .as_ref()
                    .and_then(|list| list.get(i).copied())
                    .map(|uv| [uv[0], uv[1]])
                    .unwrap_or([0.0, 0.0]);
                merged.uvs.push(uv);
            }
            // 法线缺失时按面法线补：格里高利平滑做不到，但至少不会全黑
            if normals_local.is_none() {
                doc.note(format!(
                    "`{mesh_name}` 的 primitive #{prim_index} 缺少 NORMAL，已按面法线补齐（接缝处会有硬边）"
                ));
            }

            let index_count = match primitive.get("indices").and_then(Value::as_u64) {
                Some(index_accessor) => match g.read_indices(&buffers, index_accessor) {
                    Ok(values) => values,
                    Err(err) => {
                        doc.note(format!("`{mesh_name}` 的索引读取失败：{err}"));
                        continue;
                    }
                },
                None => (0..positions_local.len() as u32).collect(),
            };
            merged
                .indices
                .extend(index_count.iter().map(|index| index + base));

            let triangle_count = (index_count.len() / 3) as u32;
            let material = primitive
                .get("material")
                .and_then(Value::as_u64)
                .map(|m| m as u32);
            let material_name = material
                .and_then(|m| doc.materials.get(m as usize))
                .map(|m| m.name.clone())
                .unwrap_or_default();
            merged.material_parts.push(MaterialPart {
                material,
                material_name,
                triangle_offset: ((merged.indices.len() / 3) as u32) - triangle_count,
                triangle_count,
            });
            let mode = primitive
                .get("mode")
                .and_then(Value::as_u64)
                .unwrap_or(4);
            if mode != 4 {
                doc.note(format!(
                    "`{mesh_name}` 的 primitive #{prim_index} 绘制模式为 {mode}（非三角形列表），已按三角形解释"
                ));
            }

            // 蒙皮属性
            if let (Some(joints_value), Some(weights_value)) =
                (attributes.get("JOINTS_0"), attributes.get("WEIGHTS_0"))
            {
                let joints_ix = joints_value.as_u64().unwrap_or(0);
                let weights_ix = weights_value.as_u64().unwrap_or(0);
                // JOINTS 绝不能归一化读：u16 的 300 被 /65535 就成了 0
                match (
                    g.read_indices_raw(&buffers, joints_ix, 4),
                    g.read_vec(&buffers, weights_ix, 4, false),
                ) {
                    (Ok(joints), Ok(weights)) => {
                        let joint_count = doc
                            .skins
                            .get(skin_for_mesh.unwrap_or(0) as usize)
                            .map(|skin| skin.joints.len())
                            .unwrap_or(0);
                        for (i, weight_row) in weights.iter().enumerate() {
                            let mut slots = [0u16; 4];
                            if let Some(joint_row) = joints.get(i) {
                                for k in 0..4 {
                                    let slot = joint_row[k];
                                    if slot < joint_count as u32 && slot <= u16::MAX as u32 {
                                        slots[k] = slot as u16;
                                    }
                                }
                            }
                            let mut values = [0.0f32; 4];
                            let mut sum = 0.0f32;
                            for k in 0..4 {
                                let value = weight_row[k].max(0.0);
                                values[k] = value;
                                sum += value;
                            }
                            if sum > 1e-6 {
                                for value in &mut values {
                                    *value /= sum;
                                }
                            }
                            merged.joints.push(slots);
                            merged.weights.push(values);
                        }
                        skinned = true;
                    }
                    (Err(err), _) | (_, Err(err)) => {
                        doc.note(format!("`{mesh_name}` 的蒙皮属性读取失败：{err}"));
                    }
                }
            }
        }

        if merged.positions.is_empty() {
            continue;
        }
        if !merged.aabb_min.iter().all(|v| v.is_finite()) {
            merged.aabb_min = [0.0; 3];
            merged.aabb_max = [0.0; 3];
        }
        if merged.normals.len() != merged.positions.len() {
            merged.normals.resize(merged.positions.len(), [0.0, 1.0, 0.0]);
        }
        if merged.uvs.len() != merged.positions.len() {
            merged.uvs.resize(merged.positions.len(), [0.0, 0.0]);
        }
        merged.source_vertex_count = merged.positions.len() as u32;
        merged.source_face_count = (merged.indices.len() / 3) as u32;
        merged.source_triangle_count = merged.source_face_count;
        merged.skin = if skinned { skin_for_mesh } else { None };
        let mesh_slot = doc.meshes.len() as u32;
        doc.meshes.push(merged);
        doc.nodes[node_index].mesh = Some(mesh_slot);
        doc.nodes[node_index].skin = if skinned { skin_for_node } else { None };
    }

    if skipped_nodes > 0 {
        doc.note(format!(
            "{skipped_nodes} 个带网格的节点未被默认场景引用（素材池里的孤立节点），已跳过导入"
        ));
    }

    if want_animations {
        extract_animations(doc, &g, &buffers);
    }

    // accessor 层的降级（sparse 跳过等）在底层攒着，这里统一并入 issues：
    // 下层的 `&Gltf` 没有 doc 的可变借用，集中上报才不会漏。
    let notes = g.notes.borrow();
    for note in notes.iter() {
        doc.note(note.clone());
    }

    // 注意：不在这里调 `doc.finalize()`。收尾（统计 + 一致性校验）只由公共入口
    // `tools::load` 统一执行一次，否则 `issues` 会整组重复。
    Ok(())
}

// ---------------------------------------------------------------------------
// 场景可达性
// ---------------------------------------------------------------------------

/// 求默认场景里可达的节点集合。
///
/// `scene` 字段指定默认场景（缺省 0）。文件没有 `scenes` 时返回 `None`，表示
/// 「场景信息缺失，退回全量导入」——宁可多导，也不能因为缺字段把资产导成空。
fn scene_nodes(g: &Gltf, nodes: &[Value], doc: &mut AirDoc) -> Option<HashSet<usize>> {
    let scenes = g.array("scenes");
    if scenes.is_empty() {
        return None;
    }
    let scene_index = g.json.get("scene").and_then(Value::as_u64).unwrap_or(0) as usize;
    let Some(scene) = scenes.get(scene_index) else {
        doc.note(format!(
            "`scene` 指向不存在的场景 #{scene_index}，已退回全量导入（不按场景裁剪）"
        ));
        return None;
    };
    let mut set: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = scene
        .get("nodes")
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(Value::as_u64)
                .map(|v| v as usize)
                .collect()
        })
        .unwrap_or_default();
    while let Some(index) = stack.pop() {
        // 越界与重复都要挡：glTF 允许环状 children（非法但常见），不挡就是死循环。
        if index >= nodes.len() || !set.insert(index) {
            continue;
        }
        if let Some(children) = nodes[index].get("children").and_then(Value::as_array) {
            stack.extend(children.iter().filter_map(Value::as_u64).map(|v| v as usize));
        }
    }
    if scenes.len() > 1 {
        doc.note(format!(
            "文件含 {} 个场景，已按默认场景 #{}（`scene`）导入，其余场景的节点不包含在内",
            scenes.len(),
            scene_index
        ));
    }
    Some(set)
}

// ---------------------------------------------------------------------------
// 容器解析
// ---------------------------------------------------------------------------

fn parse_container(bytes: &[u8]) -> Result<(Value, Option<Vec<u8>>, &'static str), String> {
    if bytes.len() >= 12 && &bytes[0..4] == b"glTF" {
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != 2 {
            return Err(format!("GLB 版本 {version} 不受支持（只认 2）"));
        }
        let total = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let total = total.min(bytes.len());
        let mut offset = 12usize;
        let mut json: Option<Value> = None;
        let mut bin: Option<Vec<u8>> = None;
        while offset + 8 <= total {
            let length =
                u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
                    as usize;
            let kind = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]);
            let start = offset + 8;
            let end = (start + length).min(total);
            if start >= end {
                break;
            }
            // 0x4E4F534A = "JSON"，0x004E4942 = "BIN\0"
            match kind {
                0x4E4F534A => {
                    json = Some(
                        serde_json::from_slice(&bytes[start..end])
                            .map_err(|err| format!("GLB 的 JSON chunk 解析失败：{err}"))?,
                    );
                }
                0x004E4942 => bin = Some(bytes[start..end].to_vec()),
                _ => {}
            }
            // chunk 之间按 4 字节对齐
            offset = start + ((length + 3) & !3);
        }
        let json = json.ok_or_else(|| "GLB 缺少 JSON chunk".to_string())?;
        Ok((json, bin, "glb"))
    } else {
        let json: Value = serde_json::from_slice(bytes)
            .map_err(|err| format!("glTF JSON 解析失败：{err}"))?;
        Ok((json, None, "gltf"))
    }
}

fn decode_data_uri(payload: &str) -> Option<Vec<u8>> {
    let (meta, data) = payload.split_once(',')?;
    if meta.contains("base64") {
        BASE64.decode(data.trim()).ok()
    } else {
        // 非 base64 的 data URI 做一次百分号解码
        let mut out = Vec::with_capacity(data.len());
        let raw = data.as_bytes();
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'%' && i + 2 < raw.len() {
                let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            } else {
                out.push(raw[i]);
                i += 1;
            }
        }
        Some(out)
    }
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}

fn build_path(index: usize, nodes: &[Value], parent_of: &[Option<usize>]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = Some(index);
    let mut guard = 0;
    while let Some(current) = cursor {
        if guard > 256 {
            break;
        }
        guard += 1;
        let name = nodes
            .get(current)
            .and_then(|node| node.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("node{current}"));
        parts.push(name);
        cursor = parent_of.get(current).copied().flatten();
    }
    parts.reverse();
    parts.join("/")
}

// ---------------------------------------------------------------------------
// glTF 文档读取器
// ---------------------------------------------------------------------------

struct Gltf<'a> {
    json: &'a Value,
    /// 底层（accessor / bufferView）发现的降级情况。这些地方拿不到 `&mut AirDoc`，
    /// 先攒在这里，`import` 收尾时统一并入 `doc.issues`，保证一条都不丢。
    notes: std::cell::RefCell<Vec<String>>,
}

/// 单个 accessor 的元素数硬上限。
///
/// `count` 是 JSON 里的 64 位数字，损坏或构造出来的文件可以写成天文数字；照它
/// `with_capacity` 会在分配阶段直接 OOM abort，连错误都报不出来。
const MAX_ACCESSOR_COUNT: usize = 16 * 1024 * 1024;

impl<'a> Gltf<'a> {
    /// 记一条降级/告警（同一句只留一条，避免逐顶点重复刷屏）。
    fn warn(&self, message: String) {
        let mut notes = self.notes.borrow_mut();
        if !notes.iter().any(|existing| existing == &message) {
            notes.push(message);
        }
    }

    fn array(&self, key: &str) -> &'a [Value] {
        self.json
            .get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn parent_map(&self, nodes: &[Value]) -> Vec<Option<usize>> {
        let mut parent = vec![None; nodes.len()];
        for (i, node) in nodes.iter().enumerate() {
            if let Some(children) = node.get("children").and_then(Value::as_array) {
                for child in children {
                    if let Some(child) = child.as_u64() {
                        if let Some(slot) = parent.get_mut(child as usize) {
                            *slot = Some(i);
                        }
                    }
                }
            }
        }
        parent
    }

    /// 节点的局部矩阵：有 `matrix` 就直用，否则 `T * R * S`。
    fn local_matrix(&self, node: &Value) -> [f32; 16] {
        if let Some(matrix) = node.get("matrix").and_then(Value::as_array) {
            if matrix.len() == 16 {
                let mut out = [0.0f32; 16];
                for (i, value) in matrix.iter().enumerate() {
                    out[i] = value.as_f64().unwrap_or(0.0) as f32;
                }
                return out;
            }
        }
        let translation = read_fixed(node.get("translation"), 3, 0.0);
        let rotation = read_fixed4(node.get("rotation"));
        let scale = read_fixed(node.get("scale"), 3, 1.0);
        compose_trs(translation, rotation, scale)
    }

    fn load_buffers(
        &self,
        bin: Option<Vec<u8>>,
        hint: &str,
        fetch: FetchFn<'_>,
        doc: &mut AirDoc,
    ) -> Vec<Vec<u8>> {
        let base_dir = parent_dir(hint);
        let mut out: Vec<Vec<u8>> = Vec::new();
        for buffer in self.array("buffers").iter() {
            let uri = buffer.get("uri").and_then(Value::as_str);
            let data = match uri {
                None => bin.clone().unwrap_or_default(),
                Some(uri) if uri.starts_with("data:") => {
                    decode_data_uri(uri.trim_start_matches("data:")).unwrap_or_default()
                }
                Some(uri) => {
                    let path = if base_dir.is_empty() {
                        uri.to_string()
                    } else {
                        format!("{base_dir}/{uri}")
                    };
                    match fetch(&path) {
                        Some(data) => data,
                        None => {
                            doc.note(format!(
                                "外部 buffer `{path}` 无法读取，依赖它的网格/动画会被跳过"
                            ));
                            Vec::new()
                        }
                    }
                }
            };
            // 少给字节会让后续越界读变成静默的 0，这里主动补齐到声明长度
            let declared = buffer.get("byteLength").and_then(Value::as_u64).unwrap_or(0) as usize;
            let mut data = data;
            if data.len() < declared {
                data.resize(declared, 0);
            }
            out.push(data);
        }
        if out.is_empty() && bin.is_some() {
            out.push(bin.unwrap_or_default());
        }
        out
    }

    fn accessor_bytes<'b>(
        &self,
        view_index: Option<u64>,
        buffers: &'b [Vec<u8>],
    ) -> Result<&'b [u8], String> {
        let view_index = view_index.ok_or_else(|| "bufferView 缺失".to_string())?;
        let view = self
            .array("bufferViews")
            .get(view_index as usize)
            .ok_or_else(|| format!("bufferView #{view_index} 不存在"))?;
        let buffer = view.get("buffer").and_then(Value::as_u64).unwrap_or(0) as usize;
        let data = buffers
            .get(buffer)
            .ok_or_else(|| format!("buffer #{buffer} 未加载"))?;
        let offset = view.get("byteOffset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let length = view
            .get("byteLength")
            .and_then(Value::as_u64)
            .unwrap_or(data.len() as u64) as usize;
        let end = (offset + length).min(data.len());
        if offset >= end {
            return Err(format!(
                "bufferView #{view_index} 范围越界（offset {offset} ≥ 数据长度 {}）",
                data.len()
            ));
        }
        Ok(&data[offset..end])
    }

    /// 读 float 属性。`normalized` 为 true 时把整型按 glTF 规则归一化到 [0,1]/-1..1。
    fn read_vec(
        &self,
        buffers: &[Vec<u8>],
        accessor_index: u64,
        components: usize,
        normalized: bool,
    ) -> Result<Vec<[f32; 4]>, String> {
        let raw = self.read_scalars(buffers, accessor_index, components, normalized)?;
        Ok(raw)
    }

    fn read_indices(&self, buffers: &[Vec<u8>], accessor_index: u64) -> Result<Vec<u32>, String> {
        let values = self.read_scalars(buffers, accessor_index, 1, false)?;
        Ok(values
            .iter()
            .map(|v| v[0].max(0.0) as u32)
            .collect::<Vec<u32>>())
    }

    /// 读 4 分量整型（JOINTS_0）。不做归一化，不做符号解释。
    fn read_indices_raw(
        &self,
        buffers: &[Vec<u8>],
        accessor_index: u64,
        components: usize,
    ) -> Result<Vec<[u32; 4]>, String> {
        let values = self.read_scalars(buffers, accessor_index, components, false)?;
        Ok(values
            .iter()
            .map(|row| {
                [
                    row[0].max(0.0) as u32,
                    row[1].max(0.0) as u32,
                    row[2].max(0.0) as u32,
                    row[3].max(0.0) as u32,
                ]
            })
            .collect())
    }

    fn read_scalars(
        &self,
        buffers: &[Vec<u8>],
        accessor_index: u64,
        components: usize,
        normalized: bool,
    ) -> Result<Vec<[f32; 4]>, String> {
        let accessor = self
            .array("accessors")
            .get(accessor_index as usize)
            .ok_or_else(|| format!("accessor #{accessor_index} 不存在"))?;
        // sparse 存储：按基础 bufferView 降级读取（sparse 的替换项不应用），
        // 缺一部分顶点细节总好过整份资产导不出来，所以记 issue 而不是直接失败。
        if accessor.get("sparse").is_some() {
            self.warn(format!(
                "accessor #{accessor_index} 使用 sparse 存储，已按基础数据降级读取（sparse 补丁未应用，细节可能缺失）"
            ));
        }
        let count = accessor.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
        if count > MAX_ACCESSOR_COUNT {
            return Err(format!(
                "accessor #{accessor_index} 声明 count={count}，超过单个 accessor 上限 {MAX_ACCESSOR_COUNT}（文件损坏或 count 字段异常），已拒绝分配"
            ));
        }
        let component_type = accessor
            .get("componentType")
            .and_then(Value::as_u64)
            .unwrap_or(5126);
        let declared = accessor
            .get("type")
            .and_then(Value::as_str)
            .map(type_components)
            .unwrap_or(1);
        if declared < components {
            return Err(format!(
                "accessor #{accessor_index} 的 type 只有 {declared} 个分量，实际需要 {components}"
            ));
        }
        let declared_norm = accessor
            .get("normalized")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && normalized;
        let element_size = component_size(component_type) * declared;
        let accessor_offset = accessor
            .get("byteOffset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        let view_index = accessor.get("bufferView").and_then(Value::as_u64);
        let (data, stride) = match view_index {
            Some(view_index) => {
                let bytes = self.accessor_bytes(Some(view_index), buffers)?;
                let view = self.array("bufferViews").get(view_index as usize).unwrap();
                let stride = view
                    .get("byteStride")
                    .and_then(Value::as_u64)
                    .map(|s| s as usize)
                    .unwrap_or(element_size);
                (bytes, stride)
            }
            // 没有 bufferView 的 accessor 全 0（glTF 合法写法）
            None => (&[][..], element_size),
        };

        // 按实际字节数复核 count：`count` 与 bufferView 长度不符时（截断、手工改坏、
        // 恶意构造），照 count 预分配就是一次 OOM。宁可报参数错误，也不预分配巨量内存。
        if count > 0 && !data.is_empty() {
            let span = accessor_offset
                .saturating_add((count - 1).saturating_mul(stride.max(1)))
                .saturating_add(element_size);
            if span > data.len() {
                return Err(format!(
                    "accessor #{accessor_index} 声明 {count} 个元素（offset {accessor_offset}、stride {stride}、每元素 {element_size} 字节，至少需 {span} 字节），bufferView 只有 {} 字节",
                    data.len()
                ));
            }
        }

        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let mut row = [0.0f32; 4];
            let start = accessor_offset + i * stride;
            for c in 0..components {
                let at = start + c * component_size(component_type);
                let value = if data.is_empty() {
                    0.0
                } else if at + component_size(component_type) <= data.len() {
                    read_component(&data[at..], component_type, declared_norm)
                } else {
                    0.0
                };
                row[c.min(3)] = value as f32;
            }
            out.push(row);
        }
        Ok(out)
    }
}

fn type_components(kind: &str) -> usize {
    match kind {
        "SCALAR" => 1,
        "VEC2" => 2,
        "VEC3" => 3,
        "VEC4" => 4,
        "MAT2" => 4,
        "MAT3" => 9,
        "MAT4" => 16,
        _ => 1,
    }
}

fn component_size(component_type: u64) -> usize {
    match component_type {
        5120 | 5121 => 1,
        5122 | 5123 => 2,
        5125 | 5126 => 4,
        _ => 4,
    }
}

fn read_component(data: &[u8], component_type: u64, normalized: bool) -> f64 {
    match component_type {
        5120 => {
            let value = data[0] as i8;
            if normalized {
                (value as f64 / 127.0).max(-1.0)
            } else {
                value as f64
            }
        }
        5121 => {
            let value = data[0];
            if normalized {
                value as f64 / 255.0
            } else {
                value as f64
            }
        }
        5122 => {
            let value = i16::from_le_bytes([data[0], data[1]]);
            if normalized {
                (value as f64 / 32767.0).max(-1.0)
            } else {
                value as f64
            }
        }
        5123 => {
            let value = u16::from_le_bytes([data[0], data[1]]);
            if normalized {
                value as f64 / 65535.0
            } else {
                value as f64
            }
        }
        5125 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as f64,
        _ => f32::from_le_bytes([data[0], data[1], data[2], data[3]]) as f64,
    }
}

fn read_fixed(value: Option<&Value>, len: usize, default: f32) -> [f32; 3] {
    let mut out = [default; 3];
    if let Some(array) = value.and_then(Value::as_array) {
        for (i, item) in array.iter().take(len).enumerate() {
            out[i] = item.as_f64().unwrap_or(default as f64) as f32;
        }
    }
    out
}

fn read_fixed4(value: Option<&Value>) -> [f32; 4] {
    let mut out = [0.0, 0.0, 0.0, 1.0];
    if let Some(array) = value.and_then(Value::as_array) {
        for (i, item) in array.iter().take(4).enumerate() {
            out[i] = item.as_f64().unwrap_or(0.0) as f32;
        }
    }
    util::normalize_quat(out)
}

fn compose_trs(t: [f32; 3], r: [f32; 4], s: [f32; 3]) -> [f32; 16] {
    // 列主序：先把四元数转 3x3，再按列乘缩放，最后写平移
    let basis = mat3_from_quat(r);
    let mut out = [0.0f32; 16];
    for col in 0..3 {
        for row in 0..3 {
            out[col * 4 + row] = basis[col][row] * s[col];
        }
    }
    out[12] = t[0];
    out[13] = t[1];
    out[14] = t[2];
    out[15] = 1.0;
    out
}

fn mat3_from_quat(q: [f32; 4]) -> [[f32; 3]; 3] {
    let [x, y, z, w] = q;
    let (x2, y2, z2) = (x + x, y + y, z + z);
    let (xx, yy, zz) = (x * x2, y * y2, z * z2);
    let (xy, yz, zx) = (x * y2, y * z2, z * x2);
    let (wx, wy, wz) = (w * x2, w * y2, w * z2);
    [
        [1.0 - (yy + zz), xy + wz, zx - wy],
        [xy - wz, 1.0 - (xx + zz), yz + wx],
        [zx + wy, yz - wx, 1.0 - (xx + yy)],
    ]
}

// ---------------------------------------------------------------------------
// 材质 / 蒙皮 / 动画
// ---------------------------------------------------------------------------

/// 占位：材质构建不需要改 textures 表（贴图已按 image 建好），保留签名便于扩展。
fn build_material(
    g: &Gltf<'_>,
    material: &Value,
    index: u32,
    texture_of_image: &HashMap<u32, u32>,
    textures: &mut Vec<AirTexture>,
) -> AirMaterial {
    let pbr = material.get("pbrMetallicRoughness");
    let base = pbr
        .and_then(|p| p.get("baseColorFactor"))
        .and_then(Value::as_array)
        .map(|array| {
            let mut out = [1.0f32; 4];
            for (i, item) in array.iter().take(4).enumerate() {
                out[i] = item.as_f64().unwrap_or(1.0) as f32;
            }
            out
        })
        .unwrap_or([1.0, 1.0, 1.0, 1.0]);
    let metallic = pbr
        .and_then(|p| p.get("metallicFactor"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0) as f32;
    let roughness = pbr
        .and_then(|p| p.get("roughnessFactor"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0) as f32;

    // baseColorTexture 的 index 指向 textures[]，textures[].source 才指向 images[]
    let base_color_texture = pbr
        .and_then(|p| p.get("baseColorTexture"))
        .and_then(|t| t.get("index"))
        .and_then(Value::as_u64)
        .and_then(|texture_index| g.array("textures").get(texture_index as usize))
        .and_then(|texture| texture.get("source"))
        .and_then(Value::as_u64)
        .and_then(|image_index| texture_of_image.get(&(image_index as u32)).copied());

    let name = material
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("material{index}"));
    let material_out = AirMaterial {
        index,
        name,
        base_color: base,
        metallic: metallic.clamp(0.0, 1.0),
        roughness: roughness.clamp(0.0, 1.0),
        base_color_texture,
        double_sided: material
            .get("doubleSided")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    for texture in textures.iter_mut() {
        if Some(texture.index) == base_color_texture && texture.slot.is_empty() {
            texture.slot = "baseColorTexture".to_string();
        }
    }
    material_out
}

fn build_skin(
    skin: &Value,
    index: u32,
    g: &Gltf<'_>,
    buffers: &[Vec<u8>],
    node_count: usize,
) -> AirSkin {
    let joints: Vec<u32> = skin
        .get("joints")
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(Value::as_u64)
                .map(|j| j as u32)
                .filter(|j| (*j as usize) < node_count)
                .collect()
        })
        .unwrap_or_default();

    let mut inverse_bind = Vec::with_capacity(joints.len());
    let ibm_accessor = skin.get("inverseBindMatrices").and_then(Value::as_u64);
    if let Some(accessor) = ibm_accessor {
        match read_mat4(g, buffers, accessor) {
            Ok(mats) => {
                // glTF 的 IBM 是 mesh 空间 → 关节空间，而顶点本来就留在 mesh 局部空间，
                // 两者口径一致，直接按文件原值使用，不做任何修正。
                inverse_bind = mats;
            }
            Err(_) => inverse_bind.clear(),
        }
    }
    // IBM 缺失时补单位矩阵：蒙皮仍能跑（骨骼空间 = 世界空间），只是姿态不对
    while inverse_bind.len() < joints.len() {
        inverse_bind.push(util::identity4());
    }

    AirSkin {
        index,
        name: skin
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("skin{index}")),
        joints,
        inverse_bind_matrices: inverse_bind,
    }
}

fn read_mat4(g: &Gltf<'_>, buffers: &[Vec<u8>], accessor_index: u64) -> Result<Vec<[f32; 16]>, String> {
    let accessor = g
        .array("accessors")
        .get(accessor_index as usize)
        .ok_or_else(|| format!("accessor #{accessor_index} 不存在"))?;
    let count = accessor.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
    let view_index = accessor.get("bufferView").and_then(Value::as_u64);
    let bytes = g.accessor_bytes(view_index, buffers)?;
    let offset = accessor
        .get("byteOffset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let stride = view_index
        .and_then(|v| g.array("bufferViews").get(v as usize))
        .and_then(|view| view.get("byteStride"))
        .and_then(Value::as_u64)
        .map(|s| s as usize)
        .unwrap_or(64);
    // IBM 也走同一套上限：count 先按 bufferView 字节数核对，再决定分配多少。
    let needed = count
        .checked_mul(stride.max(1))
        .and_then(|value| value.checked_add(offset))
        .ok_or_else(|| {
            format!("IBM accessor #{accessor_index} 的 count×stride 溢出，count 字段异常")
        })?;
    if count > MAX_ACCESSOR_COUNT || needed > bytes.len() {
        return Err(format!(
            "IBM accessor #{accessor_index} 声明 {count} 个矩阵（offset {offset}、stride {stride}，需 {needed} 字节），bufferView 只有 {} 字节，已拒绝预分配",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = offset + i * stride;
        if start + 64 > bytes.len() {
            return Err(format!("IBM accessor #{accessor_index} 数据不足"));
        }
        let mut matrix = [0.0f32; 16];
        for c in 0..16 {
            let at = start + c * 4;
            matrix[c] = f32::from_le_bytes([
                bytes[at],
                bytes[at + 1],
                bytes[at + 2],
                bytes[at + 3],
            ]);
        }
        out.push(matrix);
    }
    Ok(out)
}

/// glTF 的采样器插值模式 → AIR 轨道插值模式。
///
/// 只认 STEP 与 LINEAR 两种：CUBICSPLINE 的切线在提取阶段已丢弃，保留字面量只会
/// 让播放端以为能做三次插值；大小写不敏感是因为不同导出器写法不统一。
fn air_interpolation(raw: &str) -> &'static str {
    if raw.eq_ignore_ascii_case("step") {
        crate::air::INTERPOLATION_STEP
    } else {
        crate::air::INTERPOLATION_LINEAR
    }
}

fn extract_animations(doc: &mut AirDoc, g: &Gltf<'_>, buffers: &[Vec<u8>]) {
    for (anim_index, animation) in g.array("animations").iter().enumerate() {
        let name = animation
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("animation{anim_index}"));
        let samplers = animation
            .get("samplers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut clip = AirAnimation {
            name,
            duration: 0.0,
            tracks: Vec::new(),
        };
        for channel in animation
            .get("channels")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let target = channel.get("target").cloned().unwrap_or(Value::Null);
            let Some(node) = target.get("node").and_then(Value::as_u64) else {
                continue;
            };
            let property = target
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !matches!(property.as_str(), "translation" | "rotation" | "scale") {
                continue;
            }
            let Some(sampler) = channel
                .get("sampler")
                .and_then(Value::as_u64)
                .and_then(|s| samplers.get(s as usize))
            else {
                continue;
            };
            let interpolation = sampler
                .get("interpolation")
                .and_then(Value::as_str)
                .unwrap_or("LINEAR");
            let components = if property == "rotation" { 4 } else { 3 };

            let Some(input) = sampler.get("input").and_then(Value::as_u64) else {
                continue;
            };
            let Some(output) = sampler.get("output").and_then(Value::as_u64) else {
                continue;
            };
            let times = match g.read_scalars(buffers, input, 1, false) {
                Ok(rows) => rows.iter().map(|row| row[0]).collect::<Vec<f32>>(),
                Err(err) => {
                    doc.note(format!("动画 `{}` 的时间轴读取失败：{err}", clip.name));
                    continue;
                }
            };
            let values = match g.read_scalars(buffers, output, components, false) {
                Ok(rows) => rows,
                Err(err) => {
                    doc.note(format!("动画 `{}` 的关键帧读取失败：{err}", clip.name));
                    continue;
                }
            };
            // CUBICSPLINE 每个关键帧存 3 组（入切线/值/出切线），只取中间那组；
            // 不处理会让轨道以三倍速播放且插值完全错乱。
            let mut picked: Vec<[f32; 4]> = Vec::with_capacity(times.len());
            if interpolation == "CUBICSPLINE" {
                if values.len() >= times.len() * 3 {
                    for i in 0..times.len() {
                        picked.push(values[i * 3 + 1]);
                    }
                } else {
                    doc.note(format!(
                        "动画 `{}` 声明 CUBICSPLINE 但数据量不足，已按线性截取",
                        clip.name
                    ));
                    picked.extend(values.iter().take(times.len()).copied());
                }
            } else {
                picked.extend(values.iter().take(times.len()).copied());
            }
            if picked.len() < 2 {
                continue;
            }
            let mut values_out = Vec::with_capacity(picked.len());
            for mut row in picked {
                if property == "rotation" {
                    row = util::normalize_quat(row);
                } else {
                    row[3] = 0.0;
                }
                values_out.push(row);
            }
            if let Some(last) = times.last() {
                clip.duration = clip.duration.max(*last);
            }
            clip.tracks.push(AirTrack {
                node: node as u32,
                property,
                // STEP 的语义是「保持前一帧」，必须原样带进 AIR；CUBICSPLINE 的切线
                // 上面已丢弃，只能降级成 linear。
                interpolation: air_interpolation(interpolation).to_string(),
                times,
                values: values_out,
            });
        }
        if !clip.tracks.is_empty() {
            doc.animations.push(clip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interpolation_mapping_only_keeps_step() {
        assert_eq!(air_interpolation("STEP"), crate::air::INTERPOLATION_STEP);
        assert_eq!(air_interpolation("step"), crate::air::INTERPOLATION_STEP);
        assert_eq!(air_interpolation("LINEAR"), crate::air::INTERPOLATION_LINEAR);
        // CUBICSPLINE 的切线已丢弃，必须降级，绝不能把名字原样带出去。
        assert_eq!(
            air_interpolation("CUBICSPLINE"),
            crate::air::INTERPOLATION_LINEAR
        );
        assert_eq!(air_interpolation("随便写的"), crate::air::INTERPOLATION_LINEAR);
    }

    /// 造一个最小的 float VEC3 accessor 文档，`buffers` 只放一份 12 字节数据。
    fn doc_with_accessor(accessor: Value) -> Value {
        json!({
            "asset": { "version": "2.0" },
            "buffers": [{ "byteLength": 12 }],
            "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": 12 }],
            "accessors": [accessor],
        })
    }

    fn read_one(json: &Value) -> (Result<Vec<[f32; 4]>, String>, Vec<String>) {
        let g = Gltf {
            json,
            notes: std::cell::RefCell::new(Vec::new()),
        };
        let buffers = vec![vec![0u8; 12]];
        let result = g.read_scalars(&buffers, 0, 3, false);
        let notes = g.notes.borrow().clone();
        (result, notes)
    }

    /// 正常 accessor 必须照读，且不产生任何降级记录（不然 issue 会被噪音淹掉）。
    #[test]
    fn plain_accessor_reads_without_issue() {
        let json = doc_with_accessor(json!({
            "bufferView": 0, "componentType": 5126, "count": 1, "type": "VEC3"
        }));
        let (result, notes) = read_one(&json);
        assert_eq!(result.expect("正常 accessor 应当读成功").len(), 1);
        assert!(notes.is_empty(), "正常读取不该有降级记录：{notes:?}");
    }

    /// `count` 天文数字必须在分配前拦住：照它 with_capacity 会 OOM abort，
    /// 那时连错误都回不去（宿主只看到进程消失）。
    #[test]
    fn accessor_count_above_limit_is_rejected() {
        let json = doc_with_accessor(json!({
            "bufferView": 0,
            "componentType": 5126,
            "count": (MAX_ACCESSOR_COUNT as u64) + 1,
            "type": "VEC3"
        }));
        let (result, _) = read_one(&json);
        let err = result.expect_err("超过上限的 count 必须被拒绝");
        assert!(err.contains("上限"), "错误要说清撞的是 accessor 上限：{err}");
        assert!(err.contains("拒绝分配"), "错误要说明没有真的去分配：{err}");
    }

    /// 刚好等于上限的 count 是合法值，不能因为「大」就误杀（边界另一侧）。
    #[test]
    fn accessor_count_at_limit_is_not_rejected_by_count() {
        let json = doc_with_accessor(json!({
            "bufferView": 0,
            "componentType": 5126,
            "count": MAX_ACCESSOR_COUNT as u64,
            "type": "VEC3"
        }));
        let (result, _) = read_one(&json);
        // 数据只有 12 字节，这里必然因为「数据不够」失败——但不能是「超过上限」那条。
        let err = result.expect_err("数据不足应当报错");
        assert!(!err.contains("上限"), "等于上限不该被 count 闸门拦下：{err}");
    }

    /// sparse 存储不应用补丁，但要降级读取并留下 issue：缺细节好过整份资产导不出来，
    /// 而静默降级等于让用户永远不知道自己看到的顶点是错的。
    #[test]
    fn sparse_accessor_degrades_with_issue() {
        let json = doc_with_accessor(json!({
            "bufferView": 0,
            "componentType": 5126,
            "count": 1,
            "type": "VEC3",
            "sparse": {
                "count": 1,
                "indices": { "bufferView": 0, "componentType": 5125 },
                "values": { "bufferView": 0 }
            }
        }));
        let (result, notes) = read_one(&json);
        assert!(result.is_ok(), "sparse 应当降级读取而不是失败");
        assert_eq!(notes.len(), 1, "降级记录必须恰好一条：{notes:?}");
        assert!(notes[0].contains("sparse"), "issue 要说明是 sparse 降级：{}", notes[0]);
    }

    /// 同一条降级只记一次：read_scalars 每个属性/每份网格都会碰同一个 accessor，
    /// 不去重的话 issues 里会堆出几十条同样的句子。
    #[test]
    fn repeated_degradation_is_deduplicated() {
        let json = doc_with_accessor(json!({
            "bufferView": 0,
            "componentType": 5126,
            "count": 1,
            "type": "VEC3",
            "sparse": { "count": 1, "indices": { "bufferView": 0 }, "values": { "bufferView": 0 } }
        }));
        let g = Gltf {
            json: &json,
            notes: std::cell::RefCell::new(Vec::new()),
        };
        let buffers = vec![vec![0u8; 12]];
        for _ in 0..3 {
            let _ = g.read_scalars(&buffers, 0, 3, false);
        }
        assert_eq!(g.notes.borrow().len(), 1, "同一句降级只留一条");
    }
}
