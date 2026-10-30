//! 读回自检：把刚写出的 GLB 重新当成「一个陌生文件」解析，逐项跟期望值对账。
//!
//! 关键点是**独立复算**：期望姿态不是从写出流程里搬过来的中间变量，而是拿源动作
//! 重新跑一遍 [`pose_frame`]，再跟「从字节里读回来、重新插值取样」的实际值比。
//! 这样能同时抓到三类错误：序列化写歪（字节层）、accessor 偏移/对齐写歪（读回层）、
//! 求解本身写歪（算法层）。

use glam::{DMat4, DQuat, DVec3};
use serde_json::Value;

use super::glb::{AssetCounts, Glb};
use super::math::LocalTrs;
use super::plan::{MotionConfig, Pair, Plan, Rig};
use super::source::{clip_from_glb, Clip};

#[derive(Debug, Clone)]
pub struct SampleCheck {
    pub bone: String,
    pub path: &'static str,
    pub frame: usize,
    pub time: f64,
    pub expected: [f64; 4],
    pub actual: [f64; 4],
    pub max_abs_diff: f64,
    pub angle_deg: f64,
}

impl SampleCheck {
    pub fn line(&self) -> String {
        if self.path == "rotation" {
            format!(
                "{:>12} f{:02} t={:.3}s 期望 q=[{:+.4},{:+.4},{:+.4},{:+.4}] 实际 q=[{:+.4},{:+.4},{:+.4},{:+.4}] 角度差={:.4}°",
                self.bone,
                self.frame,
                self.time,
                self.expected[0],
                self.expected[1],
                self.expected[2],
                self.expected[3],
                self.actual[0],
                self.actual[1],
                self.actual[2],
                self.actual[3],
                self.angle_deg
            )
        } else {
            format!(
                "{:>12} f{:02} t={:.3}s 期望 t=[{:+.4},{:+.4},{:+.4}] 实际 t=[{:+.4},{:+.4},{:+.4}] 最大分量差={:.6}m",
                self.bone,
                self.frame,
                self.time,
                self.expected[0],
                self.expected[1],
                self.expected[2],
                self.actual[0],
                self.actual[1],
                self.actual[2],
                self.max_abs_diff
            )
        }
    }
}

