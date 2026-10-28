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

use crate::air::{AirDoc, AirMaterial, AirTexture, MaterialPart};

/// 贴图像素上限（64 Mi 像素 ≈ 4096×4096 的 RGBA）。超过就不解码：视口是看图的，
/// 为了看一眼资产把 8K 贴图整块展开成 256 MB 像素，代价和收益完全不成比例。
const MAX_TEXTURE_PIXELS: u64 = 4096 * 4096;

/// GPU 顶点：位置 + 法线 + UV。与 `shader.wgsl` 的 `VsIn` 逐字段对应，
/// 步长 32 字节（`f32 × 8`）。
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

/// 解码后的贴图：RGBA8 紧密排列，行序自上而下（与 wgpu 的纹理坐标 v=0 对齐）。
pub struct TexImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
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
            crate::gltf::import(&mut doc, &bytes, &hint, false, true, fetch_ref)?;
        }
        _ => crate::fbx::import(&mut doc, &bytes, &hint, false, true)?,
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
        for (index, position) in mesh.positions.iter().enumerate() {
            let world_pos = world.transform_point3(Vec3::from_array(*position));
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
            vertices.push(Vertex {
                pos: world_pos.to_array(),
                normal: normal.to_array(),
                uv,
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

    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for vertex in &vertices {
        let position = Vec3::from_array(vertex.pos);
        if position.is_finite() {
            min = min.min(position);
            max = max.max(position);
        }
    }
    if !min.is_finite() || !max.is_finite() {
        return Err("顶点坐标里没有有限值（资产可能全是 NaN）".to_string());
    }
    let center = (min + max) * 0.5;
    // 退化资产（单个面片）半径会是 0：给个下限，否则相机距离除零。
    let radius = ((max - min).length() * 0.5).max(1e-3);

    Ok(Scene {
        triangle_count: indices.len() / 3,
        vertices,
        indices,
        parts,
        center,
        radius,
        mesh_count,
        texture_count,
        notes,
    })
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
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    if u64::from(width) * u64::from(height) > MAX_TEXTURE_PIXELS {
        return Err(format!(
            "{width}×{height} 超过 {} 像素上限，未展开",
            MAX_TEXTURE_PIXELS
        ));
    }
    let _ = texture.width;
    Ok(TexImage {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

/// 节点世界矩阵 = 父链累乘。`depth` 只是坏 `parent`（自环 / 越界）的保险丝。
fn world_matrix(doc: &AirDoc, index: usize, cache: &mut Vec<Option<Mat4>>, depth: u32) -> Mat4 {
    if let Some(Some(cached)) = cache.get(index) {
        return *cached;
    }
    let Some(node) = doc.nodes.get(index) else {
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
            world_matrix(doc, parent as usize, cache, depth + 1) * local
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
        });
        vertices.push(Vertex {
            pos: to,
            normal: color,
            uv: [0.0, 0.0],
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
        center: Vec3::ZERO,
        radius: span,
        mesh_count: 0,
        triangle_count: 0,
        texture_count: 0,
        notes: Vec::new(),
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
    use crate::air::{AirMesh, AirNode};

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
}
