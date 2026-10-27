//! asset3d —— ai-ide 的 3D 资产导入插件。
//!
//! 职责：把工作区里的 `.fbx` / `.glb` / `.gltf` 解析成引擎内核可直接消费的
//! **AIR**（Asset Intermediate Representation），并提供导出、预览与批量清单能力。
//!
//! 进程模型（与 `crates/plugin` 的宿主侧约定逐条对齐，改这里之前先看
//! `docs/14` 插件协议）：
//!
//! - **stdin**：宿主发来的 NDJSON 帧。带 `method` 的是请求，只有 `id` 的是
//!   `host/*` 回包，两者都在这一条线程上读——所以工具执行绝不能跑在这里，
//!   否则解析大资产的那几百毫秒里连自己的宿主回包都收不到，直接互相等死。
//! - **stdout**：只放 JSON 帧，一行一帧、写完立刻 flush。**单帧上限 1 MiB**
//!   （`crates/plugin/src/loader.rs: FRAME_LIMIT_BYTES`），超了宿主会判协议
//!   违规并把插件踢下线——因此回包只放摘要文本，大产物一律走 `host/fs.write`。
//! - **stderr**：自由日志。宿主只留尾巴用于崩溃诊断，不当协议数据。
//!
//! 退出约定：`-32000` 是宿主侧的「进程已退出」哨兵（`EXITED_REPLY_CODE`），
//! 插件任何情况下都不能用它当错误码，否则一次普通失败会被记成崩溃。

mod air;
mod air_bin;
mod fbx;
mod gltf;
mod rpc;
mod tools;
mod util;
mod view;

use std::io::BufRead;
use std::sync::Arc;

use serde_json::{json, Value};

use rpc::{Host, Wire};

/// 插件清单里的 id，必须与 `plugin.json` 一致（宿主按它拼工具命名空间）。
const PLUGIN_ID: &str = "asset3d";
const PLUGIN_NAME: &str = "asset3d";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const PLUGIN_API_MAJOR: u64 = 1;
const PLUGIN_API_MINOR: u64 = 0;

const HANDSHAKE_METHOD: &str = "plugin/handshake";
const LEGACY_HANDSHAKE_METHOD: &str = "plugin/initialize";
const TOOL_INVOKE_METHOD: &str = "plugin/tool-invoke";
const SHUTDOWN_METHOD: &str = "plugin/shutdown";

/// 工具线程栈：glTF 节点树、FBX 递归层级加上 `serde_json` 的深递归，
/// 2 MiB 默认栈在极端资产上会溢出，直接 abort 整个进程（`panic=abort`）。
const TOOL_STACK_BYTES: usize = 16 * 1024 * 1024;

fn main() {
    // `asset3d --manifest`：打印清单 JSON，安装包时用来生成/核对 plugin.json，
    // 免得代码里的工具表和清单里的声明各自漂移。
    if std::env::args().skip(1).any(|arg| arg == "--manifest") {
        match serde_json::to_string_pretty(&manifest()) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("asset3d: 生成清单失败：{err}");
                std::process::exit(1);
            }
        }
        return;
    }

    let host = Arc::new(Host::new());
    let wire = host.wire();
    if let Err(err) = serve(Arc::clone(&host), wire) {
        eprintln!("asset3d: 主循环异常退出：{err}");
        std::process::exit(1);
    }
}

/// 读 stdin 直到 EOF 或 `plugin/shutdown`。
fn serve(host: Arc<Host>, wire: Arc<Wire>) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(line) = lines.next() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // 帧上限同样适用于入站方向：宿主不会发超限帧，真有就是有人在手工喂数据。
        if line.len() > 16 * 1024 * 1024 {
            eprintln!("asset3d: 忽略超长入站帧（{} 字节）", line.len());
            continue;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(err) => {
                // 单帧坏掉不该带走整个插件：记一行日志继续读，下一个帧还能干活。
                eprintln!("asset3d: 非法 JSON 帧：{err}");
                continue;
            }
        };
        let id = frame.get("id").filter(|value| !value.is_null()).cloned();
        let method = frame.get("method").and_then(Value::as_str).map(str::to_string);

        match (id, method) {
            (Some(id), Some(method)) => {
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                match method.as_str() {
                    HANDSHAKE_METHOD | LEGACY_HANDSHAKE_METHOD => {
                        apply_handshake_params(&host, &params);
                        reply(&wire, &id, Ok(handshake_result()));
                    }
                    TOOL_INVOKE_METHOD => {
                        if !spawn_tool(Arc::clone(&host), Arc::clone(&wire), id.clone(), params) {
                            reply(
                                &wire,
                                &id,
                                Err((-32603, "无法创建工具线程".to_string())),
                            );
                        }
                    }
                    SHUTDOWN_METHOD => {
                        reply(&wire, &id, Ok(json!({ "ok": true })));
                        break;
                    }
                    other => reply(
                        &wire,
                        &id,
                        Err((-32601, format!("asset3d 未实现方法 {other}"))),
                    ),
                }
            }
            // 只有 id：宿主对 `host/*` 的应答，交给等待方。
            (Some(_), None) => host.deliver(&frame),
            // 只有 method：宿主通知（`plugin/event` 等）。本插件不订阅任何事件。
            (None, Some(method)) => eprintln!("asset3d: 收到通知 {method}（未订阅，忽略）"),
            (None, None) => eprintln!("asset3d: 收到既无 id 也无 method 的帧，忽略"),
        }
    }
    Ok(())
}

