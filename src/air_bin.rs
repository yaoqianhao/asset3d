//! AIRB：AIR 的二进制容器。
//!
//! 布局（小端）：
//!
//! ```text
//! [0..4)   magic  "AIRB"
//! [4..8)   u32    version      // 当前 1
//! [8..12)  u32    meta_len
//! [12..20) u64    blob_len
//! [20..)   meta     UTF-8 JSON（`AirbMeta`，其中的 `air.meshes[i]` 几何数组已被清空）
//!          blob     平铺几何块（每段 4 字节对齐，偏移相对 blob 起点）
//! ```
//!
//! 为什么要这个格式：JSON 的浮点文本解析在引擎启动时是一笔实打实的开销
//! （10 万顶点 ≈ 30 MB 文本），而 AIRB 让几何数据退化成「读进内存 + 直接当
//! 顶点缓冲用」，只留一份很小的 JSON 头给元数据。
//!
//! `air.meshes[i].material_parts` / `aabb_*` 等少量元数据仍留在 JSON 里，
//! 几何本身则通过 `meshes[i]` 的 span 定位到 blob：
//!
//! | 段 | 元素 | 每元素字节 | 对齐 |
//! | --- | --- | --- | --- |
//! | positions | f32×3 | 12 | 4 |
//! | normals   | f32×3 | 12 | 4 |
//! | uvs       | f32×2 | 8  | 4 |
//! | indices   | u32   | 4  | 4 |
//! | joints    | u16×4 | 8  | 4 |
//! | weights   | f32×4 | 16 | 4 |
//!
//! 空数组的 `count` 为 0，`offset` 指向当前写入位置（可以安全忽略）。

use serde::{Deserialize, Serialize};

use crate::air::AirDoc;

pub const MAGIC: [u8; 4] = *b"AIRB";
pub const VERSION: u32 = 1;
/// 头部固定长度：magic(4) + version(4) + meta_len(4) + blob_len(8)。
pub const HEADER_LEN: usize = 20;

/// blob 内一段数组的位置。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Span {
    /// 相对 blob 起点的字节偏移。
    pub offset: u32,
    /// 元素个数（不是字节数）。
    pub count: u32,
}

impl Span {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// 一个网格在 blob 内的六段数组位置。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MeshSpans {
    pub positions: Span,
    pub normals: Span,
    pub uvs: Span,
    pub indices: Span,
    pub joints: Span,
    pub weights: Span,
}

impl MeshSpans {
    /// 这些段是否全空（空网格不该被导出，但容错）。
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
            && self.normals.is_empty()
            && self.uvs.is_empty()
            && self.indices.is_empty()
            && self.joints.is_empty()
            && self.weights.is_empty()
    }
}

/// AIRB 的 JSON 头。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirbMeta {
    /// 固定 `"airb"`，与 magic 双保险。
    pub format: String,
    pub airb_version: u32,
    /// blob 总字节数（等于头部 `blob_len`，便于消费方交叉校验）。
    pub blob_bytes: u64,
    /// 与 `air.meshes` 一一对应。
    pub meshes: Vec<MeshSpans>,
    /// 元数据：几何数组为空，其余字段与 `.air.json` 完全一致。
    pub air: AirDoc,
}

/// 编码结果。
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub meta: AirbMeta,
}

