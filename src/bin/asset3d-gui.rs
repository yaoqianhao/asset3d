//! `asset3d-gui.exe` —— 3D 视口进程（docs/24 §5.1 / §5.3）。
//!
//! 与插件宿主（`asset3d.exe`）**分进程**：winit 的事件循环会独占线程，塞进宿主的
//! stdio 帧泵里两边只会互相等死。两个进程之间只有一条命名管道，协议见
//! [`viewport::protocol`]（`docs/24 §5.2`）。
//!
//! ## 三种启动形态（argv 区分，`docs/24 §5.3.1`）
//!
//! | argv | 形态 | 行为 |
//! |---|---|---|
//! | `--pipe <name>` | IDE 托管 | 监听指定管道，`open` 指令决定画面；IDE 断开后进程继续存活 |
//! | `--standalone <asset>` | 独立看图 | 不建管道，启动即打开该资产；同一资产只开一个窗口 |
//! | 无参数 | 手动双击 | 抢 `Local\asset3d.viewport`；抢不到就把已有窗口置前后退出；抢到就弹文件对话框 |
//!
//! 参数错误退出码 2、路径错误 3（`docs/24 §5.3.1` 约束 1/2）；正常退出 0。
//!
//! ## 交互（第三刀 / 第四刀）
//!
//! 左键拖动转视角、滚轮推拉、`O` 打开文件、拖文件进窗口直接看、`Esc` 清空。
//! 第二刀到「打开资产能看出形状」为止（纯色几何 + 参考网格）；第三刀补上材质贴图：
//! 顶点带 UV，按 AIR 的 `material_parts` 分区各配一份 baseColor 与贴图，缺图退回纯色。
//! 第四刀把「绑定姿态」算对：顶点带 4 个关节槽位与权重，关节矩阵走 `group(2)` 的
//! 只读 storage buffer，在顶点着色器里做线性混合蒙皮。
//!
//! 第五刀（M-UI-4 第三刀）起视口**跟时间轴**：`Rig` 按播放头求值写回关节矩阵，
//! 于是 `capabilities` 自述为 `["static", "animated"]`——对端可以放心地按动画资产
//! 对待它。播放/暂停走空格，左右逐帧、`PageUp/PageDown` 半秒、`L` 循环、`B` 骨骼
//! 线框、`N` 切动画；底部一条时间轴覆盖层始终显示进度与时间码。
//! 没有资产时不再是一个「什么都没反应」的空窗口：双击形态启动即弹文件对话框，
//! 窗口标题也写明了拖入 / `O` 两种入口。

// 双击启动时不要闪一个黑控制台：release 走 GUI 子系统，日志交给宿主（托管形态下
// stdio 本来就是 `ignore`）。debug 构建保留控制台，`eprintln` 还能当场看见。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 复用插件侧的管道客户端：无参形态抢不到互斥体时，用它给已有实例发一帧 `focus`。
// 客户端与视口服务端是同一套约定的两端，能复用就不抄第二份。
#[path = "../pipe.rs"]
// 复用的是插件侧的客户端，它只暴露 `normalize_pipe_path` / `send_line` 两个入口，
// `PIPE_PREFIX` 之类只有插件 crate 自用——bin 里没用上不代表该删，故整模块放行。
#[allow(dead_code)]
mod pipe;
// 资产解析链（与 `main.rs` 的 `mod` 列表同源）：视口要自己解析资产，不能只靠
// 宿主的 `tools` 输出——两个 exe 是独立编译单元，没有可共享的 crate 边界。
// 复用源文件而不是复制一份，保证「宿主读出来的 AIR」与「视口画出来的形状」永远一致。
#[path = "../air.rs"]
#[allow(dead_code)]
mod air;
#[path = "../fbx.rs"]
#[allow(dead_code)]
mod fbx;
#[path = "../gltf.rs"]
#[allow(dead_code)]
mod gltf;
#[path = "../util.rs"]
#[allow(dead_code)]
mod util;

mod viewport;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use viewport::camera::Orbit;
use viewport::gfx::Gfx;
use viewport::protocol::{self, Request};
use viewport::scene;
use viewport::win32::{self, InstanceLock, LineHandler, PipeServer};

/// 参数互斥 / 缺值：退出码 2（与 `getopt` 系工具的约定一致）。
const EXIT_USAGE: i32 = 2;
/// 路径不存在、不是目录、`canonicalize` 失败：退出码 3。
const EXIT_BAD_PATH: i32 = 3;

/// 窗口默认尺寸：够看一个模型，又不会在 1080p 上盖满整屏。
const DEFAULT_SIZE: (f64, f64) = (1100.0, 720.0);

/// 无资产时的窗口标题。标题是手动形态唯一的「说明书」——写清入口，避免
/// 「双击之后什么都没反应」的观感。
const TITLE_IDLE: &str = "asset3d 视口（拖入模型，或按 O 打开）";

