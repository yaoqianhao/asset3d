//! 人话报告：把映射规划、写出统计、读回自检拼成一段可以直接贴给同事看的文本。

use std::fmt::Write as _;

use super::plan::{Plan, Rig};
use super::source::Clip;
use super::verify::VerifyReport;
use super::writer::Written;

pub struct ReportInput<'a> {
    pub source_path: &'a str,
    pub target_path: &'a str,
    pub out_path: &'a str,
    pub src: &'a Clip,
    pub plan: &'a Plan,
    pub rig: &'a Rig,
    pub written: &'a Written,
    pub verify: &'a VerifyReport,
    pub scale_motion: bool,
    pub no_twist: bool,
    pub map_file: Option<&'a str>,
}

pub fn render(input: ReportInput<'_>) -> String {
    let plan = input.plan;
    let written = input.written;
    let verify = input.verify;
    let mut out = String::new();

    let _ = writeln!(out, "============================================================");
    let _ = writeln!(out, " asset3d-retarget 重定向报告");
    let _ = writeln!(out, "============================================================");
    let _ = writeln!(out, "源动作  : {}", input.source_path);
    let _ = writeln!(
        out,
        "          动作 \"{}\" / {} 帧 / {:.4}s / 格式 {} / 蒙皮关节 {}",
        input.src.name,
        input.src.frame_count(),
        input.src.duration(),
        input.src.kind,
        input.src.joints.len()
    );
    let _ = writeln!(out, "目标角色: {}", input.target_path);
    let _ = writeln!(out, "输出    : {}", input.out_path);
    let _ = writeln!(
        out,
        "选项    : scale_motion={} no_twist={} map={}",
        input.scale_motion,
        input.no_twist,
        input.map_file.unwrap_or("<内置表>")
    );

    let _ = writeln!(out, "\n-- 映射 ----------------------------------------------------");
    let _ = writeln!(
        out,
        "骨骼映射覆盖率: {:.1}%（源侧带动画的骨 {} 根，命中 {} 根）",
        plan.coverage * 100.0,
        plan.animated_src,
        plan.animated_mapped
    );
    let _ = writeln!(
        out,
        "源骨架 {} 骨 → 目标骨架 {} 骨，最终建立 {} 对映射（其中 --map 覆盖 {} 对）",
        plan.src_bones,
        plan.dst_bones,
        plan.pairs.len(),
        plan.pairs.iter().filter(|p| p.from_map).count()
    );
    let _ = writeln!(out, "映射明细（源 → 目标）:");
    for pair in &plan.pairs {
        let _ = writeln!(
            out,
            "  {:<22} → {:<20} {}",
            input.src.skeleton.names[pair.src],
            name_of(input.rig, pair.dst),
            if pair.from_map { "[--map]" } else { "" }
        );
    }

    // 动作幅度：每根已映射骨在整个动作里的最大转角。全部接近 0 就说明写出来的
    // 是一段「会播但不动」的动画，这种事光看轨道数看不出来。
    let mut swing_rows: Vec<(String, f64)> = Vec::new();
    for pair in &plan.pairs {
        let f0 = 0usize;
        let f1 = input.src.frame_count();
        let deg = input.src.rotation_swing(pair.src, f0, f1);
        if deg >= 1.0 {
            swing_rows.push((name_of(input.rig, pair.dst), deg));
        }
    }
    swing_rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    let _ = writeln!(
        out,
        "静态通过的骨: {} 根映射骨里 {} 根的最大转角 ≥ 1°（其余保持 rest，符合「未驱动即静止」）",
        plan.pairs.len(),
        swing_rows.len()
    );
    if !swing_rows.is_empty() {
        let _ = writeln!(out, "动作幅度（目标骨 / 全段最大转角）:");
        for (name, deg) in swing_rows.iter().take(8) {
            let _ = writeln!(out, "  {:<22} {:>7.2}°", name, deg);
        }
    } else {
        let _ = writeln!(out, "  ! 所有映射骨的最大转角都小于 1°，这段动作几乎是静止的，请核对源动画选择");
    }

    if !plan.unmapped_src.is_empty() {
        let _ = writeln!(
            out,
            "\n未映射的源骨骼（{} 根，逐条说明处理方式）:",
            plan.unmapped_src.len()
        );
        for item in &plan.unmapped_src {
            let hint = match &item.suggestion {
                Some(s) => format!("可考虑 --map \"{}\": \"{}\"", item.name, s),
                None => "无可用候选".to_string(),
            };
            let _ = writeln!(out, "  {:<22} {}（{}）", item.name, item.reason, hint);
        }
    }

    if !plan.missing_target.is_empty() {
        let _ = writeln!(
            out,
            "\n目标侧未驱动骨骼（{} 根）:",
            plan.missing_target.len()
        );
        for item in &plan.missing_target {
            let _ = writeln!(out, "  {:<22} {}", item.name, item.note);
        }
    }

    let _ = writeln!(out, "\n-- 位移 ----------------------------------------------------");
    let _ = writeln!(
        out,
        "腿长比（目标/源）= {:.4}（源 {:.4}m / 目标 {:.4}m），位移缩放={}",
        plan.leg_scale,
        plan.src_leg_len,
        plan.dst_leg_len,
        if input.scale_motion { "开" } else { "关（1:1）" }
    );
    let _ = writeln!(
        out,
        "承载位移的骨骼: {}",
        plan.motion_bones
            .iter()
            .map(|b| name_of(input.rig, *b))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let _ = writeln!(out, "\n-- 输出 ----------------------------------------------------");
    let _ = writeln!(
        out,
        "文件字节数      : {} 字节（{:.2} MB）",
        written.bytes.len(),
        written.bytes.len() as f64 / 1048576.0
    );
    let _ = writeln!(out, "animations      : {}", verify.after.animations);
    let _ = writeln!(
        out,
        "新动画          : \"{}\"（{} 条 channel / {} 个关键帧 / {:.4}s / {} 个采样点）",
        verify.animation_name,
        verify.animation_channels,
        written.keyframes,
        verify.animation_duration,
        verify.animation_samples
    );
    let _ = writeln!(
        out,
        "写入口径        : {} 条 channel / {} 个采样点 / 采样时刻与源一一对齐（不重采样）",
        written.channels, verify.animation_samples
    );
    let _ = writeln!(
        out,
        "资产保留        : mesh {} / material {} / texture {} / image {} / skin {} / joints {} / nodes {}",
        verify.after.meshes,
        verify.after.materials,
        verify.after.textures,
        verify.after.images,
        verify.after.skins,
        verify.after.joints,
        verify.after.nodes
    );

    let _ = writeln!(out, "\n-- 读回自检 -----------------------------------------------");
    let _ = writeln!(
        out,
        "关节数 {} → {}；mesh {} → {}；材质 {} → {}；纹理 {} → {}；动画 {} → {}",
        verify.before.joints,
        verify.after.joints,
        verify.before.meshes,
        verify.after.meshes,
        verify.before.materials,
        verify.after.materials,
        verify.before.textures,
        verify.after.textures,
        verify.before.animations,
        verify.after.animations
    );
    for sample in &verify.samples {
        let _ = writeln!(out, "  {}", sample.line());
    }
    let _ = writeln!(
        out,
        "所有已映射骨旋转的最大角度误差: {:.4}°（{}）",
        verify.worst_angle_deg,
        if verify.worst_bone.is_empty() {
            "-"
        } else {
            &verify.worst_bone
        }
    );
    for note in &verify.notes {
        let _ = writeln!(out, "  备注: {note}");
    }

    if !plan.warnings.is_empty() {
        let _ = writeln!(out, "\n-- 警告 ----------------------------------------------------");
        for w in &plan.warnings {
            let _ = writeln!(out, "  ! {w}");
        }
    }

    if verify.ok() {
        let _ = writeln!(out, "\n自检结论: 通过（逐项比对全部在容差内）");
    } else {
        let _ = writeln!(out, "\n自检结论: 失败");
        for f in &verify.failures {
            let _ = writeln!(out, "  x {f}");
        }
    }
    let _ = writeln!(out, "============================================================");
    out
}

/// 目标骨名（只做展示用）。
fn name_of(rig: &Rig, bone: usize) -> String {
    rig.names
        .get(bone)
        .cloned()
        .unwrap_or_else(|| format!("#{bone}"))
}
