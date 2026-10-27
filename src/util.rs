//! 与协议无关的小工具：容器嗅探、图片尺寸嗅探、矩阵与路径处理。
//!
//! 图片尺寸为什么不引 `image` crate：这里只需要宽高与编码类型，为几个字节的
//! 头部信息拉进一个带 SIMD/多编解码器的重依赖，会让插件的编译时间与二进制体积
//! 成倍增长；贴图本身由消费方（引擎/浏览器）解码，插件不碰像素。

/// 人读的字节数。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// 小写扩展名（不含点）。
pub fn extension(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 去掉目录与扩展名的文件主干，用于派生输出文件名。
pub fn stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("asset")
        .to_string()
}

/// 把主干变成安全的文件名片段（保留中文与 `-_.`，其余换成 `-`）。
pub fn safe_stem(path: &str) -> String {
    let raw = stem(path);
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == '.' || !ch.is_ascii() {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "asset".to_string()
    } else {
        trimmed
    }
}

/// 交换 `a/b` 与 `b/a` 两个过滤器比较器的便捷写法（用于浮点排序）。
pub fn cmp_f32(a: &f32, b: &f32) -> std::cmp::Ordering {
    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
}

// ---------------------------------------------------------------------------
// 容器嗅探
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    FbxBinary,
    FbxAscii,
    Glb,
    Gltf,
}

impl Container {
    pub fn as_str(self) -> &'static str {
        match self {
            Container::FbxBinary => "fbx",
            Container::FbxAscii => "fbx",
            Container::Glb => "glb",
            Container::Gltf => "gltf",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::FbxBinary => "FBX（二进制）",
            Container::FbxAscii => "FBX（ASCII）",
            Container::Glb => "glTF 二进制（GLB）",
            Container::Gltf => "glTF JSON（.gltf）",
        }
    }
}

/// 按内容而不是扩展名判定容器：导入失败最常见的原因就是「扩展名是 .glb，内容
/// 其实是别的」，先给一条能自证的错误比让解析器抛底层异常更好诊断。
pub fn sniff_container(path: &str, bytes: &[u8]) -> Result<Container, String> {
    if bytes.len() >= 12 {
        // GLB: magic "glTF" + version(4) + length(4)
        if &bytes[0..4] == b"glTF" {
            return Ok(Container::Glb);
        }
        // FBX 二进制: "Kaydara FBX Binary  \x00"
        if bytes.starts_with(b"Kaydara FBX Binary") {
            return Ok(Container::FbxBinary);
        }
    }
    let head = &bytes[..bytes.len().min(2048)];
    let text = String::from_utf8_lossy(head);
    if text.contains("Kaydara FBX Binary") || text.contains("FBXHeaderExtension") {
        return Ok(if text.contains("FBXHeaderExtension") {
            Container::FbxAscii
        } else {
            Container::FbxBinary
        });
    }
    if text.trim_start().starts_with('{') {
        // .gltf 是纯 JSON；能落到这里说明是文本 glTF（JSON 的 asset.version 是 2.x）
        if text.contains("\"asset\"") || text.contains("\"meshes\"") || text.contains("\"scenes\"") {
            return Ok(Container::Gltf);
        }
    }
    let ext = extension(path);
    Err(format!(
        "无法识别的 3D 容器：扩展名 `{ext}`，开头字节 {:02X?}。支持 .fbx / .glb / .gltf",
        &bytes[..bytes.len().min(8)]
    ))
}

// ---------------------------------------------------------------------------
// 图片头部嗅探
// ---------------------------------------------------------------------------

/// 从编码后的图片字节里解出 `(宽, 高, mime)`。
pub fn probe_image_size(bytes: &[u8]) -> Option<(u32, u32, &'static str)> {
    if bytes.len() < 12 {
        return None;
    }
    // PNG: 8 字节签名 + IHDR（长度 4 + 类型 4 + 宽 4 + 高 4）
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        if bytes.len() >= 24 {
            let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            return Some((width, height, "image/png"));
        }
    }
    // JPEG: 扫描段直到 SOFn
    if bytes.starts_with(&[0xFF, 0xD8]) {
        if let Some((width, height)) = jpeg_size(bytes) {
            return Some((width, height, "image/jpeg"));
        }
    }
    // GIF: "GIF87a" / "GIF89a"，随后是小端宽高
    if bytes.starts_with(b"GIF8") {
        if bytes.len() >= 10 {
            let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
            let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
            return Some((width, height, "image/gif"));
        }
    }
    // BMP: "BM" + 文件头 14 字节 + DIB 头里的宽高（有符号，负高表示自顶向下）
    if bytes.starts_with(b"BM") && bytes.len() >= 26 {
        let width = i32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]);
        let height = i32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]);
        if width > 0 && height != 0 {
            return Some((width as u32, height.unsigned_abs(), "image/bmp"));
        }
    }
    // WEBP: "RIFF" size "WEBP" + chunk
    if bytes.starts_with(b"RIFF") && bytes.len() >= 30 && &bytes[8..12] == b"WEBP" {
        let chunk = &bytes[12..16];
        if chunk == b"VP8X" && bytes.len() >= 30 {
            let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
            let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
            return Some((width, height, "image/webp"));
        }
        if chunk == b"VP8 " && bytes.len() >= 30 {
            let width = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF;
            let height = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF;
            return Some((width as u32, height as u32, "image/webp"));
        }
        if chunk == b"VP8L" && bytes.len() >= 25 {
            let b = &bytes[21..25];
            let width = 1 + (((b[1] & 0x3F) as u32) << 8) + b[0] as u32;
            let height = 1 + (((b[3] & 0x0F) as u32) << 10) + ((b[2] as u32) << 2)
                + ((b[1] as u32) >> 6);
            return Some((width, height, "image/webp"));
        }
    }
    None
}

fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // 无载荷的标记
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF0..SOF15，排除 DHT(C4) / JPG(C8) / DAC(CC)
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((width, height));
        }
        if length < 2 {
            return None;
        }
        i += 2 + length;
    }
    None
}

// ---------------------------------------------------------------------------
// 矩阵
// ---------------------------------------------------------------------------

/// ufbx 的 4x3 列主序矩阵 → `[f32; 16]`（列主序，与 wgpu/glam 一致）。
pub fn mat4_from_ufbx(m: &ufbx::Matrix) -> [f32; 16] {
    [
        m.m00 as f32,
        m.m10 as f32,
        m.m20 as f32,
        0.0,
        m.m01 as f32,
        m.m11 as f32,
        m.m21 as f32,
        0.0,
        m.m02 as f32,
        m.m12 as f32,
        m.m22 as f32,
        0.0,
        m.m03 as f32,
        m.m13 as f32,
        m.m23 as f32,
        1.0,
    ]
}

/// 用 3x3 余子式矩阵变换法线，再归一化。
///
/// 法线的正确变换是 `inverse(M)^T`；而余子式矩阵等于 `det(M) · inverse(M)^T`，
/// 只差一个正标量，归一化后完全等价——省掉一次 4x4 求逆，也避开奇异矩阵除零。
pub fn transform_normal(m: &[f32; 16], n: [f32; 3]) -> [f32; 3] {
    // 列主序：m[c * 4 + r]
    let a = [m[0], m[1], m[2]];
    let b = [m[4], m[5], m[6]];
    let c = [m[8], m[9], m[10]];
    // 余子式的列 = 另外两列的叉积
    let c0 = cross(b, c);
    let c1 = cross(c, a);
    let c2 = cross(a, b);
    let out = [
        c0[0] * n[0] + c1[0] * n[1] + c2[0] * n[2],
        c0[1] * n[0] + c1[1] * n[1] + c2[1] * n[2],
        c0[2] * n[0] + c1[2] * n[1] + c2[2] * n[2],
    ];
    normalize(out, [0.0, 1.0, 0.0])
}

pub fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn normalize(v: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let len2 = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if len2 > 1e-20 && len2.is_finite() {
        let inv = 1.0 / len2.sqrt();
        [v[0] * inv, v[1] * inv, v[2] * inv]
    } else {
        fallback
    }
}

