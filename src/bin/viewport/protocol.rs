//! 会话协议 `asset3d.session/1`（docs/24 §5.2）的视口侧编解码。
//!
//! 行分隔 JSON，`{"v":1,"op":"...","seq":N,...}`。三条硬规则：
//!
//! 1. **未知 `op`、未知字段一律忽略**——协议要能往前加东西，老视口遇到新指令
//!    不能崩，只能这条不响应；
//! 2. `seq=0` 表示单向通知，不回包；`seq>0` 才需要按同样的 `seq` 回；
//! 3. `screenshot` 本期是**占位**：收到就回 `error{code:"unsupported"}`，
//!    让对端明确知道"不是没收到，是这版不做"，而不是静默超时。

use serde_json::{json, Value};

/// 视口支持的能力集，握手时自述（`ready.capabilities`）。
///
/// 第三刀起报 `animated`：时间轴（play / scrub / loop）与骨骼线框都已落地，
/// 蒙皮矩阵每帧按动画求值——对端可以据此把「播放」相关控件放出来了。
pub const CAPABILITIES: [&str; 2] = ["static", "animated"];

/// 协议版本，进每一帧的 `v` 字段。
pub const PROTOCOL_VERSION: u32 = 1;

/// 一条已解析的指令。
#[derive(Debug, PartialEq)]
pub enum Request {
    /// 打开资产。`path` 是工作区相对路径，由插件宿主校验后转发。
    Open {
        path: String,
        animation: Option<String>,
        play: bool,
        seq: u64,
    },
    /// 关闭当前资产，进程不退。
    Close { seq: u64 },
    /// 置前并抢焦点。
    Focus { seq: u64 },
    /// 心跳。
    Ping { seq: u64 },
    /// 截图：协议占位，回 `unsupported`。
    Screenshot { seq: u64 },
    /// 协议里没有的 op：忽略（不回包也不报错）。
    Unknown,
}

impl Request {
    /// 该帧要不要回、按哪个 `seq` 回。`None` 表示不响应。
    ///
    /// 这一刀所有回包都在 [`parse`] 之后就地生成，没有走这条通用取 `seq` 的路径；
    /// 等 M-UI-4 加「`open` 异步完成再 ack」时它就是统一入口，先留着并放行警告。
    #[allow(dead_code)]
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Open { seq, .. }
            | Self::Close { seq }
            | Self::Focus { seq }
            | Self::Ping { seq }
            | Self::Screenshot { seq } => Some(*seq),
            Self::Unknown => None,
        }
    }
}

/// 解析一行。返回 `Err` 时调用方只记日志——畸形帧不是攻击面，是版本错位。
pub fn parse(line: &str) -> Result<Request, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|err| format!("帧不是合法 JSON：{err}"))?;
    let op = value
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| "帧缺少 `op`".to_string())?;
    let seq = value.get("seq").and_then(Value::as_u64).unwrap_or(0);

    // `v` 只在**更高**的主版本上拒绝：0/缺失按 1 处理，容忍手工调试帧。
    if let Some(v) = value.get("v").and_then(Value::as_u64) {
        if v > PROTOCOL_VERSION as u64 {
            return Err(format!("协议版本 {v} 高于本视口支持的 {PROTOCOL_VERSION}"));
        }
    }

    Ok(match op {
        "open" => Request::Open {
            path: value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            animation: value
                .get("animation")
                .and_then(Value::as_str)
                .map(str::to_string),
            play: value.get("play").and_then(Value::as_bool).unwrap_or(false),
            seq,
        },
        "close" => Request::Close { seq },
        "focus" => Request::Focus { seq },
        "ping" => Request::Ping { seq },
        "screenshot" => Request::Screenshot { seq },
        _ => Request::Unknown,
    })
}

/// 握手帧：`ready{version, capabilities}`，`seq=0`（单向）。
pub fn ready_frame() -> String {
    json!({
        "v": PROTOCOL_VERSION,
        "op": "ready",
        "seq": 0,
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": CAPABILITIES,
    })
    .to_string()
}

