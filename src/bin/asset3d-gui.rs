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
#[path = "../air_bin.rs"]
#[allow(dead_code)]
mod air_bin;
// 导出核心：AIR 编码 / 回读自检 / 加载器模板。与插件 `tools.rs` 调的是**同一份**
// 源码（不是复制），所以 GUI 按钮导出的字节与插件导出逐比特一致——格式只在
// `export_core.rs` 里定义一次。
#[path = "../export_core.rs"]
#[allow(dead_code)]
mod export_core;

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
use viewport::{ui, workspace};

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

/// 导出对话框的过滤器。AIR 二进制用 `.air.bin`（双扩展名，插件侧同一口径），
/// 所以通配写 `*.bin` 而不是 `*.air.bin`——Win32 的通配只匹配最后一段。
const AIR_BIN_FILTER: &str = "AIR 二进制 (*.air.bin)\0*.bin\0所有文件 (*.*)\0*.*\0";
const AIR_JSON_FILTER: &str = "AIR JSON (*.air.json)\0*.json\0所有文件 (*.*)\0*.*\0";
const LOADER_FILTER: &str = "Rust 加载器 (*.rs)\0*.rs\0所有文件 (*.*)\0*.*\0";

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
    /// 工作区扫描结果（后台线程扫完投回来）。带上根路径：扫的过程中用户可能已经
    /// 切到别的工作区，过期的清单不能顶掉新的。
    Assets {
        root: PathBuf,
        scan: workspace::Scan,
    },
    /// 无头导出自测的下一步：只有设了 `ASSET3D_GUI_SELFTEST` 才会被投递。
    SelfTest,
}

/// 自测一共推几步：0 起步 → 6 次点击（三次开菜单 + 三次选菜单项/按钮）→ 收尾。
///
/// 菜单是「点 ExportAir 展开 → 点条目」两段式，所以 bin / json / both 三次导出
/// 各要点两下，.rs 只有一下，最后一步校验写报告。
/// 自测最多推进多少步。取一个明显大于「真实步数」的值：目录面板要等后台扫描，
/// 等待期间只是不推进（见 `selftest_step`），步数与事件数不能绑死。真走完时
/// `selftest_finish` 会直接 `exit`，剩下的节拍事件不会白等。
const SELFTEST_STEPS: usize = 80;

/// 等一轮后台扫描 / 一帧渲染最多容忍几个节拍（300ms × 20 = 6s）。
const SELFTEST_WAIT_LIMIT: usize = 20;

/// 自测要点哪个控件。
#[derive(Clone, Copy)]
enum SelfTestTarget {
    Button(ui::ButtonId),
    PopupItem(usize),
    /// 目录面板第 `index` 行（与 `ui::Hit::Row` 同源）。
    Row(usize),
}

impl SelfTestTarget {
    /// 期望的命中结果，字符串形状与 `ui::Hit` 的 `Debug` 一致（`Button(ExportAir)`）。
    fn describe(self) -> String {
        match self {
            SelfTestTarget::Button(id) => format!("Button({id:?})"),
            SelfTestTarget::PopupItem(index) => format!("PopupItem({index})"),
            SelfTestTarget::Row(index) => format!("Row({index})"),
        }
    }
}

/// 无头导出自测的配置与过程记录，只在 `ASSET3D_GUI_SELFTEST` 有值时存在。
///
/// 这一段存在的意义是让「按钮 → 命中 → 导出 → 落盘」可以被无人值守地验证：点击
/// 合成后走的还是 `on_left_pressed` / `on_left_released` 这一条路，命中还是
/// `Layout::hit`，导出还是 `export_core`。唯一被替换的环节是「另存为」对话框——
/// 目标目录直接取 `ASSET3D_GUI_SELFTEST_DIR`，否则自测会卡在模态窗口上。
struct SelfTest {
    /// 产物根目录：四类导出各占一个子目录，互不覆盖。
    dir: PathBuf,
    /// 结果报告（JSON）落点。
    report: PathBuf,
    /// 互斥体名字的一次性后缀，见 `main` 里 Standalone 分支。
    nonce: u64,
    /// 下一步的序号。
    step: usize,
    /// 当前导出动作的子目录名：`ask_export_path` 用它决定往哪个子目录写。
    tag: &'static str,
    /// 每步的命中记录，原样写进报告。
    trace: Vec<String>,
    /// 有一步没按预期命中：最终 `all_ok` 一定是 false。
    failed: bool,
    /// 等待节拍已用掉几个（扫描 / 渲染各算各的，见 [`SELFTEST_WAIT_LIMIT`]）。
    waits: usize,
    /// 工作区根（`--workspace` 透传后的值），面板扫描的输入。
    workspace: Option<String>,
    /// 面板扫描到的资产条数。
    scan_total: Option<usize>,
    /// 面板首行的展示路径（相对工作区根）。
    first_row: Option<String>,
    /// 点首行之前的窗口标题。
    title_before: String,
    /// 点首行之后的窗口标题。
    title_after: String,
    /// 「点目录按钮」这一步命中正确且面板真的开了。
    panel_ok: bool,
    /// 「点首行 → 载入的资产变成首行那条」。
    row_loaded_ok: bool,
    /// 截图证据：标签、路径、尺寸、抓法。
    shots: Vec<(String, PathBuf, u32, u32, String)>,
}

impl SelfTest {
    fn from_env() -> Option<SelfTest> {
        let flag = std::env::var("ASSET3D_GUI_SELFTEST").unwrap_or_default();
        if flag.is_empty() || flag == "0" {
            return None;
        }
        let dir = std::env::var_os("ASSET3D_GUI_SELFTEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("asset3d-selftest"));
        let report = std::env::var_os("ASSET3D_GUI_SELFTEST_REPORT")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("report.json"));
        Some(SelfTest {
            dir,
            report,
            nonce: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0),
            step: 0,
            tag: "air-bin",
            trace: Vec::new(),
            failed: false,
            waits: 0,
            workspace: None,
            scan_total: None,
            first_row: None,
            title_before: String::new(),
            title_after: String::new(),
            panel_ok: false,
            row_loaded_ok: false,
            shots: Vec::new(),
        })
    }
}

/// 播放截帧形态：`ASSET3D_POSE_SHOT=<glb>`。
///
/// 存在的理由只有一个——重定向出来的动画「能不能播、姿态对不对」必须有画面证据，
/// 而不是「窗口开了」就算通过。载入资产后依次把播放头停到给定时间比例处，
/// 每处等三帧再抓客户区截图，抓完写清单退出。留三帧是因为载入后的第一次
/// present 可能还没进合成器，抓早了会拿到一张纯背景的图。
struct PoseShots {
    dir: PathBuf,
    /// 停靠点：`(标签, 时长比例)`。
    stops: Vec<(String, f32)>,
    /// 当前停到第几个。
    at: usize,
    /// 0 = 待设时刻，1 = 已设时刻等首帧，2 = 再等一帧，>=3 = 可以抓了。
    settle: u8,
    /// 已抓到的证据。
    shots: Vec<(String, PathBuf, u32, u32, String)>,
    /// 首次抓帧只当热身：载入后的第一张 present 还没进合成器，抓到的是一张空背景。
    warmed: bool,
}

