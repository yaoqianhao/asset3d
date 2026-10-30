//! `asset3d-retarget` —— 把 UE5 Mannequin 的动作序列重定向到 Tripo 绑定的人形角色。
//!
//! 一次调用做四件事：读源动作 → 建映射 → 逐帧求解 → 写出新 GLB 并读回自检。
//! 全过程不碰目标资产已有的 mesh / material / skin / 纹理，只在 JSON 尾部追加
//! `animations` 与配套 accessor。

mod retarget;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use retarget::glb::Glb;
use retarget::mapping::{load_map_file, BoneMap, MapOverride};
use retarget::plan::{build_plan, pose_clip, MotionConfig, Rig};
use retarget::report::{render, ReportInput};
use retarget::source::{load_clip, skeleton_from_glb};
use retarget::verify::{verify, VerifyInput};
use retarget::writer::write_animation;

const USAGE: &str = "\
asset3d-retarget —— UE5 Mannequin 动作 → Tripo 人形角色 重定向

用法:
  asset3d-retarget --source <src.glb> --target <target.glb> --out <dst.glb> [选项]

必填:
  --source <path>        源动作文件（UE5 骨骼的 .glb / .fbx，如 Bow_Attack_Seq.glb）
  --target <path>        目标角色（Tripo 绑定的 .glb，带 mesh + skin，可无动画）
  --out <path>           输出 .glb 路径（新文件，不会覆盖输入）

选项:
  --map <json>           追加/覆盖骨骼映射。两种写法都支持：
                           {\"spine_04\": \"Spine02\", \"neck_02\": \"NeckTwist02\"}
                         [ {\"source\": \"spine_04\", \"target\": \"Spine02\"} ]
  --anim <name>          源文件有多条动作时指定用哪条（默认第 0 条）
  --scale-motion         根位移按「目标腿长 / 源腿长」缩放（默认开）
  --no-scale-motion      根位移 1:1 搬运（两侧骨架比例接近时才用）
  --no-twist             *Twist01/02 一组辅助骨全部不映射（排除扭转骨干扰时用）
  --picks a,b,c          自检时逐帧数值比对的代表骨骼（默认自动挑 upperarm/thigh/spine）
  --report <path>        把报告另存为文本文件
  --quiet                只打印结论行，不打印完整报告
  -h, --help             显示本帮助

示例:
  asset3d-retarget --source D:\\share\\Bow_Attack_Seq.glb \\
                   --target D:\\share\\character\\rebelsoldier.glb \\
                   --out    D:\\share\\character\\rebelsoldier_bow_attack.glb

退出码: 0 成功且自检通过 / 1 用法或读写错误 / 2 自检不通过（文件已写出但不合格）
";

#[derive(Debug)]
struct Args {
    source: PathBuf,
    target: PathBuf,
    out: PathBuf,
    map: Option<PathBuf>,
    anim: Option<String>,
    scale_motion: bool,
    no_twist: bool,
    picks: Option<Vec<String>>,
    report: Option<PathBuf>,
    quiet: bool,
}

/// 取当前参数后面的值；越界就是用户少写了值。
fn take(argv: &[String], i: &mut usize, name: &str) -> Result<String, String> {
    *i += 1;
    argv.get(*i)
        .cloned()
        .ok_or_else(|| format!("{name} 后面缺一个参数值\n\n{USAGE}"))
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut source: Option<PathBuf> = None;
    let mut target: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut map = None;
    let mut anim = None;
    let mut scale_motion = true;
    let mut no_twist = false;
    let mut picks = None;
    let mut report = None;
    let mut quiet = false;

    let mut i = 0usize;
    while i < argv.len() {
        let arg = argv[i].clone();
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--source" => source = Some(PathBuf::from(take(&argv, &mut i, "--source")?)),
            "--target" => target = Some(PathBuf::from(take(&argv, &mut i, "--target")?)),
            "--out" => out = Some(PathBuf::from(take(&argv, &mut i, "--out")?)),
            "--map" => map = Some(PathBuf::from(take(&argv, &mut i, "--map")?)),
            "--anim" => anim = Some(take(&argv, &mut i, "--anim")?),
            "--report" => report = Some(PathBuf::from(take(&argv, &mut i, "--report")?)),
            "--picks" => {
                let raw = take(&argv, &mut i, "--picks")?;
                let list: Vec<String> = raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if list.is_empty() {
                    return Err(format!("--picks 至少要给一个骨骼名\n\n{USAGE}"));
                }
                picks = Some(list);
            }
            "--scale-motion" => scale_motion = true,
            "--no-scale-motion" => scale_motion = false,
            "--no-twist" => no_twist = true,
            "--quiet" => quiet = true,
            other => {
                return Err(format!(
                    "认不出参数 `{other}`（不支持的写法还是拼错了？）\n\n{USAGE}"
                ))
            }
        }
        i += 1;
    }

    let source = source.ok_or_else(|| format!("缺 --source\n\n{USAGE}"))?;
    let target = target.ok_or_else(|| format!("缺 --target\n\n{USAGE}"))?;
    let out = out.ok_or_else(|| format!("缺 --out\n\n{USAGE}"))?;

    if !source.is_file() {
        return Err(format!("源文件不存在：{}", source.display()));
    }
    if !target.is_file() {
        return Err(format!("目标角色不存在：{}", target.display()));
    }
    if out
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| !e.eq_ignore_ascii_case("glb"))
        .unwrap_or(true)
    {
        return Err("--out 必须以 .glb 结尾（本工具只写 GLB 容器）".to_string());
    }
    let out_norm = normalize(&out);
    if out_norm == normalize(&source) || out_norm == normalize(&target) {
        return Err("--out 不能和 --source / --target 指向同一个文件（会毁掉原始资产）".to_string());
    }
    if let Some(m) = &map {
        if !m.is_file() {
            return Err(format!("映射文件不存在：{}", m.display()));
        }
    }

    Ok(Some(Args {
        source,
        target,
        out,
        map,
        anim,
        scale_motion,
        no_twist,
        picks,
        report,
        quiet,
    }))
}

fn normalize(p: &Path) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().to_lowercase())
        .unwrap_or_else(|_| p.to_string_lossy().to_lowercase())
}

