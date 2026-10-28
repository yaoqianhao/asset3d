//! 资产 → 可画的三角形集合（M-UI-3 第二刀）。
//!
//! AIR 的顶点留在 **mesh 局部空间**（见 `src/gltf.rs` 头注释第 2 条），世界坐标要按
//! `AirNode` 的 TRS 沿父链累乘。视口只关心「一组三角形 + 一个包围盒」，所以这里把
//! 节点变换直接烘进顶点，运行时不再维护场景图——省掉一层可能写错的递归。
//!
//! 与宿主插件侧的差别：插件不允许碰磁盘（外部资源经 `host/fs.read` 取），视口进程是
//! 用户自己双击起来的独立进程，没有宿主闸门可借，因此这里直接读文件；`gltf` 的外部
//! `uri` 也只允许落在资产同目录下，越界一律当读不到（缺失贴图不该让看图失败）。

use std::path::{Path, PathBuf};

use glam::{Mat3, Mat4, Quat, Vec3};

use crate::air::AirDoc;

/// GPU 顶点：位置 + 法线。与 `shader.wgsl` 的 `VsIn` 逐字段对应，
/// 步长 24 字节（`f32 × 6`）。
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
}

/// 一份可画的场景：合并后的顶点/索引 + 取景用的包围球。
pub struct Scene {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// 包围盒中心（世界空间）。
    pub center: Vec3,
    /// 包围球半径，用来定相机距离与远近裁剪面。
    pub radius: f32,
    pub mesh_count: usize,
    pub triangle_count: usize,
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
    let mut doc = AirDoc::new(container, &hint);
    match ext.as_str() {
        "glb" | "gltf" => {
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let mut fetch = |uri: &str| read_sibling(&dir, uri);
            let fetch_ref: &mut dyn FnMut(&str) -> Option<Vec<u8>> = &mut fetch;
            crate::gltf::import(&mut doc, &bytes, &hint, false, false, fetch_ref)?;
        }
        _ => crate::fbx::import(&mut doc, &bytes, &hint, false, false)?,
    }
    merge(&doc)
}

/// 把 `AirDoc` 的所有网格按节点世界矩阵合并成一份顶点流。
fn merge(doc: &AirDoc) -> Result<Scene, String> {
    // 世界矩阵按需缓存：扁平资产（mesh 挂在根上）一次就够，深骨架也只算一遍。
    let mut cache: Vec<Option<Mat4>> = vec![None; doc.nodes.len()];
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut mesh_count = 0usize;

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
            vertices.push(Vertex {
                pos: world_pos.to_array(),
                normal: normal.to_array(),
            });
        }
        // 索引可能指向不存在的顶点（畸形资产）：逐条过一遍，宁缺勿崩。
        let limit = mesh.positions.len() as u32;
        for triangle in mesh.indices.chunks_exact(3) {
            if triangle.iter().all(|index| *index < limit) {
                indices.extend(triangle.iter().map(|index| base + *index));
            }
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
        center,
        radius,
        mesh_count,
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
/// 「RGB 颜色」而不是法线，省一个顶点布局。
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
        });
        vertices.push(Vertex {
            pos: to,
            normal: color,
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
        center: Vec3::ZERO,
        radius: span,
        mesh_count: 0,
        triangle_count: 0,
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
