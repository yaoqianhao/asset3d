//! 资产白模审计：把「这一目录下的 `.glb` 到底会不会被画成白模」变成一张可核对的表。
//!
//! 背景：8K 贴图超 `MAX_TEXTURE_PIXELS` 被丢弃时，视口只剩 `baseColorFactor`，
//! 而绝大多数资产的白模就是「贴图没绑上 + baseColor 恰是纯白」。肉眼判断不可靠，
//! 也没有可复跑的证据，所以这里把解析链路真实的中间值全掏出来：
//!
//! 1. **结构层**走 `export_core::import_doc`——与插件导出、GUI 导出同一份解析代码，
//!    给 mesh / material / texture / animation 计数与 `baseColorFactor`；
//! 2. **显示层**走 [`super::scene::load`]——视口真正用来上屏的那条链，给「解码成功的
//!    贴图张数」、降采样记录与非致命 `notes`（贴图解码失败就落在这里）；
//! 3. **像素层**对解码后的 RGBA 做 5×5 抽样，算「非纯白像素占比」与平均亮度：
//!    贴图解码成功但整张纯白（占位图 / 只填了 alpha）同样是白模，结构层看不出来。
//!
//! 两条链都判「无可见颜色」才定白模，避免单点误判：结构层有贴图但显示层解码失败
//! （`notes` 会说原因）也算白——因为用户看到的就是白的。
//!
//! 只读：不写源资产，只在调用方给的报告路径下落 `.md` 与 `.json`。

use std::path::{Path, PathBuf};

use super::scene;

/// 抽样网格边长：25 个像素足够区分「纯白占位图」与「真有内容」，又不至于为了
/// 39 个 8K 资产各扫一遍全图（那是 20 亿次像素比较）。
const SAMPLE_GRID: u32 = 5;

/// 单通道低于它就算「不是纯白」。留 8 的余量给 JPEG 噪点：真纯白贴图解码出来
/// 偶有 254，不能因为差 1 就判成「有颜色」。
const WHITE_CUT: u8 = 247;

/// `baseColorFactor` 三通道都 ≥ 它就算「接近纯白」。
const BASE_WHITE_CUT: f32 = 0.95;

/// 一个资产的审计结果。字段名即报告列名，方便直接照着核对。
#[derive(Debug, Clone)]
pub struct Row {
    /// 相对审计根目录的展示路径（正斜杠）。
    pub rel: String,
    pub bytes: u64,
    /// 结构层：`AirDoc.meshes.len()`。
    pub meshes: usize,
    /// 结构层：`AirDoc.materials.len()`。
    pub materials: usize,
    /// 结构层：`AirDoc.textures.len()`。
    pub textures: usize,
    /// 结构层：`AirDoc.animations.len()`（顺带记录，本轮不判）。
    pub animations: usize,
    /// 所有材质 `base_color` 的 RGB 是否都 ≥ [`BASE_WHITE_CUT`]。
    pub base_white: bool,
    /// 显示层：视口真正解码成功的贴图张数。
    pub decoded_textures: usize,
    /// 像素层：抽样到的最大「非纯白像素占比」（0.0 ~ 1.0）。
    pub nonwhite_ratio: f64,
    /// 抽样到的最小平均亮度（0 ~ 255）。全白贴图 = 255。
    pub mean_luma: f64,
    /// 降采样记录（原图 → 上传尺寸），空表示没有超限贴图。
    pub downscaled: Vec<String>,
    /// 解析非致命问题（贴图丢失 / 解码失败 / 缺 UV 等）。
    pub notes: Vec<String>,
    /// 解析直接失败时的原因（此时其余字段无意义）。
    pub error: Option<String>,
    /// 判定结论。
    pub verdict: Verdict,
}

/// 判定：只有「确实会显示成白」才红，其余分两档便于看清来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 有可见颜色：贴图非全白，或 baseColor 本身有色。
    Colored,
    /// 白模：没有一张可用贴图且 baseColor 接近纯白。
    White,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Colored => "有可见颜色",
            Verdict::White => "可能显示为白",
        }
    }
}

/// `ASSET3D_ASSET_AUDIT` 指定的审计根目录（不是审计形态时为 `None`）。
pub fn dir_from_env() -> Option<PathBuf> {
    std::env::var_os("ASSET3D_ASSET_AUDIT")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// 递归收集 `root` 下指定扩展名的文件（大小写不敏感，按路径排序保证可复跑）。
pub fn collect(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            match item.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let hit = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case(ext))
                        .unwrap_or(false);
                    if hit {
                        out.push(path);
                    }
                }
                _ => {}
            }
        }
    }
    out.sort_by(|a, b| {
        a.to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&b.to_string_lossy().to_ascii_lowercase())
    });
    out
}

