//! 命名管道客户端：把一帧会话指令交给 `asset3d-gui.exe`（docs/24 §5.2）。
//!
//! 插件宿主是个短生命周期的 RPC 进程：一次 `plugin/command-invoke` 跑完就返回，
//! 命令线程随即结束。所以这里**不做长连接**，每次命令开一个短连接、写一帧、
//! 立刻断开。视口侧的读循环按「连接里读到几帧就处理几帧，读到 EOF 收工」实现，
//! 两边才对得上。
//!
//! 为什么不用 `std::fs::File` 打开 `\\.\pipe\...`：Windows 上 `CreateFile` 的
//! 打开模式与 `FILE_FLAG_OVERLAPPED` 都不是 Rust 标准库能表达的，`OpenOptions`
//! 打开管道会按普通文件语义走，遇到 `ERROR_PIPE_BUSY` 也没有可用的退避接口。
//! 这里只用到三个 API，直接 FFI 比拖一个 `windows-sys` 进来干净——本插件的
//! 依赖树里已经有一个靠 cc 构建的 ufbx，能少一层是一层。
//!
//! 非 Windows 平台一律回 `Unsupported`：视口本身只有 Windows 构建（清单里的
//! `external.program` 也必须以 `.exe` 结尾），其他平台没有可连的管道。

/// 管道名前缀。宿主侧 `viewport-host.ts: PIPE_PREFIX` 是 `aiide.viewport.`，
/// 完整路径是 `\\.\pipe\` + 该前缀 + 插件 id + `.` + 令牌。
pub const PIPE_PREFIX: &str = "aiide.viewport.";
/// Windows 命名管道路径前缀。
pub const PIPE_PATH_PREFIX: &str = r"\\.\pipe\";

/// 等管道空位的最长时间：视口进程刚被宿主拉起时，管道可能还没创建好。
/// 1.5 s 足够覆盖进程启动的那一下，又不至于让一次命令卡成超时。
const PIPE_WAIT_MS: u32 = 1_500;

/// 把清单 / 外壳给的管道名归一成完整路径。
///
/// 接受三种写法：完整路径 `\\.\pipe\xxx`、裸名 `xxx`、带前缀的裸名。
/// 名字里出现路径分隔符或 `..` 一律拒绝——管道名是**用户可见入口**的一部分
/// （`--pipe` 可被手工传入），不做校验等于给了一条向任意管道路径写数据的通道。
pub fn normalize_pipe_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("管道名为空".to_string());
    }
    let name = match trimmed.strip_prefix(PIPE_PATH_PREFIX) {
        Some(rest) => rest,
        None => trimmed,
    };
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

/// 打开管道、写一行、关掉。返回是否真的写出去了。
///
/// 调用方拿到的 `Err` 已经是**给人看的话**：这类失败用户没法自己修，只能照提示
/// 选下一步（重开视口 / 升级插件 / 看日志），所以文案里必须带「接下来做什么」。
pub fn send_line(pipe: &str, line: &str) -> Result<(), String> {
    let path = normalize_pipe_path(pipe)?;
    send_line_windows(&path, line)
}

#[cfg(windows)]
fn send_line_windows(path: &str, line: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut core::ffi::c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: isize,
        ) -> isize;
        fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
        fn WriteFile(
            file: isize,
            buffer: *const u8,
            bytes_to_write: u32,
            bytes_written: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
        fn CloseHandle(object: isize) -> i32;
    }

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;

    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // `ERROR_PIPE_BUSY`(231) 时 CreateFile 直接失败，得先 WaitNamedPipe。
    // 反之（管道不存在，2/3）等待没有意义，白等 1.5 秒。
    let mut handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if code == 231 {
            unsafe {
                WaitNamedPipeW(wide.as_ptr(), PIPE_WAIT_MS);
            }
            handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    0,
                )
            };
        }
    }
    if handle == INVALID_HANDLE_VALUE {
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(match code {
            2 | 3 => format!(
                "视口没有在监听管道 `{path}`（视口进程可能没起来或已经退出）：再点一次「在 3D 视口中打开」，\
                 宿主会重新拉起视口。"
            ),
            5 => format!("无权访问管道 `{path}`：管道属于另一个用户会话，换个会话里的 IDE 打不开它。"),
            _ => format!(
                "连接视口管道 `{path}` 失败（Windows 错误码 {code}）：看插件日志里的 stderr 尾巴。"
            ),
        });
    }

    let bytes = line.as_bytes();
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
    let write_error = if ok == 0 {
        Some(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        None
    };
    unsafe {
        CloseHandle(handle);
    }
    match write_error {
        None if written as usize == bytes.len() => Ok(()),
        None => Err(format!(
            "指令只写进去 {written}/{} 字节：视口可能刚退出。",
            bytes.len()
        )),
        Some(code) => Err(format!(
            "向视口管道 `{path}` 写指令失败（Windows 错误码 {code}）：视口进程可能已经退出。"
        )),
    }
}

#[cfg(not(windows))]
fn send_line_windows(_path: &str, _line: &str) -> Result<(), String> {
    Err("外部视口只支持 Windows：当前平台没有可连接的命名管道。".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_full_and_bare_names() {
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
}