/// 文件对话框的过滤器串。`\0` 分隔：说明 + 通配，双 `\0` 收尾（Win32 约定）。
const ASSET_FILTER: &str = "3D 资产 (*.glb;*.gltf;*.fbx)\0*.glb;*.gltf;*.fbx\0所有文件 (*.*)\0*.*\0";

/// 进程形态。
#[derive(Debug, PartialEq, Clone)]
enum Mode {
    /// IDE 托管：监听宿主分配的管道。
    Hosted {
        pipe: String,
        /// 工作区根，用来把 `open` 的工作区相对路径拼成绝对路径。
        workspace: Option<PathBuf>,
    },
    /// 独立看图：不建管道、不抢视口互斥体。
    Standalone {
        asset: PathBuf,
        workspace: Option<PathBuf>,
    },
    /// 手动双击：按互斥体规则决定「建管道监听」还是「置前已有实例」。
    Manual,
}

/// 主线程与管道读线程之间只传原始行：解析放在主线程做，读线程不碰协议状态。
enum UserEvent {
    Frame(String),
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_mode(&argv) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("asset3d-gui: {err}");
            eprintln!("用法：asset3d-gui [--pipe <name> | --standalone <asset>] [--workspace <dir>]");
            std::process::exit(EXIT_USAGE);
        }
    };
    let mode = match normalize_mode(mode) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("asset3d-gui: {err}");
            std::process::exit(EXIT_BAD_PATH);
        }
    };

    // 互斥体：托管形态不抢——同一条管道最多一个实例由宿主判（管道已存在就直接连接），
    // 抢了反而会让「IDE 起的视口」和「用户双击的视口」互相打死。
    let mut _lock = None;
    if mode == Mode::Manual {
        match InstanceLock::acquire(win32::INSTANCE_MUTEX) {
            Ok(lock) if lock.acquired() => _lock = Some(lock),
            Ok(lock) => {
                // 已有实例：把它的窗口置前，然后本进程安静退出（不弹窗、非错误）。
                drop(lock);
                forward_focus();
                return;
            }
            Err(err) => {
                eprintln!("asset3d-gui: {err}");
                std::process::exit(1);
            }
        }
    }
    if let Mode::Standalone { asset, .. } = &mode {
        // 注意：CreateMutexW 的名字只有开头的 `Local\` / `Global\` 前缀里能出现 `\`，
        // 后半段再出现会被当成不存在的路径（ERROR_PATH_NOT_FOUND）。所以层级用 `.` 拼。
        let key = format!(
            "{}.standalone.{:016x}",
            win32::INSTANCE_MUTEX,
            hash_path(asset)
        );
        match InstanceLock::acquire(&key) {
            Ok(lock) if lock.acquired() => _lock = Some(lock),
            Ok(_) => {
                // 同一资产已有一个看图窗口：让它把窗口置前（它就是为这个资产开的）。
                let frame = format!(
                    r#"{{"v":{},"op":"focus","seq":0}}"#,
                    protocol::PROTOCOL_VERSION
                );
                let _ = pipe::send_line(win32::DEFAULT_PIPE_NAME, &frame);
                return;
            }
            Err(err) => {
                eprintln!("asset3d-gui: {err}");
                std::process::exit(1);
            }
        }
    }

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(err) => {
            eprintln!("asset3d-gui: 创建事件循环失败：{err}");
            std::process::exit(1);
        }
    };
    let proxy = event_loop.create_proxy();

    // 管道读线程必须先于事件循环起来：宿主 spawn 之后立刻就会连，晚起一秒它就报
    // `pipe_timeout`（`viewport-host.ts` 只等很短的时间）。
    let pipe_server = match pipe_name(&mode) {
        Some(name) => match win32::normalize_pipe_path(&name) {
            Ok(full) => match PipeServer::start(&full, line_handler(proxy)) {
                Ok(server) => Some(server),
                Err(err) => {
                    eprintln!("asset3d-gui: {err}");
                    std::process::exit(1);
                }
            },
            Err(err) => {
                eprintln!("asset3d-gui: {err}");
                std::process::exit(EXIT_USAGE);
            }
        },
        None => None,
    };

    let mut app = ViewportApp {
        mode,
        window: None,
        gfx: None,
        pipe: pipe_server,
        current: None,
        camera: viewport::gfx::idle_camera(),
        dragging: false,
        last_cursor: None,
        pending_open: false,
        rig: None,
        animation: None,
        animation_name: None,
        time: 0.0,
        playing: false,
        looping: true,
        show_bones: false,
        last_tick: None,
        title_accum: 0.0,
    };
    if let Err(err) = event_loop.run_app(&mut app) {
        eprintln!("asset3d-gui: 事件循环异常终止：{err}");
        std::process::exit(1);
    }
}

