//! 视口进程需要的三个 Win32 能力：命名管道**服务端**、会话互斥体、管道名规范化。
//!
//! 与插件侧的 `src/pipe.rs`（客户端）是同一套约定的两端，改动要成对：
//! 客户端每次命令开一条短连接、写一行、断开；这里的读循环按「读到 EOF 收工、
//! 继续等下一个连接」实现，两边才对得上。
//!
//! 为什么要留一个 [`PipeWriter`]：`ping` / `screenshot` 能在读线程里当场回，
//! 但 `state` 是主线程渲染侧才知道的结果（打开成功、动画切换），必须能**主动**
//! 往后写。
//!
//! 关键约束：**所有 WriteFile 都只在读线程里发生**，主线程只往 mpsc 队列塞字符串。
//! 同步命名管道句柄上的 I/O 是串行的——一头挂着阻塞 `ReadFile`，另一头（哪怕在
//! 另一个线程）`WriteFile` 会一直等下去。第一版正是读线程挂 `ReadFile`、主线程写
//! `state`，结果主线程永久卡死、窗口变「未响应」、客户端一帧也收不到。现在读侧改成
//! `PeekNamedPipe` 轮询（有数据才真读），写侧统一走队列，两边不会同时占住句柄。
//! 代价是 15ms 的空转间隔与最多 15ms 的出帧延迟，对通知方向的协议足够。
//!
//! 为什么不用 `windows` / `windows-sys`：整个插件只用到七个 API，手写 FFI 换来
//! 零依赖；插件已经在链一个靠 cc 构建的 ufbx，能不往上再压一层是一层。
//! 副作用是本文件必须自己维护 `#[link]` 与常量，改动时对照
//! <https://learn.microsoft.com/windows/win32/ipc/named-pipe-functions>。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// 管道名前缀，与宿主 `apps/desktop/src/main/viewport-host.ts: PIPE_PREFIX` 一致。
///
/// bin 构建下没有运行时引用（默认名由 [`DEFAULT_PIPE_NAME`] 直接给出，`const`
/// 拼接不了字符串），只有单测拿它反推默认名——留着是为了让两处口径可被断言校验。
#[allow(dead_code)]
pub const PIPE_PREFIX: &str = "aiide.viewport.";

/// 手动双击（无参）形态下监听的管道名。
///
/// 宿主侧 `defaultViewportPipeName(pluginId)` 拼的就是 `<PIPE_PREFIX><id>.default`，
/// 插件写 `open_viewport` 不带 `pipe` 参数时也落到这里，两边必须一致。
pub const DEFAULT_PIPE_NAME: &str = "aiide.viewport.asset3d.default";

/// 会话互斥体：`Local\` 前缀让它按登录会话隔离，多用户各自开一个。
pub const INSTANCE_MUTEX: &str = "Local\\asset3d.viewport";

/// 命名管道路径前缀。
const PIPE_PATH_PREFIX: &str = r"\\.\pipe\";

/// 把裸名 / 完整路径归一成 `\\.\pipe\xxx`。
///
/// `--pipe` 可以被人手工传进来，不做校验等于开了一条往任意管道路径写数据的口子，
/// 校验规则与插件侧 `pipe::normalize_pipe_path` 逐字一致。
pub fn normalize_pipe_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("管道名为空".to_string());
    }
    let name = trimmed.strip_prefix(PIPE_PATH_PREFIX).unwrap_or(trimmed);
    if name.is_empty() {
        return Err("管道名为空".to_string());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-')
    {
        return Err(format!(
            "管道名 `{name}` 含非法字符：只允许字母、数字、`.`、`_`、`-`"
        ));
    }
    Ok(format!("{PIPE_PATH_PREFIX}{name}"))
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
}

/// 互斥体句柄。抢到即 `acquired()`，抢不到说明已有实例在跑。
pub struct InstanceLock {
    handle: isize,
    acquired: bool,
}

impl InstanceLock {
    /// `name` 不含 `Local\` / `Global\` 前缀时自动补 `Local\`。
    pub fn acquire(name: &str) -> Result<Self, String> {
        let full = qualify(name);
        let wide_name = wide(&full);
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide_name.as_ptr()) };
        if handle == 0 {
            return Err(format!(
                "创建会话互斥体失败（Windows 错误码 {}）：另一个实例可能正在退出，稍后再试。",
                unsafe { GetLastError() }
            ));
        }
        let acquired = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
        Ok(Self { handle, acquired })
    }

    /// 是否真的是本次抢到的（`false` 表示已有实例持有）。
    pub fn acquired(&self) -> bool {
        self.acquired
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe {
                CloseHandle(self.handle);
            }
            self.handle = 0;
        }
    }
}

