//! 导出核心：AIR 二进制 / JSON 编码、回读自检、引擎加载器模板渲染。
//!
//! 这里的实现**同时**被两条路径调用：
//!
//! - 插件工具（`tools.rs` 的 `export_asset` / `export_engine_loader`，走宿主 `host/fs.write`）；
//! - 视口进程（`asset3d-gui` 的导出按键，走本地 `std::fs`）。
//!
//! 两边共用同一份编码与自检代码，写出的字节因此逐比特一致——这也是「不要在 GUI 里
//! 复制一份格式实现」的落点：格式只在这里定义一次，任何一侧要改格式都必须改这里。

use std::path::{Path, PathBuf};

use crate::air::AirDoc;
use crate::air_bin;
use crate::util::{self, Container};

/// 一次导出实际落盘的文件：绝对路径 + 字节数。
#[derive(Debug, Clone)]
pub struct Exported {
    pub paths: Vec<(PathBuf, u64)>,
}

impl Exported {
    pub fn summary(&self) -> String {
        self.paths
            .iter()
            .map(|(path, size)| format!("{}（{}）", path.display(), util::format_bytes(*size)))
            .collect::<Vec<_>>()
            .join("；")
    }
}

/// 读盘并解析成 AIR。口径与插件 `export_asset` 的导入一致：
/// 带动画、内嵌贴图（导出的 AIR 要能独立被引擎读，不能指望引擎回工作区找贴图）。
pub fn import_doc(source: &Path) -> Result<AirDoc, String> {
    let bytes = std::fs::read(source)
        .map_err(|err| format!("读取 {} 失败：{err}", source.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} 是空文件", source.display()));
    }
    // 路径统一成正斜杠：`sniff_container` 与 glTF 的相对引用都按它切目录。
    let hint = source.to_string_lossy().replace('\\', "/");
    let container = util::sniff_container(&hint, &bytes)?;
    let mut doc = AirDoc::new(container.as_str(), &hint);
    match container {
        Container::FbxBinary | Container::FbxAscii => {
            crate::fbx::import(&mut doc, &bytes, &hint, true, true)?;
        }
        Container::Glb | Container::Gltf => {
            // glTF 的外部 buffer / 贴图相对源文件目录；视口有真实磁盘权限，
            // 直接按目录拼接即可（`join` 遇到绝对路径会自动替换 base）。
            let base: PathBuf = source.parent().map(Path::to_path_buf).unwrap_or_default();
            let mut fetch = |rel: &str| -> Option<Vec<u8>> {
                let candidate = base.join(rel);
                std::fs::read(&candidate).ok()
            };
            crate::gltf::import(&mut doc, &bytes, &hint, true, true, &mut fetch)?;
        }
    }
    doc.finalize();
    Ok(doc)
}

/// 编码 AIRB 并回读自检，返回可直接落盘的字节。
///
/// 自检放在**写盘之前**：布局写错在这里就能发现，不会先产出一个坏文件再解释。
pub fn encode_bin(doc: &AirDoc) -> Result<Vec<u8>, String> {
    let encoded = air_bin::encode(doc);
    let check = air_bin::decode(&encoded.bytes)
        .map_err(|err| format!("AIRB 自检失败（不会写出坏文件）：{err}"))?;
    verify(doc, &check)?;
    Ok(encoded.bytes)
}

/// 编码 AIR JSON（pretty）并回读自检，返回可直接落盘的文本。
pub fn encode_json(doc: &AirDoc) -> Result<String, String> {
    let text = serde_json::to_string_pretty(doc)
        .map_err(|err| format!("序列化 JSON 失败：{err}"))?;
    let check: AirDoc = serde_json::from_str(&text)
        .map_err(|err| format!("AIR JSON 自检失败：{err}"))?;
    verify(doc, &check)?;
    Ok(text)
}

