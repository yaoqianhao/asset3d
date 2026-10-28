//! 插件命令（`plugin/command-invoke`）—— 面板按钮与小地图之外的第二类入口。
//!
//! 与工具的区别只在**谁发起**：工具由模型或用户从命令面板调，命令由面板上的
//! 按钮、视图里的 `action` 触发。两者共用同一套审批网关，所以这里**不复制**
//! 导出逻辑，一律转调 [`crate::tools`]：审批、路径闸门、64 KiB 截断都在宿主那
//! 一侧生效，插件内部再写一遍等于给自己留一条绕过闸门的路。
//!
//! 唯一有自己逻辑的是 [`open_viewport`]：它不读写工作区，只把一帧 `open` 指令
//! 交给已经在跑的视口进程（docs/24 §5.2）。

use serde_json::{json, Value};

use crate::rpc::Host;
use crate::tools;
use crate::ui;
use crate::PLUGIN_ID;

/// 「视口管道连不上」的插件自有错误码（5xxx 域，与宿主 6xxx 闸门码分开）。
const PIPE_FAILED: i64 = 5004;

/// 命令名 → 面板文案与风险档位（与 `manifest().commands` 逐字同源，只维护这一份）。
///
/// `risk` 必须与 `manifest()` 里写的完全一致：宿主的审批按清单走，握手只是
/// 展示用；两处不一致时用户看到的档位会和实际弹窗对不上。
pub fn catalog() -> Value {
    json!([
        {
            "name": ui::CMD_OPEN_VIEWPORT,
            "title": "在 3D 视口中打开",
            "description": "把资产路径发给已经启动的 3D 视口窗口；视口没起来时先由宿主拉起进程。只读，不写盘。",
            "risk": "read",
            "pathField": "path",
            "args": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "资产路径（工作区相对路径或工作区内绝对路径）"
                    },
                    "pipe": {
                        "type": "string",
                        "description": "宿主分配的视口管道名；省略时用默认管道（手动双击启动的视口）"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": ui::CMD_EXPORT_ASSET,
            "title": "导出 AIR",
            "description": "把资产导出成 .air.bin / .air.json（等价于 export_asset 工具），执行前弹审批。",
            "risk": "write",
            "pathField": "path",
            "args": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "源资产路径" },
                    "out": { "type": "string", "description": "输出路径；省略时与源文件同目录同名" },
                    "format": {
                        "type": "string",
                        "enum": ["bin", "json", "both"],
                        "description": "导出格式，默认 both"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": ui::CMD_EXPORT_LOADER,
            "title": "导出引擎加载器",
            "description": "写出读取 AIR/AIRB 的 Rust 加载器源码（等价于 export_engine_loader 工具），执行前弹审批。",
            "risk": "write",
            "pathField": "out",
            "args": {
                "type": "object",
                "properties": {
                    "out": { "type": "string", "description": "目标 .rs 路径，例如 src/air_loader.rs" },
                    "overwrite": {
                        "type": "boolean",
                        "description": "目标已存在时是否覆盖，默认 false",
                        "default": false
                    }
                },
                "required": ["out"],
                "additionalProperties": false
            }
        }
    ])
}

/// 执行一条命令。返回的字符串会被宿主原样塞进 `PluginCommandResult.content`。
pub fn invoke(host: &Host, command: &str, args: &Value) -> Result<String, (i64, String)> {
    match command {
        ui::CMD_OPEN_VIEWPORT => open_viewport(host, args),
        // 导出类命令直接转工具：风险档位由清单决定，实现只有一份。
        ui::CMD_EXPORT_ASSET => run_tool(host, "export_asset", args),
        ui::CMD_EXPORT_LOADER => run_tool(host, "export_engine_loader", args),
        other => Err((
            -32601,
            format!("asset3d 没有名为 `{other}` 的命令"),
        )),
    }
}

fn run_tool(host: &Host, tool: &str, args: &Value) -> Result<String, (i64, String)> {
    match tools::invoke(host, tool, args) {
        Ok(value) => Ok(value.as_str().unwrap_or_default().to_string()),
        Err(err) => Err((err.code, err.message)),
    }
}

/// 把 `open` 指令发给视口。
///
/// 管道名优先用宿主给的 `pipe`（`--pipe` 形态，随机令牌），缺省时退回默认管道名
/// （用户自己双击 `asset3d-gui.exe` 的形态）。两条路都失败就把原因说清楚：
/// 这个按钮点下去没反应时，用户唯一能做的就是「再点一次」或「去看日志」。
fn open_viewport(host: &Host, args: &Value) -> Result<String, (i64, String)> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if raw.is_empty() {
        return Err((-32602, "open_viewport 缺少 path".to_string()));
    }
    let rel = host.normalize_path(&raw);
    if rel.is_empty() {
        return Err((-32602, "path 不能为空".to_string()));
    }
    // 先确认文件在工作区里确实存在：视口自己也要读盘，但让用户在面板上先看到
    // 「文件不存在」比在另一个窗口里看到一个空场景有用得多。
    if let Err(err) = host.file_size(&rel) {
        return Err((
            err.code,
            format!("`{rel}` 读不到（{}）：先在资源管理器里确认文件还在。", err.message),
        ));
    }

    let pipe = args
        .get("pipe")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}{PLUGIN_ID}.default", crate::pipe::PIPE_PREFIX));

    let frame = json!({
        "v": 1,
        "op": "open",
        // `seq=0` = 单向通知，视口不回执（docs/24 §5.2）。
        "seq": 0,
        "path": rel,
    });
    let line = serde_json::to_string(&frame)
        .map_err(|err| (-32603, format!("指令序列化失败：{err}")))?;
    crate::pipe::send_line(&pipe, &line).map_err(|reason| (PIPE_FAILED, reason))?;
    Ok(format!("已把 `{rel}` 发给 3D 视口。"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_names_match_manifest_contract() {
        let raw = serde_json::to_string(&catalog()).expect("序列化");
        assert!(raw.contains(ui::CMD_OPEN_VIEWPORT));
        assert!(raw.contains(ui::CMD_EXPORT_ASSET));
        assert!(raw.contains(ui::CMD_EXPORT_LOADER));
        // 5013 是「写了 exec 档」时宿主报的错：这里一旦出现，装都装不上。
        assert!(!raw.contains("\"exec\""));
    }

    #[test]
    fn open_viewport_requires_path() {
        let host = Host::new();
        let err = open_viewport(&host, &json!({})).expect_err("缺 path 必须报错");
        assert_eq!(err.0, -32602);
    }
}
