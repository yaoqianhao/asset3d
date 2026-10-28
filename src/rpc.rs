//! NDJSON over stdio：帧收发 + `host/*` 调用客户端。
//!
//! 为什么不用现成的 JSON-RPC 库：协议只有三种帧（握手、工具调用、宿主回包），
//! 而这里最关键的两条约束——「回包必须单行、必须立刻 flush」与「工具调用期间
//! 主线程仍要能收宿主回包」——都属于调度问题而不是协议问题，手写更短也更可控。
//!
//! 线程模型（这是最容易写错的地方）：
//! - **主线程**只做一件事：`read_line` → 分派。宿主发来的 `host/*` 回包也由它
//!   交给 [`Host::deliver`]，所以它绝不能阻塞在工具执行上；
//! - **每个工具调用一个 worker 线程**：解析 FBX/GLB 是 CPU 密集且可能几百毫秒，
//!   放在主线程就等于把宿主的其它请求（含它自己的回包）一起堵死；
//! - 产物回包从 worker 线程直接写 stdout，由 [`Wire`] 的锁保证帧不交错。

use std::collections::HashMap;
use std::io::{BufWriter, Stdout, Write};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};

use crate::util;

/// 单次 `host/*` 调用的超时。
///
/// 宿主侧一次工具调用的上限是 30s（清单可拉到 120s），单个宿主调用再长就没有
/// 意义了：超时后放弃等待并回错，总比整个进程被宿主判超时更好诊断。
const HOST_CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// 握手期「补问工作区」专用短超时。
///
/// 这一步不在任何工具调用里，宿主等不到回包时不会有人来救场，只会在启动阶段
/// 白等 20 秒——而它的结果只影响「绝对路径能否归一化成工作区相对路径」，
/// 拿不到照样能干活（宿主闸门会拒绝对外路径，报错里带原因）。
/// 所以这里按「2 秒拿不到就算没有」处理，宁可直接降级也不拖住握手。
pub const HOST_CALL_TIMEOUT_SHORT: Duration = Duration::from_secs(2);

/// 宿主单帧上限（`crates/plugin/src/loader.rs` 的 `FRAME_LIMIT_BYTES`）。
///
/// 每一帧（`host/fs.read` 回包、`host/fs.write` 请求）都必须装进 1 MiB，超了
/// 宿主直接判协议违规并终止进程（6106）。所以大文件不能靠放大帧，只能靠分片：
/// 读按 `offset` 循环，写按 `append` 续写。**单帧**仍是硬顶，这里是它的数字来源。
pub const FRAME_LIMIT_BYTES: usize = 1024 * 1024;

/// 给帧头、路径、转义与字段名留的余量。
const FRAME_RESERVE_BYTES: usize = 64 * 1024;

/// 宿主 `host/fs.read` 的单次窗口硬顶（`plugin_host.rs` 的 `MAX_READ_WINDOW_BYTES`）。
///
/// 传更大的 `maxBytes` 不会报错，只会被**夹到**这个值——所以插件必须按回包里的
/// `bytes` 推进 `offset`，而不是假设「我要多少就给多少」。
pub const HOST_READ_WINDOW_BYTES: u64 = 512 * 1024;

/// 宿主单次工具调用的能力调用配额（`plugin_host.rs` 的 `QUOTA_PER_CALL`）。
///
/// 读写分片每片一次调用，所以「单次工具调用能处理多大的资产」本质上被这个数字
/// 框住：96 MiB / 512 KiB = 192 次，加 stat 与 progress 刚好不超过 200。
pub const HOST_CALL_QUOTA: usize = 200;

/// 单次导入的资产上限：96 MiB。
///
/// 按**真实能力**推出来的，不是拍脑袋：读侧一次 512 KiB 窗口，96 MiB 要 192 次
/// `host/fs.read`，加上 `fs.stat` / `fs.progress` 正好落在 200 次配额内。
/// 超了就由插件自己报一条能操作的诊断，而不是让宿主回一条看不懂的配额错误。
pub const MAX_ASSET_BYTES: u64 = 96 * 1024 * 1024;

/// 旧宿主（`host/fs.read` 没有 `offset`）单帧能带回的原始字节：约 720 KiB。
///
/// 只用来判断「降级后还读不读得动」和写升级提示，不参与新路径的任何计算。
pub const LEGACY_READ_BYTES: u64 = ((FRAME_LIMIT_BYTES - FRAME_RESERVE_BYTES) as u64) / 4 * 3;