/// 一步导出：解析 → 阻断级问题检查 → 按 `format`（bin / json / both）编码 → 写盘。
///
/// `out_bin` / `out_json` 是调用方已经选定的目标路径；`format` 决定实际写哪几个。
pub fn export_air(
    source: &Path,
    format: &str,
    out_bin: Option<&Path>,
    out_json: Option<&Path>,
) -> Result<Exported, String> {
    if !matches!(format, "bin" | "json" | "both") {
        return Err(format!("format 只能是 bin / json / both，收到 `{format}`"));
    }
    let doc = import_doc(source)?;
    // 阻断级问题一律不落盘：写出「能读但画不对」的资产，比直接报错难查一个数量级。
    if doc.has_errors() {
        return Err(format!(
            "{} 存在 {} 条阻断级问题，已拒绝写出 AIR：\n{}",
            source.display(),
            doc.errors.len(),
            doc.error_report()
        ));
    }
    let mut paths: Vec<(PathBuf, u64)> = Vec::new();
    if format == "bin" || format == "both" {
        let target = out_bin.ok_or_else(|| "缺少 .air.bin 目标路径".to_string())?;
        let bytes = encode_bin(&doc)?;
        write_bytes(target, &bytes)?;
        paths.push((target.to_path_buf(), bytes.len() as u64));
    }
    if format == "json" || format == "both" {
        let target = out_json.ok_or_else(|| "缺少 .air.json 目标路径".to_string())?;
        let text = encode_json(&doc)?;
        write_bytes(target, text.as_bytes())?;
        paths.push((target.to_path_buf(), text.len() as u64));
    }
    Ok(Exported { paths })
}

/// 把引擎加载器源码写到 `target`，返回字节数。
pub fn export_loader(target: &Path) -> Result<u64, String> {
    let source = render_loader()?;
    write_bytes(target, source.as_bytes())?;
    Ok(source.len() as u64)
}

fn write_bytes(target: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("创建目录 {} 失败：{err}", parent.display()))?;
        }
    }
    std::fs::write(target, bytes).map_err(|err| format!("写入 {} 失败：{err}", target.display()))
}

// ---------------------------------------------------------------------------
// 回读自检
// ---------------------------------------------------------------------------