/// 把 AIR 文档编码成 AIRB。
pub fn encode(doc: &AirDoc) -> Encoded {
    let mut head = doc.clone();
    let mut blob: Vec<u8> = Vec::new();
    let mut spans: Vec<MeshSpans> = Vec::with_capacity(head.meshes.len());

    for (i, mesh) in head.meshes.iter_mut().enumerate() {
        let src = &doc.meshes[i];
        let span = MeshSpans {
            positions: push_f32x3(&mut blob, &src.positions),
            normals: push_f32x3(&mut blob, &src.normals),
            uvs: push_f32x2(&mut blob, &src.uvs),
            indices: push_u32(&mut blob, &src.indices),
            joints: push_u16x4(&mut blob, &src.joints),
            weights: push_f32x4(&mut blob, &src.weights),
        };
        // 几何已经在 blob 里了，JSON 头不要再带一份。
        mesh.positions.clear();
        mesh.normals.clear();
        mesh.uvs.clear();
        mesh.indices.clear();
        mesh.joints.clear();
        mesh.weights.clear();
        spans.push(span);
    }

    let meta = AirbMeta {
        format: "airb".to_string(),
        airb_version: VERSION,
        blob_bytes: blob.len() as u64,
        meshes: spans,
        air: head,
    };
    let meta_bytes = serde_json::to_vec(&meta).unwrap_or_default();

    let mut out = Vec::with_capacity(HEADER_LEN + meta_bytes.len() + blob.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    out.extend_from_slice(&meta_bytes);
    out.extend_from_slice(&blob);

    Encoded { bytes: out, meta }
}

/// 只读头部，判断字节流是不是 AIRB（预览工具直接吃 `.air.bin` 时用）。
pub fn looks_like_airb(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN && bytes[0..4] == MAGIC
}

/// 解析 AIRB，还原成完整 `AirDoc`（几何从 blob 拷回数组）。
///
/// 这既是自检手段（导出后立刻回读一遍能抓到绝大多数布局写错），
/// 也是引擎侧加载器的参考实现。
pub fn decode(bytes: &[u8]) -> Result<AirDoc, String> {
    if !looks_like_airb(bytes) {
        return Err("不是 AIRB：缺少 `AIRB` 魔数".to_string());
    }
    let version = read_u32(bytes, 4)?;
    if version != VERSION {
        return Err(format!("AIRB 版本 {version} 与本工具（{VERSION}）不符"));
    }
    let meta_len = read_u32(bytes, 8)? as usize;
    let blob_len = read_u64(bytes, 12)? as usize;
    let meta_start = HEADER_LEN;
    let meta_end = meta_start
        .checked_add(meta_len)
        .ok_or_else(|| "AIRB 头长度溢出".to_string())?;
    if meta_end + blob_len != bytes.len() {
        return Err(format!(
            "AIRB 长度不符：头声明 {} 字节，实际 {} 字节（文件被截断或尾部被追加）",
            meta_end + blob_len,
            bytes.len()
        ));
    }

    let meta: AirbMeta = serde_json::from_slice(&bytes[meta_start..meta_end])
        .map_err(|err| format!("AIRB 元数据不是合法 JSON：{err}"))?;
    if meta.meshes.len() != meta.air.meshes.len() {
        return Err(format!(
            "AIRB span 表（{}）与网格数（{}）不一致",
            meta.meshes.len(),
            meta.air.meshes.len()
        ));
    }

    let blob = &bytes[meta_end..];
    let mut doc = meta.air;
    for (mesh, span) in doc.meshes.iter_mut().zip(meta.meshes.iter()) {
        mesh.positions = read_f32x3(blob, span.positions)?;
        mesh.normals = read_f32x3(blob, span.normals)?;
        mesh.uvs = read_f32x2(blob, span.uvs)?;
        mesh.indices = read_u32_span(blob, span.indices)?;
        mesh.joints = read_u16x4(blob, span.joints)?;
        mesh.weights = read_f32x4(blob, span.weights)?;
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// blob 写入
// ---------------------------------------------------------------------------

fn align4(blob: &mut Vec<u8>) {
    while blob.len() % 4 != 0 {
        blob.push(0);
    }
}

fn push_f32x3(blob: &mut Vec<u8>, values: &[[f32; 3]]) -> Span {
    align4(blob);
    let offset = blob.len() as u32;
    for v in values {
        for c in v {
            blob.extend_from_slice(&c.to_le_bytes());
        }
    }
    Span {
        offset,
        count: values.len() as u32,
    }
}

fn push_f32x2(blob: &mut Vec<u8>, values: &[[f32; 2]]) -> Span {
    align4(blob);
    let offset = blob.len() as u32;
    for v in values {
        for c in v {
            blob.extend_from_slice(&c.to_le_bytes());
        }
    }
    Span {
        offset,
        count: values.len() as u32,
    }
}

fn push_f32x4(blob: &mut Vec<u8>, values: &[[f32; 4]]) -> Span {
    align4(blob);
    let offset = blob.len() as u32;
    for v in values {
        for c in v {
            blob.extend_from_slice(&c.to_le_bytes());
        }
    }
    Span {
        offset,
        count: values.len() as u32,
    }
}

fn push_u16x4(blob: &mut Vec<u8>, values: &[[u16; 4]]) -> Span {
    align4(blob);
    let offset = blob.len() as u32;
    for v in values {
        for c in v {
            blob.extend_from_slice(&c.to_le_bytes());
        }
    }
    Span {
        offset,
        count: values.len() as u32,
    }
}

fn push_u32(blob: &mut Vec<u8>, values: &[u32]) -> Span {
    align4(blob);
    let offset = blob.len() as u32;
    for v in values {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    Span {
        offset,
        count: values.len() as u32,
    }
}

// ---------------------------------------------------------------------------
// blob 读取
// ---------------------------------------------------------------------------

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let slice = bytes
        .get(at..at + 4)
        .ok_or_else(|| format!("读取 u32 越界（offset {at}）"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    let slice = bytes
        .get(at..at + 8)
        .ok_or_else(|| format!("读取 u64 越界（offset {at}）"))?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(slice);
    Ok(u64::from_le_bytes(buf))
}

/// 定位一段并校验边界：`stride` 是单元素字节数。
fn slice_of(blob: &[u8], span: Span, stride: usize) -> Result<&[u8], String> {
    let start = span.offset as usize;
    let len = span.count as usize * stride;
    if start > blob.len() || start + len > blob.len() {
        return Err(format!(
            "AIRB 段越界：offset {start} + {len} 字节，blob 只有 {} 字节",
            blob.len()
        ));
    }
    Ok(&blob[start..start + len])
}

fn read_f32x3(blob: &[u8], span: Span) -> Result<Vec<[f32; 3]>, String> {
    let data = slice_of(blob, span, 12)?;
    let mut out = Vec::with_capacity(span.count as usize);
    for chunk in data.chunks_exact(12) {
        out.push([
            f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            f32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]),
        ]);
    }
    Ok(out)
}

fn read_f32x2(blob: &[u8], span: Span) -> Result<Vec<[f32; 2]>, String> {
    let data = slice_of(blob, span, 8)?;
    let mut out = Vec::with_capacity(span.count as usize);
    for chunk in data.chunks_exact(8) {
        out.push([
            f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
        ]);
    }
    Ok(out)
}

fn read_f32x4(blob: &[u8], span: Span) -> Result<Vec<[f32; 4]>, String> {
    let data = slice_of(blob, span, 16)?;
    let mut out = Vec::with_capacity(span.count as usize);
    for chunk in data.chunks_exact(16) {
        out.push([
            f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            f32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]),
            f32::from_le_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]),
        ]);
    }
    Ok(out)
}