/// 一次读分片的原始字节数：取宿主窗口硬顶。
///
/// base64 后约 683 KB，加路径与字段名仍远低于「1 MiB 减 64 KiB 预留」，
/// 所以读分片的上限是宿主那 512 KiB，而不是帧预算。
pub const READ_WINDOW_BYTES: usize = HOST_READ_WINDOW_BYTES as usize;

/// 读/写 `bytes` 字节需要多少次宿主调用（按分片长度向上取整）。
pub fn calls_for(bytes: u64) -> usize {
    let window = READ_WINDOW_BYTES as u64;
    ((bytes + window - 1) / window) as usize
}

/// 一次写分片的原始字节数：按路径算，保证 base64 之后仍在一帧内。
///
/// 与读分片取小：两边用同一个片长，排障时「第几片」在读写两侧能直接对上。
pub fn write_chunk_bytes(path: &str) -> usize {
    let by_frame = payload_budget(path, "base64") / 4 * 3;
    by_frame.min(READ_WINDOW_BYTES).max(3)
}

/// 某条路径 + 编码下，单次写盘**载荷**能有多少字节。
///
/// 与 `ensure_frame_budget` 用同一套算式：调用方（预览分片）必须在动笔前就知道
/// 闸门在哪，才能保证每一片都过得去，而不是写完最后一片才被宿主杀掉进程。
pub fn payload_budget(path: &str, encoding: &str) -> usize {
    (FRAME_LIMIT_BYTES - FRAME_RESERVE_BYTES).saturating_sub(path.len() + encoding.len() + 256)
}

/// 写盘前先算帧大小：`host/fs.write` 一次请求就是一帧，超了宿主判协议违规。
///
/// 0.3.0 起它只是「能不能一条写完」的判定器：能就照旧整文件写（老宿主照样认），
/// 不能就转 [`Host::write_sharded`] 分片。真正的兜底在分片写那边，所以这里的
/// 错误文案只在「单片也装不下、分片也帮不上」时才会被人看到。
fn ensure_frame_budget(path: &str, payload: usize, encoding: &str) -> Result<(), HostError> {
    let budget = FRAME_LIMIT_BYTES - FRAME_RESERVE_BYTES;
    let frame = payload + path.len() + encoding.len() + 256;
    if frame <= budget {
        return Ok(());
    }
    let raw_hint = if encoding == "base64" {
        format!("（原始数据约 {}）", util::format_bytes((payload as u64) / 4 * 3))
    } else {
        String::new()
    };
    Err(HostError::transport(format!(
        "写 `{path}` 需要约 {}{raw_hint} 的单帧，超过宿主 1 MiB 帧上限（单帧载荷上限 {}）",
        util::format_bytes(frame as u64),
        util::format_bytes(budget as u64)
    )))
}

/// 一片 `host/fs.read` 的回包：内容 + 定位信息。
struct ReadChunk {
    bytes: Vec<u8>,
    /// 老宿主没有这个字段——它同时是「宿主认不认 offset」的探针。
    eof: Option<bool>,
    size: Option<u64>,
}

/// 分片读失败：要不要退回旧宿主的整文件路径。
enum PagedError {
    /// 宿主不认识 `offset`（或回包缺新字段）：值得退回整文件读。
    Legacy(HostError),
    /// 真错误（越界、解不出来、配额不够）：直接报给用户。
    Fatal(HostError),
}

fn classify_read_error(err: &HostError, key: &str) -> PagedError {
    if is_unsupported(err, key) {
        PagedError::Legacy(err.clone())
    } else {
        PagedError::Fatal(err.clone())
    }
}

/// 宿主是不是在说「不认识这个参数」。
///
/// 老宿主对未知参数有两种脾气：回 `-32602` 并点名参数，或者静默忽略（那种情况
/// 靠回包里缺 `eof` / `size` 认出来，见 [`Host::read_paged`]）。这里只覆盖前一种。
fn is_unsupported(err: &HostError, key: &str) -> bool {
    if err.code != -32602 {
        return false;
    }
    let message = err.message.to_lowercase();
    message.contains(key)
        || message.contains("unknown")
        || message.contains("unexpected")
        || message.contains("未知")
        || message.contains("不支持")
}