impl PoseShots {
    /// 没设 `ASSET3D_POSE_SHOT` 就返回 `None`，正常看图流程完全不受影响。
    fn from_env() -> Option<PoseShots> {
        let flag = std::env::var("ASSET3D_POSE_SHOT").unwrap_or_default();
        if flag.is_empty() || flag == "0" {
            return None;
        }
        let dir = std::env::var_os("ASSET3D_POSE_SHOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("asset3d-pose-shots"));
        // 写法：`0,0.5,1` 或 `start:0,mid:0.5,end:1`。
        let raw = std::env::var("ASSET3D_POSE_SHOT_AT")
            .unwrap_or_else(|_| "0,0.5,1".to_string());
        let mut stops = Vec::new();
        for item in raw.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (label, value) = match item.split_once(':') {
                Some((label, value)) => (label.trim().to_string(), value.trim()),
                None => (format!("{item}"), item),
            };
            let Ok(ratio) = value.parse::<f32>() else {
                eprintln!("asset3d-gui: ASSET3D_POSE_SHOT_AT 里的 `{item}` 不是数字，忽略");
                continue;
            };
            let label = label.replace(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-', "_");
            stops.push((label, ratio.clamp(0.0, 1.0)));
        }
        if stops.is_empty() {
            stops.push(("0".to_string(), 0.0));
        }
        Some(PoseShots {
            dir,
            stops,
            at: 0,
            settle: 0,
            shots: Vec::new(),
            warmed: false,
        })
    }
}

/// 启动时是否直接打开骨架：`ASSET3D_SHOW_BONES=1`。
///
/// 只为「截帧验证骨架画得对不对」服务——正常看图一律默认关，靠按钮或 B 键切换。
fn bones_from_env() -> bool {
    matches!(
        std::env::var("ASSET3D_SHOW_BONES").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    )
}

/// 白模审计的报告落点：`ASSET3D_ASSET_AUDIT_REPORT` 优先，否则落系统临时目录
/// （带时间戳目录，多轮复跑不互相覆盖）。
fn audit_report_path(root: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("ASSET3D_ASSET_AUDIT_REPORT") {
        return PathBuf::from(path);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().replace(|ch: char| !ch.is_ascii_alphanumeric(), "_"))
        .unwrap_or_else(|| "assets".to_string());
    std::env::temp_dir()
        .join("asset3d-audit")
        .join(format!("{name}-{stamp}"))
        .join("audit.md")
}

fn main() {
    // 白模审计形态（`ASSET3D_ASSET_AUDIT=<目录>`）：只解析、不开窗口、不建管道、
    // 不抢互斥体。把它放在 argv 解析之前，是因为审计与「看哪个资产」无关，
    // 不该因为少了 `--standalone` 就被判成用法错误。
    if let Some(root) = viewport::audit::dir_from_env() {
        let report = audit_report_path(&root);
        if let Some(parent) = report.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!("asset3d 审计：建报告目录失败：{err}");
                std::process::exit(1);
            }
        }
        std::process::exit(viewport::audit::run(&root, &report));
    }

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

    // 无头导出自测的开关先读：它决定下面要不要抢互斥体（自测要能连着跑）。
    let selftest = SelfTest::from_env();
    // 播放截帧同理：连着跑同一资产时要绕开「已有实例」的置前逻辑。
    let pose_shots = PoseShots::from_env();

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
        //
        // 自测实例给名字加一次性后缀：否则上一轮（还没退干净）的窗口会把这一轮
        // 直接挡回去，验证就成了「什么都不做也算通过」。
        let key = match (&selftest, &pose_shots) {
            (Some(selftest), _) => format!(
                "{}.standalone.selftest.{:x}",
                win32::INSTANCE_MUTEX,
                selftest.nonce
            ),
            (None, Some(_)) => format!(
                "{}.standalone.shot.{:x}",
                win32::INSTANCE_MUTEX,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos() as u64)
                    .unwrap_or(0)
            ),
            (None, None) => format!(
                "{}.standalone.{:016x}",
                win32::INSTANCE_MUTEX,
                hash_path(asset)
            ),
        };
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
            Ok(full) => match PipeServer::start(&full, line_handler(proxy.clone())) {
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
        proxy,
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
        show_bones: bones_from_env(),
        last_tick: None,
        title_accum: 0.0,
        host_workspace: None,
        workspace: None,
        workspace_root: None,
        scanning: false,
        panel_open: false,
        panel_scroll: 0,
        popup_open: false,
        status: String::new(),
        status_bad: false,
        layout: None,
        hover: None,
        pressed: None,
        ui_capture: false,
        timeline_drag: false,
        ui_dirty: true,
        selftest,
        pose_shots,
    };
    // 自测驱动另起一条线程：只按节拍投递「下一步」，不碰任何窗口状态。
    // 走事件而不是在事件循环里连跑，是为了每步之间真的过一遍 `RedrawRequested`，
    // 让下一步命中的布局就是刚刚画出去的那一版。
    if app.selftest.is_some() {
        let proxy = app.proxy.clone();
        std::thread::spawn(move || {
            // 首个等待：建窗口 + wgpu 初始化 + 首次合成 UI 位图（实测 1s 内跑完）。
            std::thread::sleep(std::time::Duration::from_millis(1500));
            for _ in 0..SELFTEST_STEPS {
                if proxy.send_event(UserEvent::SelfTest).is_err() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        });
    }
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
    /// 事件循环的投递句柄：后台扫描线程扫完用它把清单送回主线程。
    proxy: EventLoopProxy<UserEvent>,
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

    // ---- GUI 按键（第四刀：可点的工具条 / 目录面板 / 导出）----
    /// 宿主在 `open` 帧里带来的工作区根。优先级最高：同一台机器上开了两个 IDE
    /// 窗口时，用户点开的是哪个工作区，对话框就该从哪个工作区起步。
    host_workspace: Option<PathBuf>,
    /// 最近一次扫描出来的资产清单（相对路径 + 绝对路径）。
    workspace: Option<workspace::Scan>,
    /// 清单当前对应的根。根变了旧清单立刻失效，不能拿前一工作区的条目去开文件。
    workspace_root: Option<PathBuf>,
    /// 后台扫描进行中：面板底部显示「扫描中…」，不重复起线程。
    scanning: bool,
    /// 目录面板是否展开（工具条「目录」键）。
    panel_open: bool,
    /// 面板滚动偏移：第一条可见行的下标。
    panel_scroll: usize,
    /// AIR 导出菜单（bin / json / 两者）是否展开。
    popup_open: bool,
    /// 状态条文字。导出结果、载入失败都写这里——视口没有别的输出窗口。
    status: String,
    /// 状态条是不是错误态（配色不同）。
    status_bad: bool,
    /// 上一屏的布局。鼠标事件用它做命中测试，和绘制读的是同一份矩形表。
    layout: Option<ui::Layout>,
    hover: Option<ui::Hit>,
    pressed: Option<ui::Hit>,
    /// 本次按下落在了 UI 上：拖拽期间既不转视角也不拖时间轴。
    ui_capture: bool,
    /// 正在拖时间轴。
    timeline_drag: bool,
    /// UI 位图需要重画（状态变了才重画，鼠标划过每个像素都重画会白烧 CPU）。
    ui_dirty: bool,
    /// 无头导出自测的状态：`ASSET3D_GUI_SELFTEST` 没设就是 `None`，正常运行
    /// 一条分支都不会多走。
    /// 播放截帧形态的状态：`ASSET3D_POSE_SHOT` 没设就是 `None`。
    pose_shots: Option<PoseShots>,
    selftest: Option<SelfTest>,
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
        // 相对路径只按「工作区根」拼：`open` 帧带来的根优先于 `--workspace`。
        match self.workspace_root() {
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
                    "asset3d-gui: {}，骨骼线框 {} 条 / {} 个关节（当前{}）",
                    rig.animation_summary(),
                    rig.bone_lines().len(),
                    rig.joint_points().len(),
                    if self.show_bones { "显示" } else { "隐藏" }
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
        // 换了资产就要重画工具条 / 时间轴文案（时长、动画名都变了）。
        self.ui_dirty = true;
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
        self.popup_open = false;
        self.status.clear();
        self.status_bad = false;
        self.ui_dirty = true;
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

    /// 键盘 `N`：向后切一条（保持原有语义）。
    fn cycle_animation(&mut self) {
        self.step_animation(1);
    }

    /// 切到相邻动画。`delta` 为 -1 时向前、+1 时向后，两端**环绕**——动画多的
    /// 资产上点「上一动画」不该在第一条上卡住。
    fn step_animation(&mut self, delta: isize) {
        let Some((next, name, duration)) = self.rig.as_ref().and_then(|rig| {
            let count = rig.animation_count();
            if count == 0 {
                return None;
            }
            let next = match self.animation {
                Some(index) => (index as isize + delta).rem_euclid(count as isize) as usize,
                // 还没选中任何动画：向后取第一条，向前取最后一条。
                None => {
                    if delta < 0 {
                        count - 1
                    } else {
                        0
                    }
                }
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
        // 没有骨架的资产（纯静态网格）打开开关也看不到任何线：说清原因，别让
        // 用户对着「骨骼：开」的按钮怀疑是不是坏了。
        let has_bones = self
            .rig
            .as_ref()
            .map(|rig| !rig.bone_lines().is_empty())
            .unwrap_or(false);
        if self.show_bones && !has_bones {
            self.set_status("当前资产没有骨骼，显示不出骨架", true);
            return;
        }
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

    /// 工作区根：`open` 帧的 `workspace` 字段 > `--workspace` 参数。
    ///
    /// 只用于**解析相对路径**，所以不把「当前资产所在目录」算进来——那是给对话框
    /// 起步用的（[`Self::dialog_root`]），拿来拼相对路径只会开出个错文件。
    fn workspace_root(&self) -> Option<PathBuf> {
        if let Some(root) = &self.host_workspace {
            return Some(root.clone());
        }
        match &self.mode {
            Mode::Hosted { workspace, .. } | Mode::Standalone { workspace, .. } => workspace.clone(),
            Mode::Manual => None,
        }
    }

    /// 打开 / 保存对话框的初始目录：没有明确的工作区就退到当前资产所在目录，
    /// 再没有就交给系统默认。用户永远不用从「此电脑」翻起。
    fn dialog_root(&self) -> Option<PathBuf> {
        self.workspace_root().or_else(|| {
            self.current
                .as_ref()
                .and_then(|path| Path::new(path).parent().map(Path::to_path_buf))
        })
    }

    /// 写状态条并重画。成功/失败都走这里：视口没有第二个能显示结果的地方。
    fn set_status(&mut self, text: impl Into<String>, bad: bool) {
        self.status = text.into();
        self.status_bad = bad;
        self.ui_dirty = true;
        self.redraw();
    }

    // ---- 工作区面板 ----

    /// 后台扫描工作区。`force` 为真时即使根没变也重扫（用户可能刚在磁盘上加了模型）。
    fn sync_workspace(&mut self, force: bool) {
        let Some(root) = self.dialog_root() else {
            return;
        };
        let same = self.workspace_root.as_deref() == Some(root.as_path());
        if same && !force {
            return;
        }
        if self.scanning && same {
            return;
        }
        self.workspace_root = Some(root.clone());
        self.workspace = None;
        self.panel_scroll = 0;
        self.scanning = true;
        self.ui_dirty = true;
        // 扫描一定在后台线程：`D:\share` 这种目录几万项，主线程扫一遍窗口直接白屏。
        let proxy = self.proxy.clone();
        let target = root.clone();
        std::thread::spawn(move || {
            let scan = match workspace::scan(&target) {
                Ok(scan) => scan,
                Err(err) => {
                    eprintln!("asset3d-gui: 扫描工作区 {} 失败：{err}", target.display());
                    workspace::Scan::default()
                }
            };
            if proxy
                .send_event(UserEvent::Assets {
                    root: target,
                    scan,
                })
                .is_err()
            {
                // 事件循环已退出：扫描结果没人收，正常丢。
            }
        });
    }

    fn toggle_panel(&mut self) {
        self.panel_open = !self.panel_open;
        if self.panel_open {
            self.sync_workspace(false);
        }
        self.ui_dirty = true;
        self.redraw();
    }

    /// 面板滚动。滚到头就停住，不回卷——回卷会让「再滚一格」变成反向跳。
    fn scroll_panel(&mut self, delta: isize) {
        let total = self
            .workspace
            .as_ref()
            .map(|scan| scan.entries.len())
            .unwrap_or(0);
        let visible = self
            .layout
            .as_ref()
            .map(|layout| layout.visible_rows)
            .unwrap_or(0)
            .max(1);
        let max = total.saturating_sub(visible) as isize;
        let next = (self.panel_scroll as isize + delta).clamp(0, max) as usize;
        if next != self.panel_scroll {
            self.panel_scroll = next;
            self.ui_dirty = true;
            self.redraw();
        }
    }

    /// 点面板里的第 `index` 条直接载入。
    fn open_row(&mut self, index: usize) {
        let Some(entry) = self
            .workspace
            .as_ref()
            .and_then(|scan| scan.entries.get(index))
            .cloned()
        else {
            return;
        };
        match self.load(&entry.path) {
            Ok(()) => {
                self.set_status(format!("已载入 {}", entry.rel), false);
                self.emit_state();
            }
            Err(err) => {
                eprintln!("asset3d-gui: 打开 {} 失败：{err}", entry.path.display());
                self.set_status(format!("载入 {} 失败：{err}", entry.rel), true);
                self.report(0, "load_failed", &err);
            }
        }
    }

    // ---- 工具条 ----

    fn on_button(&mut self, id: ui::ButtonId) {
        match id {
            ui::ButtonId::Import => self.ask_for_file(),
            ui::ButtonId::Play => self.set_playing(!self.playing),
            ui::ButtonId::Loop => self.toggle_loop(),
            ui::ButtonId::PrevAnim => self.step_animation(-1),
            ui::ButtonId::NextAnim => self.step_animation(1),
            ui::ButtonId::Bones => self.toggle_bones(),
            ui::ButtonId::Panel => self.toggle_panel(),
            ui::ButtonId::ExportAir => {
                // 没有资产时导出菜单没有意义：直接提示，免得用户选了格式才发现没源。
                if self.current.is_none() {
                    self.set_status("先导入一个资产，再导出 AIR", true);
                    return;
                }
                self.popup_open = !self.popup_open;
                self.ui_dirty = true;
                self.redraw();
            }
            ui::ButtonId::ExportLoader => self.export_loader(),
        }
    }

    fn export_stem(&self) -> String {
        self.current
            .as_ref()
            .and_then(|path| {
                Path::new(path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
            })
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "asset".to_string())
    }

    /// 导出对话框的初始目录：优先源资产所在目录（导出通常和源放一起）。
    fn export_dir(&self) -> Option<PathBuf> {
        self.current
            .as_ref()
            .and_then(|path| Path::new(path).parent().map(Path::to_path_buf))
            .or_else(|| self.dialog_root())
    }

    /// 导出 AIR。`format` 取 `bin` / `json` / `both`，语义与插件 `tools.rs` 完全一致
    /// （都走 [`export_core::export_air`]）。
    fn export_air(&mut self, format: &str) {
        let Some(source) = self.current.as_ref().map(PathBuf::from) else {
            self.set_status("先导入一个资产，再导出 AIR", true);
            return;
        };
        let owner = hwnd_of_opt(self.window.as_deref());
        let stem = self.export_stem();
        let want_bin = format == "bin" || format == "both";
        let want_json = format == "json" || format == "both";
        let mut out_bin = None;
        let mut out_json = None;
        if want_bin {
            let name = format!("{stem}.air.bin");
            match self.ask_export_path("导出 AIR 二进制", AIR_BIN_FILTER, "bin", &name) {
                Some(path) => out_bin = Some(path),
                None => {
                    self.set_status("已取消导出", false);
                    return;
                }
            }
        }
        if want_json {
            let name = format!("{stem}.air.json");
            match self.ask_export_path("导出 AIR JSON", AIR_JSON_FILTER, "json", &name) {
                Some(path) => out_json = Some(path),
                None => {
                    self.set_status("已取消导出", false);
                    return;
                }
            }
        }
        let what = match format {
            "bin" => "AIR 二进制",
            "json" => "AIR JSON",
            _ => "AIR（二进制 + JSON）",
        };
        match export_core::export_air(&source, format, out_bin.as_deref(), out_json.as_deref()) {
            Ok(exported) => {
                let text = format!("已导出{what}：{}", exported.summary());
                eprintln!("asset3d-gui: {text}");
                self.set_status(text, false);
            }
            Err(err) => {
                // 解析失败 / 有阻断级问题 / 写盘失败：一律弹框 + 写状态条，进程不退。
                eprintln!("asset3d-gui: 导出{what}失败：{err}");
                self.set_status(format!("导出{what}失败：{err}"), true);
                // 自测里绝不弹模态框：弹出来就没人点，整轮验证会卡死在这里。
                if self.selftest.is_none() {
                    win32::show_error(owner, "asset3d 导出失败", &err);
                }
            }
        }
    }

    /// 导出引擎加载器 `.rs`（模板与插件侧同一份）。
    fn export_loader(&mut self) {
        let owner = hwnd_of_opt(self.window.as_deref());
        let name = format!("{}.air_loader.rs", self.export_stem());
        let Some(target) =
            self.ask_export_path("导出加载器（.rs）", LOADER_FILTER, "rs", &name)
        else {
            self.set_status("已取消导出", false);
            return;
        };
        match export_core::export_loader(&target) {
            Ok(size) => {
                let text = format!(
                    "已导出加载器：{}（{}）",
                    target.display(),
                    util::format_bytes(size)
                );
                eprintln!("asset3d-gui: {text}");
                self.set_status(text, false);
            }
            Err(err) => {
                eprintln!("asset3d-gui: 导出加载器失败：{err}");
                self.set_status(format!("导出加载器失败：{err}"), true);
                if self.selftest.is_none() {
                    win32::show_error(owner, "asset3d 导出加载器失败", &err);
                }
            }
        }
    }

    /// 取一个导出目标路径：正常运行时弹「另存为」，自测时直接由环境变量给的目录
    /// 拼名字（跳过模态对话框——它一弹，无人值守就整轮停住）。
    ///
    /// 两条路的**产物路径语义相同**：都是「目录 + 默认文件名」，所以自测验证到的
    /// 落盘行为与用户点确定之后完全一致。
    fn ask_export_path(
        &self,
        title: &str,
        filter: &str,
        default_ext: &str,
        default_name: &str,
    ) -> Option<PathBuf> {
        if let Some(selftest) = &self.selftest {
            let dir = selftest.dir.join(selftest.tag);
            if let Err(err) = std::fs::create_dir_all(&dir) {
                eprintln!("asset3d-gui: 自测目录 {} 建不出来：{err}", dir.display());
                return None;
            }
            return Some(dir.join(default_name));
        }
        win32::pick_save_file(
            hwnd_of_opt(self.window.as_deref()),
            title,
            filter,
            default_ext,
            default_name,
            self.export_dir().as_deref(),
        )
    }

    // ---- 无头导出自测（仅 ASSET3D_GUI_SELFTEST）----

    /// 给接下来的导出动作选子目录，避免 bin / json / both / loader 互相覆盖。
    fn selftest_tag(&mut self, tag: &'static str) {
        if let Some(selftest) = self.selftest.as_mut() {
            selftest.tag = tag;
        }
    }

    fn selftest_note(&mut self, line: String) {
        if let Some(selftest) = self.selftest.as_mut() {
            eprintln!("asset3d-gui: [selftest] {line}");
            selftest.trace.push(line);
        }
    }

    /// 按控件中心合成一次点击。
    ///
    /// 关键在于「同一份布局、同一个入口」：布局取 `ui::Layout::compute` 的结果
    /// （`refresh_ui` 里算的那一份，和绘制用的一模一样），点击调用
    /// `on_left_pressed` / `on_left_released`（真实鼠标事件也只调这两个），
    /// 命中走 `Layout::hit`。这里没有任何绕过按钮直接调 `export_core` 的捷径。
    fn selftest_click(&mut self, target: SelfTestTarget) -> Result<Option<ui::Hit>, String> {
        if self.ui_dirty || self.layout.is_none() {
            self.refresh_ui();
        }
        let (x, y) = {
            let layout = self
                .layout
                .as_ref()
                .ok_or_else(|| "布局还没算出来（UI 未合成）".to_string())?;
            let rect = match target {
                SelfTestTarget::Button(id) => layout
                    .buttons
                    .iter()
                    .find(|(button, _, _)| *button == id)
                    .map(|(_, rect, _)| *rect)
                    .ok_or_else(|| format!("工具条里没有 {:?} 这个按钮", id))?,
                SelfTestTarget::PopupItem(index) => *layout
                    .popup_items
                    .get(index)
                    .ok_or_else(|| format!("下拉菜单里没有第 {index} 项"))?,
                // 面板行带滚动偏移，所以按「绝对下标」找，不能用 `rows[index]`。
                SelfTestTarget::Row(index) => layout
                    .rows
                    .iter()
                    .find(|(row, _, _)| *row == index)
                    .map(|(_, rect, _)| *rect)
                    .ok_or_else(|| format!("目录面板里看不到第 {index} 行"))?,
            };
            (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
        };
        // 与鼠标事件同序：先落坐标，再按下、松开。`on_left_released` 内部会再做
        // 一次命中并要求与按下时一致——这就是真实点击的「按在哪儿、松在哪儿」。
        self.last_cursor = Some(PhysicalPosition::new(x as f64, y as f64));
        let hit = self.hit_at(x, y);
        self.on_left_pressed();
        self.on_left_released();
        Ok(hit)
    }

    /// 点一个控件并核对命中结果：命中的分支必须是预期的那个，否则记失败但仍往下
    /// 走（后面几步会自然失败，报告里能看到整条时间线）。
    fn selftest_expect(&mut self, target: SelfTestTarget) {
        let want = target.describe();
        match self.selftest_click(target) {
            Ok(hit) => {
                let got = hit
                    .map(|hit| format!("{hit:?}"))
                    .unwrap_or_else(|| "None".to_string());
                let ok = got == want;
                if !ok {
                    if let Some(selftest) = self.selftest.as_mut() {
                        selftest.failed = true;
                    }
                }
                self.selftest_note(format!("点 {want} → 命中 {got}（ok={ok}）"));
            }
            Err(err) => {
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.failed = true;
                }
                self.selftest_note(format!("点 {want} 失败：{err}"));
            }
        }
    }

    /// 推进一步自测。
    ///
    /// 步骤表（前六步是「IDE 目录 → GUI 按键导入」那条链，后面接原有的导出链）：
    /// 0 起步 → 1 点「目录」→ 2 等后台扫描 → 3 截面板图 → 4 点面板首行 →
    /// 5 截载入后的图 → 6..12 三种 AIR 导出 + loader → 之后收尾。
    fn selftest_step(&mut self) {
        let Some(step) = self.selftest.as_ref().map(|selftest| selftest.step) else {
            return;
        };
        // 需要等外部事件的步骤：条件没满足就**不推进**，让节拍线程再投一次。
        // 扫描是后台线程投 `UserEvent::Assets` 回来的，写死步数会让它撞车。
        if !self.selftest_ready(step) {
            let waited = {
                let selftest = self.selftest.as_mut().expect("自测开关已判过");
                selftest.waits += 1;
                selftest.waits
            };
            if waited > SELFTEST_WAIT_LIMIT {
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.failed = true;
                }
                self.selftest_note(format!("第 {step} 步等待超时（{SELFTEST_WAIT_LIMIT} 拍）"));
                self.selftest_finish();
            } else {
                self.redraw();
            }
            return;
        }
        if let Some(selftest) = self.selftest.as_mut() {
            selftest.step += 1;
            selftest.waits = 0;
        }
        match step {
            0 => {
                // 起步：资产在 `resumed` 里已经载入（自测一律用 `--standalone` 起），
                // 这里只把首版 UI 合成出来，后面每步才知道按钮画在哪。
                self.refresh_ui();
                let asset = self.current.clone().unwrap_or_else(|| "（无）".to_string());
                let root = self
                    .workspace_root()
                    .map(|root| root.display().to_string())
                    .unwrap_or_else(|| "（无）".to_string());
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.workspace = Some(root.clone());
                }
                self.selftest_note(format!("自测开始，资产 {asset}，工作区根 {root}"));
            }
            1 => {
                self.selftest_expect(SelfTestTarget::Button(ui::ButtonId::Panel));
                // 命中对 + 面板真的开了，才算这一步过。
                let ok = self.panel_open && self.workspace_root.is_some();
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.panel_ok = ok;
                    if !ok {
                        selftest.failed = true;
                    }
                }
                self.selftest_note(format!(
                    "面板 open={} root={:?} scanning={}",
                    self.panel_open, self.workspace_root, self.scanning
                ));
            }
            2 => {
                // 走到这里说明 `selftest_ready(2)` 已确认扫描结束。
                let (total, first) = self
                    .workspace
                    .as_ref()
                    .map(|scan| {
                        (
                            scan.entries.len(),
                            scan.entries.first().map(|entry| entry.rel.clone()),
                        )
                    })
                    .unwrap_or((0, None));
                let truncated = self
                    .workspace
                    .as_ref()
                    .map(|scan| scan.truncated)
                    .unwrap_or(false);
                let ok = total > 0 && first.is_some();
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.scan_total = Some(total);
                    selftest.first_row = first.clone();
                    if !ok {
                        selftest.failed = true;
                    }
                }
                self.selftest_note(format!(
                    "面板扫描完成：{total} 条，首行 {:?}，truncated={truncated}",
                    first.unwrap_or_else(|| "（无）".to_string())
                ));
            }
            3 => {
                // 上一拍已经 RedrawRequested 过一次，此时抓屏抓到的才是带面板的那一帧。
                self.selftest_shot("01-panel-open");
            }
            4 => {
                let before = self.window_title();
                let first_path = self
                    .workspace
                    .as_ref()
                    .and_then(|scan| scan.entries.first())
                    .map(|entry| entry.path.clone());
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.title_before = before.clone();
                }
                self.selftest_expect(SelfTestTarget::Row(0));
                let after = self.window_title();
                let current = self.current.clone();
                // 断言：载入的资产确实换成了所点的那一行（路径逐字比较）。
                let ok = match (&first_path, &current) {
                    (Some(want), Some(got)) => want.display().to_string() == *got,
                    _ => false,
                };
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.title_after = after.clone();
                    selftest.row_loaded_ok = ok;
                    if !ok {
                        selftest.failed = true;
                    }
                }
                self.selftest_note(format!(
                    "点首行 → 载入 {:?}（期望 {:?}，ok={ok}）；标题「{}」→「{}」",
                    current,
                    first_path.map(|path| path.display().to_string()),
                    before,
                    after
                ));
            }
            5 => self.selftest_shot("02-row-loaded"),
            6 => self.selftest_expect(SelfTestTarget::Button(ui::ButtonId::ExportAir)),
            7 => {
                self.selftest_tag("air-bin");
                self.selftest_expect(SelfTestTarget::PopupItem(0));
            }
            8 => self.selftest_expect(SelfTestTarget::Button(ui::ButtonId::ExportAir)),
            9 => {
                self.selftest_tag("air-json");
                self.selftest_expect(SelfTestTarget::PopupItem(1));
            }
            10 => self.selftest_expect(SelfTestTarget::Button(ui::ButtonId::ExportAir)),
            11 => {
                self.selftest_tag("air-both");
                self.selftest_expect(SelfTestTarget::PopupItem(2));
            }
            12 => {
                self.selftest_tag("loader");
                self.selftest_expect(SelfTestTarget::Button(ui::ButtonId::ExportLoader));
            }
            _ => self.selftest_finish(),
        }
    }

    /// 这一步是否已经可以执行（只拦「等后台事件」的那两步）。
    fn selftest_ready(&self, step: usize) -> bool {
        match step {
            // 等后台扫描收工：`scanning` 归位且结果已回主线程。
            2 => !self.scanning && self.workspace.is_some() && self.panel_open,
            // 截图前必须先有一帧画完的：`pending_shot` 是上一拍置的，这里只确认
            // 窗口与 gfx 已就绪。
            3 | 5 => self.window.is_some() && self.gfx.is_some(),
            _ => true,
        }
    }

    /// 抓一张客户区截图存进自测目录；抓不到就记一条失败（截图是证据，不是可选项）。
    fn selftest_shot(&mut self, tag: &str) {
        let Some(window) = self.window.as_ref() else {
            if let Some(selftest) = self.selftest.as_mut() {
                selftest.failed = true;
            }
            self.selftest_note(format!("截图 {tag} 失败：还没有窗口"));
            return;
        };
        let dir = self
            .selftest
            .as_ref()
            .map(|selftest| selftest.dir.join("shots"))
            .unwrap_or_else(|| std::env::temp_dir().join("asset3d-selftest").join("shots"));
        if let Err(err) = std::fs::create_dir_all(&dir) {
            if let Some(selftest) = self.selftest.as_mut() {
                selftest.failed = true;
            }
            self.selftest_note(format!("截图 {tag} 失败：建目录 {err}"));
            return;
        }
        let path = dir.join(format!("{tag}.png"));
        let hwnd = hwnd_of(window);
        match viewport::shot::capture_client(hwnd, &path) {
            Ok(shot) => {
                let (width, height, method) = (shot.width, shot.height, shot.method.to_string());
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest
                        .shots
                        .push((tag.to_string(), path.clone(), width, height, method.clone()));
                }
                self.selftest_note(format!(
                    "截图 {tag} → {}（{width}×{height} via {method}）",
                    path.display()
                ));
            }
            Err(err) => {
                if let Some(selftest) = self.selftest.as_mut() {
                    selftest.failed = true;
                }
                self.selftest_note(format!("截图 {tag} 失败：{err}"));
            }
        }
    }

    /// 收尾：只认磁盘上的文件（这是「真的落盘了」的唯一证据），再把 bin 读回来，
    /// 写报告并按结果定退出码。
    fn selftest_finish(&mut self) {
        let Some(selftest) = self.selftest.as_ref() else {
            return;
        };
        let dir = selftest.dir.clone();
        let report_path = selftest.report.clone();
        let trace = selftest.trace.clone();
        let clicked_all = !selftest.failed;
        let asset = self.current.clone().unwrap_or_default();
        let workspace = selftest.workspace.clone();
        let scan_total = selftest.scan_total;
        let first_row = selftest.first_row.clone();
        let title_before = selftest.title_before.clone();
        let title_after = selftest.title_after.clone();
        let panel_ok = selftest.panel_ok;
        let row_loaded_ok = selftest.row_loaded_ok;
        let shots = selftest.shots.clone();

        let mut files: Vec<(String, PathBuf, u64, String)> = Vec::new();
        for kind in ["air-bin", "air-json", "air-both", "loader"] {
            let Ok(entries) = std::fs::read_dir(dir.join(kind)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let bytes = std::fs::read(&path).unwrap_or_default();
                let head = bytes
                    .iter()
                    .take(8)
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                files.push((kind.to_string(), path, bytes.len() as u64, head));
            }
        }
        let pick = |kind: &str, suffix: &str| -> Option<(PathBuf, u64, String)> {
            files
                .iter()
                .find(|(file_kind, path, ..)| {
                    file_kind == kind && path.to_string_lossy().ends_with(suffix)
                })
                .map(|(_, path, size, head)| (path.clone(), *size, head.clone()))
        };

        let bin = pick("air-bin", ".air.bin");
        let json = pick("air-json", ".air.json");
        let both_bin = pick("air-both", ".air.bin");
        let both_json = pick("air-both", ".air.json");
        let loader = pick("loader", ".rs");

        // bin 的头四个字节就是 `AIRB`（41 49 52 42），先按魔数认一遍。
        let bin_ok = bin
            .as_ref()
            .map(|(_, size, head)| *size > 0 && head.starts_with("41 49 52 42"))
            .unwrap_or(false);
        // pretty JSON 的第一个字节是 `{`（7b）。
        let json_ok = json
            .as_ref()
            .map(|(_, size, head)| *size > 2 && head.starts_with("7b"))
            .unwrap_or(false);
        let both_ok = both_bin.is_some() && both_json.is_some();
        let rs_ok = loader
            .as_ref()
            .map(|(path, size, _)| {
                *size > 0
                    && std::fs::read_to_string(path)
                        .map(|text| {
                            text.contains("pub fn")
                                && export_core::find_placeholder(&text).is_none()
                        })
                        .unwrap_or(false)
            })
            .unwrap_or(false);

        // 读回：视口不认识 `.air.bin`，资产解析链上唯一的入口就是 `air_bin::decode`，
        // 所以「能被现有加载路径读回」= 魔数对 + decode 成功 + 与源资产逐顶点抽样一致。
        let (readback_ok, readback) = match bin.as_ref() {
            Some((path, ..)) => match std::fs::read(path) {
                Ok(bytes) if !air_bin::looks_like_airb(&bytes) => (
                    false,
                    serde_json::json!({ "ok": false, "detail": "魔数不是 AIRB" }),
                ),
                Ok(bytes) => match air_bin::decode(&bytes) {
                    Ok(doc) => {
                        let verify = if asset.is_empty() {
                            Err("没有源资产可比对".to_string())
                        } else {
                            export_core::import_doc(Path::new(&asset))
                                .and_then(|source| export_core::verify(&source, &doc))
                        };
                        match verify {
                            Ok(()) => (
                                true,
                                serde_json::json!({
                                    "ok": true,
                                    "magic": "AIRB",
                                    "nodes": doc.nodes.len(),
                                    "meshes": doc.meshes.len(),
                                    "materials": doc.materials.len(),
                                    "textures": doc.textures.len(),
                                    "animations": doc.animations.len(),
                                    "detail": "air_bin::decode 成功，且与源资产抽样逐分量一致",
                                }),
                            ),
                            Err(err) => (
                                false,
                                serde_json::json!({ "ok": false, "magic": "AIRB", "detail": err }),
                            ),
                        }
                    }
                    Err(err) => (
                        false,
                        serde_json::json!({ "ok": false, "detail": format!("air_bin::decode 失败：{err}") }),
                    ),
                },
                Err(err) => (
                    false,
                    serde_json::json!({ "ok": false, "detail": format!("读回失败：{err}") }),
                ),
            },
            None => (
                false,
                serde_json::json!({ "ok": false, "detail": "没有 .air.bin 产物" }),
            ),
        };

        // 面板链路：工作区根非空 + 扫到 ≥1 条 + 点首行后资产变成首行那条 + 两张截图。
        let shots_ok = shots.len() >= 2;
        let all_ok = clicked_all
            && panel_ok
            && row_loaded_ok
            && shots_ok
            && bin_ok
            && json_ok
            && both_ok
            && rs_ok
            && readback_ok;
        let payload = serde_json::json!({
            "selftest": true,
            "asset": asset,
            "workspace": workspace,
            "out_dir": dir.display().to_string(),
            "steps": trace,
            "panel": {
                "ok": panel_ok,
                "scanned": scan_total,
                "first_row": first_row,
                "title_before": title_before,
                "title_after": title_after,
                "loaded_ok": row_loaded_ok,
            },
            "shots": shots
                .iter()
                .map(|(label, path, width, height, method)| {
                    serde_json::json!({
                        "label": label,
                        "path": path.display().to_string(),
                        "width": width,
                        "height": height,
                        "method": method,
                    })
                })
                .collect::<Vec<_>>(),
            "files": files
                .iter()
                .map(|(kind, path, size, head)| {
                    serde_json::json!({
                        "kind": kind,
                        "path": path.display().to_string(),
                        "bytes": size,
                        "head_hex": head,
                    })
                })
                .collect::<Vec<_>>(),
            "checks": {
                "panel_ok": panel_ok,
                "row_loaded_ok": row_loaded_ok,
                "shots_ok": shots_ok,
                "bin_ok": bin_ok,
                "json_ok": json_ok,
                "both_ok": both_ok,
                "rs_ok": rs_ok,
                "readback": readback,
            },
            "all_ok": all_ok,
        });
        let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
        if let Some(parent) = report_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&report_path, &text) {
            Ok(()) => eprintln!(
                "asset3d-gui: [selftest] 报告 {}（all_ok={all_ok}）",
                report_path.display()
            ),
            Err(err) => eprintln!("asset3d-gui: [selftest] 报告写不进去：{err}"),
        }
        // 自测不是交互程序：写完报告直接退，别等人来关窗；退出码给脚本用。
        std::process::exit(if all_ok { 0 } else { 1 });
    }

    // ---- 时间轴 ----

    /// 时间轴那一行的说明文字。没有动画就整行不画（留空由 `ui` 判）。
    fn timeline_text(&self) -> String {
        let duration = self.duration();
        if duration <= 0.0 {
            return String::new();
        }
        let count = self
            .rig
            .as_ref()
            .map(|rig| rig.animation_count())
            .unwrap_or(0);
        let index = self.animation.map(|index| index + 1).unwrap_or(0);
        let name = self
            .animation_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or("(未命名)");
        format!("动画 {index}/{count} · {name} · {:.2}/{:.2}s", self.time, duration)
    }

    /// 按进度（0..=1）定位播放头。拖动时连续调用，所以只做必要的事：
    /// 求值姿态 → 推 GPU → 报 `state`（`refresh_view` 顺带刷标题与重绘）。
    fn scrub_to_progress(&mut self, progress: f32) {
        let duration = self.duration();
        if duration <= 0.0 {
            return;
        }
        self.time = (progress.clamp(0.0, 1.0) * duration).clamp(0.0, duration);
        // 拖动即停播：一边拖一边自动前进，永远定位不到想看的姿势。
        self.playing = false;
        self.last_tick = None;
        self.refresh_view();
    }

    // ---- 命中测试与 UI 刷新 ----

    fn hit_at(&self, x: f32, y: f32) -> Option<ui::Hit> {
        self.layout.as_ref().and_then(|layout| layout.hit(x, y))
    }

    /// 重算布局并重画一屏 UI 位图。只在 `ui_dirty`（状态变化 / 尺寸变化）时调用。
    fn refresh_ui(&mut self) {
        let Some((width, height)) = self.gfx.as_ref().map(|gfx| gfx.size()) else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        let scale = self
            .window
            .as_ref()
            .map(|window| window.scale_factor() as f32)
            .unwrap_or(1.0)
            .max(0.5);
        // 标签表复制一份：`LayoutInput` 借它，而下面要可变借用 `self` 存布局。
        let labels: Vec<String> = self
            .workspace
            .as_ref()
            .map(|scan| scan.entries.iter().map(|entry| entry.rel.clone()).collect())
            .unwrap_or_default();
        let truncated = self.workspace.as_ref().map(|scan| scan.truncated).unwrap_or(false);
        let timeline_info = self.timeline_text();
        let input = ui::LayoutInput {
            width,
            height,
            scale,
            playing: self.playing,
            looping: self.looping,
            show_bones: self.show_bones,
            has_asset: self.current.is_some(),
            panel: ui::PanelView {
                open: self.panel_open,
                scanning: self.scanning,
                truncated,
                scroll: self.panel_scroll,
                labels: &labels,
            },
            popup: self.popup_open,
            status: &self.status,
            status_bad: self.status_bad,
            timeline_info: &timeline_info,
            hover: self.hover,
            pressed: self.pressed,
        };
        let layout = ui::Layout::compute(&input);
        let bitmap = ui::render(&input, &layout);
        // 位图画完就解掉对 `self` 字段的借用，才能把新布局写回去。
        drop(input);
        self.layout = Some(layout);
        if let (Some((bitmap_w, bitmap_h, rgba)), Some(gfx)) = (bitmap, self.gfx.as_mut()) {
            gfx.set_ui_bitmap(bitmap_w, bitmap_h, &rgba);
        }
        self.ui_dirty = false;
    }

    /// 左键按下：先判 UI（工具条 / 菜单 / 面板），再判时间轴，最后才是转视角。
    fn on_left_pressed(&mut self) {
        let Some(cursor) = self.last_cursor else {
            // 还没收到过光标位置（刚启动就按）：按「转视角」处理，最无害。
            self.dragging = true;
            return;
        };
        let (x, y) = (cursor.x as f32, cursor.y as f32);

        // 菜单展开时，点菜单外面第一下只负责收菜单——不能顺手再触发一次按钮，
        // 否则「关掉菜单」会变成「又导出了一次」。
        if self.popup_open {
            self.ui_capture = true;
            match self.hit_at(x, y) {
                Some(hit @ ui::Hit::PopupItem(_)) => {
                    self.pressed = Some(hit);
                }
                _ => {
                    self.popup_open = false;
                    self.pressed = None;
                }
            }
            self.ui_dirty = true;
            self.redraw();
            return;
        }

        if let Some(hit) = self.hit_at(x, y) {
            self.pressed = Some(hit);
            self.ui_capture = true;
            self.ui_dirty = true;
            self.redraw();
            return;
        }

        // 时间轴：有动画时底部那条带子既是显示也是拖动条。
        if self.duration() > 0.0 {
            if let Some(progress) = self
                .gfx
                .as_ref()
                .and_then(|gfx| gfx.timeline_progress_at(x, y))
            {
                self.timeline_drag = true;
                self.ui_capture = true;
                self.scrub_to_progress(progress);
                return;
            }
        }

        // 面板盖住的空白处：既不开文件也不转视角（拖面板把模型转飞很出戏）。
        if self
            .layout
            .as_ref()
            .map(|layout| layout.panel_covers(x, y))
            .unwrap_or(false)
        {
            self.ui_capture = true;
            return;
        }

        self.dragging = true;
    }

    /// 左键松开：只有「按下与松开落在同一个控件上」才算点击（拖出去等于取消）。
    fn on_left_released(&mut self) {
        let pressed = self.pressed.take();
        let cursor = self.last_cursor;
        self.dragging = false;
        self.timeline_drag = false;
        self.ui_capture = false;
        self.ui_dirty = true;
        self.redraw();
        // 松开时清掉基准点：不然下次按下后的第一帧会拿旧坐标算出一次大跳。
        self.last_cursor = None;
        let (Some(hit), Some(cursor)) = (pressed, cursor) else {
            return;
        };
        if self.hit_at(cursor.x as f32, cursor.y as f32) == Some(hit) {
            self.activate(hit);
        }
    }

    fn activate(&mut self, hit: ui::Hit) {
        // 点击留痕：从控制台启动时能一眼看出点中了哪个控件，排障比截图可靠。
        eprintln!("asset3d-gui: 点击 {hit:?}");
        match hit {
            ui::Hit::Button(id) => self.on_button(id),
            ui::Hit::Row(index) => self.open_row(index),
            // 菜单项：先收菜单再动作，导出对话框弹出时背后不会挂着一层菜单。
            ui::Hit::PopupItem(0) => {
                self.popup_open = false;
                self.ui_dirty = true;
                self.export_air("bin");
            }
            ui::Hit::PopupItem(1) => {
                self.popup_open = false;
                self.ui_dirty = true;
                self.export_air("json");
            }
            ui::Hit::PopupItem(_) => {
                self.popup_open = false;
                self.ui_dirty = true;
                self.export_air("both");
            }
        }
    }

    /// 离散操作后统一收口：重算姿态、刷标题、重绘、报 `state`。
    ///
    /// `ui_dirty` 在这里一并置位：播放/暂停、循环、切动画、拖时间轴都会改工具条与
    /// 时间轴文案，漏一处就会出现「按钮写着播放、其实已经在播」。
    fn refresh_view(&mut self) {
        self.pose();
        if let Some(window) = &self.window {
            window.set_title(&self.window_title());
            window.request_redraw();
        }
        self.ui_dirty = true;
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
    ///
    /// 对话框的初始目录按 `workspace` 帧字段 > `--workspace` > 当前资产目录三级取，
    /// 用户不用每次从「此电脑」翻起。
    fn ask_for_file(&mut self) {
        let owner = hwnd_of_opt(self.window.as_deref());
        let initial = self.dialog_root();
        let Some(path) = win32::pick_asset_file(
            owner,
            "打开 3D 资产",
            ASSET_FILTER,
            initial.as_deref(),
        ) else {
            return;
        };
        match self.load(&path) {
            // 手动打开不自动播：用户按 `O` 是想先看清这个模型，不是想让它动起来。
            Ok(()) => {
                let name = Path::new(&path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                self.set_status(format!("已载入 {name}"), false);
                self.emit_state();
            }
            Err(err) => {
                eprintln!("asset3d-gui: 打开 {} 失败：{err}", path.display());
                self.set_status(format!("载入失败：{err}"), true);
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

    /// 推进一步播放截帧流程。
    ///
    /// 节奏是「设播放头 → 等两帧 → 抓图 → 下一个停靠点」：第一帧可能还在合成 UI
    /// 位图，只等一帧容易抓到上一姿态。窗口 / 骨架没就绪时直接返回，等下一次
    /// `about_to_wait` 再来。
    fn drive_pose_shots(&mut self) {
        if self.window.is_none() || self.rig.is_none() {
            return;
        }
        let Some((at, settle, total)) = self
            .pose_shots
            .as_ref()
            .map(|shots| (shots.at, shots.settle, shots.stops.len()))
        else {
            return;
        };
        if at >= total {
            self.finish_pose_shots();
            return;
        }
        let (label, ratio) = match self.pose_shots.as_ref() {
            Some(shots) => shots.stops[at].clone(),
            None => return,
        };
        match settle {
            0 => {
                let duration = self.duration();
                self.time = (duration * ratio).clamp(0.0, duration);
                self.playing = false;
                self.last_tick = None;
                self.refresh_view();
                if let Some(shots) = self.pose_shots.as_mut() {
                    shots.settle = 1;
                }
            }
            1 => {
                self.redraw();
                if let Some(shots) = self.pose_shots.as_mut() {
                    shots.settle = 2;
                }
            }
            2 => {
                self.redraw();
                if let Some(shots) = self.pose_shots.as_mut() {
                    shots.settle = 3;
                }
            }
            _ => {
                // 热身轮什么都没抓，这一停靠点必须重来：at 不能往前走。
                if self.capture_pose_shot(&label, at) {
                    if let Some(shots) = self.pose_shots.as_mut() {
                        shots.at = at + 1;
                        shots.settle = 0;
                    }
                } else if let Some(shots) = self.pose_shots.as_mut() {
                    shots.settle = 0;
                }
            }
        }
    }

    /// 抓一张当前姿态的客户区截图，结果记进 `pose_shots.shots`。
    fn capture_pose_shot(&mut self, label: &str, index: usize) -> bool {
        let Some(dir) = self.pose_shots.as_ref().map(|shots| shots.dir.clone()) else {
            return true;
        };
        if !self.pose_shots.as_ref().map(|shots| shots.warmed).unwrap_or(false) {
            // 热身轮：丢掉这一张，回到本次停靠点重走「设时刻 → 等三帧 → 抓帧」。
            if let Some(shots) = self.pose_shots.as_mut() {
                shots.warmed = true;
                shots.settle = 0;
            }
            return false;
        }
        if let Err(err) = std::fs::create_dir_all(&dir) {
            eprintln!("asset3d-gui: 截帧目录建不出来（{}）：{err}", dir.display());
            self.pose_shots.as_mut().map(|shots| shots.at = usize::MAX);
            return true;
        }
        let path = dir.join(format!("{index:02}-{label}.png"));
        let Some(window) = self.window.as_ref() else {
            return true;
        };
        let hwnd = hwnd_of(window);
        match viewport::shot::capture_client(hwnd, &path) {
            Ok(shot) => {
                let (width, height, method) = (shot.width, shot.height, shot.method.to_string());
                eprintln!(
                    "asset3d-gui: 截帧 {label} t={:.3}s → {}（{width}×{height} via {method}）",
                    self.time,
                    path.display()
                );
                if let Some(shots) = self.pose_shots.as_mut() {
                    shots
                        .shots
                        .push((label.to_string(), path, width, height, method));
                }
            }
            Err(err) => {
                eprintln!("asset3d-gui: 截帧 {label} 失败：{err}");
                if let Some(shots) = self.pose_shots.as_mut() {
                    // 用 MAX 标记失败：收尾时据此把退出码定成 2。
                    shots.at = usize::MAX;
                }
            }
        }
        true
    }

    /// 收尾：把截帧清单写成 JSON，非全绿则退出码 2。
    fn finish_pose_shots(&mut self) {
        let Some(shots) = self.pose_shots.as_ref() else {
            return;
        };
        let dir = shots.dir.clone();
        let asset = self.current.clone().unwrap_or_default();
        let animation = self.animation_name.clone().unwrap_or_default();
        let duration = self.duration();
        let failed = shots.at == usize::MAX;
        let list = shots.shots.clone();
        let mut json = String::from("{\n");
        json.push_str(&format!(
            "  \"asset\": \"{}\",\n  \"animation\": \"{}\",\n  \"duration\": {:.4},\n",
            asset.replace('\\', "\\\\"),
            animation.replace('"', "'"),
            duration
        ));
        json.push_str("  \"shots\": [\n");
        for (i, (label, path, width, height, method)) in list.iter().enumerate() {
            json.push_str(&format!(
                "    {{\"label\": \"{label}\", \"path\": \"{}\", \"width\": {width}, \"height\": {height}, \"method\": \"{method}\"}}{}\n",
                path.to_string_lossy().replace('\\', "\\\\"),
                if i + 1 == list.len() { "" } else { "," }
            ));
        }
        json.push_str("  ]\n}\n");
        let report = dir.join("pose-shots.json");
        if let Err(err) = std::fs::write(&report, json) {
            eprintln!("asset3d-gui: 截帧清单写不出（{}）：{err}", report.display());
        }
        eprintln!(
            "asset3d-gui: 截帧完成，{} 张，清单 {}",
            list.len(),
            report.display()
        );
        std::process::exit(if failed { 2 } else { 0 });
    }

    /// 请求重绘一次。窗口还没建好时是空操作。
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
            UserEvent::Frame(line) => {
                // `workspace` 是 `open` 帧**新增**的字段，单独取值、不并进
                // `Request::Open`：老视口遇到它照旧当未知字段忽略，新视口多拿一条
                // 线索，协议语义和既有测试都不动（见 `protocol::open_workspace`）。
                if let Some(root) = protocol::open_workspace(&line) {
                    let root = PathBuf::from(root);
                    if self.host_workspace.as_deref() != Some(root.as_path()) {
                        eprintln!("asset3d-gui: 宿主工作区 {}", root.display());
                        self.host_workspace = Some(root);
                        // 换了工作区：面板里的旧清单立刻作废，不能拿上一个项目的
                        // 条目去开文件。
                        if self.panel_open {
                            self.sync_workspace(true);
                        } else {
                            self.workspace = None;
                            self.workspace_root = None;
                        }
                    }
                }
                match protocol::parse(&line) {
                    Ok(req) => self.on_request(req),
                    Err(err) => eprintln!("asset3d-gui: 主线程丢弃畸形帧（{err}）"),
                }
            }
            UserEvent::Assets { root, scan } => {
                self.scanning = false;
                // 过期结果直接丢：扫的过程中工作区可能已经切走了。
                if self.workspace_root.as_deref() == Some(root.as_path()) {
                    eprintln!(
                        "asset3d-gui: 工作区 {} 扫到 {} 个资产{}",
                        root.display(),
                        scan.entries.len(),
                        if scan.truncated { "（已截断）" } else { "" }
                    );
                    self.workspace = Some(scan);
                    self.panel_scroll = 0;
                    self.ui_dirty = true;
                    self.redraw();
                }
            }
            UserEvent::SelfTest => self.selftest_step(),
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
                    // 上一次合成的 UI 位图尺寸已经不对了：顶点着色器按老尺寸换算会
                    // 把工具条画歪。主动清掉，下次 `refresh_ui` 必定重新合成。
                    gfx.clear_ui();
                }
                // UI 位图与窗口同尺寸：尺寸变了必须按新尺寸重画一遍。
                self.ui_dirty = true;
                self.redraw();
            }
            WindowEvent::RedrawRequested => {
                // 先合成 UI 位图再画 3D：UI 是最后叠上去的一层透明纹理。
                if self.ui_dirty {
                    self.refresh_ui();
                }
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
                // `MouseInput` 不带坐标，靠 `CursorMoved` 记下的最新位置做命中。
                match state {
                    ElementState::Pressed => self.on_left_pressed(),
                    ElementState::Released => self.on_left_released(),
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.timeline_drag {
                    // 拖动时间轴：一路扫过整条带子都算（横向出界夹到两端）。
                    if let Some(progress) = self
                        .gfx
                        .as_ref()
                        .and_then(|gfx| gfx.timeline_progress_at(position.x as f32, position.y as f32))
                    {
                        self.scrub_to_progress(progress);
                    }
                    self.last_cursor = Some(position);
                    return;
                }
                if self.dragging {
                    if let Some(last) = self.last_cursor {
                        self.camera.orbit(
                            (position.x - last.x) as f32,
                            (position.y - last.y) as f32,
                        );
                        self.redraw();
                    }
                } else {
                    // hover 反馈：只在真的换了控件时重画，鼠标每移动一像素都重画
                    // 一张全屏位图就是白烧 CPU。
                    let hit = self.hit_at(position.x as f32, position.y as f32);
                    if hit != self.hover {
                        self.hover = hit;
                        self.ui_dirty = true;
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
                // 指针在面板上：滚轮滚列表；否则照旧缩放视角。
                let over_panel = self
                    .last_cursor
                    .map(|cursor| {
                        self.layout
                            .as_ref()
                            .map(|layout| layout.panel_covers(cursor.x as f32, cursor.y as f32))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if over_panel && self.panel_open {
                    self.scroll_panel(-(steps * 3.0) as isize);
                } else {
                    self.camera.zoom(steps);
                    self.redraw();
                }
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
        if self.pose_shots.is_some() {
            // 截帧形态自己控制播放头，播放泵必须停：让它自动前进就抓不到指定时刻。
            self.drive_pose_shots();
            return;
        }
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