fn read_u16x4(blob: &[u8], span: Span) -> Result<Vec<[u16; 4]>, String> {
    let data = slice_of(blob, span, 8)?;
    let mut out = Vec::with_capacity(span.count as usize);
    for chunk in data.chunks_exact(8) {
        out.push([
            u16::from_le_bytes([chunk[0], chunk[1]]),
            u16::from_le_bytes([chunk[2], chunk[3]]),
            u16::from_le_bytes([chunk[4], chunk[5]]),
            u16::from_le_bytes([chunk[6], chunk[7]]),
        ]);
    }
    Ok(out)
}

fn read_u32_span(blob: &[u8], span: Span) -> Result<Vec<u32>, String> {
    let data = slice_of(blob, span, 4)?;
    let mut out = Vec::with_capacity(span.count as usize);
    for chunk in data.chunks_exact(4) {
        out.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::air::{AirDoc, AirMesh};

    fn sample() -> AirDoc {
        let mut doc = AirDoc::new("glb", "models/a.glb");
        let mut mesh = AirMesh::default();
        mesh.name = "body".into();
        mesh.node = "Root".into();
        mesh.positions = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        mesh.normals = vec![[0.0, 0.0, 1.0]; 3];
        mesh.uvs = vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        mesh.indices = vec![0, 1, 2];
        mesh.joints = vec![[0, 1, 0, 0]; 3];
        mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 3];
        doc.meshes.push(mesh);
        doc.finalize();
        doc
    }

    #[test]
    fn round_trip_keeps_geometry() {
        let doc = sample();
        let encoded = encode(&doc);
        let back = decode(&encoded.bytes).expect("decode");
        let a = &doc.meshes[0];
        let b = &back.meshes[0];
        assert_eq!(a.positions, b.positions);
        assert_eq!(a.uvs, b.uvs);
        assert_eq!(a.indices, b.indices);
        assert_eq!(a.joints, b.joints);
        assert_eq!(a.weights, b.weights);
        // JSON 头里不该再留一份几何。
        assert!(encoded.meta.air.meshes[0].positions.is_empty());
    }

    #[test]
    fn truncated_blob_is_rejected() {
        let doc = sample();
        let mut encoded = encode(&doc).bytes;
        encoded.truncate(encoded.len() - 4);
        assert!(decode(&encoded).is_err());
    }
}