/// 管道读线程的回调：能在读线程里当场回的，就地回；要动窗口的交主线程。
///
/// 就地回的三类：`ping`（心跳，回 `pong`）、`screenshot`（本期不做，回
/// `unsupported`，让对端明确知道不是超时）、畸形帧（只记日志，不回）。
/// `open` / `close` / `focus` 必须回主线程——它们要碰窗口与渲染状态。
fn line_handler(proxy: EventLoopProxy<UserEvent>) -> LineHandler {
    Box::new(move |line: &str| match protocol::parse(line) {
        Ok(Request::Ping { seq }) => Some(protocol::pong_frame(seq)),
        Ok(Request::Screenshot { seq }) => Some(protocol::error_frame(
            seq,
            "unsupported",
            "这版视口不做截图（docs/24 §5.2：`screenshot` 仅占位），后续版本再评估。",
        )),
        Ok(Request::Unknown) => None,
        Ok(_) => {
            if proxy.send_event(UserEvent::Frame(line.to_string())).is_err() {
                // 事件循环已退出：进程正在收尾，这条指令不回包是预期行为。
                eprintln!("asset3d-gui: 事件循环已退出，丢弃一帧指令");
            }
            None
        }
        Err(err) => {
            // 畸形帧是版本错位，不是攻击面：记一行就够，不回包也不退出。
            eprintln!("asset3d-gui: 忽略畸形帧（{err}）：{line}");
            None
        }
    })
}

struct ViewportApp {
    mode: Mode,
    window: Option<Arc<Window>>,
    gfx: Option<Gfx>,
    pipe: Option<PipeServer>,
    /// 当前资产（绝对路径）。
    current: Option<String>,
    camera: Orbit,
    /// 左键是否按下：拖拽只在按下期间转视角，移开鼠标不会误转。
    dragging: bool,
    /// 上一帧的光标位置（物理像素）。用来把「光标位置」变成「增量」。
    last_cursor: Option<PhysicalPosition<f64>>,
    /// 手动形态的首帧画完后要弹文件选择器。延后一帧的理由见 `resumed`。
    pending_open: bool,
    /// 骨架与动画（第三刀）。没有骨骼的资产这里是 `None`，播放相关的一切操作
    /// 都退化成空操作——按钮按了不响，但不会崩。
    rig: Option<scene::Rig>,
    /// 当前动画在 `rig` 里的下标。`None` 且 `rig` 存在时表示「有骨骼没动画」。
    animation: Option<usize>,
    /// 对端（或文件名）指定的动画名，回写 `state` 时原样带回去。
    animation_name: Option<String>,
    /// 播放头位置（秒），恒落在 `0..=duration`。
    time: f32,
    playing: bool,
    looping: bool,
    /// 骨骼线框开关（`B`）。默认关：线框是排错工具，不是常态显示。
    show_bones: bool,
    /// 上一帧的时刻。用 `Instant` 而不是累加固定步长：窗口拖动、断点都会让
    /// 事件循环卡顿，固定步长会累积成「动画越跑越慢」。
    last_tick: Option<Instant>,
    /// 标题里的时间码有 0.01s 精度，每帧都 `set_title` 等于每秒 60 次
    /// `WM_SETTEXT`，白费力气。攒够 0.1s 再刷一次。
    title_accum: f32,
}

impl ViewportApp {
    fn window_title(&self) -> String {
        match &self.current {
            Some(path) => {
                let name = Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                // 有动画时标题带时间码：视口没有别的 HUD，标题是唯一能看出
                // 「播到哪儿了」的地方。
                format!("{name} — {}asset3d 视口", self.playback_status())
            }
            None => TITLE_IDLE.to_string(),
        }
    }

    /// 往后写一帧。没有管道（standalone / 手动形态）或对端已断开时静默丢弃：
    /// 视口是通知方向，丢一帧 `state` 只影响对端展示，不影响正确性。
    fn push_frame(&self, frame: String) {
        if let Some(pipe) = &self.pipe {
            pipe.writer().write_line(&frame);
        }
    }

    /// 把 `open` 的路径按形态解析成绝对路径。
    ///
    /// 宿主当前只 spawn `--pipe <name>`，**没传 `--workspace`**：这种情况下工作区
    /// 相对路径解析不出来，只能保留原样并打日志——真读盘时会以 `file_missing`
    /// 明确失败，而不是悄悄打开一个错的路径。
    fn resolve(&self, path: &str) -> String {
        let raw = PathBuf::from(path);
        if raw.is_absolute() {
            return path.to_string();
        }
        let root = match &self.mode {
            Mode::Hosted { workspace, .. } | Mode::Standalone { workspace, .. } => workspace.clone(),
            Mode::Manual => None,
        };
        match root {
            Some(root) => root.join(raw).to_string_lossy().to_string(),
            None => {
                eprintln!(
                    "asset3d-gui: 收到工作区相对路径 `{path}`，但没拿到工作区根（宿主未传 `--workspace`），保留原样"
                );
                path.to_string()
            }
        }
    }

