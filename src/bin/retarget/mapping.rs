//! UE（Unreal Engine Mannequin）→ Tripo 人形绑定的骨骼映射表。
//!
//! 表是「内置默认 + `--map` 覆盖」两层：内置表保证开箱即用，`--map` 里同名的条目
//! 直接顶掉内置（改线），新名字则追加（补漏）。
//!
//! 命名差异是这张表存在的原因：
//! - UE 用小写下划线（`upperarm_l` / `thigh_r` / `spine_02`），Tripo 用大驼峰 + 段号
//!   （`L_Upperarm` / `L_Thigh` / `Spine02`）；
//! - UE 的 `pelvis` 是「髋部根」，两条腿和脊柱都从它分叉，所以对应 Tripo 的 `Hip`
//!   （Tripo 的 `Hip` 下面同样分出 `Pelvis`→腿 与 `Waist`→脊柱 两支）；Tripo 的
//!   `Pelvis` 只是腿部那一支的中转节点，源侧没有对应骨，保持 rest 即可；
//! - UE 三节脊柱 `spine_01..03` 对应 Tripo `Waist` / `Spine01` / `Spine02`，
//!   正好把脊柱三节接到 `Spine02`（它下面就是脖子与锁骨）；
//! - UE 的 `clavicle` / `lowerarm` / `ball` 分别对应 `Clavicle` / `Forearm` / `ToeBase`；
//! - 两侧的 `*Twist01/02` 是 Tripo 防止蒙皮打滑的辅助骨，UE 也有对应 twist，
//!   但 UE 每段只有 `twist_01`，所以 `*Twist02` 永远缺席（保持 rest 跟随父骨）；
//!   `--no-twist` 时整组不映射。

use std::collections::BTreeMap;
use std::path::Path;

/// 内置映射：(源骨骼名, 目标骨骼名, 可选?)。源名一律小写比较。
pub const BUILTIN: &[(&str, &str, bool)] = &[
    // 根与躯干
    ("root", "Root", false),
    ("pelvis", "Hip", false),
    ("spine_01", "Waist", false),
    ("spine_02", "Spine01", false),
    ("spine_03", "Spine02", false),
    ("neck_01", "NeckTwist01", false),
    ("neck_02", "NeckTwist02", true),
    ("head", "Head", false),
    // 左臂
    ("clavicle_l", "L_Clavicle", false),
    ("upperarm_l", "L_Upperarm", false),
    ("upperarm_twist_01_l", "L_UpperarmTwist01", true),
    ("upperarm_twist_02_l", "L_UpperarmTwist02", true),
    ("lowerarm_l", "L_Forearm", false),
    ("lowerarm_twist_01_l", "L_ForearmTwist01", true),
    ("lowerarm_twist_02_l", "L_ForearmTwist02", true),
    ("hand_l", "L_Hand", false),
    // 右臂
    ("clavicle_r", "R_Clavicle", false),
    ("upperarm_r", "R_Upperarm", false),
    ("upperarm_twist_01_r", "R_UpperarmTwist01", true),
    ("upperarm_twist_02_r", "R_UpperarmTwist02", true),
    ("lowerarm_r", "R_Forearm", false),
    ("lowerarm_twist_01_r", "R_ForearmTwist01", true),
    ("lowerarm_twist_02_r", "R_ForearmTwist02", true),
    ("hand_r", "R_Hand", false),
    // 左腿
    ("thigh_l", "L_Thigh", false),
    ("thigh_twist_01_l", "L_ThighTwist01", true),
    ("thigh_twist_02_l", "L_ThighTwist02", true),
    ("calf_l", "L_Calf", false),
    ("calf_twist_01_l", "L_CalfTwist01", true),
    ("calf_twist_02_l", "L_CalfTwist02", true),
    ("foot_l", "L_Foot", false),
    ("ball_l", "L_ToeBase", false),
    // 右腿
    ("thigh_r", "R_Thigh", false),
    ("thigh_twist_01_r", "R_ThighTwist01", true),
    ("thigh_twist_02_r", "R_ThighTwist02", true),
    ("calf_r", "R_Calf", false),
    ("calf_twist_01_r", "R_CalfTwist01", true),
    ("calf_twist_02_r", "R_CalfTwist02", true),
    ("foot_r", "R_Foot", false),
    ("ball_r", "R_ToeBase", false),
];

/// `--map` 里的一条覆盖/追加。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapOverride {
    pub source: String,
    pub target: String,
}

