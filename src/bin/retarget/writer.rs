//! 写出层：把目标 GLB 原样搬运，只往 JSON 追加 animations 与配套 accessor。
//!
//! 三条硬约束：
//!
//! 1. **只追加不重排**：`meshes` / `materials` / `textures` / `images` / `skins` /
//!    `accessors[0..N]` 全部按原字节保留，75 MB 的贴图不会因为写动画被重新打包；
//! 2. **采样时刻与源一致**：`input` accessor 直接写源动作的时间轴（UE 的 81 帧就是
//!    81 帧），不重采样、不降帧，`interpolation` 沿用源的 LINEAR/STEP/CUBICSPLINE；
//! 3. **轨道只写真正需要的**：每根有增量或位移的骨各 1 条 rotation 通道，根链额外 1 条
//!    translation；scale 一律不写（目标 rest 的缩放本来就没变过）。

use glam::DQuat;
use serde_json::{json, Value};

use super::glb::Glb;
use super::math::{LocalTrs, same_hemisphere};
use super::plan::{Plan, Rig};
use super::source::Clip;

/// 一根骨的输出轨道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Rotation,
    Translation,
}

impl Path {
    pub fn as_str(&self) -> &'static str {
        match self {
            Path::Rotation => "rotation",
            Path::Translation => "translation",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TrackPlan {
    pub bone: usize,
    pub path: Path,
}

#[derive(Debug, Clone)]
pub struct Written {
    pub bytes: Vec<u8>,
    pub animation_index: usize,
    pub channels: usize,
    pub keyframes: usize,
    pub duration: f64,
    pub tracks: Vec<TrackPlan>,
}

/// 哪些骨需要写哪条轨道。
///
/// - 已映射骨：写 rotation（增量可能为零，但零增量必须显式写出来，否则目标骨会保持
///   自己的 rest 世界朝向，在父骨转动时表现为「该跟着转却没转」）；
/// - 未映射骨：不写。它的局部旋转就是 rest，写进去只是浪费 accessor。
/// - 根链骨：额外写 translation。
pub fn track_plan(plan: &Plan, rig: &Rig) -> Vec<TrackPlan> {
    let mut out = Vec::new();
    for pair in &plan.pairs {
        if pair.dst >= rig.len() {
            continue;
        }
        out.push(TrackPlan {
            bone: pair.dst,
            path: Path::Rotation,
        });
    }
    for &bone in &plan.motion_bones {
        if bone < rig.len() && !plan.pairs.iter().any(|p| p.dst == bone) {
            out.push(TrackPlan {
                bone,
                path: Path::Rotation,
            });
        }
        out.push(TrackPlan {
            bone,
            path: Path::Translation,
        });
    }
    out.sort_by_key(|t| (t.bone, t.path.as_str()));
    out.dedup_by(|a, b| a.bone == b.bone && a.path == b.path);
    out
}

fn sampler_interpolation(src: &Clip) -> &'static str {
    // 源读取层已经把 CUBICSPLINE 展开成逐帧采样值，输出按逐帧 LINEAR 写，
    // 数学上与原曲线在采样点重合，且不会因为 tangents 缺失被查看器判非法。
    if src.frame_count() <= 1 {
        "STEP"
    } else {
        "LINEAR"
    }
}

/// 把姿态序列写进目标 GLB，返回新文件字节。
pub fn write_animation(
    target_bytes: &[u8],
    src: &Clip,
    plan: &Plan,
    rig: &Rig,
    posed: &[Vec<LocalTrs>],
    anim_name: &str,
) -> Result<Written, String> {
    let mut glb = Glb::parse(target_bytes)?;

    let frames = posed.len();
    if frames == 0 {
        return Err("姿态序列为空，没有可写的动画".to_string());
    }
    let times: Vec<f64> = src.times.clone();
    if times.len() != frames {
        return Err(format!(
            "时间轴 {} 个采样点与姿态 {} 帧不一致",
            times.len(),
            frames
        ));
    }

    let tracks = track_plan(plan, rig);
    if tracks.is_empty() {
        return Err("没有任何轨道可写：映射表没命中目标的任何骨骼".to_string());
    }

    // input：源时间轴原样写入（含 min/max，glTF 强制要求）
    let input = glb.push_f32_accessor(&times, "SCALAR", true)?;

    let mut samplers: Vec<Value> = Vec::new();
    let mut channels: Vec<Value> = Vec::new();
    let mut keyframes = 0usize;

    for track in &tracks {
        let values: Vec<f64> = match track.path {
            Path::Rotation => {
                // 四元数半球对齐：相邻帧点积为负会让插值绕远路，表现为抽搐
                let mut prev: Option<DQuat> = None;
                let mut flat = Vec::with_capacity(frames * 4);
                for local in posed {
                    let q = local[track.bone].r.normalize();
                    let q = match prev {
                        Some(p) => same_hemisphere(q, p),
                        None => q,
                    };
                    prev = Some(q);
                    flat.extend_from_slice(&[q.x, q.y, q.z, q.w]);
                }
                flat
            }
            Path::Translation => {
                let mut flat = Vec::with_capacity(frames * 3);
                for local in posed {
                    let t = local[track.bone].t;
                    flat.extend_from_slice(&[t.x, t.y, t.z]);
                }
                flat
            }
        };
        let ty = match track.path {
            Path::Rotation => "VEC4",
            Path::Translation => "VEC3",
        };
        let output = glb.push_f32_accessor(&values, ty, false)?;
        keyframes += frames;
        let sampler_index = samplers.len();
        samplers.push(json!({
            "input": input,
            "output": output,
            "interpolation": sampler_interpolation(src),
        }));
        channels.push(json!({
            "sampler": sampler_index,
            "target": { "node": track.bone, "path": track.path.as_str() },
        }));
    }

    let animation = json!({
        "name": anim_name,
        "samplers": samplers,
        "channels": channels,
    });

    // animations 数组可能不存在（Tripo 资产就是 0 条）：保留已有动画，新动画追加在尾部
    match glb.json.get_mut("animations").and_then(Value::as_array_mut) {
        Some(list) => list.push(animation),
        None => {
            glb.json["animations"] = json!([animation]);
        }
    }
    let animation_index = glb
        .json
        .get("animations")
        .and_then(Value::as_array)
        .map(|a| a.len() - 1)
        .unwrap_or(0);

    let bytes = glb.encode()?;
    Ok(Written {
        bytes,
        animation_index,
        channels: channels.len(),
        keyframes,
        duration: src.duration(),
        tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retarget::math::LocalTrs;
    use crate::retarget::source::{Clip, Skeleton};
    use glam::{DQuat, DVec3};

    fn skeleton(names: &[&str], parent: &[Option<usize>], rest: Vec<LocalTrs>) -> Skeleton {
        Skeleton {
            names: names.iter().map(|s| s.to_string()).collect(),
            parent: parent.to_vec(),
            rest,
        }
    }

    fn target_bytes() -> Vec<u8> {
        let json = json!({
            "asset": {"version": "2.0"},
            "nodes": [
                {"name": "Root"},
                {"name": "Hip", "translation": [0.0, 0.0, 0.5]},
            ],
            "meshes": [{"name": "m"}],
            "materials": [{"name": "mat"}],
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

    fn src_clip() -> Clip {
        let rest = vec![LocalTrs::IDENTITY, LocalTrs::new(DVec3::ZERO, DQuat::IDENTITY, DVec3::ONE)];
        Clip {
            name: "walk".into(),
            skeleton: skeleton(&["root", "pelvis"], &[None, Some(0)], rest.clone()),
            times: vec![0.0, 0.5, 1.0],
            // [骨][帧]：2 根骨 × 3 帧，静止姿态。
            frames: rest.iter().map(|s| vec![*s; 3]).collect(),
            animated: vec![true, true],
            joints: vec![0, 1],
            kind: "test",
        }
    }

    #[test]
    fn writes_animation_and_keeps_asset_intact() {
        let src = src_clip();
        let dst_skel = skeleton(
            &["Root", "Hip"],
            &[None, Some(0)],
            vec![
                LocalTrs::IDENTITY,
                LocalTrs::new(DVec3::new(0.0, 0.0, 0.5), DQuat::IDENTITY, DVec3::ONE),
            ],
        );
        let map = crate::retarget::mapping::BoneMap::new(&[], false);
        let plan = crate::retarget::plan::build_plan(&src, &dst_skel, &map);
        let rig = Rig::new(&dst_skel);
        let posed = crate::retarget::plan::pose_clip(
            &src,
            &plan,
            &rig,
            crate::retarget::plan::MotionConfig::default(),
        );
        let written = write_animation(&target_bytes(), &src, &plan, &rig, &posed, "walk_rt")
            .expect("写出");
        assert_eq!(written.channels, written.tracks.len());
        assert!(written.keyframes >= 3);

        let back = Glb::parse(&written.bytes).expect("读回");
        let counts = back.asset_counts();
        assert_eq!(counts.meshes, 1);
        assert_eq!(counts.materials, 1);
        assert_eq!(counts.joints, 2);
        assert_eq!(counts.animations, 1);
        // 2 根骨各一条 rotation；root 与 pelvis 都在位移承载链上（ROOT_MOTION_SOURCES），
        // 所以 translation 也是 2 条 —— 输出报告里「承载位移的骨骼: Root, Hip」即此项。
        assert_eq!(counts.channels, 4);
        assert_eq!(written.bytes.len() % 4, 0);
    }

    #[test]
    fn rejects_mismatched_timeline() {
        let mut src = src_clip();
        let dst_skel = skeleton(&["Root"], &[None], vec![LocalTrs::IDENTITY]);
        let map = crate::retarget::mapping::BoneMap::new(&[], false);
        let plan = crate::retarget::plan::build_plan(&src, &dst_skel, &map);
        let rig = Rig::new(&dst_skel);
        let posed = crate::retarget::plan::pose_clip(
            &src,
            &plan,
            &rig,
            crate::retarget::plan::MotionConfig::default(),
        );
        src.times.pop();
        let err = write_animation(&target_bytes(), &src, &plan, &rig, &posed, "x")
            .expect_err("应当报错");
        assert!(err.contains("不一致"), "{err}");
    }
}