    /// 解析资产并上传到 GPU。失败时**保持现有画面**：打不开的文件不该把已经
    /// 看得见的东西弄丢。
    fn load(&mut self, path: &Path) -> Result<(), String> {
        let mut scene = scene::load(path)?;
        if let Some(gfx) = &mut self.gfx {
            gfx.set_scene(&scene);
        }
        // `Rig` 只在载入时算一次绑定姿态与轨道，播放期间复用：每帧重建它等于把
        // 整个节点树复制一遍，60fps 下白给。
        let rig = scene.rig.take();
        // 播放器状态一律清零：换资产而保留播放头，会取到旧动画的时间点。
        self.time = 0.0;
        self.playing = false;
        self.last_tick = None;
        self.title_accum = 0.0;
        self.animation = None;
        self.animation_name = None;
        if let Some(rig) = &rig {
            if let Some(gfx) = &mut self.gfx {
                gfx.set_skeleton(rig.bone_lines(), rig.joint_points());
                gfx.set_skeleton_visible(self.show_bones);
            }
            self.animation = rig.pick_animation(None);
            self.animation_name = self
                .animation
                .and_then(|index| rig.animation_name(index))
                .filter(|name| !name.is_empty())
                .map(str::to_string);
        } else if let Some(gfx) = &mut self.gfx {
            // 上一份资产的骨架不能留：它连的是旧关节槽位。
            gfx.set_skeleton(&[], &[]);
            gfx.set_timeline(None);
        }
        self.rig = rig;
        self.camera = Orbit::fit(scene.center, scene.radius);
        self.current = Some(path.to_string_lossy().to_string());
        // 先把 0 秒姿态摆正再画：否则第一帧看到的是绑定姿态，和后面播起来的
        // 画面差一个姿势，观感像「点了播放才跳一下」。
        self.pose();
        if let Some(window) = &self.window {
            window.set_title(&self.window_title());
            window.request_redraw();
        }
        eprintln!(
            "asset3d-gui: 已载入 {}（{} 个网格 / {} 个三角形 / {} 张贴图）",
            path.display(),
            scene.mesh_count,
            scene.triangle_count,
            scene.texture_count
        );
        // 蒙皮单独报一行：资产「有骨骼」和「真的在用骨骼」是两件事，出问题时
        // 第一眼要看的就是这里是不是 0。
        if !scene.joint_matrices.is_empty() {
            eprintln!(
                "asset3d-gui: 蒙皮 {} 个关节矩阵，{} 个顶点参与变形",
                scene.joint_matrices.len(),
                scene.skinned_vertex_count
            );
        }
        // 动画也单独报一行：`n 条动画` 直接决定了「空格按下去有没有反应」。
        if let Some(rig) = &self.rig {
            if rig.animation_count() > 0 {
                eprintln!(
                    "asset3d-gui: {}，骨骼线框 {} 条",
                    rig.animation_summary(),
                    rig.bone_lines().len()
                );
            } else if !rig.bone_lines().is_empty() {
                eprintln!(
                    "asset3d-gui: 有骨骼（{} 条连线）但没有动画，按 B 可看骨架",
                    rig.bone_lines().len()
                );
            }
        }
        // 贴图缺失、尺寸超限这类问题不影响开窗，但会让资产看起来是纯色的。
        // 不打出来，用户第一反应就是「视口坏了」。
        for note in &scene.notes {
            eprintln!("asset3d-gui: 提示：{note}");
        }
        Ok(())
    }

    fn unload(&mut self) {
        if let Some(gfx) = &mut self.gfx {
            gfx.clear_scene();
        }
        self.rig = None;
        self.animation = None;
        self.animation_name = None;
        self.time = 0.0;
        self.playing = false;
        self.last_tick = None;
        self.camera = viewport::gfx::idle_camera();
        self.current = None;
        if let Some(window) = &self.window {
            window.set_title(TITLE_IDLE);
            window.request_redraw();
        }
    }

    // ---- 播放器（第三刀）----
    //
    // 所有播放控制都走「改状态 → `pose()` 重算矩阵 → 推 GPU → 报 `state`」这一条
    // 路，不另开旁路：否则标题、画面、对端三方各说各话。

    /// 当前动画时长（秒）。没有骨骼 / 没有动画 / 时长非正，一律 0。
    fn duration(&self) -> f32 {
        let (Some(rig), Some(index)) = (&self.rig, self.animation) else {
            return 0.0;
        };
        let duration = rig.duration(index);
        if duration.is_finite() && duration > 0.0 {
            duration
        } else {
            0.0
        }
    }