/// 宿主调用失败。
#[derive(Debug, Clone)]
pub struct HostError {
    pub code: i64,
    pub message: String,
}

impl HostError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// stdout 写入端：所有帧（回包与请求）共用，锁保证一帧不会与另一帧交错。
pub struct Wire {
    out: Mutex<BufWriter<Stdout>>,
}

impl Wire {
    fn new() -> Self {
        Self {
            out: Mutex::new(BufWriter::new(std::io::stdout())),
        }
    }

    pub fn send(&self, frame: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(frame)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        line.push(b'\n');
        let mut guard = self.out.lock().unwrap_or_else(|poison| poison.into_inner());
        // 必须立刻 flush：对端是逐行读的，缓冲住就等于「不回包」。
        guard.write_all(&line)?;
        guard.flush()
    }
}

/// 宿主客户端：发 `host/*` 请求并同步等回包。
pub struct Host {
    wire: Arc<Wire>,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    next_id: AtomicU64,
    workspace_root: Mutex<String>,
    workspace_name: Mutex<String>,
    plugin_id: Mutex<String>,
    /// 宿主分片能力探测结果，见 [`IoMode`]。
    io_mode: AtomicU8,
    /// 本次工具调用已经用掉的宿主调用次数（宿主按工具调用重置配额）。
    calls_used: AtomicU64,
}

/// 宿主分片读写能力：探一次就缓存，避免每个大文件都先撞一遍错误。
///
/// 判定方式有两条：`host/fs.read` 回包里有没有 `eof` / `size`（老宿主没有），
/// 以及调用是否被回 `-32602` 且消息里点名 `offset` / `append`。
#[derive(Clone, Copy, PartialEq, Eq)]
enum IoMode {
    Unknown,
    Paged,
    Legacy,
}

impl Host {
    pub fn new() -> Self {
        Self {
            wire: Arc::new(Wire::new()),
            pending: Mutex::new(HashMap::new()),
            // 与握手请求的 id 空间刻意错开：宿主只按方向区分请求/回包，
            // 但错开后抓包时一眼能分清「谁问谁」。
            next_id: AtomicU64::new(1000),
            workspace_root: Mutex::new(String::new()),
            workspace_name: Mutex::new(String::new()),
            plugin_id: Mutex::new(String::new()),
            io_mode: AtomicU8::new(0),
            calls_used: AtomicU64::new(0),
        }
    }

    /// 每次工具调用开始时归零，与宿主 `begin_call` 对齐。
    pub fn reset_call_quota(&self) {
        self.calls_used.store(0, Ordering::Relaxed);
    }

    /// 本次工具调用还剩多少次宿主调用可用。
    fn calls_left(&self) -> usize {
        HOST_CALL_QUOTA.saturating_sub(self.calls_used.load(Ordering::Relaxed) as usize)
    }

    fn io_mode(&self) -> IoMode {
        match self.io_mode.load(Ordering::Relaxed) {
            1 => IoMode::Paged,
            2 => IoMode::Legacy,
            _ => IoMode::Unknown,
        }
    }

    fn mark_io_mode(&self, mode: IoMode) {
        let raw = match mode {
            IoMode::Unknown => 0,
            IoMode::Paged => 1,
            IoMode::Legacy => 2,
        };
        self.io_mode.store(raw, Ordering::Relaxed);
    }

    pub fn wire(&self) -> Arc<Wire> {
        Arc::clone(&self.wire)
    }

    pub fn set_workspace(&self, root: &str, name: &str) {
        *self.workspace_root.lock().unwrap_or_else(|p| p.into_inner()) = root.to_string();
        *self.workspace_name.lock().unwrap_or_else(|p| p.into_inner()) = name.to_string();
    }

