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
mod command;
mod fbx;
mod gltf;
mod pipe;
mod rpc;
mod tools;
mod ui;
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
/// 1.1 = 支持清单 `views`（声明式面板视图 + 外部视口）。宿主 `HOST_PLUGIN_API_MINOR`
/// 现在是 1，两边对齐：低于它的宿主会忽略 `views`（面板显示「此插件不需要面板」），
/// 而不是把插件判成「要求更高的 API」拒绝加载。
const PLUGIN_API_MINOR: u64 = 1;

/// 页面里的视图 id：面板按它拉 `view.render`，也按它找外部程序。
const VIEW_ID: &str = "main";

const HANDSHAKE_METHOD: &str = "plugin/handshake";
const LEGACY_HANDSHAKE_METHOD: &str = "plugin/initialize";
const TOOL_INVOKE_METHOD: &str = "plugin/tool-invoke";
const COMMAND_INVOKE_METHOD: &str = "plugin/command-invoke";
const SHUTDOWN_METHOD: &str = "plugin/shutdown";

/// 工具线程栈：glTF 节点树、FBX 递归层级加上 `serde_json` 的深递归，
/// 2 MiB 默认栈在极端资产上会溢出，直接 abort 整个进程（`panic=abort`）。
const TOOL_STACK_BYTES: usize = 16 * 1024 * 1024;

/// 进程形态，由 argv 决定（`docs/24 §5.3` 的三形态）。
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// 无参 / 只有 `--pipe`：宿主拉起的会话形态，stdin/stdout 走 NDJSON。
    Session,
    /// `--manifest`：打印清单 JSON，打包时用来生成 plugin.json。
    Manifest,
    /// `--standalone <workspace> [--path <asset>]`：不带 IDE 单独跑，把视图
    /// 描述树打到 stdout。M-UI-3/4 的视口实现要反复调同一份渲染逻辑，
    /// 每次回 IDE 里点一遍太慢。
    Standalone {
        root: String,
        asset: Option<String>,
    },
}

/// 参数互斥 / 缺值：退出码 2（与 `getopt` 系工具的约定一致，脚本里判得出来）。
const EXIT_USAGE: i32 = 2;
/// 路径不对（工作区不存在、不是目录、资产读不到）：退出码 3。
const EXIT_BAD_PATH: i32 = 3;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_mode(&argv) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("asset3d: {message}\n\n用法：\n  asset3d                         # 宿主会话（stdin/stdout NDJSON）\n  asset3d --manifest              # 打印 plugin.json\n  asset3d --standalone <工作区> [--path <资产>]   # 离线渲染视图描述树");
            std::process::exit(EXIT_USAGE);
        }
    };

    match mode {
        // `asset3d --manifest`：打印清单 JSON，安装包时用来生成/核对 plugin.json，
        // 免得代码里的工具表和清单里的声明各自漂移。
        Mode::Manifest => match serde_json::to_string_pretty(&manifest()) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("asset3d: 生成清单失败：{err}");
                std::process::exit(1);
            }
        },
        Mode::Standalone { root, asset } => {
            std::process::exit(run_standalone(&root, asset.as_deref()));
        }
        Mode::Session => {
            let host = Arc::new(Host::new());
            let wire = host.wire();
            if let Err(err) = serve(Arc::clone(&host), wire) {
                eprintln!("asset3d: 主循环异常退出：{err}");
                std::process::exit(1);
            }
        }
    }
}