    /// 在 `time` 处重算姿态并推到 GPU。没有骨架时只把时间轴收起来。
    fn pose(&mut self) {
        let duration = self.duration();
        let progress = if duration > 0.0 {
            Some((self.time / duration).clamp(0.0, 1.0))
        } else {
            None
        };
        let animation = self.animation;
        let time = self.time;
        let Some(rig) = self.rig.as_mut() else {
            if let Some(gfx) = self.gfx.as_mut() {
                gfx.set_timeline(None);
            }
            return;
        };
        rig.pose_at(animation, time);
        if let Some(gfx) = self.gfx.as_mut() {
            gfx.update_bones(rig.joint_matrices());
            gfx.update_skeleton(rig.joint_points());
            gfx.set_timeline(progress);
        }
    }

    /// 推进一帧。返回 `true` 表示还要继续跑（调用方据此再 `request_redraw`）。
    fn advance(&mut self, now: Instant) -> bool {
        // 卡顿后不要一次跳过好几秒：窗口被拖动、宿主暂停都会让 `dt` 变得很大。
        let dt = self
            .last_tick
            .map(|last| (now - last).as_secs_f32())
            .unwrap_or(0.0)
            .clamp(0.0, 0.1);
        self.last_tick = Some(now);
        if !self.playing {
            return false;
        }
        let duration = self.duration();
        if duration <= 0.0 {
            // 没有可播的动画：别在空转里一直重绘，直接把状态收回暂停。
            self.playing = false;
            return false;
        }
        let (time, keep_playing) = scene::advance_playhead(self.time, dt, duration, self.looping);
        self.time = time;
        self.playing = keep_playing;
        self.pose();
        self.title_accum += dt;
        if self.title_accum >= 0.1 || !self.playing {
            self.title_accum = 0.0;
            if let Some(window) = &self.window {
                window.set_title(&self.window_title());
            }
        }
        self.playing
    }

    fn set_playing(&mut self, playing: bool) {
        if playing && self.duration() <= 0.0 {
            return;
        }
        self.playing = playing;
        // 重新开播时清掉基准时刻：暂停期间 `about_to_wait` 不再刷新 `last_tick`，
        // 留着就是一次「一开机就跳半秒」。
        self.last_tick = None;
        self.title_accum = 0.0;
        self.refresh_view();
    }

    fn scrub(&mut self, delta: f32) {
        let duration = self.duration();
        if duration <= 0.0 {
            return;
        }
        self.time = (self.time + delta).clamp(0.0, duration);
        // 手动拖时间就停播：一边拖一边自动前进，用户永远定位不到想看的姿势。
        self.playing = false;
        self.last_tick = None;
        self.refresh_view();
    }

    fn cycle_animation(&mut self) {
        let Some((next, name, duration)) = self.rig.as_ref().and_then(|rig| {
            let count = rig.animation_count();
            if count == 0 {
                return None;
            }
            let next = match self.animation {
                Some(index) => (index + 1) % count,
                None => 0,
            };
            let name = rig
                .animation_name(next)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            Some((next, name, rig.duration(next)))
        }) else {
            return;
        };
        eprintln!(
            "asset3d-gui: 切到动画 #{next}（{}），时长 {:.2}s",
            name.as_deref().unwrap_or("(未命名)"),
            duration
        );
        self.animation = Some(next);
        self.animation_name = name;
        self.time = 0.0;
        self.playing = true;
        self.last_tick = None;
        self.refresh_view();
    }

    fn toggle_bones(&mut self) {
        self.show_bones = !self.show_bones;
        if let Some(gfx) = &mut self.gfx {
            gfx.set_skeleton_visible(self.show_bones);
        }
        eprintln!(
            "asset3d-gui: 骨骼线框{}",
            if self.show_bones { "开" } else { "关" }
        );
        self.refresh_view();
    }

    fn toggle_loop(&mut self) {
        self.looping = !self.looping;
        eprintln!(
            "asset3d-gui: 循环{}",
            if self.looping { "开" } else { "关（播到末尾停在最后）" }
        );
        self.refresh_view();
    }

    /// 离散操作后统一收口：重算姿态、刷标题、重绘、报 `state`。
    fn refresh_view(&mut self) {
        self.pose();
        if let Some(window) = &self.window {
            window.set_title(&self.window_title());
            window.request_redraw();
        }
        self.emit_state();
    }

    /// 把当前播放状态报给宿主。没有管道时是空操作（`push_frame` 自己判）。
    fn emit_state(&self) {
        let path = self.current.clone().unwrap_or_default();
        let name = self.animation_name.clone();
        self.push_frame(protocol::state_frame(
            &path,
            name.as_deref(),
            self.time,
            self.playing,
            self.duration(),
        ));
    }

    /// 标题里的播放信息。没有动画就整段省略，不留一个空的 `0.00/0.00s`。
    fn playback_status(&self) -> String {
        let duration = self.duration();
        if duration <= 0.0 {
            return String::new();
        }
        let name = self
            .animation_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or("(未命名)");
        format!(
            "{name} {:.2}/{:.2}s {} · ",
            self.time,
            duration,
            if self.playing { "播放" } else { "暂停" }
        )
    }