/// 状态帧：`state{path, animation, time, playing, duration}`，只在离散事件上发。
///
/// `duration` 是当前动画的总时长（没有动画时为 0），对端据此画时间轴刻度——
/// 让对端去猜时长只会画出比例永远不对的滑条。新增字段对老对端无影响（未知字段
/// 一律忽略，见文件头第 1 条）。
pub fn state_frame(
    path: &str,
    animation: Option<&str>,
    time: f32,
    playing: bool,
    duration: f32,
) -> String {
    json!({
        "v": PROTOCOL_VERSION,
        "op": "state",
        "seq": 0,
        "path": path,
        "animation": animation,
        "time": time,
        "playing": playing,
        "duration": duration,
    })
    .to_string()
}

/// 错误帧。`seq` 与请求对上，对端才能判断是哪条指令失败。
pub fn error_frame(seq: u64, code: &str, message: &str) -> String {
    json!({
        "v": PROTOCOL_VERSION,
        "op": "error",
        "seq": seq,
        "code": code,
        "message": message,
    })
    .to_string()
}

/// 心跳回执。
pub fn pong_frame(seq: u64) -> String {
    json!({
        "v": PROTOCOL_VERSION,
        "op": "pong",
        "seq": seq,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_with_defaults() {
        let req = parse(r#"{"v":1,"op":"open","path":"art/triangle.gltf","seq":0}"#).unwrap();
        assert_eq!(
            req,
            Request::Open {
                path: "art/triangle.gltf".to_string(),
                animation: None,
                play: false,
                seq: 0
            }
        );
    }

    #[test]
    fn parses_open_with_animation() {
        let req =
            parse(r#"{"op":"open","path":"a.glb","animation":"Run","play":true,"seq":7}"#).unwrap();
        assert_eq!(
            req,
            Request::Open {
                path: "a.glb".to_string(),
                animation: Some("Run".to_string()),
                play: true,
                seq: 7
            }
        );
    }

    #[test]
    fn ignores_unknown_op_and_fields() {
        assert_eq!(
            parse(r#"{"v":1,"op":"teleport","x":1,"seq":3}"#).unwrap(),
            Request::Unknown
        );
        // 已知 op 上的未知字段不能影响解析。
        assert!(matches!(
            parse(r#"{"op":"ping","extra":{"a":[1,2]},"seq":1}"#).unwrap(),
            Request::Ping { seq: 1 }
        ));
    }

    #[test]
    fn rejects_future_version_and_malformed() {
        assert!(parse(r#"{"v":2,"op":"ping","seq":1}"#).is_err());
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"seq":1}"#).is_err());
    }

    #[test]
    fn screenshot_stays_unsupported() {
        assert_eq!(
            parse(r#"{"op":"screenshot","path":"out.png","seq":2}"#).unwrap(),
            Request::Screenshot { seq: 2 }
        );
    }

    #[test]
    fn frames_round_trip() {
        let ready: Value = serde_json::from_str(&ready_frame()).unwrap();
        assert_eq!(ready["op"], "ready");
        assert_eq!(ready["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(ready["capabilities"][0], "static");
        // 第三刀的能力自述：对端据此决定要不要放播放控件。
        assert_eq!(ready["capabilities"][1], "animated");

        let pong: Value = serde_json::from_str(&pong_frame(9)).unwrap();
        assert_eq!(pong["seq"], 9);

        let err: Value = serde_json::from_str(&error_frame(4, "unsupported", "x")).unwrap();
        assert_eq!(err["code"], "unsupported");
        assert_eq!(err["seq"], 4);
    }

    #[test]
    fn state_frame_reports_duration() {
        let state: Value = serde_json::from_str(&state_frame("a.glb", Some("Run"), 0.5, true, 1.25))
            .unwrap();
        assert_eq!(state["animation"], "Run");
        assert_eq!(state["playing"], true);
        assert!((state["duration"].as_f64().unwrap_or_default() - 1.25).abs() < 1e-6);
        // 没有动画时也别省字段：对端拿 null 和拿 0 是两种分支。
        let idle: Value =
            serde_json::from_str(&state_frame("a.glb", None, 0.0, false, 0.0)).unwrap();
        assert_eq!(idle["duration"], 0.0);
    }
}