/// 解析 argv。三条硬规则：`--manifest` 与 `--standalone` 互斥、`--standalone`
/// 与 `--pipe` 互斥、`--standalone` 必须带工作区。互斥不是洁癖——两种形态抢
/// 同一个 stdout，谁先跑谁赢，静默选一个只会让人以为参数生效了。
fn parse_mode(argv: &[String]) -> Result<Mode, String> {
    let mut manifest = false;
    let mut standalone: Option<String> = None;
    let mut asset: Option<String> = None;
    let mut pipe = false;
    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_str();
        match arg {
            "--manifest" => {
                manifest = true;
                index += 1;
            }
            // 宿主 spawn 会话形态时会带 `--pipe`：本进程不连管道（管道是视口
            // 进程的事），但必须认这个参数，否则外壳传进来就成了「未知参数」。
            "--pipe" => {
                pipe = true;
                // 管道名可选：外壳可能只写 `--pipe` 让进程用默认名。
                if argv.get(index + 1).is_some_and(|next| !next.starts_with("--")) {
                    index += 1;
                }
                index += 1;
            }
            "--standalone" => {
                let value = argv
                    .get(index + 1)
                    .filter(|next| !next.starts_with("--"))
                    .cloned()
                    .ok_or_else(|| "--standalone 需要一个工作区目录参数".to_string())?;
                standalone = Some(value);
                index += 2;
            }
            "--path" => {
                let value = argv
                    .get(index + 1)
                    .cloned()
                    .ok_or_else(|| "--path 需要一个资产路径".to_string())?;
                asset = Some(value);
                index += 2;
            }
            other if other.starts_with("--standalone=") => {
                standalone = Some(other["--standalone=".len()..].to_string());
                index += 1;
            }
            other if other.starts_with("--path=") => {
                asset = Some(other["--path=".len()..].to_string());
                index += 1;
            }
            other => return Err(format!("无法识别的参数 `{other}`")),
        }
    }

    if manifest {
        if standalone.is_some() || pipe {
            return Err("--manifest 不能与 --standalone / --pipe 同时使用".to_string());
        }
        return Ok(Mode::Manifest);
    }
    match standalone {
        Some(root) => {
            if pipe {
                return Err("--standalone 不能与 --pipe 同时使用：一个要独占 stdout，另一个是宿主会话".to_string());
            }
            if root.trim().is_empty() {
                return Err("--standalone 的工作区目录不能为空".to_string());
            }
            Ok(Mode::Standalone { root, asset })
        }
        None => {
            if asset.is_some() {
                return Err("--path 只在 --standalone 下有意义".to_string());
            }
            Ok(Mode::Session)
        }
    }
}

/// `--standalone`：本地只读渲染一遍视图，把描述树打到 stdout。
///
/// 退出码按「用户下一步能做什么」分：3 = 路径错了（改参数），1 = 资产本身解析
/// 不了（换文件）。把两者混成 1，脚本里就分不清是「我传错了」还是「这文件坏了」。
fn run_standalone(root: &str, asset: Option<&str>) -> i32 {
    let host = match Host::new_local(root) {
        Ok(host) => host,
        Err(err) => {
            eprintln!("asset3d: {}", err.message);
            return EXIT_BAD_PATH;
        }
    };
    let args = match asset {
        Some(path) if !path.trim().is_empty() => json!({ "path": path }),
        _ => json!({}),
    };
    host.reset_call_quota();
    match tools::invoke(&host, "view_render", &args) {
        Ok(value) => {
            let text = value.as_str().unwrap_or_default();
            match serde_json::from_str::<Value>(text) {
                Ok(tree) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&tree).unwrap_or_else(|_| text.to_string())
                    );
                    0
                }
                Err(err) => {
                    eprintln!("asset3d: 视图树不是合法 JSON：{err}");
                    1
                }
            }
        }
        Err(err) => {
            eprintln!("asset3d: {}（code={}）", err.message, err.code);
            if err.code == rpc::STANDALONE_BAD_PATH {
                EXIT_BAD_PATH
            } else {
                1
            }
        }
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
                    COMMAND_INVOKE_METHOD => {
                        if !spawn_command(Arc::clone(&host), Arc::clone(&wire), id.clone(), params) {
                            reply(
                                &wire,
                                &id,
                                Err((-32603, "无法创建命令线程".to_string())),
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

/// 命令同样要开工作线程：`exportAsset` 会写盘并回读校验，`openViewport` 可能
/// 卡在管道等待上（1.5 s）。跑在主循环里会把宿主回包一起堵住。
fn spawn_command(host: Arc<Host>, wire: Arc<Wire>, id: Value, params: Value) -> bool {
    let builder = std::thread::Builder::new()
        .name(format!("asset3d-cmd-{id}"))
        .stack_size(TOOL_STACK_BYTES);
    builder
        .spawn(move || {
            let outcome = run_command(&host, &params);
            reply(&wire, &id, outcome);
        })
        .is_ok()
}

/// `plugin/command-invoke`：参数 `{command, args}`（见 `crates/plugin/src/host.rs`）。
///
/// 与工具一样剥掉可能的命名空间前缀；配额计数器同理归零——命令内部会转调
/// `host/fs.*`，配额不重置的话第二条命令一上来就撞 200 次上限。
fn run_command(host: &Host, params: &Value) -> Result<Value, (i64, String)> {
    let raw = params
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if raw.is_empty() {
        return Err((-32602, "plugin/command-invoke 缺少 command".to_string()));
    }
    let name = raw.rsplit("__").next().unwrap_or(&raw).to_string();
    host.reset_call_quota();
    let args = params
        .get("args")
        .cloned()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({}));
    command::invoke(host, &name, &args).map(Value::String)
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
    // 宿主的 200 次调用配额是「按工具调用」重置的（`begin_call`），插件这边的
    // 计数器必须同步归零，否则第二次工具调用一开始就以为自己没额度了。
    host.reset_call_quota();
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
        "commands": command::catalog(),
        "subscriptions": [],
    })
}