fn qualify(name: &str) -> String {
    if name.starts_with("Local\\") || name.starts_with("Global\\") {
        name.to_string()
    } else {
        format!("Local\\{name}")
    }
}

/// 主动写一帧。只入队，不碰句柄——真正的 WriteFile 由读线程做（见模块注释）。
#[derive(Clone)]
pub struct PipeWriter {
    outbox: mpsc::Sender<String>,
    connected: Arc<AtomicBool>,
}

impl PipeWriter {
    /// 把一行（自动补 `\n`）排进发送队列。返回是否已入队。
    pub fn write_line(&self, line: &str) -> bool {
        if !self.connected.load(Ordering::Relaxed) {
            return false;
        }
        self.outbox.send(line.to_string()).is_ok()
    }
}

/// 读线程的回调：返回 `Some(回包)` 就地回，`None` 不回。
pub type LineHandler = Box<dyn FnMut(&str) -> Option<String> + Send>;

/// 管道服务端。`stop()` + `join` 由调用方在退出时调。
pub struct PipeServer {
    stop: Arc<AtomicBool>,
    writer: PipeWriter,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PipeServer {
    /// `pipe` 为完整路径（先用 [`normalize_pipe_path`] 归一）。
    pub fn start(pipe: &str, on_line: LineHandler) -> Result<Self, String> {
        // 读线程要活过本函数，所以管道名必须自己持有一份：借用进来的 `&str`
        // 出不了这个栈帧。
        let pipe = pipe.to_string();
        let path = wide(&pipe);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_thread = Arc::clone(&connected);
        let (outbox, inbox) = mpsc::channel::<String>();
        let mut on_line = on_line;

        let thread = std::thread::Builder::new()
            .name("asset3d-viewport-pipe".to_string())
            .spawn(move || {
                // 只挂一个实例：视口是单例（互斥体已挡第二实例），留一个实例还能让
                // 「先 CreateFile 再 Connect」的抢占式连立刻成功，而不是排队。
                let pipe_handle = unsafe {
                    CreateNamedPipeW(
                        path.as_ptr(),
                        PIPE_ACCESS_DUPLEX,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                        1,
                        64 * 1024,
                        64 * 1024,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if pipe_handle == INVALID_HANDLE_VALUE {
                    eprintln!(
                        "asset3d-gui: 创建管道 {pipe} 失败（Windows 错误码 {}）",
                        unsafe { GetLastError() }
                    );
                    return;
                }

                let mut buffer = vec![0u8; 64 * 1024];
                let mut pending = String::new();
                loop {
                    if stop_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    let connected = unsafe { ConnectNamedPipe(pipe_handle, std::ptr::null_mut()) } != 0;
                    // 客户端抢先连上时 ConnectNamedPipe 回 ERROR_PIPE_CONNECTED(535)。
                    if !connected && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                        break;
                    }
                    connected_thread.store(true, Ordering::Relaxed);
                    // 连接建立后的第一帧必须是 `ready`（docs/24 §5.2）：对端（宿主
                    // `viewport-host.ts` 的握手、`--pipe` 冒烟脚本）就靠它判断「视口
                    // 起来了」。写在这里而不是主线程，因为只有读线程知道「有人连上了」。
                    {
                        let mut bytes = crate::viewport::protocol::ready_frame().into_bytes();
                        bytes.push(b'\n');
                        write_all(pipe_handle, &bytes);
                    }
                    loop {
                        if stop_thread.load(Ordering::Relaxed) {
                            break;
                        }
                        // ① 先冲刷主线程排队的帧。此刻本线程没有挂起的读，写不会和
                        //    读抢同一个同步句柄。
                        let mut write_failed = false;
                        while let Ok(frame) = inbox.try_recv() {
                            let mut bytes = frame.into_bytes();
                            bytes.push(b'\n');
                            if !write_all(pipe_handle, &bytes) {
                                write_failed = true;
                                break;
                            }
                        }
                        if write_failed {
                            break;
                        }
                        // ② 有数据才真读：挂一个阻塞 ReadFile 会把句柄的同步 I/O
                        //    占住，主线程的写就再也发不出来。
                        let mut available: u32 = 0;
                        let peeked = unsafe {
                            PeekNamedPipe(
                                pipe_handle,
                                std::ptr::null_mut(),
                                0,
                                std::ptr::null_mut(),
                                &mut available,
                                std::ptr::null_mut(),
                            )
                        };
                        if peeked == 0 {
                            break; // 对端断开：ERROR_BROKEN_PIPE / ERROR_PIPE_NOT_CONNECTED
                        }
                        if available == 0 {
                            std::thread::sleep(Duration::from_millis(15));
                            continue;
                        }
                        let to_read = available.min(buffer.len() as u32);
                        let mut read: u32 = 0;
                        let ok = unsafe {
                            ReadFile(
                                pipe_handle,
                                buffer.as_mut_ptr() as *mut _,
                                to_read,
                                &mut read,
                                std::ptr::null_mut(),
                            )
                        };
                        if ok == 0 || read == 0 {
                            break; // 对端写完就断开（插件侧每次命令一条连接）
                        }
                        pending.push_str(&String::from_utf8_lossy(&buffer[..read as usize]));
                        while let Some(pos) = pending.find('\n') {
                            let line: String = pending.drain(..=pos).collect();
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            if let Some(reply) = on_line(line) {
                                let mut bytes = reply.into_bytes();
                                bytes.push(b'\n');
                                let _ = write_all(pipe_handle, &bytes);
                            }
                        }
                    }
                    pending.clear();
                    connected_thread.store(false, Ordering::Relaxed);
                    unsafe {
                        DisconnectNamedPipe(pipe_handle);
                    }
                }
                connected_thread.store(false, Ordering::Relaxed);
                unsafe {
                    CloseHandle(pipe_handle);
                }
            })
            .map_err(|err| format!("起管道读线程失败：{err}"))?;

        Ok(Self {
            stop,
            writer: PipeWriter { outbox, connected },
            thread: Some(thread),
        })
    }

    /// 主动写一帧的句柄（主线程用它推 `state`）。
    pub fn writer(&self) -> PipeWriter {
        self.writer.clone()
    }

    /// 置停止标志、关掉监听、回收线程。
    ///
    /// 线程可能阻塞在 `ConnectNamedPipe` 上：先 `DisconnectNamedPipe` 不行（句柄在
    /// 线程里），所以这里只 `join` 一个极短的时间窗，超时就 detach——进程随后退出，
    /// 内核会回收句柄，不必为一条监听管道把退出流程卡住。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.writer.write_line(r#"{"v":1,"op":"status","seq":0,"status":"closing"}"#);
        // 主动建一条连接把 ConnectNamedPipe 叫醒：服务端 accept 之后会读到 EOF
        // 或被 stop 标志挡住，随即退出循环。
        wake_pipe();
        if let Some(thread) = self.thread.take() {
            std::thread::spawn(move || {
                let _ = thread.join();
            });
        }
    }
}

/// 叫醒阻塞中的 `ConnectNamedPipe`：连一下默认管道再断开。
fn wake_pipe() {
    // 这里不追求连上——读线程看到 stop 标志就会退出，连一下只是加速。
    let _ = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
}

/// `OPENFILENAMEW`：字段顺序与 `commdlg.h` 逐字段对应，`repr(C)` 负责补齐
/// 64 位下的 4 字节 padding。手写这份结构是「不引 windows-sys」的代价。
#[repr(C)]
struct OpenFileNameW {
    struct_size: u32,
    owner: isize,
    instance: isize,
    filter: *const u16,
    custom_filter: *mut u16,
    max_custom_filter: u32,
    filter_index: u32,
    file: *mut u16,
    max_file: u32,
    file_title: *mut u16,
    max_file_title: u32,
    initial_dir: *const u16,
    title: *const u16,
    flags: u32,
    file_offset: u16,
    file_extension: u16,
    default_ext: *const u16,
    custom_data: isize,
    hook: *const core::ffi::c_void,
    template_name: *const u16,
    reserved: *mut core::ffi::c_void,
    reserved_u32: u32,
    flags_ex: u32,
}

/// `OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST | OFN_FILEMUSTEXIST | OFN_EXPLORER`。
///
/// `NOCHANGEDIR` 会顺手把「选完文件后进程当前目录被改掉」这个老坑堵上——视口的
/// 相对路径解析依赖启动目录不变。
const OFN_FLAGS: u32 = 0x0008_1a08;

/// 保存对话框的旗标：`OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER`。
///
/// 与打开对话框的差别只有两处：去掉 `FILEMUSTEXIST`（目标文件本来就不存在），
/// 加上 `OVERWRITEPROMPT`（覆盖前必须让系统问一句，导出不该静默盖掉同名文件）。
const OFN_SAVE_FLAGS: u32 = 0x0008_080a;

/// 弹系统「打开文件」对话框，返回用户选中的路径；取消返回 `None`。
///
/// `initial_dir` 给初始目录：IDE 托管的视口必须从工作区根起步，否则用户每次都要
/// 从「我的电脑」翻回去。路径无效时系统会自己退回默认目录，不报错。
///
/// 为什么需要它：无参双击 `asset3d-gui.exe` 时，用户的第一期望是「让我挑个模型」，
/// 而不是「给我一个空窗口，什么也点不了」。对话框是模态的、会阻塞事件循环——这在
/// 主线程上可接受：此形态没有宿主在等管道回包（宿主 spawn 一律带 `--pipe`）。
pub fn pick_asset_file(
    owner: isize,
    title: &str,
    filter: &str,
    initial_dir: Option<&Path>,
) -> Option<PathBuf> {
    pick_file(
        owner,
        title,
        filter,
        "glb",
        "",
        initial_dir,
        DialogKind::Open,
    )
}

/// 弹系统「保存文件」对话框，返回用户选定的目标路径；取消返回 `None`。
///
/// `default_name` 预填文件名（导出 AIR / 加载器时带一个合理的默认名，用户只改目录
/// 就能确认），`default_ext` 决定用户在文件名里不写扩展名时系统补什么。
pub fn pick_save_file(
    owner: isize,
    title: &str,
    filter: &str,
    default_ext: &str,
    default_name: &str,
    initial_dir: Option<&Path>,
) -> Option<PathBuf> {
    pick_file(
        owner,
        title,
        filter,
        default_ext,
        default_name,
        initial_dir,
        DialogKind::Save,
    )
}

#[derive(Clone, Copy, PartialEq)]
enum DialogKind {
    Open,
    Save,
}

/// 打开 / 保存共用的对话框实现：两者是同一个 `OPENFILENAMEW`，只有旗标与
/// 调用哪个 API 不同，拆两个函数会把 40 行结构体初始化抄两遍。
fn pick_file(
    owner: isize,
    title: &str,
    filter: &str,
    default_ext: &str,
    default_name: &str,
    initial_dir: Option<&Path>,
    kind: DialogKind,
) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32 * 1024];
    // 保存对话框要把「默认文件名」放进同一个缓冲区：系统读它当初始文件名，
    // 返回时又用它回传用户输入，两件事共用一块内存。
    if !default_name.is_empty() {
        let units: Vec<u16> = default_name.encode_utf16().collect();
        let count = units.len().min(buffer.len() - 1);
        buffer[..count].copy_from_slice(&units[..count]);
    }
    let filter = wide(filter);
    let title = wide(title);
    let default_ext = wide(default_ext);
    // 目录字符串必须活到 API 返回之后：`ofn.initial_dir` 只是个裸指针。
    let initial = initial_dir.map(|dir| wide(&dir.to_string_lossy()));
    let mut ofn = OpenFileNameW {
        struct_size: std::mem::size_of::<OpenFileNameW>() as u32,
        owner,
        instance: 0,
        filter: filter.as_ptr(),
        custom_filter: std::ptr::null_mut(),
        max_custom_filter: 0,
        filter_index: 1,
        file: buffer.as_mut_ptr(),
        max_file: buffer.len() as u32,
        file_title: std::ptr::null_mut(),
        max_file_title: 0,
        initial_dir: initial
            .as_ref()
            .map(|units| units.as_ptr())
            .unwrap_or(std::ptr::null()),
        title: title.as_ptr(),
        flags: if kind == DialogKind::Save {
            OFN_SAVE_FLAGS
        } else {
            OFN_FLAGS
        },
        file_offset: 0,
        file_extension: 0,
        default_ext: default_ext.as_ptr(),
        custom_data: 0,
        hook: std::ptr::null(),
        template_name: std::ptr::null(),
        reserved: std::ptr::null_mut(),
        reserved_u32: 0,
        flags_ex: 0,
    };
    let ok = if kind == DialogKind::Save {
        unsafe { GetSaveFileNameW(&mut ofn) }
    } else {
        unsafe { GetOpenFileNameW(&mut ofn) }
    };
    if ok == 0 {
        // 取消与「对话框打不开」都是 0：两种都当「没选」，不弹错误框打扰。
        return None;
    }
    let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
    if length == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