/// 四元数归一化（FBX/GLB 都可能给出未归一化的旋转）。
pub fn normalize_quat(q: [f32; 4]) -> [f32; 4] {
    let len2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
    if len2 > 1e-20 && len2.is_finite() {
        let inv = 1.0 / len2.sqrt();
        [q[0] * inv, q[1] * inv, q[2] * inv, q[3] * inv]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

/// 从列主序 4x4 里取平移/旋转（转四元数）/缩放，用于填充节点 TRS。
pub fn decompose(m: &[f32; 16]) -> ([f32; 3], [f32; 4], [f32; 3]) {
    let translation = [m[12], m[13], m[14]];
    let mut basis = [
        [m[0], m[1], m[2]],
        [m[4], m[5], m[6]],
        [m[8], m[9], m[10]],
    ];
    let scale = [
        length(basis[0]),
        length(basis[1]),
        length(basis[2]),
    ];
    for (axis, value) in scale.iter().enumerate() {
        if *value > 1e-12 {
            basis[axis] = [
                basis[axis][0] / value,
                basis[axis][1] / value,
                basis[axis][2] / value,
            ];
        }
    }
    // 左手/镜像：行列式为负时缩放取负，避免旋转出现镜像
    let det = dot(cross(basis[0], basis[1]), basis[2]);
    let mut scale = scale;
    if det < 0.0 {
        scale[0] = -scale[0];
        basis[0] = [-basis[0][0], -basis[0][1], -basis[0][2]];
    }
    let rotation = quat_from_mat3(basis);
    (translation, rotation, scale)
}

fn length(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// 3x3（列主序，列为 `basis[0..3]`）→ 四元数。
pub fn quat_from_mat3(basis: [[f32; 3]; 3]) -> [f32; 4] {
    // 行优先取值：r[row][col] = basis[col][row]
    let m00 = basis[0][0];
    let m01 = basis[1][0];
    let m02 = basis[2][0];
    let m10 = basis[0][1];
    let m11 = basis[1][1];
    let m12 = basis[2][1];
    let m20 = basis[0][2];
    let m21 = basis[1][2];
    let m22 = basis[2][2];
    let trace = m00 + m11 + m22;
    let q = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [
            (m21 - m12) / s,
            (m02 - m20) / s,
            (m10 - m01) / s,
            0.25 * s,
        ]
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        [
            0.25 * s,
            (m01 + m10) / s,
            (m02 + m20) / s,
            (m21 - m12) / s,
        ]
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        [
            (m01 + m10) / s,
            0.25 * s,
            (m12 + m21) / s,
            (m02 - m20) / s,
        ]
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        [
            (m02 + m20) / s,
            (m12 + m21) / s,
            0.25 * s,
            (m10 - m01) / s,
        ]
    };
    normalize_quat(q)
}

/// 列主序 4x4 乘法 `a * b`（先 b 后 a）。
pub fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k * 4 + row] * b[col * 4 + k];
            }
            out[col * 4 + row] = sum;
        }
    }
    out
}

/// 变换一个点（带平移）。
pub fn transform_point(m: &[f32; 16], p: [f32; 3]) -> [f32; 3] {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

pub fn identity4() -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

/// 列主序 4x4 求逆。奇异矩阵返回单位矩阵并置零平移（宁可看着不对，也不要 NaN
/// 扩散到整块蒙皮）。
pub fn mat4_invert(m: &[f32; 16]) -> [f32; 16] {
    // 先按行优先把元素拆出来，套用经典 4x4 伴随矩阵算法
    let a00 = m[0];
    let a01 = m[4];
    let a02 = m[8];
    let a03 = m[12];
    let a10 = m[1];
    let a11 = m[5];
    let a12 = m[9];
    let a13 = m[13];
    let a20 = m[2];
    let a21 = m[6];
    let a22 = m[10];
    let a23 = m[14];
    let a30 = m[3];
    let a31 = m[7];
    let a32 = m[11];
    let a33 = m[15];

    let b00 = a00 * a11 - a01 * a10;
    let b01 = a00 * a12 - a02 * a10;
    let b02 = a00 * a13 - a03 * a10;
    let b03 = a01 * a12 - a02 * a11;
    let b04 = a01 * a13 - a03 * a11;
    let b05 = a02 * a13 - a03 * a12;
    let b06 = a20 * a31 - a21 * a30;
    let b07 = a20 * a32 - a22 * a30;
    let b08 = a20 * a33 - a23 * a30;
    let b09 = a21 * a32 - a22 * a31;
    let b10 = a21 * a33 - a23 * a31;
    let b11 = a22 * a33 - a23 * a32;

    let det = b00 * b11 - b01 * b10 + b02 * b09 + b03 * b08 - b04 * b07 + b05 * b06;
    if det.abs() < 1e-20 || !det.is_finite() {
        return identity4();
    }
    let inv_det = 1.0 / det;

    [
        (a11 * b11 - a12 * b10 + a13 * b09) * inv_det,
        (a02 * b10 - a01 * b11 - a03 * b09) * inv_det,
        (a31 * b05 - a32 * b04 + a33 * b03) * inv_det,
        (a22 * b04 - a21 * b05 - a23 * b03) * inv_det,
        (a12 * b08 - a10 * b11 - a13 * b07) * inv_det,
        (a00 * b11 - a02 * b08 + a03 * b07) * inv_det,
        (a32 * b02 - a30 * b05 - a33 * b01) * inv_det,
        (a20 * b05 - a22 * b02 + a23 * b01) * inv_det,
        (a10 * b10 - a11 * b08 + a13 * b06) * inv_det,
        (a01 * b08 - a00 * b10 - a03 * b06) * inv_det,
        (a30 * b04 - a31 * b02 + a33 * b00) * inv_det,
        (a21 * b02 - a20 * b04 - a23 * b00) * inv_det,
        (a11 * b07 - a10 * b09 - a12 * b06) * inv_det,
        (a00 * b09 - a01 * b07 + a02 * b06) * inv_det,
        (a31 * b01 - a30 * b03 - a32 * b00) * inv_det,
        (a20 * b03 - a21 * b01 + a22 * b00) * inv_det,
    ]
}
