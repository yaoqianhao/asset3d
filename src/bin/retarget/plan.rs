//! 重定向核心：映射规划 + 逐帧姿态求解。
//!
//! 算法（局部空间增量法的世界空间等价形式 + rest 朝向对齐）：
//!
//! ```text
//! src 世界增量   delta[i] = src_world_anim[i] * src_world_rest[i]^-1
//! rest 对齐      align[i] = shortest_arc(dst_rest_dir[i] → src_rest_dir[src(i)])
//! dst 世界姿态   W[i]     = delta[i] * align[i] * dst_world_rest[i]   (已映射骨)
//!                W[i]     = W[parent] * dst_rest_local_rot[i]         (未映射骨)
//! dst 局部姿态   local[i] = W[parent]^-1 * W[i]
//! ```
//!
//! 去掉 `align` 就是纯世界增量：它只保证「相对各自 rest 转了多少」一致，一旦两侧
//! rest 姿势差得远（源骨架是 UE 的 T-pose，Tripo 生成的角色多是手臂下垂的 A-pose），
//! 源抬手 90° 落到目标身上会被下垂的初始角吃掉，画面上看就是「手臂抬不起来」。
//! `align` 先把目标 rest 骨朝向旋到源 rest 骨朝向，增量于是落在同一基准上；用最短弧
//! 而不是任意对齐矩阵，是为了不动绕骨轴的 twist 自由度，不给四肢凭空加扭转。
//!
//! 未映射骨保持自己的 rest 局部旋转、刚性跟随父骨：这正是缺骨骼（手指 / 无对应段）
//! 时该有的行为，不会把子树拧成麻花，也不会凭空继承祖先的增量多转一份。
//!
//! 为什么必须走世界空间：UE 的每根骨 +X 指向子骨，Tripo 是 +Y 指向子骨，两侧
//! 局部轴约定不同。`src_local_rest^-1 * src_local_anim` 在同一个绑定里是纯增量，
//! 但直接喂给目标骨的局部通道会因为轴不同而整体歪掉；换成世界增量后，轴约定差异
//! 天然被 rest 对齐吸收，只剩「目标骨应该朝哪」这一个信息。
//!
//! 平移只重定向根链（root / pelvis 对应的目标骨），位移量按腿长比缩放；其余骨骼的
//! 局部平移保持目标 rest —— 因为局部平移就是骨长与关节连接点，照抄源骨架等于把
//! 4.7 MB 的 UE 骨架比例硬塞进 75 MB 的 Tripo 角色，必然拉断。缩放通道完全不重定向。

use glam::{DMat4, DQuat, DVec3};

use super::math::{shortest_arc, LocalTrs};
use super::mapping::{suggest_target, BoneMap};
use super::source::{Clip, Skeleton};

/// 会做平移重定向的源骨：UE 里真正承载位移的只有 root 与 pelvis。
const ROOT_MOTION_SOURCES: &[&str] = &["root", "pelvis"];

#[derive(Debug, Clone, Copy)]
pub struct Pair {
    pub src: usize,
    pub dst: usize,
    pub from_map: bool,
}