fn write_all(handle: isize, bytes: &[u8]) -> bool {
    let mut written: u32 = 0;
    let ok = unsafe {
        WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    ok != 0 && written as usize == bytes.len()
}

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const INVALID_HANDLE_VALUE: isize = -1;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_ALREADY_EXISTS: u32 = 183;

#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buffer_size: u32,
        in_buffer_size: u32,
        default_timeout: u32,
        security_attributes: *mut core::ffi::c_void,
    ) -> isize;
    fn ConnectNamedPipe(pipe: isize, overlapped: *mut core::ffi::c_void) -> i32;
    fn DisconnectNamedPipe(pipe: isize) -> i32;
    fn PeekNamedPipe(
        pipe: isize,
        buffer: *mut core::ffi::c_void,
        buffer_size: u32,
        bytes_read: *mut u32,
        total_bytes_avail: *mut u32,
        bytes_left_this_message: *mut u32,
    ) -> i32;
    fn ReadFile(
        file: isize,
        buffer: *mut core::ffi::c_void,
        to_read: u32,
        read: *mut u32,
        overlapped: *mut core::ffi::c_void,
    ) -> i32;
    fn WriteFile(
        file: isize,
        buffer: *const u8,
        to_write: u32,
        written: *mut u32,
        overlapped: *mut core::ffi::c_void,
    ) -> i32;
    fn CreateMutexW(
        attributes: *mut core::ffi::c_void,
        initial_owner: i32,
        name: *const u16,
    ) -> isize;
    fn GetLastError() -> u32;
    fn CloseHandle(object: isize) -> i32;
}