    pub fn workspace_root(&self) -> String {
        self.workspace_root
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn workspace_name(&self) -> String {
        self.workspace_name
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn set_plugin_id(&self, id: &str) {
        *self.plugin_id.lock().unwrap_or_else(|p| p.into_inner()) = id.to_string();
    }

    pub fn plugin_id(&self) -> String {
        self.plugin_id.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// 把宿主回包交给等待中的调用方；不是回包（或没人等）就丢掉。
    pub fn deliver(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            return;
        };
        let waiter = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        if let Some(tx) = waiter {
            // 接收端可能已超时离开，发送失败不是错误。
            let _ = tx.send(frame.clone());
        }
    }

    /// 调一次宿主能力，返回 `result`（使用默认 20s 超时）。
    pub fn call(&self, method: &str, params: Value) -> Result<Value, HostError> {
        self.call_with_timeout(method, params, HOST_CALL_TIMEOUT)
    }

    /// 同上，但可指定超时。握手期补问工作区走短超时，避免启动阶段被拖住。
    pub fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // 每一次 `host/*` 都吃宿主配额（含 stat / progress），记账要跟宿主同口径。
        self.calls_used.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(err) = self.wire.send(&frame) {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(HostError::transport(format!("写 stdout 失败：{err}")));
        }
        match rx.recv_timeout(timeout) {
            Ok(reply) => {
                if let Some(error) = reply.get("error") {
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("宿主返回错误")
                        .to_string();
                    Err(HostError { code, message })
                } else {
                    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(HostError {
                    code: -32001,
                    message: format!("宿主调用 {method} 超时（{}s）", timeout.as_secs()),
                })
            }
        }
    }

    /// 读一个文件的原始字节。
    ///
    /// 小文件仍是一条 `host/fs.read`（不带 `offset`，回包与 0.2.0 逐字一致，老宿主
    /// 照样能跑）；大文件按 [`READ_WINDOW_BYTES`] 循环带 `offset` 读，严格按回包里的
    /// `bytes` / `eof` / `size` 推进——坑就在这儿：宿主会把 `maxBytes` 夹到 512 KiB，
    /// 凭「我要了多大」推偏移会漏字节。
    pub fn read_bytes(&self, path: &str) -> Result<Vec<u8>, HostError> {
        let size = self.file_size(path)?;
        if size == 0 {
            return Ok(Vec::new());
        }
        if size > MAX_ASSET_BYTES {
            return Err(Self::too_large(path, size));
        }
        if size <= READ_WINDOW_BYTES as u64 {
            // 一个窗口就装得下：不传 offset，走最短路径。
            return self.read_chunk(path, 0, size).map(|chunk| chunk.bytes);
        }
        if self.io_mode() != IoMode::Legacy {
            match self.read_paged(path, size) {
                Ok(bytes) => {
                    self.mark_io_mode(IoMode::Paged);
                    return Ok(bytes);
                }
                Err(PagedError::Legacy(err)) => {
                    self.mark_io_mode(IoMode::Legacy);
                    return self.read_legacy(path, size, Some(err));
                }
                Err(PagedError::Fatal(err)) => return Err(err),
            }
        }
        self.read_legacy(path, size, None)
    }

    fn too_large(path: &str, size: u64) -> HostError {
        HostError::transport(format!(
            "文件 {path} 有 {}，超过单次导入上限 {}。\
             这个上限按真实能力算：宿主单次读窗口 {}，读满 {} 需要 {} 次 host/fs.read，\
             再加 fs.stat / progress 的调用正好用尽单次工具调用 {HOST_CALL_QUOTA} 次配额。\
             请拆分资产（如 .gltf + 外部 .bin 分包）、先减面或去贴图后重试。",
            util::format_bytes(size),
            util::format_bytes(MAX_ASSET_BYTES),
            util::format_bytes(HOST_READ_WINDOW_BYTES),
            util::format_bytes(MAX_ASSET_BYTES),
            calls_for(MAX_ASSET_BYTES),
        ))
    }

    /// 一次 `host/fs.read`：读回一片，连带回包里的定位信息。
    fn read_chunk(&self, path: &str, offset: u64, want: u64) -> Result<ReadChunk, HostError> {
        let mut params = json!({
            "path": path,
            "maxBytes": want.max(1),
            "encoding": "base64",
        });
        if offset > 0 {
            params["offset"] = json!(offset);
        }
        let reply = self.call("host/fs.read", params)?;
        let content = reply
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| HostError::transport("宿主 fs.read 回包缺少 content"))?;
        let bytes = BASE64
            .decode(content)
            .map_err(|err| HostError::transport(format!("回包不是合法 base64：{err}")))?;
        let reported = reply
            .get("bytes")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(bytes.len());
        if reported != bytes.len() {
            return Err(HostError::transport(format!(
                "宿主 fs.read 回包自相矛盾：bytes={reported}，但 base64 解出 {} 字节",
                bytes.len()
            )));
        }
        Ok(ReadChunk {
            bytes,
            eof: reply.get("eof").and_then(Value::as_bool),
            size: reply.get("size").and_then(Value::as_u64),
        })
    }

    /// 循环带 offset 读完整文件。
    fn read_paged(&self, path: &str, size: u64) -> Result<Vec<u8>, PagedError> {
        let mut out: Vec<u8> = Vec::with_capacity(size as usize);
        let mut calls = 0usize;
        while (out.len() as u64) < size {
            // 读之前先看真实剩余配额：撞上去再报错，用户只看到宿主的 6109。
            if self.calls_left() < 2 {
                return Err(PagedError::Fatal(HostError::transport(format!(
                    "本次工具调用的宿主调用配额不足：读 {path} 已用 {calls} 次、已读 {} / {}，\
                     剩余额度不够再读一片。请把小资产、可拆分的资产分开处理，或减小资产规模后重试",
                    util::format_bytes(out.len() as u64),
                    util::format_bytes(size)
                ))));
            }
            calls += 1;
            let offset = out.len() as u64;
            let want = (size - offset).min(READ_WINDOW_BYTES as u64);
            let chunk = self
                .read_chunk(path, offset, want)
                .map_err(|err| classify_read_error(&err, "offset"))?;
            // 老宿主会静默忽略 offset：回包里根本没有 eof/size 这两个新字段。
            if chunk.eof.is_none() || chunk.size.is_none() {
                return Err(PagedError::Legacy(HostError::transport(
                    "宿主 fs.read 回包没有 offset/size/eof 字段",
                )));
            }
            if chunk.bytes.is_empty() {
                return Err(PagedError::Fatal(HostError::transport(format!(
                    "宿主在 offset {offset} 处返回 0 字节却声称没到文件末尾（共 {}）",
                    util::format_bytes(size)
                ))));
            }
            let eof = chunk.eof.unwrap_or(false);
            let total = chunk.size.unwrap_or(size);
            out.extend_from_slice(&chunk.bytes);
            if eof || out.len() as u64 >= total {
                break;
            }
        }
        if (out.len() as u64) < size {
            return Err(PagedError::Fatal(HostError::transport(format!(
                "只读到 {} / {}：文件在读取期间变短了，请重试",
                util::format_bytes(out.len() as u64),
                util::format_bytes(size)
            ))));
        }
        Ok(out)
    }

    /// 旧宿主降级路径：一次整文件读，读得动就读，读不动就把升级话讲清楚。
    fn read_legacy(
        &self,
        path: &str,
        size: u64,
        reason: Option<HostError>,
    ) -> Result<Vec<u8>, HostError> {
        let why = reason
            .map(|err| format!("（宿主对 offset 的回应：{}）", err.message))
            .unwrap_or_default();
        if size > LEGACY_READ_BYTES {
            return Err(HostError::transport(format!(
                "宿主版本过旧：host/fs.read 不支持 offset 分片{why}，单帧最多带回 {}（1 MiB 帧 / base64 4:3），\
                 而 {path} 有 {}。asset3d 0.3.0 起依赖宿主分片读写，请升级宿主后再试；\
                 临时绕过可改用 .gltf + 外部 .bin 把资源拆到 {} 以下。",
                util::format_bytes(LEGACY_READ_BYTES),
                util::format_bytes(size),
                util::format_bytes(LEGACY_READ_BYTES)
            )));
        }
        let chunk = self.read_chunk(path, 0, size)?;
        if chunk.bytes.len() as u64 != size {
            return Err(HostError::transport(format!(
                "宿主只返回了 {} / {} 字节（host/fs.read 无 offset 分片，无法接着读），{path} 读不完整",
                util::format_bytes(chunk.bytes.len() as u64),
                util::format_bytes(size)
            )));
        }
        Ok(chunk.bytes)
    }

    pub fn file_size(&self, path: &str) -> Result<u64, HostError> {
        let reply = self.call("host/fs.stat", json!({ "path": path }))?;
        reply
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| HostError::transport(format!("拿不到 {path} 的大小")))
    }

