//! `asset3d-gui.exe` —— 3D 视口进程（docs/24 §5.1 / §5.3）。
//!
//! 与插件宿主（`asset3d.exe`）**分进程**：winit 的事件循环会独占线程，塞进宿主的
//! stdio 帧泵里两边只会互相等死。两个进程之间只有一条命名管道，协议见
//! [`viewport::protocol`]（`docs/24 §5.2`）。
//!
//! 这一刀（M-UI-3 第 1 刀）的判据是「进程能起、窗口能开、管道能连」，所以渲染侧
//! 只清屏、`open` 只记录路径不解析资产——把进程/协议问题与图形问题分开验，
//! 免得一次失败要在三个层面里猜。
//!
//! 三种启动形态（argv 区分，`docs/24 §5.3.1`）：
//!
//! | argv | 形态 | 行为 |
//! |---|---|---|
//! | `--pipe <name>` | IDE 托管 | 监听指定管道，IDE 断开后进程继续存活 |
//! | `--standalone <asset>` | 独立看图 | 不建管道，启动即 `open`，窗口标题带资产名 |
//! | 无参数 | 手动双击 | 抢 `Local\asset3d.viewport`；抢不到就把已有窗口置前后退出 |
//!
//! 参数错误退出码 2、路径错误 3（`docs/24 §5.3.1` 约束 1/2）；正常退出 0。

// 复用插件侧的管道客户端：无参形态抢不到互斥体时，用它给已有实例发一帧 `focus`。
// 客户端与视口服务端是同一套约定的两端，能复用就不抄第二份。
#[path = "../pipe.rs"]
// 复用的是插件侧的客户端，它只暴露 `normalize_pipe_path` / `send_line` 两个入口，
// `PIPE_PREFIX` 之类只有插件 crate 自用——bin 里没用上不代表该删，故整模块放行。
#[allow(dead_code)]
mod pipe;
mod viewport;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use viewport::gfx::Gfx;
use viewport::protocol::{self, Request};
use viewport::win32::{self, InstanceLock, LineHandler, PipeServer};

/// 参数互斥 / 缺值：退出码 2（与 `getopt` 系工具的约定一致）。
const EXIT_USAGE: i32 = 2;
/// 路径不存在、不是目录、`canonicalize` 失败：退出码 3。
const EXIT_BAD_PATH: i32 = 3;

/// 窗口默认尺寸：够看一个模型，又不会在 1080p 上盖满整屏。
const DEFAULT_SIZE: (f64, f64) = (1100.0, 720.0);

/// 无资产时的窗口标题。
const TITLE_IDLE: &str = "asset3d 视口";

/// 进程形态。
#[derive(Debug, PartialEq)]
enum Mode {
    /// IDE 托管：监听宿主分配的管道。
    Hosted {
        pipe: String,
        /// 工作区根，用来把 `open` 的工作区相对路径拼成绝对路径。
        ///
        /// 宿主当前只 spawn `--pipe <name>`（`viewport-host.ts`），**没传这一项**，
        /// 所以托管形态下相对路径暂时解析不了——这一刀 `open` 本来也不读盘，
        /// 排在 M-UI-3 第二刀（真解析）之前必须补上，否则视口拿到的是相对路径。
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
    };
    app.seed_standalone();
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
    /// 当前资产（已解析成绝对路径；解析不了时保留原样）。
    current: Option<String>,
}

impl ViewportApp {
    /// `--standalone` 形态启动即打开，不经过管道。
    fn seed_standalone(&mut self) {
        if let Mode::Standalone { asset, .. } = &self.mode {
            self.current = Some(asset.to_string_lossy().to_string());
        }
    }

    fn window_title(&self) -> String {
        match &self.current {
            Some(path) => {
                let name = Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                format!("{name} — {TITLE_IDLE}")
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
    /// 解析不出来时**不报错**：这一刀 `open` 只记录不读盘，路径对不对要等
    /// M-UI-3 第二刀真去打开文件时才知道。但要在 stderr 留一行，免得第二刀
    /// 排查时误以为是解析逻辑丢了。
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

    fn abs_exists(&self, resolved: &str) -> bool {
        Path::new(resolved).is_file()
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
                let absolute = Path::new(&resolved).is_absolute();
                if absolute && !self.abs_exists(&resolved) {
                    self.push_frame(protocol::error_frame(
                        seq,
                        "file_missing",
                        &format!("资产不存在：{resolved}"),
                    ));
                    return;
                }
                self.current = Some(resolved);
                if let Some(window) = &self.window {
                    window.set_title(&self.window_title());
                    window.request_redraw();
                }
                // `state` 是单向帧（seq=0）：`open` 的成功回执就是它，协议里没有
                // 单独的 `ack`——`docs/24 §5.2` 的 op 表里只有这几条。
                self.push_frame(protocol::state_frame(
                    &path,
                    animation.as_deref(),
                    0.0,
                    play,
                ));
            }
            Request::Close { .. } => {
                self.current = None;
                if let Some(window) = &self.window {
                    window.set_title(TITLE_IDLE);
                    window.request_redraw();
                }
                self.push_frame(protocol::state_frame("", None, 0.0, false));
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
                event_loop.exit();
                return;
            }
        }
        window.request_redraw();
        self.window = Some(window);
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
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(gfx) = &mut self.gfx {
                    if let Err(err) = gfx.render() {
                        // 单帧失败不退出：拔显示器、改缩放都会让 surface 抖一下，
                        // 下一次 request_redraw 自然再试。真不可恢复的（显存不足）
                        // 已经由 `Err` 的文案说清了。
                        eprintln!("asset3d-gui: {err}");
                    }
                }
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(pipe) = &mut self.pipe {
            pipe.stop();
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