    /// 报错：有管道就回协议错误帧，没有管道（双击 / standalone）就弹系统消息框——
    /// 双击启动时 stderr 没人看得见。
    fn report(&self, seq: u64, code: &str, message: &str) {
        if self.pipe.is_some() {
            self.push_frame(protocol::error_frame(seq, code, message));
        } else {
            win32::show_error(hwnd_of_opt(self.window.as_deref()), "asset3d 视口", message);
        }
    }

    /// 弹「打开文件」对话框并载入选中的资产。用户取消就什么都不做。
    fn ask_for_file(&mut self) {
        let owner = hwnd_of_opt(self.window.as_deref());
        let Some(path) = win32::pick_asset_file(owner, "打开 3D 资产", ASSET_FILTER) else {
            return;
        };
        match self.load(&path) {
            // 手动打开不自动播：用户按 `O` 是想先看清这个模型，不是想让它动起来。
            Ok(()) => self.emit_state(),
            Err(err) => {
                eprintln!("asset3d-gui: 打开 {} 失败：{err}", path.display());
                self.report(0, "load_failed", &err);
            }
        }
    }

    fn on_request(&mut self, req: Request) {
        match req {
            Request::Open {
                path,
                animation,
                play,
                seq,
            } => {
                if path.trim().is_empty() {
                    self.push_frame(protocol::error_frame(seq, "file_missing", "资产路径为空"));
                    return;
                }
                let resolved = self.resolve(&path);
                let raw = Path::new(&resolved);
                if raw.is_absolute() && !raw.is_file() {
                    self.push_frame(protocol::error_frame(
                        seq,
                        "file_missing",
                        &format!("资产不存在：{resolved}"),
                    ));
                    return;
                }
                match self.load(raw) {
                    Ok(()) => {
                        // 对端指定的动画名优先；对不上就退回第一条并记一行——静默降级会
                        // 让对端以为播的是它要的那条。
                        if let Some(rig) = &self.rig {
                            if let Some(index) = rig.pick_animation(animation.as_deref()) {
                                if !rig.animation_name_matches(index, animation.as_deref()) {
                                    eprintln!(
                                        "asset3d-gui: 资产里没有动画 `{}`，改播第一条",
                                        animation.as_deref().unwrap_or_default()
                                    );
                                }
                                let name = rig
                                    .animation_name(index)
                                    .filter(|name| !name.is_empty())
                                    .map(str::to_string)
                                    .or_else(|| animation.clone());
                                self.animation = Some(index);
                                self.animation_name = name;
                            }
                        }
                        self.time = 0.0;
                        self.playing = play && self.duration() > 0.0;
                        self.last_tick = None;
                        self.refresh_view();
                    }
                    Err(err) => {
                        eprintln!("asset3d-gui: 打开 {resolved} 失败：{err}");
                        self.push_frame(protocol::error_frame(seq, "load_failed", &err));
                    }
                }
            }
            Request::Close { .. } => {
                self.unload();
                self.emit_state();
            }
            Request::Focus { .. } => {
                if let Some(window) = &self.window {
                    window.focus_window();
                }
            }
            // `ping` / `screenshot` / 未知 op 已在读线程处理，走到这里说明是重复投递。
            Request::Ping { .. } | Request::Screenshot { .. } | Request::Unknown => {}
        }
    }

    fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl ApplicationHandler<UserEvent> for ViewportApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.window_title())
            .with_inner_size(LogicalSize::new(DEFAULT_SIZE.0, DEFAULT_SIZE.1));
        let window = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(err) => {
                eprintln!("asset3d-gui: 创建窗口失败：{err}");
                event_loop.exit();
                return;
            }
        };
        match Gfx::new(Arc::clone(&window)) {
            Ok(gfx) => self.gfx = Some(gfx),
            Err(err) => {
                // 建不出 GPU 设备就没有任何可看的：报清楚原因再退，不留在白窗口上。
                eprintln!("asset3d-gui: {err}");
                win32::show_error(hwnd_of(&window), "asset3d 视口无法启动", &err);
                event_loop.exit();
                return;
            }
        }
        window.request_redraw();
        // 先落 window 再弹对话框：对话框要把窗口当 owner，否则会跑到窗口后面去。
        self.window = Some(window);
        // 被别的程序顺带拉起时窗口可能带着「最小化」状态，先自己抢一次焦点。
        if let Some(window) = &self.window {
            window.focus_window();
        }

        match self.mode.clone() {
            // 无参双击：等第一帧画完再弹文件选择器（见 `pending_open`）。直接在 `resumed`
            // 里弹是同步模态调用，事件循环被挡住，用户会先看到一块没画过的白板——
            // 观感和「启动失败」没区别。
            Mode::Manual => self.pending_open = true,
            Mode::Standalone { asset, .. } => {
                if let Err(err) = self.load(&asset) {
                    eprintln!("asset3d-gui: 打开 {} 失败：{err}", asset.display());
                    self.report(0, "load_failed", &err);
                } else if self.duration() > 0.0 {
                    // 独立看图没有对端发 `open{play:true}`：打开一个带动画的资产却停在
                    // 绑定姿态，用户只会以为动画没导进来。这里直接开播，空格随时暂停。
                    self.set_playing(true);
                }
            }
            Mode::Hosted { .. } => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame(line) => match protocol::parse(&line) {
                Ok(req) => self.on_request(req),
                Err(err) => eprintln!("asset3d-gui: 主线程丢弃畸形帧（{err}）"),
            },
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gfx) = &mut self.gfx {
                    gfx.resize(size);
                }
                self.redraw();
            }
            WindowEvent::RedrawRequested => {
                if let Some(gfx) = &mut self.gfx {
                    if let Err(err) = gfx.render(&self.camera) {
                        // 单帧失败不退出：拔显示器、改缩放都会让 surface 抖一下，
                        // 下一次 request_redraw 自然再试。真不可恢复的（显存不足）
                        // 已经由 `Err` 的文案说清了。
                        eprintln!("asset3d-gui: {err}");
                    }
                }
                // 第一帧（地板网格）已经在屏幕上了，这时再弹模态对话框：即便用户
                // 立刻取消，窗口里也有东西可看，而不是一块白板。
                if self.pending_open {
                    self.pending_open = false;
                    self.ask_for_file();
                }
            }
            // 拖文件进窗口直接看：比「按 O 再翻目录」快，也是看图工具的通用手势。
            WindowEvent::DroppedFile(path) => {
                if let Err(err) = self.load(&path) {
                    eprintln!("asset3d-gui: 打开 {} 失败：{err}", path.display());
                    self.report(0, "load_failed", &err);
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = state == ElementState::Pressed;
                if !self.dragging {
                    // 松开时清掉基准点：不然下次按下后的第一帧会拿旧坐标算出一次大跳。
                    self.last_cursor = None;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging {
                    if let Some(last) = self.last_cursor {
                        self.camera.orbit(
                            (position.x - last.x) as f32,
                            (position.y - last.y) as f32,
                        );
                        self.redraw();
                    }
                }
                self.last_cursor = Some(position);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    // 一行 ≈ 一格滚轮；像素级（触控板）按 60px 折算成一格。
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 60.0) as f32,
                };
                self.camera.zoom(steps);
                self.redraw();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state: ElementState::Pressed,
                        logical_key,
                        ..
                    },
                ..
            } => match logical_key.as_ref() {
                Key::Character("o") | Key::Character("O") => self.ask_for_file(),
                Key::Named(NamedKey::Escape) => self.unload(),
                // 第三刀：空格播放 / 暂停，左右键逐帧（1/30s），PageUp/PageDown 跳
                // 半秒；`L` 循环开关、`B` 骨骼线框、`N` 下一条动画。
                Key::Named(NamedKey::Space) => self.set_playing(!self.playing),
                Key::Named(NamedKey::ArrowLeft) => self.scrub(-1.0 / 30.0),
                Key::Named(NamedKey::ArrowRight) => self.scrub(1.0 / 30.0),
                Key::Named(NamedKey::PageUp) => self.scrub(-0.5),
                Key::Named(NamedKey::PageDown) => self.scrub(0.5),
                Key::Character("l") | Key::Character("L") => self.toggle_loop(),
                Key::Character("b") | Key::Character("B") => self.toggle_bones(),
                Key::Character("n") | Key::Character("N") => self.cycle_animation(),
                Key::Named(NamedKey::Home) => {
                    if let Some(path) = self.current.clone() {
                        // 重载：既能把视角拉回默认，也能在资产被外部改过后刷新。
                        if let Err(err) = self.load(Path::new(&path)) {
                            eprintln!("asset3d-gui: 重载失败：{err}");
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(pipe) = &mut self.pipe {
            pipe.stop();
        }
    }

    /// 播放的驱动点。winit 没有「每帧回调」，用 `about_to_wait` 当泵：
    /// 只要还在播就推进一步并 `request_redraw`，重绘完又回到这里，形成闭环。
    /// 暂停后返回 `false`，GPU 立刻停下来——静止画面不烧显卡。
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.advance(Instant::now()) {
            self.redraw();
        }
    }
}

/// 该形态要监听哪条管道。`None` = 不建管道（standalone）。
fn pipe_name(mode: &Mode) -> Option<String> {
    match mode {
        Mode::Hosted { pipe, .. } => Some(pipe.clone()),
        Mode::Manual => Some(win32::DEFAULT_PIPE_NAME.to_string()),
        Mode::Standalone { .. } => None,
    }
}

/// 抢不到互斥体时把已有窗口置前：连默认管道发一帧 `focus`。
///
/// 连不上就是连不上（已有实例可能正忙），不重试也不报错——用户看到的就是
/// 「双击之后原来的窗口跳出来了」，这正是期望行为。
fn forward_focus() {
    let frame = format!(
        r#"{{"v":{},"op":"focus","seq":0}}"#,
        protocol::PROTOCOL_VERSION
    );
    if pipe::send_line(win32::DEFAULT_PIPE_NAME, &frame).is_err() {
        eprintln!("asset3d-gui: 已有实例在运行，但窗口没能置前（管道未连上）");
    }
}

/// 取窗口的 Win32 句柄，用于给对话框 / 消息框当 owner。
fn hwnd_of(window: &Window) -> isize {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get() as isize,
        // 非 Windows 句柄不该出现（本 crate 只编 Windows），返回 0 表示无 owner：
        // 对话框照样能用，只是不跟着窗口居中。
        _ => 0,
    }
}