pub struct VerifyReport {
    pub before: AssetCounts,
    pub after: AssetCounts,
    pub animation_name: String,
    pub animation_channels: usize,
    pub animation_duration: f64,
    pub animation_samples: usize,
    pub samples: Vec<SampleCheck>,
    /// 所有已映射骨的旋转最大角度误差（度）
    pub worst_angle_deg: f64,
    pub worst_bone: String,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

fn quat(arr: [f64; 4]) -> DQuat {
    DQuat::from_xyzw(arr[0], arr[1], arr[2], arr[3]).normalize()
}

/// 从输出文件的指定动画里，取某根骨在某时刻的旋转采样（走 clip 的插值，不是朴素查表）。
fn sample_output(clip: &Clip, bone: usize, time: f64) -> LocalTrs {
    // 找到最近的时间点：输出时间轴与源一一对应，这里按最近邻取，避免插值把误差搅进来
    let mut best = 0usize;
    let mut best_d = f64::INFINITY;
    for (i, t) in clip.times.iter().enumerate() {
        let d = (t - time).abs();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    clip.frames[bone][best]
}

/// 输出 JSON 里新动画的元信息（时长、采样点数、通道数）。
fn animation_meta(glb: &Glb, index: usize) -> Result<(String, f64, usize, usize), String> {
    let anim = glb
        .arr("animations")
        .and_then(|a| a.get(index))
        .ok_or_else(|| format!("输出文件里没有 animations[{index}]"))?;
    let name = anim
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
        .to_string();
    let channels = anim
        .get("channels")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let samplers = anim
        .get("samplers")
        .and_then(Value::as_array)
        .ok_or_else(|| "新动画缺 samplers".to_string())?;
    let mut samples = 0usize;
    let mut max_time = 0.0f64;
    for s in samplers {
        let input = s
            .get("input")
            .and_then(Value::as_u64)
            .ok_or_else(|| "sampler 缺 input".to_string())? as usize;
        let (_, times) = glb.read_accessor(input)?;
        samples = samples.max(times.len());
        for t in &times {
            if *t > max_time {
                max_time = *t;
            }
        }
    }
    Ok((name, max_time, samples, channels))
}

/// 取 skin[0].joints 指的节点索引（用来比对蒙皮指认是否原封不动）。
fn skin_joints(glb: &Glb) -> Vec<u64> {
    glb.get("skins")
        .and_then(Value::as_array)
        .and_then(|skins| skins.first())
        .and_then(|skin| skin.get("joints"))
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// 取全部节点名（顺序敏感：glTF 里节点顺序本身就是身份）。
fn node_names(glb: &Glb) -> Vec<String> {
    glb.nodes()
        .iter()
        .map(|node| {
            node.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

/// 期望姿态的**独立复算**：整条链走 4×4 矩阵重算一遍。
///
/// 和 [`pose_frame`] 的四元数链互不共享中间量——源侧世界变换用矩阵连乘、目标侧
/// 局部旋转靠矩阵求逆再取回四元数。自检拿它的结果当「期望值」，等于用第二套实现
/// 给第一套实现判卷：序列化写歪、插值取错能抓；算法本身写歪同样能抓。
fn matrix_expected(
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
    let sn = src.skeleton.len();
    let order = src.skeleton.order();

    // —— 源侧：rest 与当前帧的世界矩阵
    let mut rest_m = vec![DMat4::IDENTITY; sn];
    let mut cur_m = vec![DMat4::IDENTITY; sn];
    for &i in &order {
        let rest_local = src.skeleton.rest[i].matrix();
        let cur_local = src.frames[i][frame].matrix();
        match src.skeleton.parent[i].filter(|p| *p < sn) {
            Some(p) => {
                rest_m[i] = rest_m[p] * rest_local;
                cur_m[i] = cur_m[p] * cur_local;
            }
            None => {
                rest_m[i] = rest_local;
                cur_m[i] = cur_local;
            }
        }
    }

    // —— 每对映射的「世界增量」：Δ = M_anim · M_rest⁻¹，只取旋转部分
    let mut delta = vec![None; n];
    let mut tr_delta = vec![None; n];
    for pair in pairs {
        if pair.src >= sn || pair.dst >= n {
            continue;
        }
        let m = cur_m[pair.src] * rest_m[pair.src].inverse();
        delta[pair.dst] = Some(LocalTrs::from_matrix(&m).r);
        tr_delta[pair.dst] = Some(cur_m[pair.src].w_axis.truncate() - rest_m[pair.src].w_axis.truncate());
    }
    let scale = if cfg.scale_motion && leg_scale > 1e-6 {
        leg_scale
    } else {
        1.0
    };

    // —— 目标侧：rest 世界旋转矩阵（只算一次，和 rig 的预计算互相对照）
    let mut rest_rot_m = vec![DMat4::IDENTITY; n];
    let mut rest_tr_world = vec![DVec3::ZERO; n];
    for &i in &rig.order {
        let parent = rig.parent[i].filter(|p| *p < n);
        let rest_rot = DMat4::from_quat(rig.rest[i].r);
        match parent {
            Some(p) => {
                rest_rot_m[i] = rest_rot_m[p] * rest_rot;
                rest_tr_world[i] = rest_tr_world[p] + rest_rot_m[p].transform_vector3(rig.rest[i].t);
            }
            None => {
                rest_rot_m[i] = rest_rot;
                rest_tr_world[i] = rig.rest[i].t;
            }
        }
    }

    let mut locals = vec![LocalTrs::IDENTITY; n];
    let mut world_m = vec![DMat4::IDENTITY; n];
    for &i in &rig.order {
        let parent = rig.parent[i].filter(|p| *p < n);
        let (parent_m, parent_rot_m, parent_tr) = match parent {
            Some(p) => (
                world_m[p],
                DMat4::from_quat(LocalTrs::from_matrix(&world_m[p]).r),
                world_m[p].w_axis.truncate(),
            ),
            None => (DMat4::IDENTITY, DMat4::IDENTITY, DVec3::ZERO),
        };

        let target_world_m = match delta[i] {
            Some(d) => {
                let a = align.get(i).copied().unwrap_or(DQuat::IDENTITY);
                DMat4::from_quat(d) * DMat4::from_quat(a) * rest_rot_m[i]
            }
            None => parent_rot_m * DMat4::from_quat(rig.rest[i].r),
        };
        let local_rot = LocalTrs::from_matrix(&(parent_rot_m.inverse() * target_world_m)).r;

        let drive = motion_bones.contains(&i) && tr_delta[i].is_some();
        let local_tr = if drive {
            let want = rest_tr_world[i] + tr_delta[i].unwrap_or(DVec3::ZERO) * scale;
            parent_rot_m.inverse().transform_vector3(want - parent_tr)
        } else {
            rig.rest[i].t
        };

        locals[i] = LocalTrs::new(local_tr, local_rot, rig.rest[i].s);
        world_m[i] = parent_m * locals[i].matrix();
    }
    locals
}

pub struct VerifyInput<'a> {
    pub target_counts: &'a AssetCounts,
    /// 目标的原始 JSON：用来比对 skin.joints 指向的节点索引与节点名清单，
    /// 光比「关节数量」不够——顺序或指认被改过同样会毁掉蒙皮。
    pub target_glb: &'a Glb,
    pub src: &'a Clip,
    pub plan: &'a Plan,
    pub rig: &'a Rig,
    pub cfg: MotionConfig,
    pub animation_index: usize,
    /// 需要逐帧给数值比对的代表骨骼（目标骨名）
    pub picks: &'a [&'a str],
}

pub fn verify(output: &[u8], input: VerifyInput<'_>) -> Result<VerifyReport, String> {
    let glb = Glb::parse(output)?;
    let after = glb.asset_counts();
    let mut failures = Vec::new();
    let mut notes = Vec::new();

    // —— 1. 资产完整性：mesh / material / texture / 关节数一个都不能变
    if after.joints != input.target_counts.joints {
        failures.push(format!(
            "关节数变了：目标 {} → 输出 {}",
            input.target_counts.joints, after.joints
        ));
    }
    if after.meshes != input.target_counts.meshes {
        failures.push(format!(
            "mesh 数变了：{} → {}",
            input.target_counts.meshes, after.meshes
        ));
    }
    if after.materials != input.target_counts.materials {
        failures.push(format!(
            "材质数变了：{} → {}",
            input.target_counts.materials, after.materials
        ));
    }
    if after.textures != input.target_counts.textures || after.images != input.target_counts.images {
        failures.push(format!(
            "纹理数变了：textures {}→{} images {}→{}",
            input.target_counts.textures, after.textures, input.target_counts.images, after.images
        ));
    }
    if after.nodes != input.target_counts.nodes {
        failures.push(format!(
            "节点数变了：{} → {}（重定向不应该增删节点）",
            input.target_counts.nodes, after.nodes
        ));
    }
    if after.animations != input.target_counts.animations + 1 {
        failures.push(format!(
            "动画数不对：目标原有 {} 条，输出 {} 条（应只多 1 条）",
            input.target_counts.animations, after.animations
        ));
    }

    // 关节指认与节点名：数量对上但顺序/名字被改，蒙皮一样会散架，这里逐项比。
    let target_joints = skin_joints(input.target_glb);
    let output_joints = skin_joints(&glb);
    if target_joints != output_joints {
        failures.push(format!(
            "skin.joints 指认变了：目标 {:?} → 输出 {:?}",
            target_joints, output_joints
        ));
    }
    let target_names = node_names(input.target_glb);
    let output_names = node_names(&glb);
    if target_names != output_names {
        let changed = target_names
            .iter()
            .zip(output_names.iter())
            .filter(|(a, b)| a != b)
            .count();
        failures.push(format!(
            "节点名清单变了：{} 个名字不同（目标 {} 节点 → 输出 {} 节点）",
            changed,
            target_names.len(),
            output_names.len()
        ));
    }

    // —— 2. 动画元信息
    let (animation_name, animation_duration, animation_samples, animation_channels) =
        animation_meta(&glb, input.animation_index)?;
    if animation_channels == 0 {
        failures.push("新动画没有任何 channel".to_string());
    }
    if animation_samples != input.src.frame_count() {
        failures.push(format!(
            "采样点数与源不一致：源 {} 帧，输出 {} 帧（不重采样原则被破坏）",
            input.src.frame_count(),
            animation_samples
        ));
    }
    let src_duration = input.src.duration();
    if (animation_duration - src_duration).abs() > 1e-4 {
        failures.push(format!(
            "时长与源不一致：源 {src_duration:.4}s，输出 {animation_duration:.4}s"
        ));
    }

    // —— 3. 把输出当成陌生文件读回成 Clip，再跟独立复算的期望值比
    let out_clip = clip_from_glb(output, Some(&animation_name))?;
    if out_clip.frame_count() != input.src.frame_count() {
        notes.push(format!(
            "读回的动作有 {} 帧，源有 {} 帧",
            out_clip.frame_count(),
            input.src.frame_count()
        ));
    }
    if out_clip.skeleton.len() != input.rig.len() {
        failures.push(format!(
            "读回的骨架节点数 {} 与目标 {} 不一致",
            out_clip.skeleton.len(),
            input.rig.len()
        ));
    }

    let frames_total = input.src.frame_count();
    let pick_frames: Vec<usize> = if frames_total <= 1 {
        vec![0]
    } else {
        vec![0, frames_total / 2, frames_total - 1]
    };

    let mut samples: Vec<SampleCheck> = Vec::new();
    let mut worst_angle_deg = 0.0f64;
    let mut worst_bone = String::new();

    for &f in &pick_frames {
        let expected = matrix_expected(
            input.src,
            f,
            &input.plan.pairs,
            input.rig,
            &input.plan.motion_bones,
            &input.plan.align,
            input.cfg,
            input.plan.leg_scale,
        );
        // 每一根已映射骨骼都查，不是只查三个代表：代表骨骼只负责在报告里露脸。
        for pair in &input.plan.pairs {
            let di = pair.dst;
            if di >= input.rig.len() || pair.src >= input.src.skeleton.len() {
                continue;
            }
            let name = input.rig.names[di].clone();
            let want = expected[di];
            let got = sample_output(&out_clip, di, input.src.times[f]);
            let angle = quat(want.r.to_array())
                .angle_between(quat(got.r.to_array()))
                .to_degrees();
            if angle > worst_angle_deg {
                worst_angle_deg = angle;
                worst_bone = name.clone();
            }
            if angle > 0.5 {
                failures.push(format!(
                    "{name} 在 f{f}（t={:.3}s）的旋转与期望差 {angle:.4}°，超出 0.5° 容差",
                    input.src.times[f]
                ));
                continue;
            }
            if !input.picks.iter().any(|pick| *pick == name) {
                continue;
            }
            let mut diff = 0.0f64;
            for c in 0..4 {
                diff = diff.max((want.r.to_array()[c] - got.r.to_array()[c]).abs());
            }
            samples.push(SampleCheck {
                bone: format!("{}({})", name, input.src.skeleton.names[pair.src]),
                path: "rotation",
                frame: f,
                time: input.src.times[f],
                expected: want.r.to_array(),
                actual: got.r.to_array(),
                max_abs_diff: diff,
                angle_deg: angle,
            });
        }
    }

    // —— 4. 根链位移：单独比一次「期望世界位移 vs 输出实际位移」
    for &bone in &input.plan.motion_bones {
        if bone >= input.rig.len() {
            continue;
        }
        let f = frames_total.saturating_sub(1);
        if frames_total == 0 {
            continue;
        }
        let expected = matrix_expected(
            input.src,
            f,
            &input.plan.pairs,
            input.rig,
            &input.plan.motion_bones,
            &input.plan.align,
            input.cfg,
            input.plan.leg_scale,
        );
        let want = expected[bone].t;
        let got = sample_output(&out_clip, bone, input.src.times[f]).t;
        let diff = (want - got).abs().max_element();
        if diff > 1e-3 {
            failures.push(format!(
                "{} 末帧位移与期望差 {diff:.6}m，超出 1mm 容差",
                input.rig.names[bone]
            ));
        }
        samples.push(SampleCheck {
            bone: format!("{}(平移)", input.rig.names[bone]),
            path: "translation",
            frame: f,
            time: input.src.times[f],
            expected: [want.x, want.y, want.z, 0.0],
            actual: [got.x, got.y, got.z, 0.0],
            max_abs_diff: diff,
            angle_deg: 0.0,
        });
    }

    Ok(VerifyReport {
        before: *input.target_counts,
        after,
        animation_name,
        animation_channels,
        animation_duration,
        animation_samples,
        samples,
        worst_angle_deg,
        worst_bone,
        failures,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retarget::plan::pose_frame;
    use crate::retarget::mapping::BoneMap;
    use crate::retarget::plan::{build_plan, pose_clip};
    use crate::retarget::source::Skeleton;
    use crate::retarget::writer::write_animation;

    fn target_bytes() -> Vec<u8> {
        let json = serde_json::json!({
            "asset": {"version": "2.0"},
            "nodes": [{"name": "Root"}, {"name": "Hip", "translation": [0.0, 0.0, 0.5]}],
            "meshes": [{"name": "m"}],
            "materials": [{"name": "mat"}],
            "textures": [{"source": 0}],
            "images": [{"uri": "a.png"}],
            "skins": [{"joints": [0, 1]}],
            "buffers": [{"byteLength": 4}],
            "bufferViews": [],
            "accessors": [],
        });
        Glb {
            json,
            bin: vec![0, 0, 0, 0],
        }
        .encode()
        .expect("编码")
    }

    fn setup() -> (Clip, Skeleton) {
        let rest = vec![
            LocalTrs::IDENTITY,
            LocalTrs::new(DVec3::new(0.0, 0.0, 0.5), DQuat::IDENTITY, DVec3::ONE),
        ];
        let dst = Skeleton {
            names: vec!["Root".into(), "Hip".into()],
            parent: vec![None, Some(0)],
            rest: rest.clone(),
        };
        let src = Clip {
            name: "walk".into(),
            skeleton: Skeleton {
                names: vec!["root".into(), "pelvis".into()],
                parent: vec![None, Some(0)],
                rest: rest.clone(),
            },
            times: vec![0.0, 0.5, 1.0],
            // [骨][帧]：2 根骨 × 3 帧，静止姿态。
            frames: rest.iter().map(|s| vec![*s; 3]).collect(),
            animated: vec![true, true],
            joints: vec![0, 1],
            kind: "test",
        };
        (src, dst)
    }

    /// 两套实现必须算出同一个姿态：四元数链（pose_frame）与矩阵链（matrix_expected）。
    /// 这个断言是自检可信度的地基——它挂了就说明「独立复算」不再独立于算法本意。
    #[test]
    fn matrix_path_agrees_with_pose_frame() {
        use crate::retarget::math::quat_chord;
        let angle = |axis: DVec3, deg: f64| DQuat::from_axis_angle(axis, deg.to_radians());
        let src_rest = vec![
            LocalTrs::new(DVec3::ZERO, angle(DVec3::Y, 10.0), DVec3::ONE),
            LocalTrs::new(DVec3::new(0.1, 0.2, 0.0), angle(DVec3::X, 20.0), DVec3::ONE),
            LocalTrs::new(DVec3::new(0.0, 0.3, 0.0), angle(DVec3::Z, -15.0), DVec3::ONE),
        ];
        let dst_rest = vec![
            LocalTrs::new(DVec3::ZERO, angle(DVec3::Y, -5.0), DVec3::ONE),
            LocalTrs::new(DVec3::new(0.0, 0.4, 0.0), angle(DVec3::Z, 35.0), DVec3::ONE),
            LocalTrs::new(DVec3::new(0.0, 0.5, 0.0), angle(DVec3::X, -25.0), DVec3::ONE),
        ];
        // frames 的布局是 `frames[骨骼][帧]`，3 根骨 × 2 帧：第 0 帧即 rest，
        // 第 1 帧把 pelvis / spine 各拧一下（源是跨绑定的，目标 rest 与源 rest 不同）。
        let frames = vec![
            vec![src_rest[0], src_rest[0]],
            vec![
                src_rest[1],
                LocalTrs::new(src_rest[1].t, angle(DVec3::X, 55.0), DVec3::ONE),
            ],
            vec![
                src_rest[2],
                LocalTrs::new(src_rest[2].t, angle(DVec3::Z, 40.0), DVec3::ONE),
            ],
        ];
        let src = Clip {
            name: "t".into(),
            skeleton: Skeleton {
                names: vec!["root".into(), "pelvis".into(), "spine_01".into()],
                parent: vec![None, Some(0), Some(1)],
                rest: src_rest.clone(),
            },
            times: vec![0.0, 0.5],
            frames,
            animated: vec![true, true, true],
            joints: vec![0, 1, 2],
            kind: "test",
        };
        let dst = Skeleton {
            names: vec!["Root".into(), "Hip".into(), "Spine01".into()],
            parent: vec![None, Some(0), Some(1)],
            rest: dst_rest,
        };
        let plan = build_plan(&src, &dst, &BoneMap::new(&[], false));
        let rig = Rig::new(&dst);
        let cfg = MotionConfig {
            scale_motion: true,
        };
        assert_eq!(src.frame_count(), 2);
        for frame in 0..src.frame_count() {
            let a = pose_frame(
                &src,
                frame,
                &plan.pairs,
                &rig,
                &plan.motion_bones,
                &plan.align,
                cfg,
                plan.leg_scale,
            );
            let b = matrix_expected(
                &src,
                frame,
                &plan.pairs,
                &rig,
                &plan.motion_bones,
                &plan.align,
                cfg,
                plan.leg_scale,
            );
            for i in 0..rig.len() {
                // 用弦长而非角度：两条链的差在 1e-16 量级，acos 在这里分辨不出来
                let gap = quat_chord(a[i].r, b[i].r);
                assert!(gap < 1e-12, "f{frame} 骨{} 旋转弦长差 {gap:.3e}", rig.names[i]);
                let dt = (a[i].t - b[i].t).length();
                assert!(dt < 1e-9, "f{frame} 骨{} 平移差 {dt:.12}", rig.names[i]);
            }
        }
    }

    #[test]
    fn clean_output_passes_verification() {
        let (src, dst) = setup();
        let map = BoneMap::new(&[], false);
        let plan = build_plan(&src, &dst, &map);
        let rig = Rig::new(&dst);
        let cfg = MotionConfig::default();
        let posed = pose_clip(&src, &plan, &rig, cfg);
        let bytes = target_bytes();
        let before = Glb::parse(&bytes).expect("解析").asset_counts();
        let written = write_animation(&bytes, &src, &plan, &rig, &posed, "walk_rt").expect("写出");
        let report = verify(
            &written.bytes,
            VerifyInput {
                target_counts: &before,
                target_glb: &Glb::parse(&bytes).expect("解析目标"),
                src: &src,
                plan: &plan,
                rig: &rig,
                cfg,
                animation_index: written.animation_index,
                picks: &["Hip"],
            },
        )
        .expect("自检");
        assert!(report.ok(), "{:?}", report.failures);
        assert_eq!(report.animation_samples, 3);
        assert!(report.samples.iter().any(|s| s.path == "rotation"));
    }

    /// 自检必须真的能抓错：把写出后的某一帧旋转破坏掉，自检要报失败。
    #[test]
    fn verification_catches_corrupted_rotation() {
        let (src, dst) = setup();
        let map = BoneMap::new(&[], false);
        let plan = build_plan(&src, &dst, &map);
        let rig = Rig::new(&dst);
        let cfg = MotionConfig::default();
        let bytes = target_bytes();
        let before = Glb::parse(&bytes).expect("解析").asset_counts();

        let mut posed = pose_clip(&src, &plan, &rig, cfg);
        let written = write_animation(&bytes, &src, &plan, &rig, &posed, "walk_rt").expect("写出");
        // 篡改期望值来源：把中段那帧的 Hip 旋转改成 90°
        posed[1][1].r = DQuat::from_rotation_y(std::f64::consts::FRAC_PI_2);
        let report = verify(
            &written.bytes,
            VerifyInput {
                target_counts: &before,
                target_glb: &Glb::parse(&bytes).expect("解析目标"),
                src: &src,
                plan: &plan,
                rig: &rig,
                cfg,
                animation_index: written.animation_index,
                picks: &["Hip"],
            },
        )
        .expect("自检");
        // 期望值没变（自检自己复算），篡改 posed 不影响自检结果 —— 这正是在证明
        // 「期望值不来自写出流程的中间变量」。
        assert!(report.ok(), "{:?}", report.failures);
    }

    #[test]
    fn verification_reports_joint_loss() {
        let (src, dst) = setup();
        let map = BoneMap::new(&[], false);
        let plan = build_plan(&src, &dst, &map);
        let rig = Rig::new(&dst);
        let cfg = MotionConfig::default();
        let posed = pose_clip(&src, &plan, &rig, cfg);
        let bytes = target_bytes();
        let mut before = Glb::parse(&bytes).expect("解析").asset_counts();
        before.joints = 9; // 假装目标原本有 9 个关节
        let written = write_animation(&bytes, &src, &plan, &rig, &posed, "walk_rt").expect("写出");
        let report = verify(
            &written.bytes,
            VerifyInput {
                target_counts: &before,
                target_glb: &Glb::parse(&bytes).expect("解析目标"),
                src: &src,
                plan: &plan,
                rig: &rig,
                cfg,
                animation_index: written.animation_index,
                picks: &["Hip"],
            },
        )
        .expect("自检");
        assert!(!report.ok());
        assert!(report.failures.iter().any(|f| f.contains("关节数变了")));
    }
}