    /// 列一层目录：返回 `(相对路径, 是否目录, 字节数)`。
    pub fn list(&self, path: &str) -> Result<Vec<(String, bool, u64)>, HostError> {
        let reply = self.call("host/fs.list", json!({ "path": path }))?;
        let entries = reply
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            let dir = entry.get("dir").and_then(Value::as_bool).unwrap_or(false);
            let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
            let joined = if path.is_empty() || path == "." {
                name.to_string()
            } else {
                format!("{}/{}", path.trim_end_matches('/'), name)
            };
            out.push((joined, dir, size));
        }
        Ok(out)
    }

    pub fn write_text(&self, path: &str, text: &str) -> Result<Value, HostError> {
        if ensure_frame_budget(path, text.len(), "utf8").is_ok() {
            return self.call(
                "host/fs.write",
                json!({ "path": path, "content": text, "encoding": "utf8" }),
            );
        }
        // 超帧：按原始字节分片、走 base64 —— UTF-8 多字节字符绝不能被切开。
        self.write_sharded(path, text.as_bytes())
    }

    pub fn write_bytes(&self, path: &str, bytes: &[u8]) -> Result<Value, HostError> {
        // base64 后才是真正进帧的长度，按 4 倍向上取整再算。
        let encoded_len = (bytes.len() + 2) / 3 * 4;
        if ensure_frame_budget(path, encoded_len, "base64").is_ok() {
            return self.call(
                "host/fs.write",
                json!({ "path": path, "content": BASE64.encode(bytes), "encoding": "base64" }),
            );
        }
        self.write_sharded(path, bytes)
    }

    /// 分片写：首片整文件替换（顺带建文件），后续片 `append` 续写。
    ///
    /// 三条底线：
    /// 1. 每一片都各自过帧预算（片长由 [`write_chunk_bytes`] 按路径算好）；
    /// 2. 每次回包都用 `size` 核对「累计写入」——老宿主会静默忽略 `append`、
    ///    当成整文件替换，不核对就会留下一个只剩最后一片的坏文件；
    /// 3. 任何一片失败都把半截文件抹成 0 字节再报错，绝不留半个 AIRB。
    fn write_sharded(&self, path: &str, bytes: &[u8]) -> Result<Value, HostError> {
        let chunk = write_chunk_bytes(path);
        let pieces: Vec<&[u8]> = bytes.chunks(chunk).collect();
        let total = bytes.len();
        let legacy_hint = || {
            HostError::transport(format!(
                "载荷 {} 装不进宿主 1 MiB 单帧（单帧载荷上限 {}），必须分片写；\
                 但当前宿主版本过旧，host/fs.write 不支持 append/offset 分片。\
                 asset3d 0.3.0 起依赖宿主分片读写，请升级宿主；\
                 临时绕过可改用 embedTextures=false 或拆分资产。",
                util::format_bytes(total as u64),
                util::format_bytes(payload_budget(path, "base64") as u64)
            ))
        };
        if pieces.len() + 8 > self.calls_left() {
            return Err(HostError::transport(format!(
                "本次工具调用的宿主配额装不下这次写盘：`{path}` 有 {}，按单片 {} 要切 {} 片，\
                 但只剩 {} 次调用（单次工具调用总配额 {HOST_CALL_QUOTA}，读源资产已经用掉一部分）。\
                 请减小输出规模（maxTriangles / embedTextures=false）或分多次导出。",
                util::format_bytes(total as u64),
                util::format_bytes(chunk as u64),
                pieces.len(),
                self.calls_left()
            )));
        }
        if self.io_mode() == IoMode::Legacy {
            return Err(legacy_hint());
        }

        let mut created = Value::Bool(false);
        let mut written = 0usize;
        for (index, piece) in pieces.iter().enumerate() {
            let mut params = json!({
                "path": path,
                "content": BASE64.encode(piece),
                "encoding": "base64",
            });
            if index > 0 {
                params["append"] = json!(true);
            }
            let reply = match self.call("host/fs.write", params) {
                Ok(reply) => reply,
                Err(err) => {
                    let reason = if is_unsupported(&err, "append") || is_unsupported(&err, "offset")
                    {
                        legacy_hint()
                    } else {
                        err
                    };
                    return Err(self.abandon_partial(path, index, pieces.len(), written, total, reason));
                }
            };
            if index == 0 {
                created = reply.get("created").cloned().unwrap_or(Value::Bool(false));
            }
            written += piece.len();
            match reply.get("size").and_then(Value::as_u64) {
                // 老宿主的写回包没有 size 字段：`append` 被静默降级成整文件替换了。
                None => {
                    return Err(self.abandon_partial(
                        path,
                        index,
                        pieces.len(),
                        written,
                        total,
                        legacy_hint(),
                    ))
                }
                Some(size) if size != written as u64 => {
                    return Err(self.abandon_partial(
                        path,
                        index,
                        pieces.len(),
                        written,
                        total,
                        HostError::transport(format!(
                            "宿主回包 size={size} 与累计写入 {written} 对不上：分片没有按 append 语义落盘"
                        )),
                    ))
                }
                Some(_) => {}
            }
        }
        self.mark_io_mode(IoMode::Paged);
        Ok(json!({
            "path": path,
            "created": created,
            "bytes": written,
            "size": written,
            "chunks": pieces.len(),
        }))
    }

    /// 分片写失败时的收尾：抹掉半截文件，并把「发生了什么、留下了什么」讲清楚。
    ///
    /// 宿主没有删除原语（`host/fs` 只有 read / write / stat / list），所以最干净的
    /// 收尾就是写空内容覆盖——留一个 0 字节占位，总比留半个 AIRB 好排查。
    fn abandon_partial(
        &self,
        path: &str,
        index: usize,
        total_chunks: usize,
        written: usize,
        expected: usize,
        reason: HostError,
    ) -> HostError {
        let cleanup = match self.call(
            "host/fs.write",
            json!({ "path": path, "content": "", "encoding": "utf8" }),
        ) {
            Ok(_) => "已清空半截文件（宿主无删除原语，留下 0 字节占位，可自行删除）".to_string(),
            Err(err) => format!(
                "清理半截文件也失败（{}），`{path}` 现在是残缺的，请手工删除",
                err.message
            ),
        };
        HostError::transport(format!(
            "写 `{path}` 第 {}/{total_chunks} 片失败（已落盘 {} / {}）：{}｜{cleanup}",
            index + 1,
            util::format_bytes(written as u64),
            util::format_bytes(expected as u64),
            reason.message
        ))
    }

    /// 进度上报（`host/progress`）。失败不影响主流程：进度是锦上添花。
    pub fn progress(&self, message: &str) {
        let _ = self.call("host/progress", json!({ "message": message }));
    }

    /// 把用户给的路径规范成「工作区相对路径」。
    ///
    /// 宿主闸门只接受工作区内路径，所以绝对路径要能落到工作区里才有意义：
    /// 命中就转相对，不命中就原样交给宿主——让宿主回一条带原因的 PATH_OUT_OF_WORKSPACE，
    /// 比插件自己猜一个"看起来对"的错误更可信。
    pub fn normalize_path(&self, raw: &str) -> String {
        let cleaned = raw.trim().trim_matches('"').replace('\\', "/");
        let root = self.workspace_root().replace('\\', "/");
        let root = root.trim_end_matches('/');
        if !root.is_empty() {
            let lower = cleaned.to_ascii_lowercase();
            let root_lower = root.to_ascii_lowercase();
            if lower == root_lower {
                return String::new();
            }
            if lower.starts_with(&format!("{root_lower}/")) {
                return cleaned[root.len() + 1..].to_string();
            }
        }
        cleaned.trim_start_matches("./").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帧预算的基准：1 MiB 减掉帧头/路径/转义的预留。
    fn budget() -> usize {
        FRAME_LIMIT_BYTES - FRAME_RESERVE_BYTES
    }

    /// 恰好卡在预算线上：必须放行。
    ///
    /// 这条是「等于」而不是「小于」——闸门写的是 `frame <= budget`，
    /// 少放行一字节就等于白白浪费掉 64 KiB 预留里的一格，
    /// 而多放行一字节会让宿主直接判协议违规杀进程。边界两侧各测一次。
    #[test]
    fn frame_budget_allows_exactly_the_limit() {
        let path = "out/scene.air.json";
        let encoding = "utf8";
        let overhead = path.len() + encoding.len() + 256;

        let exact = budget() - overhead;
        assert_eq!(payload_budget(path, encoding), exact, "两个预算算式必须同源");
        assert!(ensure_frame_budget(path, exact, encoding).is_ok(), "刚好等于预算应当放行");

        let over = exact + 1;
        let err = ensure_frame_budget(path, over, encoding).expect_err("多一字节必须拦住");
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("1 MiB"), "错误里要写清撞的是宿主 1 MiB 帧上限：{}", err.message);
    }

    /// 分片窗口的三个硬约束，任何一条破了都会在大资产上翻车：
    /// ① 不超宿主单次读窗口（超了会被夹回去，靠「我要了多大」推偏移就会漏字节）；
    /// ② base64 进帧后仍不撞 1 MiB；
    /// ③ 读满 MAX_ASSET_BYTES 的调用数要留在单次工具调用的配额里。
    #[test]
    fn read_window_fits_host_and_frame_budget() {
        assert!(
            READ_WINDOW_BYTES as u64 <= HOST_READ_WINDOW_BYTES,
            "读窗口 {} 超过宿主上限 {}",
            READ_WINDOW_BYTES,
            HOST_READ_WINDOW_BYTES
        );
        let encoded = (READ_WINDOW_BYTES + 2) / 3 * 4;
        assert!(
            ensure_frame_budget("workspace/scene.air.bin", encoded, "base64").is_ok(),
            "一个读窗口 base64 后 {} 字节，装不进 1 MiB 帧",
            encoded
        );
        // 读侧：192 个窗口 + fs.stat + progress，必须留在 200 次配额内。
        let read_calls = calls_for(MAX_ASSET_BYTES);
        assert!(
            read_calls + 2 <= HOST_CALL_QUOTA,
            "读满 96 MiB 需 {read_calls} 次 host/fs.read，加 stat/progress 超配额"
        );
        // 但也要承认边界：96 MiB 已经吃掉大半配额，读写并存的工具（export/preview）
        // 必须靠 `write_sharded` 的余额检查提前报错，而不是让宿主回 6109。
        assert!(
            read_calls * 2 > HOST_CALL_QUOTA,
            "若读写各 96 MiB 都能装进配额，MAX_ASSET_BYTES 就定得过于保守"
        );
    }

    /// 写侧片长必须同时满足「一片装得进帧」和「整包切完不超配额」。
    #[test]
    fn write_chunk_fits_frame_and_quota() {
        let path = "workspace/out/scene.air.bin";
        let chunk = write_chunk_bytes(path);
        assert!(chunk > 0);
        let encoded = (chunk + 2) / 3 * 4;
        assert!(
            ensure_frame_budget(path, encoded, "base64").is_ok(),
            "单片 base64 后 {encoded} 字节，装不进 1 MiB 帧"
        );
        // 纯写场景：96 MiB 按该片长切，片数 + 若干次 stat/progress 仍在配额内。
        let pieces = (MAX_ASSET_BYTES as usize + chunk - 1) / chunk;
        assert!(
            pieces + 2 <= HOST_CALL_QUOTA,
            "96 MiB 要切 {pieces} 片，加 stat/progress 超过 {HOST_CALL_QUOTA} 次调用配额"
        );
    }

    /// 上限必须真按分片能力算，而不是继续吊在 1 MiB 帧上：
    /// 720 KiB 时代的大资产要能过闸，96 MiB 之上才拦。
    #[test]
    fn asset_limit_reflects_paged_ability() {
        assert_eq!(MAX_ASSET_BYTES, 96 * 1024 * 1024);
        assert!(MAX_ASSET_BYTES > LEGACY_READ_BYTES, "分片上限必须高于旧宿主整帧上限");
        // 旧宿主整帧上限反算：base64 4:3 塞进一帧，正是 0.2.0 的 720 KiB 来源。
        let legacy_encoded = (LEGACY_READ_BYTES + 2) / 3 * 4;
        let frame_budget = budget();
        assert!(
            legacy_encoded as usize <= frame_budget,
            "旧路径上限 {legacy_encoded} 字节装不进帧预算 {frame_budget}"
        );
    }

    /// 握手期补问工作区必须走短超时：它没有工具调用兜底，卡住就是启动卡住。
    #[test]
    fn handshake_timeout_is_short_but_not_zero() {
        assert!(HOST_CALL_TIMEOUT_SHORT <= Duration::from_secs(2), "握手超时不能超过 2 秒");
        assert!(HOST_CALL_TIMEOUT_SHORT >= Duration::from_millis(500), "太短会把慢机器误判成不支持");
        assert!(HOST_CALL_TIMEOUT_SHORT < HOST_CALL_TIMEOUT);
    }
}