/// 自检的代表骨骼：优先挑 upperarm / thigh / spine，挑不到就取前三个已映射骨。
fn pick_bones(rig: &Rig, plan: &retarget::plan::Plan, want: Option<&Vec<String>>) -> Vec<String> {
    if let Some(list) = want {
        return list.clone();
    }
    let mut out: Vec<String> = Vec::new();
    for keyword in ["Upperarm", "Thigh", "Spine"] {
        let found = plan
            .pairs
            .iter()
            .map(|p| &rig.names[p.dst])
            .find(|name| name.contains(keyword))
            .cloned();
        if let Some(name) = found {
            out.push(name);
        }
    }
    if out.is_empty() {
        for pair in plan.pairs.iter().take(3) {
            out.push(rig.names[pair.dst].clone());
        }
    }
    out
}

fn run(args: Args) -> Result<u8, String> {
    // —— 读源动作
    let src_bytes = std::fs::read(&args.source)
        .map_err(|e| format!("读不到源文件 {}：{e}", args.source.display()))?;
    let src = load_clip(&args.source, &src_bytes, args.anim.as_deref())
        .map_err(|e| format!("解析源动作失败：{e}"))?;
    if src.frame_count() == 0 {
        return Err("源动作里一帧都没有，无法重定向".to_string());
    }

    // —— 读目标角色骨架
    let tgt_bytes = std::fs::read(&args.target)
        .map_err(|e| format!("读不到目标角色 {}：{e}", args.target.display()))?;
    let tgt_glb = Glb::parse(&tgt_bytes).map_err(|e| format!("解析目标角色失败：{e}"))?;
    let tgt_skel = skeleton_from_glb(&tgt_glb).map_err(|e| format!("目标角色骨架读不出来：{e}"))?;
    let target_counts = tgt_glb.asset_counts();
    if tgt_skel.is_empty() {
        return Err("目标角色没有节点/骨架，这不是一个绑定好的角色".to_string());
    }

    // —— 映射与规划
    let overrides: Vec<MapOverride> = match &args.map {
        Some(path) => load_map_file(path)?,
        None => Vec::new(),
    };
    let map = BoneMap::new(&overrides, args.no_twist);
    let plan = build_plan(&src, &tgt_skel, &map);
    if plan.pairs.is_empty() {
        return Err(
            "映射表一条都没命中目标骨架：目标骨骼命名既不是 Tripo 风格也没有 --map 覆盖，\
             无法继续。可以用 --map 指定对应关系。"
                .to_string(),
        );
    }
    let rig = Rig::new(&tgt_skel);

    // —— 逐帧求解
    let cfg = MotionConfig {
        scale_motion: args.scale_motion,
    };
    let posed = pose_clip(&src, &plan, &rig, cfg);

    // —— 写出
    let anim_name = format!("{}_retarget", src.name);
    let written = write_animation(&tgt_bytes, &src, &plan, &rig, &posed, &anim_name)?;
    if written.bytes.len() % 4 != 0 {
        return Err(format!(
            "写出字节数 {} 不是 4 的倍数，GLB 容器对齐有问题",
            written.bytes.len()
        ));
    }
    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建不出输出目录 {}：{e}", parent.display()))?;
        }
    }
    std::fs::write(&args.out, &written.bytes)
        .map_err(|e| format!("写不出 {}：{e}", args.out.display()))?;

    // —— 读回自检
    let picks = pick_bones(&rig, &plan, args.picks.as_ref());
    let pick_refs: Vec<&str> = picks.iter().map(String::as_str).collect();
    let report = verify(
        &written.bytes,
        VerifyInput {
            target_counts: &target_counts,
            target_glb: &tgt_glb,
            src: &src,
            plan: &plan,
            rig: &rig,
            cfg,
            animation_index: written.animation_index,
            picks: &pick_refs,
        },
    )?;

    let out_abs = std::fs::canonicalize(&args.out)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| args.out.to_string_lossy().to_string());
    let text = render(ReportInput {
        source_path: &args.source.to_string_lossy(),
        target_path: &args.target.to_string_lossy(),
        out_path: &out_abs,
        src: &src,
        plan: &plan,
        rig: &rig,
        written: &written,
        verify: &report,
        scale_motion: args.scale_motion,
        no_twist: args.no_twist,
        map_file: args.map.as_ref().map(|p| p.to_string_lossy()).as_deref(),
    });
    // 写入口径与读回口径必须一致：不一致说明 JSON 里的 channel 数和实际写出的
    // 轨道数对不上，属于拿来就能骗过「看数量」的假成功。
    if written.channels != report.animation_channels {
        return Err(format!(
            "写入口径不一致：writer 写了 {} 条 channel，读回 JSON 只有 {} 条",
            written.channels, report.animation_channels
        ));
    }
    if (written.duration - report.animation_duration).abs() > 1e-6 {
        return Err(format!(
            "时长对不上：writer 算出 {:.6}s，读回 JSON 是 {:.6}s",
            written.duration, report.animation_duration
        ));
    }
    println!(
        "映射表 {} 条（内置 + --map）→ 命中 {} 对，写出 {} 条轨道",
        map.len(),
        plan.pairs.len(),
        written.tracks.len()
    );
    println!(
        "输出 {} | {} 字节 | animations={} channels={} 关键帧={} 时长={:.4}s 采样={} | 覆盖率={:.1}% | 自检={}",
        out_abs,
        written.bytes.len(),
        report.after.animations,
        report.animation_channels,
        written.keyframes,
        report.animation_duration,
        report.animation_samples,
        plan.coverage * 100.0,
        if report.ok() { "通过" } else { "失败" }
    );
    if !args.quiet {
        print!("{text}");
    }

    if let Some(path) = &args.report {
        std::fs::write(path, &text)
            .map_err(|e| format!("报告写不出 {}：{e}", path.display()))?;
        println!("报告已写入 {}", path.display());
    }

    if report.ok() {
        Ok(0)
    } else {
        Ok(2)
    }
}

/// 把控制台代码页切到 UTF-8。
///
/// 报告正文是中文，Windows 控制台默认 936 代码页会把 `映射` 打成乱码，
/// 用户重定向到文件时更明显。切页只在当前进程的控制台上生效，不影响系统设置。
#[cfg(windows)]
fn enforce_utf8_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleOutputCP(code_page: u32) -> i32;
        fn SetConsoleCP(code_page: u32) -> i32;
    }
    // SAFETY: 两个函数只接受一个 u32 代码页号，且在任何线程上都安全。
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}

#[cfg(not(windows))]
fn enforce_utf8_console() {}

fn main() -> ExitCode {
    enforce_utf8_console();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(argv) {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("参数错误：{msg}");
            return ExitCode::from(1);
        }
    };
    match run(args) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("失败：{msg}");
            ExitCode::from(1)
        }
    }
}
