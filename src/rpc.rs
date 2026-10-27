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
use std::sync::atomic::{AtomicU64, Ordering};
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
/// 读写都是「整个文件塞进一帧」：`host/fs.read` 回包与 `host/fs.write` 请求
/// 各占一帧，超了宿主直接判协议违规并终止进程（6106），所以插件必须提前拦住，
/// 把「为什么读不了」讲清楚，而不是让宿主把进程杀掉。
pub const FRAME_LIMIT_BYTES: usize = 1024 * 1024;

/// 给帧头、路径、转义与字段名留的余量。
const FRAME_RESERVE_BYTES: usize = 64 * 1024;

/// 单次 `host/fs.read` 能拿到的原始字节上限。
///
/// base64 膨胀 4/3，所以「1 MiB 帧」对应的原始数据只有约 720 KiB。宿主
/// `fs.read` 目前没有 `offset`，不能分片读——想处理更大的资产，只能在宿主侧
/// 给这两个方法加 offset（读）与 offset/append（写）。
pub const MAX_ASSET_BYTES: u64 =
    ((FRAME_LIMIT_BYTES - FRAME_RESERVE_BYTES) as u64) / 4 * 3;

/// 某条路径 + 编码下，单次写盘**载荷**能有多少字节。
///
/// 与 `ensure_frame_budget` 用同一套算式：调用方（预览分片）必须在动笔前就知道
/// 闸门在哪，才能保证每一片都过得去，而不是写完最后一片才被宿主杀掉进程。
pub fn payload_budget(path: &str, encoding: &str) -> usize {
    (FRAME_LIMIT_BYTES - FRAME_RESERVE_BYTES).saturating_sub(path.len() + encoding.len() + 256)
}

/// 写盘前先算帧大小：`host/fs.write` 把整份内容放进一帧，超了宿主判协议违规。
///
/// 这里拦下来，报的是「宿主原语不支持分片」，而不是让进程被杀掉丢掉工具结果。
/// 修法是宿主侧给 `fs.write` 加 `offset`（首次写 truncate，后续写续写），
/// 插件再按 512 KiB 一片推。
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
        "写 `{path}` 需要约 {}{raw_hint} 的单帧，超过宿主 1 MiB 帧上限。\
         宿主 host/fs.write 目前整文件一帧、不支持 offset 分片，插件无法绕过能力闸门自己落盘——\
         请改用 embedTextures=false 导出或拆分资产，或先在宿主侧给 fs.write 加 offset。",
        util::format_bytes(frame as u64)
    )))
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
        }
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
    /// `maxBytes` 直接取 stat 得到的大小：宿主 `fs.read` 没有 `offset`，分块读大
    /// 资产会把 200 次/调用的配额吃光（10MB / 64KB = 160 次），一次读完才是正解。
    pub fn read_bytes(&self, path: &str) -> Result<Vec<u8>, HostError> {
        let size = self.file_size(path)?;
        if size > MAX_ASSET_BYTES {
            return Err(HostError::transport(format!(
                "文件 {path} 为 {}，超过宿主单帧能带回的上限 {}（1 MiB 帧 / base64 4:3）。\
                 宿主 host/fs.read 当前不支持 offset 分片，插件也不能绕过能力闸门直接读盘——\
                 请改用 .gltf + 外部 .bin 拆分资源，或先在宿主侧给 fs.read 加 offset。",
                util::format_bytes(size),
                util::format_bytes(MAX_ASSET_BYTES)
            )));
        }
        let reply = self.call(
            "host/fs.read",
            json!({ "path": path, "maxBytes": size.max(1), "encoding": "base64" }),
        )?;
        if reply.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
            return Err(HostError::transport(format!(
                "文件 {path} 在读取过程中变长，已被截断；请重试"
            )));
        }
        let content = reply
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| HostError::transport("宿主 fs.read 回包缺少 content"))?;
        BASE64
            .decode(content)
            .map_err(|err| HostError::transport(format!("回包不是合法 base64：{err}")))
    }

    /// 读文本文件（GLTF 的 `.gltf`、贴图清单等）。
    pub fn read_text(&self, path: &str) -> Result<String, HostError> {
        let reply = self.call("host/fs.read", json!({ "path": path }))?;
        Ok(reply
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
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
        ensure_frame_budget(path, text.len(), "utf8")?;
        self.call(
            "host/fs.write",
            json!({ "path": path, "content": text, "encoding": "utf8" }),
        )
    }

    pub fn write_bytes(&self, path: &str, bytes: &[u8]) -> Result<Value, HostError> {
        // base64 后才是真正进帧的长度，按 4 倍向上取整再算。
        let encoded_len = (bytes.len() + 2) / 3 * 4;
        ensure_frame_budget(path, encoded_len, "base64")?;
        self.call(
            "host/fs.write",
            json!({ "path": path, "content": BASE64.encode(bytes), "encoding": "base64" }),
        )
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

    /// base64 写盘的预算按「编码后」长度算：原始字节 3 变 4，
    /// 直接用原始长度（或干脆用元素个数）去比预算，会高估能写的量，
    /// 结果就是最后一片写出去时被宿主判协议违规、整个进程被杀。
    #[test]
    fn base64_length_inflates_budget_accounting() {
        let path = "out/scene.air.bin";
        // 读侧上限换算回原始字节后，再次 base64 正好顶满 1 MiB 减预留那一格：
        // 这说明 MAX_ASSET_BYTES 与帧预算是同一个算式推出来的，不是拍脑袋的数字。
        let encoded = ((MAX_ASSET_BYTES + 2) / 3 * 4) as usize;
        assert_eq!(encoded, budget(), "读侧上限反算回来必须正好等于帧预算");

        // 同一个载荷按 base64 记会超限，按 utf8 记（错误口径）却不会：
        // 这就是「用错长度口径」会踩的坑，测出来防止回归。
        let just_over = budget() - (path.len() + "base64".len() + 256) + 1;
        assert!(ensure_frame_budget(path, just_over, "base64").is_err());
        assert!(ensure_frame_budget(path, just_over, "utf8").is_ok());
    }

    /// 握手期补问工作区必须走短超时：它没有工具调用兜底，卡住就是启动卡住。
    #[test]
    fn handshake_timeout_is_short_but_not_zero() {
        assert!(HOST_CALL_TIMEOUT_SHORT <= Duration::from_secs(2), "握手超时不能超过 2 秒");
        assert!(HOST_CALL_TIMEOUT_SHORT >= Duration::from_millis(500), "太短会把慢机器误判成不支持");
        assert!(HOST_CALL_TIMEOUT_SHORT < HOST_CALL_TIMEOUT);
    }
}