fn spawn_tool(host: Arc<Host>, wire: Arc<Wire>, id: Value, params: Value) -> bool {
    let builder = std::thread::Builder::new()
        .name(format!("asset3d-tool-{id}"))
        .stack_size(TOOL_STACK_BYTES);
    builder
        .spawn(move || {
            let outcome = run_tool(&host, &params);
            reply(&wire, &id, outcome);
        })
        .is_ok()
}

fn run_tool(host: &Host, params: &Value) -> Result<Value, (i64, String)> {
    let raw_tool = params
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if raw_tool.is_empty() {
        return Err((-32602, "plugin/tool-invoke 缺少 tool".to_string()));
    }
    // 宿主可能传全名 `plugin__asset3d__inspect_asset`（注册表口径），也可能传
    // 裸名（握手顺序）。两种都收，剥掉命名空间即可。
    let tool = raw_tool.rsplit("__").next().unwrap_or(&raw_tool).to_string();
    let args = params
        .get("arguments")
        .cloned()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({}));
    tools::invoke(host, &tool, &args).map_err(|err| (err.code, err.message))
}

fn reply(wire: &Wire, id: &Value, outcome: Result<Value, (i64, String)>) {
    let frame = match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    };
    if let Err(err) = wire.send(&frame) {
        // 写不出去只可能是宿主关了管道：记日志，主循环下一轮读 EOF 自然退出。
        eprintln!("asset3d: 回包写入失败：{err}");
    }
}

fn apply_handshake_params(host: &Host, params: &Value) {
    if let Some(id) = params.pointer("/plugin/id").and_then(Value::as_str) {
        host.set_plugin_id(id);
    }
    let root = params
        .pointer("/workspace/root")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = params
        .pointer("/workspace/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !root.is_empty() {
        host.set_workspace(root, name);
        return;
    }
    // 老版本宿主可能不在握手参数里带 workspace：补问一次，否则绝对路径无法
    // 归一化成工作区相对路径，后面的每次读写都要多绕一圈才失败。
    //
    // 这一步用短超时（2s）而不是常规 20s：握手期间没有工具调用在跑，宿主收不到
    // 回包也不会有人兜底，白等 20 秒只会让插件看起来「启动卡住」。拿不到就降级：
    // 工作区留空，绝对路径原样交给宿主，由宿主闸门回一条带原因的错误。
    match host.call_with_timeout(
        "host/workspace.info",
        json!({}),
        rpc::HOST_CALL_TIMEOUT_SHORT,
    ) {
        Ok(info) => {
            let root = info.get("root").and_then(Value::as_str).unwrap_or("");
            let name = info.get("name").and_then(Value::as_str).unwrap_or("");
            if !root.is_empty() {
                host.set_workspace(root, name);
            }
        }
        Err(err) => eprintln!(
            "asset3d: 握手期取工作区信息失败，已降级为「无工作区」（绝对路径将交给宿主闸门判定）：{err}"
        ),
    }
}

fn handshake_result() -> Value {
    json!({
        "pluginApi": { "major": PLUGIN_API_MAJOR, "minor": PLUGIN_API_MINOR },
        "name": PLUGIN_NAME,
        "version": PLUGIN_VERSION,
        "tools": tools::catalog(),
        "commands": [],
        "subscriptions": [],
    })
}

/// 生成 `plugin.json` 的内容：与 [`tools::catalog`] 同源，不手工维护第二份。
///
/// `pathField` 只给写盘工具填：审批按「哪个文件」落键，读工具不涉及审批。
fn manifest() -> Value {
    let tools: Vec<Value> = tools::catalog()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| {
            let name = tool.get("name").cloned().unwrap_or(Value::Null);
            let path_field = match name.as_str().unwrap_or("") {
                "export_asset" | "preview_asset" => Some("path"),
                "export_engine_loader" => Some("out"),
                _ => None,
            };
            let mut item = json!({
                "name": name,
                "description": tool.get("description").cloned().unwrap_or(Value::Null),
                "risk": tool.get("risk").cloned().unwrap_or(Value::Null),
                "parameters": tool.get("parameters").cloned().unwrap_or(Value::Null),
            });
            if let Some(field) = path_field {
                item["pathField"] = json!(field);
            }
            item
        })
        .collect();

    json!({
        "id": PLUGIN_ID,
        "name": "3D 资产导入（AIR）",
        "version": PLUGIN_VERSION,
        "description": "把 FBX / glTF 资产解析成引擎内核可直接消费的 AIR 数据，支持二进制/JSON 导出、离线 HTML+数据分片预览与批量清单。",
        "pluginApi": format!("{PLUGIN_API_MAJOR}.{PLUGIN_API_MINOR}"),
        // 入口是可执行文件名：宿主按清单在包内定位并 spawn。
        "main": if cfg!(windows) { "asset3d.exe" } else { "asset3d" },
        "capabilities": ["fs.read", "fs.write", "progress"],
        "tools": tools,
        "commands": [],
        "subscriptions": [],
        // 解析大 FBX 可能超过默认 30s；清单上限 120s，取满。
        "timeoutMs": 120_000,
    })
}