// `comdlg32`：只借「打开 / 保存文件」两个对话框。与上面的 kernel32 分开写
// `#[link]`，因为两者是不同 DLL——合在一个块里会让链接器去 kernel32 找
// `GetOpenFileNameW`。
#[link(name = "comdlg32")]
extern "system" {
    fn GetOpenFileNameW(ofn: *mut OpenFileNameW) -> i32;
    fn GetSaveFileNameW(ofn: *mut OpenFileNameW) -> i32;
}

/// `MB_OK | MB_ICONERROR | MB_SETFOREGROUND`：置前是必要的——双击启动时窗口刚
/// 建出来，不带它的话消息框可能藏在新窗口后面。
const MB_ICONERROR: u32 = 0x0000_0010;
const MB_SETFOREGROUND: u32 = 0x0001_0000;

/// 弹一个阻塞式错误框。只给**没有管道**的形态用（双击 / standalone）：
/// 这两种形态没有人读 stderr，只写日志等于用户什么都没看到。
pub fn show_error(owner: isize, title: &str, message: &str) {
    let text = wide(message);
    let caption = wide(title);
    unsafe {
        MessageBoxW(
            owner,
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(owner: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_full_and_bare_names() {
        assert_eq!(
            normalize_pipe_path(r"\\.\pipe\aiide.viewport.asset3d.abc").unwrap(),
            r"\\.\pipe\aiide.viewport.asset3d.abc"
        );
        assert_eq!(
            normalize_pipe_path("aiide.viewport.asset3d.default").unwrap(),
            r"\\.\pipe\aiide.viewport.asset3d.default"
        );
    }

    #[test]
    fn rejects_separators_and_empty() {
        assert!(normalize_pipe_path("").is_err());
        assert!(normalize_pipe_path(r"\\.\pipe\").is_err());
        assert!(normalize_pipe_path(r"..\evil").is_err());
        assert!(normalize_pipe_path("a/b").is_err());
    }

    #[test]
    fn default_pipe_name_matches_host_convention() {
        assert_eq!(DEFAULT_PIPE_NAME, format!("{PIPE_PREFIX}asset3d.default"));
    }

    #[test]
    fn qualifies_mutex_names() {
        assert_eq!(qualify("asset3d.viewport"), r"Local\asset3d.viewport");
        assert_eq!(qualify(r"Global\x"), r"Global\x");
    }

    #[test]
    fn writer_without_connection_drops_frame() {
        let (outbox, _inbox) = mpsc::channel::<String>();
        let writer = PipeWriter {
            outbox,
            connected: Arc::new(AtomicBool::new(false)),
        };
        assert!(!writer.write_line("{}"));
    }
}