/// 生成 `plugin.json` 的内容：与 [`tools::catalog`] 同源，不手工维护第二份。
///
/// `pathField` 只给写盘工具填：审批按「哪个文件」落键，读工具不涉及审批。
/// 视口进程的可执行文件名：`None` = 本轮不声明 `views[].external`。
///
/// 宿主的清单校验要求 `external.program` 在包内**真实存在**（`docs/24 §4.2`
/// 与宿主 `validate` 的 `views[].external.program` 分支），而视口 GUI 由
/// M-UI-3 交付。提前声明等于让整个包装不上——校验失败的是「包」而不是
/// 「视图」这一条，代价太大。等 GUI 进 dist 时把这里改成 `Some(...)` 即可，
/// 清单其余部分不用动；在改之前，面板照常渲染只读描述树（ADR-16）。
const VIEW_EXTERNAL_PROGRAM: Option<&str> = None;

/// 视图声明。`external` 只在 [`VIEW_EXTERNAL_PROGRAM`] 有值时出现。
fn view_entry() -> Value {
    // `refresh` 只挂 `workspace.changed`：资产被外部工具改写的场景几乎都在
    // 切窗回来的那一下，挂太多事件会让面板在导出期间疯狂重解析。
    let mut view = json!({
        "id": VIEW_ID,
        "title": "3D 资产",
        "location": "pluginPanel.detail",
        "entry": "view_render",
        "refresh": ["workspace.changed"],
    });
    if let Some(program) = VIEW_EXTERNAL_PROGRAM {
        view["external"] = json!({
            "program": program,
            "protocol": "asset3d.session/1",
        });
    }
    view
}

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
        "commands": command::catalog(),
        // 声明式面板视图（docs/24 §4.2）：面板本体是 `view.render` 折出来的
        // 描述树，「在 3D 视口中打开」额外拉起 `external.program`。
        //
        // `refresh` 只挂 `workspace.changed`：资产被外部工具改写的场景几乎都在
        // 切窗回来的那一下，挂太多事件会让面板在导出期间疯狂重解析。
        "views": [view_entry()],
        "subscriptions": [],
        // 解析大 FBX 可能超过默认 30s；清单上限 120s，取满。
        "timeoutMs": 120_000,
    })
}