/// 合并后的映射表。
pub struct BoneMap {
    /// 小写源名 → 目标骨名
    table: BTreeMap<String, String>,
    /// 目标骨名 → 小写源名（反查，`--map` 覆盖时要先把旧的反向条目清掉）
    reverse: BTreeMap<String, String>,
    /// 来自 `--map` 的源名（报告里要区分「内置」和「外部覆盖」）
    overridden: Vec<String>,
}

impl BoneMap {
    pub fn new(overrides: &[MapOverride], no_twist: bool) -> Self {
        let mut map = Self {
            table: BTreeMap::new(),
            reverse: BTreeMap::new(),
            overridden: Vec::new(),
        };
        for (src, dst, optional) in BUILTIN {
            if no_twist && (src.contains("twist") || dst.contains("Twist")) {
                continue;
            }
            map.insert(src, dst, false);
            let _ = optional;
        }
        for item in overrides {
            if no_twist && (item.source.to_lowercase().contains("twist") || item.target.contains("Twist"))
            {
                continue;
            }
            // 覆盖同名源：先把它的旧目标反查项清掉
            let src = item.source.to_lowercase();
            if let Some(old) = map.table.get(&src).cloned() {
                map.reverse.remove(&old);
            }
            map.insert(&item.source, &item.target, true);
        }
        map
    }

    fn insert(&mut self, src: &str, dst: &str, from_map: bool) {
        let src = src.to_lowercase();
        let dst = dst.to_string();
        if let Some(old) = self.table.get(&src).cloned() {
            if old != dst {
                self.reverse.remove(&old);
            }
        }
        if from_map && !self.overridden.contains(&src) {
            self.overridden.push(src.clone());
        }
        self.reverse.insert(dst.clone(), src.clone());
        self.table.insert(src, dst);
    }

    /// 查映射。源名大小写不敏感，先原样再小写（UE 导出有时会大写首字母）。
    pub fn get(&self, source: &str) -> Option<&str> {
        self.table
            .get(source)
            .or_else(|| self.table.get(&source.to_lowercase()))
            .map(String::as_str)
    }

    /// 目标骨是否已被某个源骨占用。
    pub fn is_from_map(&self, source: &str) -> bool {
        self.overridden.contains(&source.to_lowercase())
    }

    /// 反查：目标骨名 → 源骨名。只有测试和排查用得上，正式流程不需要。
    #[cfg(test)]
    pub fn reverse_get(&self, target: &str) -> Option<&str> {
        self.reverse.get(target).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

}

/// 读 `--map` 的 JSON。支持两种写法：
///
/// ```json
/// { "spine_04": "Spine02", "pelvis": "Pelvis" }
/// ```
/// ```json
/// [ {"source": "spine_04", "target": "Spine02"} ]
/// ```
pub fn load_map_file(path: &Path) -> Result<Vec<MapOverride>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("读不到映射文件 `{}`：{err}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("映射文件 `{}` 不是合法 JSON：{err}", path.display()))?;
    let mut out = Vec::new();
    match value {
        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                let target = v.as_str().ok_or_else(|| {
                    format!("映射文件里 \"{k}\" 的值必须是字符串（目标骨骼名），实际是 {v}")
                })?;
                if target.trim().is_empty() {
                    return Err(format!("映射文件里 \"{k}\" 的目标骨骼名为空"));
                }
                out.push(MapOverride {
                    source: k,
                    target: target.trim().to_string(),
                });
            }
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let source = item
                    .get("source")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("映射文件第 {i} 项缺 source"))?;
                let target = item
                    .get("target")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("映射文件第 {i} 项缺 target"))?;
                out.push(MapOverride {
                    source: source.to_string(),
                    target: target.trim().to_string(),
                });
            }
        }
        other => {
            return Err(format!(
                "映射文件顶层必须是对象或数组，实际是 {other}"
            ))
        }
    }
    if out.is_empty() {
        return Err(format!("映射文件 `{}` 里没有任何条目", path.display()));
    }
    Ok(out)
}