/// 审计一个资产：结构层 + 显示层 + 像素层，缺一不可。
pub fn probe(path: &Path, rel: String) -> Row {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut row = Row {
        rel,
        bytes,
        meshes: 0,
        materials: 0,
        textures: 0,
        animations: 0,
        base_white: true,
        decoded_textures: 0,
        nonwhite_ratio: 0.0,
        mean_luma: 255.0,
        downscaled: Vec::new(),
        notes: Vec::new(),
        error: None,
        verdict: Verdict::White,
    };

    // ---- 结构层：与插件/导出同一份解析（`import_doc` 会嗅探容器并完整解析）。
    let doc = match crate::export_core::import_doc(path) {
        Ok(doc) => doc,
        Err(err) => {
            row.error = Some(err);
            row.verdict = Verdict::White;
            return row;
        }
    };
    row.meshes = doc.meshes.len();
    row.materials = doc.materials.len();
    row.textures = doc.textures.len();
    row.animations = doc.animations.len();
    row.notes = doc.issues.clone();
    // baseColorFactor：没有材质时视口用兜底灰蓝，不该算白，所以「无材质」记为不白。
    row.base_white = !doc.materials.is_empty()
        && doc.materials.iter().all(|mat| {
            mat.base_color[0] >= BASE_WHITE_CUT
                && mat.base_color[1] >= BASE_WHITE_CUT
                && mat.base_color[2] >= BASE_WHITE_CUT
        });

    // ---- 显示层：视口真正上屏的那条链（贴图解码 + 超限降采样全在这里）。
    match scene::load(path) {
        Ok(scene) => {
            row.decoded_textures = scene.texture_count;
            row.notes.extend(scene.notes.iter().cloned());
            let mut best: Option<(f64, f64)> = None;
            for part in scene.parts.iter() {
                let Some(tex) = part.material.texture.as_ref() else {
                    continue;
                };
                let (nonwhite, luma) = sample(tex);
                if let Some(from) = tex.downscaled_from {
                    let note = format!(
                        "{}x{}→{}x{}",
                        from.0, from.1, tex.width, tex.height
                    );
                    if !row.downscaled.contains(&note) {
                        row.downscaled.push(note);
                    }
                }
                // 取「最有颜色的一张贴图」代表这个资产：只要有一张能出颜色，
                // 用户就看得见颜色，不该判白。
                best = Some(match best {
                    Some((n, l)) if n >= nonwhite => (n, l),
                    _ => (nonwhite, luma),
                });
            }
            if let Some((nonwhite, luma)) = best {
                row.nonwhite_ratio = nonwhite;
                row.mean_luma = luma;
            }
        }
        Err(err) => {
            // 显示层失败但结构层成功：极少数资产（例如外部贴图丢失后 glTF 严格校验
            // 报错）会走到这里，把原因记下来，判定按「白」处理并在此说明。
            row.notes.push(format!("视口解析失败：{err}"));
        }
    }

    row.verdict = if row.decoded_textures == 0 {
        if row.base_white {
            Verdict::White
        } else {
            Verdict::Colored
        }
    } else if row.nonwhite_ratio <= 0.0 {
        if row.base_white {
            Verdict::White
        } else {
            Verdict::Colored
        }
    } else {
        Verdict::Colored
    };
    row
}

/// 5×5 抽样：返回（非纯白像素占比，平均亮度）。
///
/// 「非纯白」= RGB 任一通道 < [`WHITE_CUT`]。alpha 不参与判定——很多资产把
/// 透明度烘在贴图里，全透明但 RGB 有内容的贴图仍然算有颜色。
fn sample(tex: &scene::TexImage) -> (f64, f64) {
    if tex.width == 0 || tex.height == 0 || tex.rgba.len() < 4 {
        return (0.0, 255.0);
    }
    let mut nonwhite = 0usize;
    let mut total = 0usize;
    let mut luma_sum = 0.0f64;
    for gy in 0..SAMPLE_GRID {
        for gx in 0..SAMPLE_GRID {
            // 取格子中心：贴图边缘常有 1 像素的白边，居中取样更能代表内容。
            let x = ((gx * 2 + 1) * tex.width / (SAMPLE_GRID * 2)).min(tex.width - 1);
            let y = ((gy * 2 + 1) * tex.height / (SAMPLE_GRID * 2)).min(tex.height - 1);
            let idx = ((y * tex.width + x) * 4) as usize;
            if idx + 2 >= tex.rgba.len() {
                continue;
            }
            let (r, g, b) = (tex.rgba[idx], tex.rgba[idx + 1], tex.rgba[idx + 2]);
            total += 1;
            if r < WHITE_CUT || g < WHITE_CUT || b < WHITE_CUT {
                nonwhite += 1;
            }
            luma_sum += 0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64;
        }
    }
    if total == 0 {
        return (0.0, 255.0);
    }
    (nonwhite as f64 / total as f64, luma_sum / total as f64)
}