/// 导出后的回读自检：结构、抽样数值、AABB 重算三层。
///
/// 只比长度抓不到「字段布局错位把 x 读成 y」这类错误——长度分毫不差，画面却全歪。
/// 所以每个网格再抽首/中/末三个顶点逐分量比位置、法线、UV、权重，索引比首尾，
/// 最后用回读的 positions 重算 AABB 和文件里存的包围盒对照。
pub fn verify(expected: &AirDoc, actual: &AirDoc) -> Result<(), String> {
    if expected.meshes.len() != actual.meshes.len() {
        return Err(format!(
            "自检失败：网格数 {} → {}",
            expected.meshes.len(),
            actual.meshes.len()
        ));
    }
    if expected.nodes.len() != actual.nodes.len()
        || expected.materials.len() != actual.materials.len()
        || expected.textures.len() != actual.textures.len()
        || expected.skins.len() != actual.skins.len()
        || expected.animations.len() != actual.animations.len()
    {
        return Err(format!(
            "自检失败：结构长度不一致（节点 {}→{}、材质 {}→{}、贴图 {}→{}、蒙皮 {}→{}、动画 {}→{}）",
            expected.nodes.len(),
            actual.nodes.len(),
            expected.materials.len(),
            actual.materials.len(),
            expected.textures.len(),
            actual.textures.len(),
            expected.skins.len(),
            actual.skins.len(),
            expected.animations.len(),
            actual.animations.len()
        ));
    }
    for (i, (a, b)) in expected.meshes.iter().zip(actual.meshes.iter()).enumerate() {
        if a.positions.len() != b.positions.len() || a.indices.len() != b.indices.len() {
            return Err(format!(
                "自检失败：网格 #{i} 顶点 {} → {}、索引 {} → {}",
                a.positions.len(),
                b.positions.len(),
                a.indices.len(),
                b.indices.len()
            ));
        }
        if a.joints.len() != b.joints.len() || a.weights.len() != b.weights.len() {
            return Err(format!("自检失败：网格 #{i} 的关节/权重长度不一致"));
        }
        // 抽样逐分量比对：首、中、末三个顶点足以抓住整体错位，又不用遍历全量。
        for probe in sample_indices(a.positions.len()) {
            // 位置用位模式比：回读必须是逐比特还原，带容差反而会漏掉低位写坏。
            let same_position = a.positions[probe]
                .iter()
                .map(|v| v.to_bits())
                .eq(b.positions[probe].iter().map(|v| v.to_bits()));
            if !same_position {
                return Err(format!(
                    "自检失败：网格 #{i} 顶点 #{probe} 位置不一致 {:?} → {:?}",
                    a.positions[probe], b.positions[probe]
                ));
            }
            if let (Some(na), Some(nb)) = (a.normals.get(probe), b.normals.get(probe)) {
                if !approx_eq3(na, nb, 1e-6) {
                    return Err(format!(
                        "自检失败：网格 #{i} 顶点 #{probe} 法线不一致 {na:?} → {nb:?}"
                    ));
                }
            }
            if let (Some(ua), Some(ub)) = (a.uvs.get(probe), b.uvs.get(probe)) {
                if !approx_eq2(ua, ub, 1e-6) {
                    return Err(format!(
                        "自检失败：网格 #{i} 顶点 #{probe} UV 不一致 {ua:?} → {ub:?}"
                    ));
                }
            }
            if let (Some(wa), Some(wb)) = (a.weights.get(probe), b.weights.get(probe)) {
                if !approx_eq4(wa, wb, 1e-5) {
                    return Err(format!(
                        "自检失败：网格 #{i} 顶点 #{probe} 权重不一致 {wa:?} → {wb:?}"
                    ));
                }
            }
            if let (Some(ja), Some(jb)) = (a.joints.get(probe), b.joints.get(probe)) {
                if ja != jb {
                    return Err(format!(
                        "自检失败：网格 #{i} 顶点 #{probe} 关节索引不一致 {ja:?} → {jb:?}"
                    ));
                }
            }
        }
        // 索引比首尾：索引用 `u32` 定长写，错位会直接表现成首尾元素对不上。
        if a.indices.first() != b.indices.first() || a.indices.last() != b.indices.last() {
            return Err(format!(
                "自检失败：网格 #{i} 索引首尾不一致 {:?}/{:?} → {:?}/{:?}",
                a.indices.first(),
                a.indices.last(),
                b.indices.first(),
                b.indices.last()
            ));
        }
        // AABB 重算：包围盒是导出时算好写死的，回读后用顶点重算一遍才能真正对上。
        let (min, max) = recompute_aabb(&b.positions);
        if !approx_eq3(&min, &b.aabb_min, 1e-4) || !approx_eq3(&max, &b.aabb_max, 1e-4) {
            return Err(format!(
                "自检失败：网格 #{i} AABB 与顶点不符（重算 {min:?}~{max:?}，文件存 {:?}~{:?}）",
                b.aabb_min, b.aabb_max
            ));
        }
    }
    // 蒙皮：关节表逐项比，IBM 抽首/末两个矩阵（单次导出最多几十根骨，抽检够用）。
    for (i, (a, b)) in expected.skins.iter().zip(actual.skins.iter()).enumerate() {
        if a.joints != b.joints {
            return Err(format!(
                "自检失败：蒙皮 #{i} 关节表不一致（{} → {} 项）",
                a.joints.len(),
                b.joints.len()
            ));
        }
        if a.inverse_bind_matrices.len() != b.inverse_bind_matrices.len() {
            return Err(format!(
                "自检失败：蒙皮 #{i} IBM 数量 {} → {}",
                a.inverse_bind_matrices.len(),
                b.inverse_bind_matrices.len()
            ));
        }
        for probe in sample_indices(a.inverse_bind_matrices.len()) {
            let (ma, mb) = (&a.inverse_bind_matrices[probe], &b.inverse_bind_matrices[probe]);
            if !approx_eq16(ma, mb, 1e-5) {
                return Err(format!(
                    "自检失败：蒙皮 #{i} 第 {probe} 个 IBM 不一致 {ma:?} → {mb:?}"
                ));
            }
        }
    }
    Ok(())
}

/// 抽样下标：首、中、末（去重后返回，空集返回空）。
fn sample_indices(len: usize) -> Vec<usize> {
    match len {
        0 => Vec::new(),
        1 => vec![0],
        2 => vec![0, 1],
        _ => vec![0, len / 2, len - 1],
    }
}