/// 源骨没被映射上时，给一个「长得像」的目标骨候选，方便用户一眼看出该补哪条映射。
pub fn suggest_target(source: &str, target_names: &[String]) -> Option<String> {
    let lower = source.to_lowercase();
    let side = if lower.ends_with("_l") || lower.starts_with("l_") {
        Some("L_")
    } else if lower.ends_with("_r") || lower.starts_with("r_") {
        Some("R_")
    } else {
        None
    };
    // 把 UE 的部件词换成 Tripo 的部件词，再按「侧别 + 部件」去找同名目标骨。
    let part = [
        ("upperarm", "Upperarm"),
        ("lowerarm", "Forearm"),
        ("clavicle", "Clavicle"),
        ("thigh", "Thigh"),
        ("calf", "Calf"),
        ("foot", "Foot"),
        ("ball", "ToeBase"),
        ("hand", "Hand"),
        ("spine", "Spine"),
        ("neck", "Neck"),
        ("head", "Head"),
        ("pelvis", "Pelvis"),
        ("root", "Root"),
    ]
    .iter()
    .find(|(ue, _)| lower.contains(ue))
    .map(|(_, tri)| *tri)?;
    let digit = lower
        .chars()
        .filter(char::is_ascii_digit)
        .last()
        .map(|c| c.to_string())
        .unwrap_or_default();
    let wanted = match side {
        Some(s) => format!("{s}{part}{digit}"),
        None => format!("{part}{digit}"),
    };
    target_names
        .iter()
        .find(|n| n.eq_ignore_ascii_case(&wanted))
        .cloned()
        .or_else(|| {
            target_names
                .iter()
                .find(|n| n.to_lowercase().contains(&part.to_lowercase()))
                .cloned()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_covers_all_major_chains() {
        let map = BoneMap::new(&[], false);
        for (src, dst) in [
            ("pelvis", "Hip"),
            ("spine_01", "Waist"),
            ("spine_02", "Spine01"),
            ("spine_03", "Spine02"),
            ("clavicle_l", "L_Clavicle"),
            ("upperarm_r", "R_Upperarm"),
            ("lowerarm_l", "L_Forearm"),
            ("hand_r", "R_Hand"),
            ("thigh_l", "L_Thigh"),
            ("calf_r", "R_Calf"),
            ("foot_l", "L_Foot"),
            ("ball_r", "R_ToeBase"),
            ("thigh_twist_01_l", "L_ThighTwist01"),
        ] {
            assert_eq!(map.get(src), Some(dst), "映射 {src} 应到 {dst}");
        }
    }

    #[test]
    fn no_twist_drops_twist_entries() {
        let map = BoneMap::new(&[], true);
        assert_eq!(map.get("thigh_twist_01_l"), None);
        assert_eq!(map.get("upperarm_twist_02_r"), None);
        assert_eq!(map.get("thigh_l"), Some("L_Thigh"));
    }

    #[test]
    fn override_replaces_builtin_and_retargets_reverse() {
        let overrides = vec![MapOverride {
            source: "pelvis".into(),
            target: "Pelvis".into(),
        }];
        let map = BoneMap::new(&overrides, false);
        assert_eq!(map.get("pelvis"), Some("Pelvis"));
        assert!(map.is_from_map("pelvis"));
        // 旧目标 Hip 的反查必须被清掉，新目标 Pelvis 才认领
        assert_eq!(map.reverse_get("Hip"), None);
        assert_eq!(map.reverse_get("Pelvis"), Some("pelvis"));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let map = BoneMap::new(&[], false);
        assert_eq!(map.get("Upperarm_L"), Some("L_Upperarm"));
    }

    #[test]
    fn map_file_accepts_object_and_array() {
        let dir = std::env::temp_dir();
        let obj = dir.join("asset3d_retarget_map_obj.json");
        std::fs::write(&obj, r#"{"spine_04":"Spine02"}"#).unwrap();
        let parsed = load_map_file(&obj).expect("对象形式");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].target, "Spine02");

        let arr = dir.join("asset3d_retarget_map_arr.json");
        std::fs::write(&arr, r#"[{"source":"neck_01","target":"Head"}]"#).unwrap();
        let parsed = load_map_file(&arr).expect("数组形式");
        assert_eq!(parsed[0].source, "neck_01");

        let bad = dir.join("asset3d_retarget_map_bad.json");
        std::fs::write(&bad, r#""nope""#).unwrap();
        assert!(load_map_file(&bad).is_err());
        let _ = std::fs::remove_file(&obj);
        let _ = std::fs::remove_file(&arr);
        let _ = std::fs::remove_file(&bad);
    }

    #[test]
    fn suggest_target_guesses_side_and_part() {
        let targets = vec![
            "Root".to_string(),
            "L_Thigh".to_string(),
            "R_Thigh".to_string(),
            "L_Forearm".to_string(),
        ];
        assert_eq!(
            suggest_target("thigh_r", &targets).as_deref(),
            Some("R_Thigh")
        );
        assert_eq!(
            suggest_target("lowerarm_l", &targets).as_deref(),
            Some("L_Forearm")
        );
        assert_eq!(suggest_target("ik_foot_root", &targets), None);
    }
}