/// 跑一次完整审计：收集 → 逐个 probe → 落 `.md` 与 `.json` → 打一份摘要到 stderr。
///
/// 退出码：0 全部有颜色；4 存在白模资产（便于 CI / 脚本据此判定）。
pub fn run(root: &Path, report: &Path) -> i32 {
    let files = collect(root, "glb");
    let root_str = root.to_string_lossy().replace('\\', "/");
    eprintln!(
        "asset3d 审计：{} 下 {} 个 .glb",
        root.display(),
        files.len()
    );
    let mut rows = Vec::with_capacity(files.len());
    for (i, file) in files.iter().enumerate() {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let row = probe(file, rel);
        eprintln!(
            "  [{}/{}] {} → {}",
            i + 1,
            files.len(),
            row.rel,
            row.verdict.label()
        );
        rows.push(row);
    }

    if let Err(err) = write_markdown(report, root, &rows) {
        eprintln!("asset3d 审计：写报告失败：{err}");
        return 1;
    }
    let json_path = report.with_extension("json");
    if let Err(err) = write_json(&json_path, &rows) {
        eprintln!("asset3d 审计：写 JSON 失败：{err}");
        return 1;
    }

    let white = rows
        .iter()
        .filter(|row| row.verdict == Verdict::White)
        .count();
    eprintln!(
        "asset3d 审计：{} 个资产，白模 {} 个；报告 {} / {}",
        rows.len(),
        white,
        report.display(),
        json_path.display()
    );
    // 机器可读摘要：调用方拿这段判断结果，不用再解析 Markdown。
    println!(
        "{{\"root\":\"{}\",\"total\":{},\"white\":{},\"report\":\"{}\",\"json\":\"{}\"}}",
        json_escape(&root_str),
        rows.len(),
        white,
        json_escape(&report.display().to_string()),
        json_escape(&json_path.display().to_string())
    );
    if white == 0 {
        0
    } else {
        4
    }
}

fn write_markdown(target: &Path, root: &Path, rows: &[Row]) -> Result<(), String> {
    let mut out = String::new();
    out.push_str("# asset3d 白模审计\n\n");
    out.push_str(&format!("- 审计根目录：`{}`\n", root.display()));
    out.push_str(&format!("- 资产数：{}（递归 `*.glb`）\n", rows.len()));
    let white = rows.iter().filter(|r| r.verdict == Verdict::White).count();
    out.push_str(&format!("- 判为白模：{white}\n"));
    out.push_str(&format!(
        "- 判定口径：贴图解码成功且 5×5 抽样存在非纯白像素（任一通道 < {WHITE_CUT}）→ 有可见颜色；\
         贴图全白或无贴图且 `baseColorFactor` 三通道 ≥ {BASE_WHITE_CUT} → 可能显示为白\n\n"
    ));
    out.push_str("| 文件名 | 字节数 | meshes | materials | textures | baseColor≈纯白 | 贴图非全白（抽样） | 判定 |\n");
    out.push_str("|---|---:|---:|---:|---:|---|---|---|\n");
    for row in rows {
        let nonwhite = if row.decoded_textures == 0 {
            "无解码贴图".to_string()
        } else {
            format!("{:.0}%（均值 {:.0}）", row.nonwhite_ratio * 100.0, row.mean_luma)
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.rel,
            row.bytes,
            row.meshes,
            row.materials,
            row.textures,
            if row.base_white { "是" } else { "否" },
            nonwhite,
            row.verdict.label()
        ));
    }
    // 明细：只把有话说（降采样 / notes / 报错 / 白模）的资产列出来，其余一行带过。
    out.push_str("\n## 明细\n\n");
    for row in rows {
        let interesting = row.verdict == Verdict::White
            || !row.downscaled.is_empty()
            || !row.notes.is_empty()
            || row.error.is_some();
        if !interesting {
            continue;
        }
        out.push_str(&format!("### {}（{}）\n\n", row.rel, row.verdict.label()));
        out.push_str(&format!(
            "- 结构：meshes {} / materials {} / textures {} / animations {}\n",
            row.meshes, row.materials, row.textures, row.animations
        ));
        out.push_str(&format!(
            "- 显示：解码贴图 {} 张，抽样非纯白 {:.0}%，均值亮度 {:.0}\n",
            row.decoded_textures,
            row.nonwhite_ratio * 100.0,
            row.mean_luma
        ));
        if let Some(err) = &row.error {
            out.push_str(&format!("- 解析失败：{err}\n"));
        }
        for note in &row.downscaled {
            out.push_str(&format!("- 降采样：{note}\n"));
        }
        for note in &row.notes {
            out.push_str(&format!("- 提示：{note}\n"));
        }
        out.push('\n');
    }
    std::fs::write(target, out).map_err(|err| format!("{}：{err}", target.display()))
}