#[derive(Debug, Clone)]
pub struct UnmappedSrc {
    pub name: String,
    pub reason: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MissingTarget {
    pub name: String,
    pub note: String,
}

/// 目标骨架的 rest 世界矩阵与父先于子的顺序：一次算好，逐帧复用。
pub struct Rig {
    pub names: Vec<String>,
    pub parent: Vec<Option<usize>>,
    pub rest: Vec<LocalTrs>,
    pub order: Vec<usize>,
    pub world_rest_rot: Vec<DQuat>,
    pub world_rest_tr: Vec<DVec3>,
}

impl Rig {
    pub fn new(skeleton: &Skeleton) -> Self {
        let n = skeleton.len();
        let order = skeleton.order();
        let mut world_rest_rot = vec![DQuat::IDENTITY; n];
        let mut world_rest_tr = vec![DVec3::ZERO; n];
        for &i in &order {
            let rest = skeleton.rest[i];
            match skeleton.parent[i] {
                Some(p) if p < n => {
                    world_rest_rot[i] = world_rest_rot[p] * rest.r;
                    world_rest_tr[i] = world_rest_tr[p] + world_rest_rot[p] * rest.t;
                }
                _ => {
                    world_rest_rot[i] = rest.r;
                    world_rest_tr[i] = rest.t;
                }
            }
        }
        Rig {
            names: skeleton.names.clone(),
            parent: skeleton.parent.clone(),
            rest: skeleton.rest.clone(),
            order,
            world_rest_rot,
            world_rest_tr,
        }
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }
}

/// 骨架 rest 的世界坐标：只用来求骨朝向，父链先于子。
fn rest_world_positions(parent: &[Option<usize>], rest: &[LocalTrs], order: &[usize]) -> Vec<DVec3> {
    let n = parent.len();
    let mut world_rot = vec![DQuat::IDENTITY; n];
    let mut world_tr = vec![DVec3::ZERO; n];
    for &i in order {
        match parent[i] {
            Some(p) if p < n => {
                world_rot[i] = world_rot[p] * rest[i].r;
                world_tr[i] = world_tr[p] + world_rot[p] * rest[i].t;
            }
            _ => {
                world_rot[i] = rest[i].r;
                world_tr[i] = rest[i].t;
            }
        }
    }
    world_tr
}

/// 每根骨的 rest 朝向：取「本骨 → 偏移最长的子骨」，叶子骨退回「父骨 → 本骨」。
///
/// 用途是给 `align` 提供一对可对齐的方向向量；用子骨位置而不是局部 +X/+Y，是为了
/// 不依赖各家的绑定轴约定（UE 用 +X 指向子骨，Tripo 用 +Y）。
///
/// 这里必须挑**偏移最长**的子骨而不是「第一个子骨」：UE 与 Tripo 都会在骨上挂
/// 零长度的 twist 辅助骨（局部平移为 0），取到它就等于取到零向量，退化成父骨方向，
/// 于是 `align` 会拿着上一段骨的方向去旋这一根，重定向后四肢直接反折 180°。
fn rest_bone_dirs(parent: &[Option<usize>], world_tr: &[DVec3]) -> Vec<DVec3> {
    let n = parent.len();
    let mut best_child: Vec<Option<usize>> = vec![None; n];
    let mut best_len: Vec<f64> = vec![0.0; n];
    for (i, p) in parent.iter().enumerate() {
        let Some(p) = *p else { continue };
        if p >= n {
            continue;
        }
        let len = (world_tr[i] - world_tr[p]).length();
        if len > best_len[p] {
            best_len[p] = len;
            best_child[p] = Some(i);
        }
    }
    (0..n)
        .map(|i| {
            if let Some(c) = best_child[i] {
                if best_len[i] > 1e-9 {
                    return (world_tr[c] - world_tr[i]).normalize();
                }
            }
            if let Some(p) = parent[i] {
                if p < n {
                    let v = world_tr[i] - world_tr[p];
                    if v.length_squared() > 1e-12 {
                        return v.normalize();
                    }
                }
            }
            DVec3::Y
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct MotionConfig {
    /// 位移按腿长比缩放（默认开）
    pub scale_motion: bool,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self { scale_motion: true }
    }
}

/// 映射规划结果。
pub struct Plan {
    pub pairs: Vec<Pair>,
    pub unmapped_src: Vec<UnmappedSrc>,
    pub missing_target: Vec<MissingTarget>,
    pub src_bones: usize,
    pub dst_bones: usize,
    /// 动画数据真正流动起来的骨骼比例：已映射且源侧确实有动画通道的骨 / 源侧有动画通道的骨
    pub coverage: f64,
    pub animated_src: usize,
    pub animated_mapped: usize,
    /// 目标侧把源位移搬过来的骨（root / pelvis 链）
    pub motion_bones: Vec<usize>,
    /// 每根目标骨的 rest 朝向对齐量（下标同目标节点）。
    ///
    /// `align[i]` 把目标骨 rest 朝向旋到其源骨 rest 朝向；未映射骨为 identity。
    /// 详见 [`super::math::shortest_arc`] 的注释：没有这一步，A-pose 目标的四肢会把
    /// 世界增量吃掉（源抬手 90°，目标看起来没动）。
    pub align: Vec<DQuat>,
    /// 腿长比（目标 / 源），1.0 表示两侧比例接近
    pub leg_scale: f64,
    pub src_leg_len: f64,
    pub dst_leg_len: f64,
    pub warnings: Vec<String>,
}

/// rest 世界坐标（**带父链缩放**）。
///
/// FBX 导出的 UE 骨架会把「厘米→米」的 0.01 缩放挂在一个祖先节点上，节点自身的
/// 局部平移仍然写厘米。只看局部平移会把 96.7cm 当成 96.7m；矩阵链累乘才是米。
/// 位移增量、腿长比都必须走这条链，否则「按腿长比缩放」会用错单位。
pub fn world_rest_positions(skeleton: &Skeleton) -> Vec<DVec3> {
    let n = skeleton.len();
    let mut world = vec![DMat4::IDENTITY; n];
    for &i in &skeleton.order() {
        let local = skeleton.rest[i].matrix();
        world[i] = match skeleton.parent[i].filter(|p| *p < n) {
            Some(p) => world[p] * local,
            None => local,
        };
    }
    world.iter().map(|m| m.w_axis.truncate()).collect()
}

/// 骨骼链长度：从 hip 到 foot 的世界距离，两侧骨架用同一把尺子量，比值就是位移
/// 缩放系数（米 / 米，单位无关）。
fn chain_length(
    parent: &[Option<usize>],
    world_tr: &[DVec3],
    names: &[String],
    chain: &[&str],
) -> Option<f64> {
    let mut total = 0.0;
    for want in chain {
        let idx = names.iter().position(|n| n.eq_ignore_ascii_case(want))?;
        let p = parent[idx].filter(|p| *p < world_tr.len())?;
        total += (world_tr[idx] - world_tr[p]).length();
    }
    if total > 1e-6 {
        Some(total)
    } else {
        None
    }
}

pub fn build_plan(src: &Clip, dst_skeleton: &Skeleton, map: &BoneMap) -> Plan {
    let rig = Rig::new(dst_skeleton);
    let mut pairs: Vec<Pair> = Vec::new();
    let mut used_dst: Vec<bool> = vec![false; dst_skeleton.len()];

    for (si, name) in src.skeleton.names.iter().enumerate() {
        let Some(target_name) = map.get(name) else {
            continue;
        };
        let Some(di) = dst_skeleton.find(target_name) else {
            continue;
        };
        if used_dst[di] {
            continue;
        }
        used_dst[di] = true;
        pairs.push(Pair {
            src: si,
            dst: di,
            from_map: map.is_from_map(name),
        });
    }

    // 源侧未映射：逐条给原因与建议，UI/报告直接可用
    let mut unmapped_src = Vec::new();
    for name in src.skeleton.names.iter() {
        if map.get(name).is_some() {
            continue;
        }
        let lower = name.to_lowercase();
        let reason = if lower.starts_with("ik_") || lower.contains("_ik") {
            "IK 控制骨，UE 导出时附带，本身不参与蒙皮，跳过".to_string()
        } else if ["index_", "middle_", "pinky_", "ring_", "thumb_"]
            .iter()
            .any(|t| lower.contains(t))
        {
            "手指骨：目标角色没有手指骨骼，跳过并保持目标 rest".to_string()
        } else if lower.starts_with('#') {
            "无名场景根节点（非骨骼），跳过".to_string()
        } else if map.get(name).is_some() {
            String::new()
        } else {
            "内置映射表与 --map 都没有覆盖这根骨，跳过并保持目标 rest".to_string()
        };
        let suggestion = suggest_target(name, &dst_skeleton.names);
        unmapped_src.push(UnmappedSrc {
            name: name.clone(),
            reason,
            suggestion,
        });
    }

    // 目标侧没有对应源的骨骼：典型是 Tripo 的 *Twist02 与另一侧的髋/腰分支
    let mut missing_target = Vec::new();
    for (di, name) in dst_skeleton.names.iter().enumerate() {
        if used_dst[di] {
            continue;
        }
        let note = if name.to_lowercase().contains("twist") {
            "目标侧的中段扭转辅助骨，源侧没有同段位骨骼，保持 rest（跟随父骨刚性转动）"
                .to_string()
        } else {
            "源骨架没有对应骨骼，保持 rest（跟随父骨刚性转动）".to_string()
        };
        missing_target.push(MissingTarget {
            name: name.clone(),
            note,
        });
    }

    // 位移承载骨：pairs 里源名命中 root/pelvis 的，外加其目标祖先链（Root 上面只有 Armature）
    let mut motion_bones: Vec<usize> = pairs
        .iter()
        .filter(|p| {
            let n = src.skeleton.names[p.src].to_lowercase();
            ROOT_MOTION_SOURCES.contains(&n.as_str())
        })
        .map(|p| p.dst)
        .collect();
    if motion_bones.is_empty() {
        // 源骨命名不是 UE 习惯：退化成「最顶层的那个已映射骨」
        if let Some(top) = pairs
            .iter()
            .min_by_key(|p| rig.order.iter().position(|&x| x == p.dst).unwrap_or(usize::MAX))
        {
            motion_bones.push(top.dst);
        }
    }

    // 覆盖率：只看源侧真正有动画的骨，没动画的骨映不映射都不影响动作
    let animated_src = src
        .skeleton
        .names
        .iter()
        .enumerate()
        .filter(|(i, _)| src.animated.get(*i).copied().unwrap_or(false))
        .count();
    let animated_mapped = pairs
        .iter()
        .filter(|p| src.animated.get(p.src).copied().unwrap_or(false))
        .count();
    let coverage = if animated_src > 0 {
        animated_mapped as f64 / animated_src as f64
    } else {
        0.0
    };

    // 腿长比（两侧都用带缩放的 rest 世界坐标量，单位无关）
    let src_pos = world_rest_positions(&src.skeleton);
    let src_leg = chain_length(
        &src.skeleton.parent,
        &src_pos,
        &src.skeleton.names,
        &["thigh_l", "calf_l", "foot_l"],
    );
    let dst_leg = chain_length(
        &dst_skeleton.parent,
        &rig.world_rest_tr,
        &dst_skeleton.names,
        &["L_Thigh", "L_Calf", "L_Foot"],
    );
    let (leg_scale, src_leg_len, dst_leg_len) = match (src_leg, dst_leg) {
        (Some(a), Some(b)) if a > 1e-6 => (b / a, a, b),
        _ => (1.0, 0.0, 0.0),
    };

    let mut warnings = Vec::new();
    if src_leg.is_none() || dst_leg.is_none() {
        warnings.push(
            "取不到腿长（源/目标缺少 thigh/calf/foot 命名），位移缩放退回 1.0，角色可能滑步".to_string(),
        );
    }
    if animated_mapped == 0 {
        warnings.push("没有任何一根带动画的源骨被映射上，输出的动作会是静止的".to_string());
    }
    if !pairs.iter().any(|p| src.skeleton.names[p.src].to_lowercase() == "hand_l") {
        warnings.push("源侧左手未映射到目标，手部姿态将沿用目标 rest".to_string());
    }

    // rest 朝向对齐：把目标骨 rest 朝向旋到对应源骨 rest 朝向。
    // 两侧绑定姿势差得越远（T-pose vs A-pose），这一步越关键。
    let src_order = src.skeleton.order();
    let src_rest_tr = rest_world_positions(&src.skeleton.parent, &src.skeleton.rest, &src_order);
    let src_dirs = rest_bone_dirs(&src.skeleton.parent, &src_rest_tr);
    let dst_dirs = rest_bone_dirs(&dst_skeleton.parent, &rig.world_rest_tr);
    let mut align = vec![DQuat::IDENTITY; dst_skeleton.len()];
    for pair in &pairs {
        if pair.src < src_dirs.len() && pair.dst < align.len() {
            align[pair.dst] = shortest_arc(dst_dirs[pair.dst], src_dirs[pair.src]).normalize();
        }
    }

    Plan {
        pairs,
        unmapped_src,
        missing_target,
        src_bones: src.skeleton.len(),
        dst_bones: dst_skeleton.len(),
        coverage,
        animated_src,
        animated_mapped,
        motion_bones,
        align,
        leg_scale,
        src_leg_len,
        dst_leg_len,
        warnings,
    }
}

/// 逐帧求解目标骨架的局部 TRS。
///
/// `frame` 是源动作的帧下标；返回长度等于目标骨架节点数（含非关节节点，
/// 因为它们的父链关系会影响世界矩阵）。
pub fn pose_frame(
    src: &Clip,
    frame: usize,
    pairs: &[Pair],
    rig: &Rig,
    motion_bones: &[usize],
    align: &[DQuat],
    cfg: MotionConfig,
    leg_scale: f64,
) -> Vec<LocalTrs> {
    let n = rig.len();
    let frame = frame.min(src.frame_count().saturating_sub(1));

    // 源侧：rest 世界与当前帧世界（旋转 + 平移）
    //
    // 平移必须带上祖先链的缩放：UE 导出的 FBX 把「厘米 → 米」的 0.01 挂在 Armature 上，
    // 各骨局部平移仍以厘米记。只做「旋转 + 平移」会让世界位移停在厘米量级，
    // 再乘腿长比搬给目标，角色会被甩出好几米（实测 7.0 cm 被当成 7.0 m）。
    let src_order = src.skeleton.order();
    let sn = src.skeleton.len();
    let mut src_rest_rot = vec![DQuat::IDENTITY; sn];
    let mut src_rest_tr = vec![DVec3::ZERO; sn];
    let mut src_rest_scale = vec![DVec3::ONE; sn];
    let mut src_cur_rot = vec![DQuat::IDENTITY; sn];
    let mut src_cur_tr = vec![DVec3::ZERO; sn];
    let mut src_cur_scale = vec![DVec3::ONE; sn];
    for &i in &src_order {
        let rest = src.skeleton.rest[i];
        let cur = src.frames[i][frame];
        match src.skeleton.parent[i] {
            Some(p) if p < sn => {
                src_rest_rot[i] = src_rest_rot[p] * rest.r;
                src_rest_tr[i] =
                    src_rest_tr[p] + src_rest_rot[p] * (src_rest_scale[p] * rest.t);
                src_rest_scale[i] = src_rest_scale[p] * rest.s;
                src_cur_rot[i] = src_cur_rot[p] * cur.r;
                src_cur_tr[i] = src_cur_tr[p] + src_cur_rot[p] * (src_cur_scale[p] * cur.t);
                src_cur_scale[i] = src_cur_scale[p] * cur.s;
            }
            _ => {
                src_rest_rot[i] = rest.r;
                src_rest_tr[i] = rest.t;
                src_rest_scale[i] = rest.s;
                src_cur_rot[i] = cur.r;
                src_cur_tr[i] = cur.t;
                src_cur_scale[i] = cur.s;
            }
        }
    }

    // 映射到目标节点上的增量
    let mut delta: Vec<Option<DQuat>> = vec![None; n];
    let mut src_tr_delta: Vec<Option<DVec3>> = vec![None; n];
    for pair in pairs {
        if pair.src >= sn || pair.dst >= n {
            continue;
        }
        let d = src_cur_rot[pair.src] * src_rest_rot[pair.src].inverse();
        delta[pair.dst] = Some(d.normalize());
        src_tr_delta[pair.dst] = Some(src_cur_tr[pair.src] - src_rest_tr[pair.src]);
    }
    let drive_tr: Vec<bool> = (0..n)
        .map(|i| motion_bones.contains(&i) && src_tr_delta[i].is_some())
        .collect();
    let scale = if cfg.scale_motion && leg_scale > 1e-6 {
        leg_scale
    } else {
        1.0
    };

    let mut locals = vec![LocalTrs::IDENTITY; n];
    let mut world_rot = vec![DQuat::IDENTITY; n];
    let mut world_tr = vec![DVec3::ZERO; n];
    for &i in &rig.order {
        let p = rig.parent[i].filter(|p| *p < n);
        let (parent_rot, parent_tr) = match p {
            Some(p) => (world_rot[p], world_tr[p]),
            None => (DQuat::IDENTITY, DVec3::ZERO),
        };

        // 旋转：已映射骨 = 源世界增量 × rest 对齐 × 目标 rest 世界朝向
        // 未映射骨保持自身 rest 局部旋转（刚性跟随父骨，不继承增量）
        let target_world = match delta[i] {
            Some(d) => {
                let a = align.get(i).copied().unwrap_or(DQuat::IDENTITY);
                (d * a * rig.world_rest_rot[i]).normalize()
            }
            None => parent_rot * rig.rest[i].r,
        };
        let local_rot = (parent_rot.inverse() * target_world).normalize();

        // 平移：只有根链跟着源走（按腿长比缩放），其余保持目标 rest 局部平移
        let local_tr = if drive_tr[i] {
            let want_world = rig.world_rest_tr[i] + src_tr_delta[i].unwrap_or(DVec3::ZERO) * scale;
            parent_rot.inverse() * (want_world - parent_tr)
        } else {
            rig.rest[i].t
        };

        locals[i] = LocalTrs::new(local_tr, local_rot, rig.rest[i].s);
        world_rot[i] = parent_rot * local_rot;
        world_tr[i] = parent_tr + parent_rot * local_tr;
    }
    locals
}

/// 整段动作求解。
pub fn pose_clip(
    src: &Clip,
    plan: &Plan,
    rig: &Rig,
    cfg: MotionConfig,
) -> Vec<Vec<LocalTrs>> {
    let frames = src.frame_count();
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        out.push(pose_frame(
            src,
            f,
            &plan.pairs,
            rig,
            &plan.motion_bones,
            &plan.align,
            cfg,
            plan.leg_scale,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retarget::mapping::MapOverride;

    fn skeleton(names: &[&str], parent: &[Option<usize>], rest: Vec<LocalTrs>) -> Skeleton {
        Skeleton {
            names: names.iter().map(|s| s.to_string()).collect(),
            parent: parent.to_vec(),
            rest,
        }
    }

    fn clip(names: &[&str], parent: &[Option<usize>], rest: Vec<LocalTrs>) -> Clip {
        let n = names.len();
        Clip {
            name: "t".into(),
            skeleton: skeleton(names, parent, rest.clone()),
            times: vec![0.0, 1.0],
            // 二维是 [骨][帧]：每根骨都要有 2 帧，否则 frame=1 就越界。
            frames: rest.iter().map(|s| vec![*s; 2]).collect(),
            animated: vec![true; n],
            kind: "test",
            joints: Vec::new(),
        }
    }

    /// 目标侧未映射骨必须刚性跟随父骨：它的局部旋转要回到 rest 局部值，
    /// 不能因为父骨转了 30° 就把子骨一起再拧 30°。
    #[test]
    fn unmapped_child_keeps_rest_local_rotation() {
        let rig = Rig::new(&skeleton(
            &["a", "b"],
            &[None, Some(0)],
            vec![LocalTrs::IDENTITY, LocalTrs::IDENTITY],
        ));
        let src = clip(
            &["a"],
            &[None],
            vec![LocalTrs::new(
                DVec3::ZERO,
                DQuat::from_rotation_y(0.5),
                DVec3::ONE,
            )],
        );
        let pairs = vec![Pair {
            src: 0,
            dst: 0,
            from_map: false,
        }];
        let locals = pose_frame(&src, 0, &pairs, &rig, &[], &[], MotionConfig::default(), 1.0);
        // 源动画 == 源 rest，增量应为单位：两根骨的局部旋转都等于 rest
        assert!(locals[0].r.angle_between(DQuat::IDENTITY) < 1e-12);
        assert!(locals[1].r.angle_between(DQuat::IDENTITY) < 1e-12);
    }

    /// 已映射骨必须拿到完整的世界增量：源骨转 90°，目标骨也转 90°（不管 rest 轴差多少）。
    #[test]
    fn mapped_bone_world_delta_is_preserved() {
        let rig = Rig::new(&skeleton(
            &["a", "b"],
            &[None, Some(0)],
            vec![
                LocalTrs::new(DVec3::ZERO, DQuat::IDENTITY, DVec3::ONE),
                LocalTrs::new(DVec3::ZERO, DQuat::from_rotation_x(0.3), DVec3::ONE),
            ],
        ));
        let rest = LocalTrs::new(DVec3::ZERO, DQuat::from_rotation_z(0.2), DVec3::ONE);
        let anim = LocalTrs::new(DVec3::ZERO, DQuat::from_rotation_z(0.2), DVec3::ONE);
        let mut c = clip(&["a"], &[None], vec![rest]);
        c.frames[0][0] = anim;
        let pairs = vec![Pair {
            src: 0,
            dst: 0,
            from_map: false,
        }];
        let locals = pose_frame(&c, 0, &pairs, &rig, &[], &[], MotionConfig::default(), 1.0);
        // delta = identity，因此根骨局部旋转 = 目标 rest 局部旋转
        assert!(locals[0].r.angle_between(DQuat::IDENTITY) < 1e-12);
        // 子骨是未映射的，局部旋转仍是自己的 rest
        assert!(locals[1].r.angle_between(DQuat::from_rotation_x(0.3)) < 1e-12);
    }

    /// 位移只走根链：子骨的局部平移永远等于目标 rest（否则会拉断骨架）。
    #[test]
    fn only_root_chain_translates() {
        let rig = Rig::new(&skeleton(
            &["root", "hip", "child"],
            &[None, Some(0), Some(1)],
            vec![
                LocalTrs::IDENTITY,
                LocalTrs::new(DVec3::new(0.0, 0.0, 0.5), DQuat::IDENTITY, DVec3::ONE),
                LocalTrs::new(DVec3::new(0.0, 0.3, 0.0), DQuat::IDENTITY, DVec3::ONE),
            ],
        ));
        let mut c = clip(
            &["root", "pelvis"],
            &[None, Some(0)],
            vec![LocalTrs::IDENTITY, LocalTrs::new(DVec3::ZERO, DQuat::IDENTITY, DVec3::ONE)],
        );
        // 源跟骨前移 1.0
        c.frames[1][1].t = DVec3::new(1.0, 0.0, 0.0);
        let pairs = vec![
            Pair { src: 0, dst: 0, from_map: false },
            Pair { src: 1, dst: 1, from_map: false },
        ];
        let delta = c.frames[1][1].t - c.frames[0][1].t;
        let locals = pose_frame(
            &c,
            1,
            &pairs,
            &rig,
            &[0, 1],
            &[],
            MotionConfig { scale_motion: true },
            2.0,
        );
        // hip 的世界位移 = 源位移 × 腿长比 2.0
        let hip_world = locals[0].t + locals[1].t;
        assert!((hip_world - DVec3::new(delta.x * 2.0, 0.0, 0.5)).length() < 1e-9);
        // 子骨局部平移仍是 rest
        assert!((locals[2].t - DVec3::new(0.0, 0.3, 0.0)).length() < 1e-12);
    }

    /// 祖先节点带 0.01 缩放（UE 的 FBX 厘米→米习惯）时，世界位移必须换算到米，
    /// 不能把 100cm 的位移当成 100m 直接搬给目标。
    #[test]
    fn root_motion_follows_ancestor_scale() {
        // 源：Armature(scale 0.01) → pelvis，pelvis 局部平移 0cm → 100cm
        let src_rest = vec![
            LocalTrs::new(DVec3::ZERO, DQuat::IDENTITY, DVec3::splat(0.01)),
            LocalTrs::new(DVec3::ZERO, DQuat::IDENTITY, DVec3::ONE),
        ];
        let mut c = Clip {
            name: "t".into(),
            skeleton: skeleton(&["Armature", "pelvis"], &[None, Some(0)], src_rest.clone()),
            times: vec![0.0, 1.0],
            frames: vec![vec![src_rest[0]; 2], vec![src_rest[1]; 2]],
            animated: vec![false, true],
            kind: "test",
            joints: Vec::new(),
        };
        c.frames[1][1] = LocalTrs::new(DVec3::new(100.0, 0.0, 0.0), DQuat::IDENTITY, DVec3::ONE);
        // 目标：单根 Root，位移承载骨就是它
        let rig = Rig::new(&skeleton(
            &["Root"],
            &[None],
            vec![LocalTrs::IDENTITY],
        ));
        let pairs = vec![Pair {
            src: 1,
            dst: 0,
            from_map: false,
        }];
        let locals = pose_frame(
            &c,
            1,
            &pairs,
            &rig,
            &[0],
            &[],
            MotionConfig::default(),
            1.0,
        );
        // 100cm × 0.01 = 1m
        assert!(
            (locals[0].t - DVec3::new(1.0, 0.0, 0.0)).length() < 1e-9,
            "期望 1m，实际 {:?}",
            locals[0].t
        );
    }

    #[test]
    fn plan_reports_unmapped_with_reasons() {
        let src = clip(
            &["pelvis", "spine_01", "index_01_l", "ik_foot_root"],
            &[None, Some(0), Some(1), Some(0)],
            vec![LocalTrs::IDENTITY; 4],
        );
        let dst = skeleton(
            &["Hip", "Waist"],
            &[None, Some(0)],
            vec![LocalTrs::IDENTITY, LocalTrs::IDENTITY],
        );
        let plan = build_plan(&src, &dst, &BoneMap::new(&[], false));
        assert!(plan.pairs.len() >= 2);
        let finger = plan
            .unmapped_src
            .iter()
            .find(|u| u.name == "index_01_l")
            .expect("手指应在未映射清单里");
        assert!(finger.reason.contains("手指"));
        let ik = plan
            .unmapped_src
            .iter()
            .find(|u| u.name == "ik_foot_root")
            .expect("IK 应在未映射清单里");
        assert!(ik.reason.contains("IK"));
    }

    /// 两侧 rest 姿势差很多时（源 T-pose 手臂平举、目标 A-pose 手臂下垂），
    /// 目标骨的**世界朝向**必须跟上源骨，而不是只跟上「相对 rest 的增量」。
    #[test]
    fn rest_pose_divergence_is_aligned_before_applying_delta() {
        // 目标：肩 → 上臂 → 前臂，整条手臂朝下（A-pose 下垂）
        let dst_parent = [None, Some(0), Some(1)];
        let dst = skeleton(
            &["Root", "Arm", "Hand"],
            &dst_parent,
            vec![
                LocalTrs::IDENTITY,
                LocalTrs::new(DVec3::new(0.0, -0.4, 0.0), DQuat::IDENTITY, DVec3::ONE),
                LocalTrs::new(DVec3::new(0.0, -0.4, 0.0), DQuat::IDENTITY, DVec3::ONE),
            ],
        );
        let rig = Rig::new(&dst);
        // 源：肩 → 上臂 → 前臂，整条手臂平举朝 +X（T-pose）
        let src_parent = [None, Some(0), Some(1)];
        let mut c = clip(
            &["root", "arm", "hand"],
            &src_parent,
            vec![
                LocalTrs::IDENTITY,
                LocalTrs::new(DVec3::new(0.4, 0.0, 0.0), DQuat::IDENTITY, DVec3::ONE),
                LocalTrs::new(DVec3::new(0.4, 0.0, 0.0), DQuat::IDENTITY, DVec3::ONE),
            ],
        );
        // 源动画：上臂绕 Z 转 90°，手臂从 +X 抬到 +Y
        c.frames[1][1].r = DQuat::from_rotation_z(std::f64::consts::FRAC_PI_2);
        // 源当前手臂朝向（世界）
        let src_pos = world_positions(&src_parent, &[c.frames[0][0], c.frames[1][1], c.frames[2][1]]);
        let src_dir = (src_pos[2] - src_pos[1]).normalize();
        assert!(src_dir.angle_between(DVec3::new(0.0, 1.0, 0.0)) < 1e-9);

        let pairs = vec![
            Pair { src: 0, dst: 0, from_map: false },
            Pair { src: 1, dst: 1, from_map: false },
            Pair { src: 2, dst: 2, from_map: false },
        ];
        // align：把目标「朝下」旋到源「朝 +X」
        let align = vec![
            DQuat::IDENTITY,
            shortest_arc(DVec3::new(0.0, -1.0, 0.0), DVec3::new(1.0, 0.0, 0.0)),
            DQuat::IDENTITY,
        ];
        let locals = pose_frame(&c, 1, &pairs, &rig, &[], &align, MotionConfig::default(), 1.0);
        let dst_pos = world_positions(&dst_parent, &locals);
        let dst_dir = (dst_pos[2] - dst_pos[1]).normalize();
        assert!(
            dst_dir.angle_between(src_dir) < 1e-9,
            "对齐后目标手臂朝向应与源一致，实际 {dst_dir:?} vs {src_dir:?}"
        );

        // 对照：不传 align 时，下垂的手臂只能转到 -X，跟源的 +Y 差了 90° 以上
        let plain = pose_frame(&c, 1, &pairs, &rig, &[], &[], MotionConfig::default(), 1.0);
        let plain_pos = world_positions(&dst_parent, &plain);
        let plain_dir = (plain_pos[2] - plain_pos[1]).normalize();
        assert!(plain_dir.angle_between(src_dir) > 1.5);
    }

    /// 骨架父子链上的世界坐标（测试用，只看旋转与平移，不含缩放）。
    fn world_positions(parent: &[Option<usize>], locals: &[LocalTrs]) -> Vec<DVec3> {
        let mut pos = vec![DVec3::ZERO; locals.len()];
        let mut rot = vec![DQuat::IDENTITY; locals.len()];
        for i in 0..locals.len() {
            match parent[i] {
                None => {
                    pos[i] = locals[i].t;
                    rot[i] = locals[i].r;
                }
                Some(p) => {
                    pos[i] = pos[p] + rot[p] * locals[i].t;
                    rot[i] = rot[p] * locals[i].r;
                }
            }
        }
        pos
    }

    /// `build_plan` 必须真的把 align 算出来（不是留着 identity 摆设）。
    #[test]
    fn build_plan_fills_align_for_limbs() {
        let src = clip(
            &["root", "arm"],
            &[None, Some(0)],
            vec![
                LocalTrs::IDENTITY,
                LocalTrs::new(DVec3::new(0.5, 0.0, 0.0), DQuat::IDENTITY, DVec3::ONE),
            ],
        );
        let dst = skeleton(
            &["Root", "Arm"],
            &[None, Some(0)],
            vec![
                LocalTrs::IDENTITY,
                LocalTrs::new(DVec3::new(0.0, -0.5, 0.0), DQuat::IDENTITY, DVec3::ONE),
            ],
        );
        let map = BoneMap::new(
            &[
                MapOverride { source: "root".into(), target: "Root".into() },
                MapOverride { source: "arm".into(), target: "Arm".into() },
            ],
            false,
        );
        let plan = build_plan(&src, &dst, &map);
        assert_eq!(plan.align.len(), dst.len());
        // Arm 的 rest 朝向差 90°，align 必须把 -Y 转到 +X
        let arm_align = plan.align[1];
        assert!((arm_align * DVec3::NEG_Y).angle_between(DVec3::X) < 1e-9);
    }

    /// 零长度 twist 子骨不能抢走骨朝向。
    ///
    /// 回归：`rest_bone_dirs` 早期取「第一个子骨」，而 UE / Tripo 都会把 twist 辅助骨
    /// 排在真子骨前面且局部平移为 0，于是骨朝向退化成父骨方向，align 旋错轴，
    /// 重定向后四肢反折 180°（rebelsoldier 的 R_Thigh / L_Forearm 实测如此）。
    #[test]
    fn rest_bone_dirs_skips_zero_length_twist_child() {
        let parent = [None, Some(0), Some(1), Some(1)];
        // 0:Root  1:Arm  2:Twist(Arm 的零长度辅助骨)  3:Hand(Arm 的真子骨，+X 0.5)
        let locals = vec![
            LocalTrs::IDENTITY,
            LocalTrs::new(DVec3::ZERO, DQuat::IDENTITY, DVec3::ONE),
            LocalTrs::new(DVec3::ZERO, DQuat::IDENTITY, DVec3::ONE),
            LocalTrs::new(DVec3::new(0.5, 0.0, 0.0), DQuat::IDENTITY, DVec3::ONE),
        ];
        let world = world_positions(&parent, &locals);
        let dirs = rest_bone_dirs(&parent, &world);
        assert!(
            dirs[1].angle_between(DVec3::X) < 1e-9,
            "Arm 的朝向应指向真子骨 Hand(+X)，实际 {:?}",
            dirs[1]
        );
    }

    #[test]
    fn plan_honours_map_overrides() {
        let src = clip(&["pelvis"], &[None], vec![LocalTrs::IDENTITY]);
        let dst = skeleton(
            &["Hip", "Pelvis"],
            &[None, Some(0)],
            vec![LocalTrs::IDENTITY, LocalTrs::IDENTITY],
        );
        let map = BoneMap::new(
            &[MapOverride {
                source: "pelvis".into(),
                target: "Pelvis".into(),
            }],
            false,
        );
        let plan = build_plan(&src, &dst, &map);
        assert_eq!(plan.pairs.len(), 1);
        assert_eq!(plan.pairs[0].dst, 1);
        assert!(plan.pairs[0].from_map);
    }
}