fn hwnd_of_opt(window: Option<&Window>) -> isize {
    window.map(hwnd_of).unwrap_or(0)
}

/// 同一资产的 `--standalone` 只开一个窗口：互斥体名按绝对路径哈希。
fn hash_path(path: &Path) -> u64 {
    // FNV-1a 64：只用来做互斥体名的区分，不需要抗碰撞，也不值得为它拖一个 crate。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().to_lowercase().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 裸路径参数是否像个 3D 资产：`ASSET_FILTER` 里认的三种扩展名，忽略大小写。
fn is_asset_path(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [".glb", ".gltf", ".fbx"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn take_value(argv: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    let value = argv
        .get(*index + 1)
        .ok_or_else(|| format!("`{flag}` 缺参数值"))?;
    if value.starts_with("--") {
        return Err(format!("`{flag}` 缺参数值"));
    }
    *index += 2;
    Ok(value.clone())
}

fn parse_mode(argv: &[String]) -> Result<Mode, String> {
    let mut pipe: Option<String> = None;
    let mut standalone: Option<String> = None;
    let mut workspace: Option<String> = None;
    let mut index = 0;
    while index < argv.len() {
        match argv[index].as_str() {
            "--pipe" => pipe = Some(take_value(argv, &mut index, "--pipe")?),
            "--standalone" => standalone = Some(take_value(argv, &mut index, "--standalone")?),
            "--workspace" => workspace = Some(take_value(argv, &mut index, "--workspace")?),
            "--help" | "-h" => {
                println!(
                    "用法：asset3d-gui [--pipe <name> | --standalone <asset>] [--workspace <dir>]"
                );
                std::process::exit(0);
            }
            // 资源管理器把模型拖到 exe 图标上（或文件关联双击）时，argv 就是一个裸路径。
            // 不认它等于「双击带文件什么也没发生」，所以直接当 `--standalone` 用。
            other if standalone.is_none() && is_asset_path(other) => {
                standalone = Some(other.to_string());
                index += 1;
            }
            other => return Err(format!("未知参数 `{other}`")),
        }
    }
    // 互斥而不是「两个都试」：同时给出即参数错误（docs/24 §5.3.1 约束 1）。
    if pipe.is_some() && standalone.is_some() {
        return Err("`--pipe` 与 `--standalone` 互斥，二者只能给一个".to_string());
    }
    let workspace = workspace.map(PathBuf::from);
    if let Some(name) = pipe {
        return Ok(Mode::Hosted {
            pipe: name,
            workspace,
        });
    }
    if let Some(asset) = standalone {
        return Ok(Mode::Standalone {
            asset: PathBuf::from(asset),
            workspace,
        });
    }
    Ok(Mode::Manual)
}

/// 路径类的校验集中在这里：`canonicalize` 失败、不存在、不是目录都退出码 3，
/// 且不做「两个都试」的兜底（`docs/24 §5.3.1` 约束 2）。
fn normalize_mode(mode: Mode) -> Result<Mode, String> {
    let fix_workspace = |workspace: Option<PathBuf>| -> Result<Option<PathBuf>, String> {
        match workspace {
            None => Ok(None),
            Some(root) => {
                let canonical = root
                    .canonicalize()
                    .map_err(|err| format!("工作区路径 `{}` 无效：{err}", root.display()))?;
                if !canonical.is_dir() {
                    return Err(format!("工作区路径 `{}` 不是目录", canonical.display()));
                }
                Ok(Some(canonical))
            }
        }
    };
    match mode {
        Mode::Hosted { pipe, workspace } => Ok(Mode::Hosted {
            pipe,
            workspace: fix_workspace(workspace)?,
        }),
        Mode::Standalone { asset, workspace } => {
            let canonical = asset
                .canonicalize()
                .map_err(|err| format!("资产路径 `{}` 无效：{err}", asset.display()))?;
            if !canonical.is_file() {
                return Err(format!("资产路径 `{}` 不是文件", canonical.display()));
            }
            Ok(Mode::Standalone {
                asset: canonical,
                workspace: fix_workspace(workspace)?,
            })
        }
        Mode::Manual => Ok(Mode::Manual),
    }
}