fn approx_eq3(a: &[f32; 3], b: &[f32; 3], tol: f32) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| (x - y).abs() <= tol * x.abs().max(1.0))
}

fn approx_eq2(a: &[f32; 2], b: &[f32; 2], tol: f32) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| (x - y).abs() <= tol * x.abs().max(1.0))
}

fn approx_eq4(a: &[f32; 4], b: &[f32; 4], tol: f32) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| (x - y).abs() <= tol * x.abs().max(1.0))
}

fn approx_eq16(a: &[f32; 16], b: &[f32; 16], tol: f32) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| (x - y).abs() <= tol * x.abs().max(1.0))
}

/// 用顶点重算 AABB；空网格回零盒（与导出侧口径一致）。
fn recompute_aabb(positions: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    if positions.is_empty() {
        return ([0.0; 3], [0.0; 3]);
    }
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in positions {
        for axis in 0..3 {
            min[axis] = min[axis].min(p[axis]);
            max[axis] = max[axis].max(p[axis]);
        }
    }
    (min, max)
}

// ---------------------------------------------------------------------------
// 引擎加载器模板渲染
// ---------------------------------------------------------------------------

/// 加载器模板里的四个占位符——全部都要替换，少一个就不许落盘。
pub const LOADER_PLACEHOLDERS: [&str; 4] = [
    "__GENERATED_FROM__",
    "__GENERATED_AT__",
    "__AIR_VERSION__",
    "__AIRB_VERSION__",
];

/// 渲染 `assets/air_loader.rs.txt`：占位符全量替换 + 残留兜底。
///
/// 返回 `Err` 的两种情形都是**模板自身缺陷**，宁可报错，也不要把带 `__XXX__`
/// 的源码写进用户的工程里。
pub fn render_loader() -> Result<String, String> {
    let template = include_str!("../assets/air_loader.rs.txt");
    let air_version = crate::air::AIR_VERSION;
    let airb_version = air_bin::VERSION;

    let mut source = template.to_string();
    let rendered = [
        (
            LOADER_PLACEHOLDERS[0],
            format!("asset3d 插件 export_engine_loader｜AIR v{air_version} / AIRB v{airb_version}"),
        ),
        (LOADER_PLACEHOLDERS[1], utc_timestamp_now()),
        (LOADER_PLACEHOLDERS[2], air_version.to_string()),
        (LOADER_PLACEHOLDERS[3], airb_version.to_string()),
    ];
    for (token, value) in rendered {
        source = source.replace(token, &value);
    }

    if let Some(token) = find_placeholder(&source) {
        return Err(format!(
            "加载器模板存在未替换的占位符 {token}：请修 assets/air_loader.rs.txt 或 render_loader"
        ));
    }
    Ok(source)
}

/// 在文本里找 `__LIKE_THIS__` 形状的残留占位符。
///
/// 只认「`__` + 全大写/数字/下划线 + `__`」，避免把普通文本里的 `__` 误判成占位符。
pub fn find_placeholder(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'_' && bytes[i + 1] == b'_' {
            let start = i;
            let mut j = i + 2;
            loop {
                // 先看是不是闭合的 "__"：位置必须晚于名字的至少一个字符。
                if j + 1 < bytes.len() && bytes[j] == b'_' && bytes[j + 1] == b'_' {
                    if j > start + 2 {
                        return Some(text[start..j + 2].to_string());
                    }
                    break;
                }
                // 名字段只允许大写字母、数字、下划线。
                if j >= bytes.len()
                    || !(bytes[j].is_ascii_uppercase()
                        || bytes[j].is_ascii_digit()
                        || bytes[j] == b'_')
                {
                    break;
                }
                j += 1;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

/// 当前 UTC 时间，格式 `YYYY-MM-DD HH:MM:SS UTC`。
///
/// 只为一行注释引入 chrono 不值得，这里直接用 `SystemTime` + 公历换算。
pub fn utc_timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_unix_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Unix 天数 → (年, 月, 日)，Howard Hinnant 的 civil_from_days 算法。
pub fn civil_from_unix_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}