fn write_json(target: &Path, rows: &[Row]) -> Result<(), String> {
    let mut out = String::from("{\n  \"assets\": [\n");
    for (i, row) in rows.iter().enumerate() {
        out.push_str("    {");
        out.push_str(&format!("\"rel\":\"{}\",", json_escape(&row.rel)));
        out.push_str(&format!("\"bytes\":{},", row.bytes));
        out.push_str(&format!("\"meshes\":{},", row.meshes));
        out.push_str(&format!("\"materials\":{},", row.materials));
        out.push_str(&format!("\"textures\":{},", row.textures));
        out.push_str(&format!("\"animations\":{},", row.animations));
        out.push_str(&format!("\"base_white\":{},", row.base_white));
        out.push_str(&format!("\"decoded_textures\":{},", row.decoded_textures));
        out.push_str(&format!(
            "\"nonwhite_ratio\":{:.4},",
            row.nonwhite_ratio
        ));
        out.push_str(&format!("\"mean_luma\":{:.2},", row.mean_luma));
        out.push_str(&format!("\"verdict\":\"{}\",", json_escape(row.verdict.label())));
        out.push_str("\"downscaled\":[");
        for (j, note) in row.downscaled.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{}\"", json_escape(note)));
        }
        out.push_str("],\"notes\":[");
        for (j, note) in row.notes.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{}\"", json_escape(note)));
        }
        out.push_str("],\"error\":");
        match &row.error {
            Some(err) => out.push_str(&format!("\"{}\"", json_escape(err))),
            None => out.push_str("null"),
        }
        out.push('}');
        if i + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    std::fs::write(target, out).map_err(|err| format!("{}：{err}", target.display()))
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tex(width: u32, height: u32, rgba: Vec<u8>) -> scene::TexImage {
        scene::TexImage {
            width,
            height,
            rgba,
            downscaled_from: None,
        }
    }

    #[test]
    fn pure_white_texture_has_no_nonwhite_pixels() {
        let t = tex(4, 4, vec![255; 4 * 4 * 4]);
        let (nonwhite, luma) = sample(&t);
        assert_eq!(nonwhite, 0.0);
        assert!((luma - 255.0).abs() < 0.5);
    }

    #[test]
    fn colored_texture_is_detected() {
        // 每像素都带色：抽样网格落在哪个格子都该判成非白。
        let mut rgba = Vec::new();
        for _ in 0..(8 * 8) {
            rgba.extend_from_slice(&[200, 40, 30, 255]);
        }
        let (nonwhite, _) = sample(&tex(8, 8, rgba));
        assert!((nonwhite - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_pixels_do_not_panic() {
        let (nonwhite, luma) = sample(&tex(0, 0, Vec::new()));
        assert_eq!(nonwhite, 0.0);
        assert_eq!(luma, 255.0);
    }

    #[test]
    fn collect_is_recursive_and_sorted() {
        let root = std::env::temp_dir().join(format!(
            "asset3d-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("b.glb"), b"x").unwrap();
        std::fs::write(root.join("sub").join("a.GLB"), b"x").unwrap();
        std::fs::write(root.join("sub").join("c.fbx"), b"x").unwrap();
        let found = collect(&root, "glb");
        let names: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(names, vec!["b.glb", "sub/a.GLB"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
