//! 工作区资产清单：把「工作区根」扫成一份可点击的相对路径列表。
//!
//! 三条设计约束：
//!
//! 1. **只在后台线程里扫**——`D:\share` 这种目录动辄几万项，主线程扫一遍窗口
//!    直接白屏，与「UI 不能卡死」的验收项冲突；
//! 2. **有上限**：目录深度与条目数各有一道上限，超大工作区宁可截断也不能把
//!    内存吃满（截断状态由 [`Scan::truncated`] 显式报给 UI，不静默吞掉）；
//! 3. **只认三种资产**：`.glb` / `.gltf` / `.fbx`，与导入对话框、拖放判定同一口径。

use std::path::{Path, PathBuf};

/// 一次扫描最多收集多少个资产。够大到覆盖真实工作区，小到不会拖垮内存。
pub const MAX_ENTRIES: usize = 4000;

/// 向下递归的最大层数。工作区里嵌套七八层的资产已经很少见，再深基本都是
/// `node_modules` 那种垃圾。
pub const MAX_DEPTH: usize = 8;

/// 一律不往下的目录：构建产物与包管理器缓存里不会有资产，但文件数最多。
const SKIP_DIRS: [&str; 8] = [
    "node_modules",
    "target",
    "dist",
    "release",
    "debug",
    ".git",
    ".svn",
    ".cache",
];

/// 一条资产条目：绝对路径 + 相对工作区根的展示路径（正斜杠）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub rel: String,
}

/// 一次扫描的结果。
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub entries: Vec<Entry>,
    /// 条目数触到 [`MAX_ENTRIES`] 被截断——UI 必须把这句提示出来。
    pub truncated: bool,
}

/// 扩展名判定：大小写不敏感，且只认 `.glb` / `.gltf` / `.fbx`。
pub fn is_asset(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => matches!(
            ext.to_ascii_lowercase().as_str(),
            "glb" | "gltf" | "fbx"
        ),
        None => false,
    }
}

/// 相对工作区根的展示路径，统一成正斜杠。
///
/// 路径不在根下（`strip_prefix` 失败）时退回原路径：显示一个绝对路径，
/// 总比显示空串让人以为文件在一个叫「」的目录里强。
pub fn rel_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 扫描工作区根，返回按相对路径排序的资产清单。
///
/// 用显式栈做广度优先而不是递归：目录树再深也不会把栈撑爆，深度上限也好控制。
pub fn scan(root: &Path) -> Result<Scan, String> {
    if !root.is_dir() {
        return Err(format!(
            "工作区根不存在或不是目录：{}",
            root.display()
        ));
    }
    let mut out: Vec<Entry> = Vec::new();
    let mut truncated = false;
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        // 单个目录读不了（权限、被删）只跳过它，不放弃整次扫描。
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            // 隐藏项（`.git` 之类）一律不进：用户不会把资产藏在那里。
            if name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = item.file_type() else {
                continue;
            };
            let path = item.path();
            if file_type.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    queue.push((path, depth + 1));
                }
            } else if file_type.is_file() && is_asset(&path) {
                if out.len() >= MAX_ENTRIES {
                    truncated = true;
                    break;
                }
                out.push(Entry {
                    rel: rel_string(root, &path),
                    path,
                });
            }
        }
        if truncated {
            break;
        }
    }
    // 按相对路径排序：面板里同一目录的文件挨在一起，找东西不用来回扫。
    out.sort_by(|a, b| {
        a.rel
            .to_ascii_lowercase()
            .cmp(&b.rel.to_ascii_lowercase())
    });
    Ok(Scan {
        entries: out,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "asset3d-ws-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn asset_extension_is_case_insensitive() {
        assert!(is_asset(Path::new("a.glb")));
        assert!(is_asset(Path::new("a.GLB")));
        assert!(is_asset(Path::new("a.gltf")));
        assert!(is_asset(Path::new("a.Fbx")));
        assert!(!is_asset(Path::new("a.png")));
        assert!(!is_asset(Path::new("a")));
    }

    #[test]
    fn rel_path_uses_forward_slashes() {
        let root = Path::new(r"D:\ws");
        let path = Path::new(r"D:\ws\sub\a.glb");
        assert_eq!(rel_string(root, path), "sub/a.glb");
        // 不在根下的路径不 panic，退回原样。
        assert!(rel_string(root, Path::new(r"E:\x\a.glb")).contains("a.glb"));
    }

    #[test]
    fn scan_is_recursive_and_sorted_and_skips_noise() {
        let root = temp_dir("scan");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("b.glb"), b"x").unwrap();
        std::fs::write(root.join("sub").join("a.FBX"), b"x").unwrap();
        std::fs::write(root.join("sub").join("note.txt"), b"x").unwrap();
        std::fs::write(root.join("node_modules").join("skip.glb"), b"x").unwrap();
        std::fs::write(root.join(".hidden").join("skip.glb"), b"x").unwrap();

        let scan = scan(&root).unwrap();
        let rels: Vec<&str> = scan.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, vec!["b.glb", "sub/a.FBX"]);
        assert!(!scan.truncated);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_missing_root_reports_error() {
        assert!(scan(Path::new(r"D:\definitely-not-exists-asset3d")).is_err());
    }
}
