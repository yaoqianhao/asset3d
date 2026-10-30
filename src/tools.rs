//! 插件的工具表与实现。
//!
//! 五个工具覆盖「读进来 → 存下去 → 看一眼 → 批量找 → 接进引擎」这条最短链路：
//!
//! | 工具 | 风险 | 作用 |
//! | --- | --- | --- |
//! | `inspect_asset` | read | 解析资产并输出 AIR 摘要，不写盘 |
//! | `export_asset` | write | 导出 `.air.bin` / `.air.json`，写完立刻回读校验 |
//! | `preview_asset` | write | 生成静态网格预览：薄 HTML + 若干数据分片（WebGL2，无蒙皮/动画播放） |
//! | `list_assets` | read | 递归找出工作区里的 3D 资产 |
//! | `export_engine_loader` | write | 把引擎侧加载器源码写到指定位置 |

use serde_json::{json, Value};

use crate::air::AirDoc;
use crate::rpc::{self, Host, HostError};
use crate::util::{self, Container};
use crate::view;

/// 工具执行失败。`code` 会被原样写进 JSON-RPC error。
///
/// 注意：**绝不能用 -32000**——那是宿主判「插件进程已退出」的哨兵码，
/// 用它报错会让一次普通的参数错误被记成崩溃。
pub struct ToolError {
    pub code: i64,
    pub message: String,
}

impl ToolError {
    pub fn param(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            code: 5001,
            message: message.into(),
        }
    }
}

impl From<HostError> for ToolError {
    fn from(err: HostError) -> Self {
        let mut message = format!("宿主调用失败：{}", err.message);
        if message.contains("工作区") || message.contains("workspace") {
            message.push_str("（插件只能访问当前打开的工作区：请把资产放进项目目录，或先切换工作区）");
        }
        // 5xxx 是插件自有域：`--standalone` 的「路径不对」靠它传出去，
        // 统一压成 5002 的话调用方就分不清「参数写错」和「文件不在」。
        let code = if (5000..6000).contains(&err.code) {
            err.code
        } else {
            5002
        };
        Self { code, message }
    }
}

/// 工具清单：握手与 `--manifest` 共用这一份，避免两处描述漂移。
pub fn catalog() -> Value {
    json!([
        {
            "name": "inspect_asset",
            "description": "解析工作区内的 .fbx / .glb / .gltf 三维资产，输出 AIR 摘要（网格、材质、贴图、骨骼、动画、包围盒）与导入期问题清单。只读，不写盘。",
            "risk": "read",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "资产路径：工作区相对路径或工作区内绝对路径"
                    },
                    "animations": {
                        "type": "boolean",
                        "description": "是否解析动画轨道，默认 true"
                    },
                    "embedTextures": {
                        "type": "boolean",
                        "description": "是否把贴图字节读进 AIR，默认 false（摘要里只报尺寸）"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "export_asset",
            "description": "把 .fbx / .glb / .gltf 导入成引擎内核可直接消费的 AIR：导出 .air.bin（AIRB 二进制，几何平铺可直灌顶点缓冲）或 .air.json（便于 diff 与手改）。写盘后立即回读校验。",
            "risk": "write",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "源资产路径"
                    },
                    "out": {
                        "type": "string",
                        "description": "输出路径；省略时写到源文件同目录、同名的 .air.bin / .air.json"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["bin", "json", "both"],
                        "description": "导出格式，默认 both"
                    },
                    "animations": {
                        "type": "boolean",
                        "description": "是否导出动画轨道，默认 true"
                    },
                    "embedTextures": {
                        "type": "boolean",
                        "description": "是否把贴图以 base64 内嵌进 AIR，默认 false"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "preview_asset",
            "description": "生成离线可打开的预览：薄 HTML（模板/索引/元数据）+ 若干 .shard.NN.js 数据分片，浏览器或 Electron 直接打开，无需服务器。只画静态网格与材质（轨道相机、节点层级世界矩阵、贴图、统计面板），不播放骨骼动画、不做蒙皮变形；分片需与 HTML 同目录。",
            "risk": "write",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "源资产路径"
                    },
                    "out": {
                        "type": "string",
                        "description": "HTML 输出路径；省略时写到源文件同目录、同名的 .preview.html"
                    },
                    "maxTriangles": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "预览三角形预算，超出时先整网格丢弃、只剩一个网格则截断索引，并在页面标注，默认 400000；0 表示不限制"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "list_assets",
            "description": "递归扫描工作区目录，列出所有 .fbx / .glb / .gltf 资产及体积，便于批量导入前先摸清家底。",
            "risk": "read",
            "parameters": {
                "type": "object",
                "properties": {
                    "dir": {
                        "type": "string",
                        "description": "起始目录（工作区相对），默认工作区根"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "是否递归子目录，默认 true"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "最多返回多少个文件，默认 200"
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "view_render",
            "description": "插件的面板视图入口（aiide.view/1）：把资产折成一棵声明式描述树（概览/材质/贴图/动画/骨骼/导入问题/动作按钮），交给 IDE 面板渲染。只读，不写盘，不解析动画轨道。省略 path 时列出工作区里可打开的资产。",
            "risk": "read",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "资产路径；省略时返回空态视图（含工作区资产清单）"
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "export_engine_loader",
            "description": "把引擎侧读取 AIR/AIRB 的 Rust 加载器源码写到指定路径（纯 std + serde，可直接放进 r3e-asset 一类的 crate 使用），并附带格式说明。",
            "risk": "write",
            "parameters": {
                "type": "object",
                "properties": {
                    "out": {
                        "type": "string",
                        "description": "目标 .rs 路径，例如 src/air_loader.rs"
                    },
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

/// 执行一个工具。
pub fn invoke(host: &Host, tool: &str, args: &Value) -> Result<Value, ToolError> {
    let text = match tool {
        "inspect_asset" => inspect(host, args)?,
        "export_asset" => export(host, args)?,
        "preview_asset" => preview(host, args)?,
        "list_assets" => list(host, args)?,
        "view_render" => view_render(host, args)?,
        "export_engine_loader" => engine_loader(host, args)?,
        other => {
            return Err(ToolError {
                code: -32601,
                message: format!("asset3d 没有名为 `{other}` 的工具"),
            })
        }
    };
    // 回字符串而不是对象：宿主会把非字符串回包再 JSON 序列化一次，
    // 换行会变成 `\n`、中文会变成转义，模型读起来反而更费劲。
    Ok(Value::String(text))
}

// ---------------------------------------------------------------------------
// 工具实现
// ---------------------------------------------------------------------------

fn inspect(host: &Host, args: &Value) -> Result<String, ToolError> {
    let path = require_path(args)?;
    let animations = bool_arg(args, "animations", true)?;
    let embed = bool_arg(args, "embedTextures", false)?;
    let (_, doc) = load(host, &path, animations, embed)?;

    let mut out = doc.summary_text();
    out.push_str(&format!(
        "\n估算体积  JSON 约 {} / AIRB 约 {}",
        util::format_bytes(doc.estimated_json_bytes()),
        util::format_bytes(estimate_binary_bytes(&doc))
    ));
    if embed {
        let carried = doc
            .textures
            .iter()
            .filter(|texture| texture.data_base64.is_some())
            .count();
        out.push_str(&format!("\n贴图内嵌  {carried}/{} 张", doc.textures.len()));
    }
    Ok(out)
}

fn export(host: &Host, args: &Value) -> Result<String, ToolError> {
    let path = require_path(args)?;
    let animations = bool_arg(args, "animations", true)?;
    let embed = bool_arg(args, "embedTextures", false)?;
    let format = string_arg(args, "format").unwrap_or_else(|| "both".to_string());
    if !matches!(format.as_str(), "bin" | "json" | "both") {
        return Err(ToolError::param(format!(
            "format 只能是 bin / json / both，收到 `{format}`"
        )));
    }

    let (rel, doc) = load(host, &path, animations, embed)?;
    // 阻断级问题一律不落盘：写出「能读但画不对」的资产，比直接报错难查一个数量级。
    if doc.has_errors() {
        return Err(ToolError::fail(format!(
            "{rel} 存在 {} 条阻断级问题，已拒绝写出 AIR（源文件或用 inspect_asset 先修）：\n{}",
            doc.errors.len(),
            doc.error_report()
        )));
    }
    host.progress("编码 AIR");
    let wanted_out = string_arg(args, "out");

    let mut lines = Vec::new();
    lines.push(format!("导出 AIR：{rel}"));

    if format == "bin" || format == "both" {
        let (target, renamed) = match &wanted_out {
            Some(raw) => force_extension(host, raw, ".air.bin"),
            None => (default_output(&rel, ".air.bin"), false),
        };
        // 编码 + 回读自检都在 `export_core` 里完成（与视口导出共用同一份实现）。
        let bytes = crate::export_core::encode_bin(&doc).map_err(ToolError::fail)?;
        let written = host.write_bytes(&target, &bytes)?;
        let created = written
            .get("created")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        lines.push(format!(
            "  [bin ] {target}｜{}｜{}｜回读一致（结构 + 抽样数值 + AABB 重算）：{} 网格 / {} 顶点 / {} 三角形{}",
            util::format_bytes(bytes.len() as u64),
            if created { "新建" } else { "覆盖" },
            doc.meshes.len(),
            doc.stats.vertex_count,
            doc.stats.triangle_count,
            if renamed { "｜out 后缀已按 format 规范为 .air.bin" } else { "" }
        ));
    }

    if format == "json" || format == "both" {
        let (target, renamed) = match &wanted_out {
            // json 只认 `.json`：按 format 归一后缀，避免 `format=json` 却写出 `.txt`。
            Some(raw) => force_extension(host, raw, ".json"),
            None => (default_output(&rel, ".air.json"), false),
        };
        let text = crate::export_core::encode_json(&doc).map_err(ToolError::fail)?;
        let written = host.write_text(&target, &text)?;
        let created = written
            .get("created")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        lines.push(format!(
            "  [json] {target}｜{}｜{}｜回读一致（结构 + 抽样数值 + AABB 重算）{}",
            util::format_bytes(text.len() as u64),
            if created { "新建" } else { "覆盖" },
            if renamed { "｜out 后缀已按 format 规范为 .json" } else { "" }
        ));
    }

    lines.push(format!(
        "统计：节点 {} / 网格 {} / 顶点 {} / 三角形 {} / 材质 {} / 贴图 {} / 骨骼 {} / 动画 {}",
        doc.stats.node_count,
        doc.stats.mesh_count,
        doc.stats.vertex_count,
        doc.stats.triangle_count,
        doc.stats.material_count,
        doc.stats.texture_count,
        doc.stats.skin_count,
        doc.stats.animation_count
    ));
    if !doc.textures.is_empty() && !embed {
        lines.push(
            "注意：贴图未内嵌（embedTextures=false），加载方需要自己按 source_path 找回贴图。"
                .to_string(),
        );
    }
    if !doc.issues.is_empty() {
        lines.push(format!("导入期注意 {} 条，详见 inspect_asset", doc.issues.len()));
    }
    lines.push(
        "引擎侧读取：先用导出 .air.bin（AIRB，几何可直接当顶点缓冲），需要手改再用 .air.json。"
            .to_string(),
    );
    Ok(lines.join("\n"))
}

fn preview(host: &Host, args: &Value) -> Result<String, ToolError> {
    let path = require_path(args)?;
    let max_triangles = u64_arg(args, "maxTriangles", 400_000)?;
    // 预览页只画静态网格，动画轨道没有任何消费方：关掉它，别让关键帧白占分片体积。
    let (rel, doc) = load(host, &path, false, true)?;
    host.progress("生成预览页（模板 + 数据分片）");

    let target = match string_arg(args, "out") {
        Some(raw) => host.normalize_path(&raw),
        None => default_output(&rel, ".preview.html"),
    };
    // 分片名比 HTML 名长几字节：按最宽的名字算宿主预算，保证每一片都过得去。
    let (dir, name) = split_dir_name(&target);
    let base = name
        .rsplit_once('.')
        .map(|(stem, _)| stem.to_string())
        .unwrap_or_else(|| name.clone());
    let widest = join_dir(&dir, &format!("{base}.shard.999.js"));
    let shard_budget = rpc::payload_budget(&widest, "utf8");

    let outcome = view::render(&doc, max_triangles, &base, shard_budget).map_err(ToolError::fail)?;

    // HTML 骨架（模板 + 分片索引 + script 标签）自己也走同一条单帧闸门。
    let html_budget = rpc::payload_budget(&target, "utf8");
    if outcome.html.len() > html_budget {
        return Err(ToolError::fail(format!(
            "预览骨架 {} 超过宿主单帧预算 {}：请把 out 指到更短的路径，或改用 export_asset",
            util::format_bytes(outcome.html.len() as u64),
            util::format_bytes(html_budget as u64)
        )));
    }

    // 先写分片、最后写 HTML：中途失败时，用户手上不会出现一个指向空目录的页面。
    let mut written: Vec<(String, usize)> = Vec::new();
    for (index, (shard_name, text)) in outcome.shards.iter().enumerate() {
        let rel_path = join_dir(&dir, shard_name);
        match host.write_text(&rel_path, text) {
            Ok(_) => written.push((rel_path, outcome.shard_bytes[index])),
            Err(err) => {
                let done = if written.is_empty() {
                    "尚未写入任何分片".to_string()
                } else {
                    format!(
                        "已写入 {} 个分片（{}）",
                        written.len(),
                        written
                            .iter()
                            .map(|(path, _)| path.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    )
                };
                return Err(ToolError::fail(format!(
                    "写数据分片 `{rel_path}` 失败：{}（{done}）",
                    err.message
                )));
            }
        }
    }

    let written_html = host.write_text(&target, &outcome.html)?;
    let created = written_html
        .get("created")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let shard_total: usize = outcome.shard_bytes.iter().sum();
    let shard_biggest = outcome.shard_bytes.iter().copied().max().unwrap_or(0);
    let mut lines = vec![format!(
        "预览已生成：{target}｜骨架 {}｜数据分片 {} 个（共 {}，单片最大 {}，均低于宿主单帧预算 {}）｜{}",
        util::format_bytes(outcome.html.len() as u64),
        outcome.shards.len(),
        util::format_bytes(shard_total as u64),
        util::format_bytes(shard_biggest as u64),
        util::format_bytes(shard_budget as u64),
        if created { "新建" } else { "覆盖" }
    )];
    lines.push("分片清单（必须与 HTML 放在同一目录，缺片页面会提示）：".to_string());
    for (rel_path, bytes) in &written {
        lines.push(format!(
            "  - {rel_path}（{}）",
            util::format_bytes(*bytes as u64)
        ));
    }

    // 预览的用途就是「看图找问题」，所以阻断级问题不拦写盘，但必须顶在最前面。
    if doc.has_errors() {
        lines.push(format!(
            "警告：{rel} 存在 {} 条阻断级问题，页面几何不可信，export_asset 会拒绝导出：\n{}",
            doc.errors.len(),
            doc.error_report()
        ));
    }
    lines.push(format!(
        "内容：{} 网格 / {} 三角形 / {} 贴图内嵌 / {} 贴图未内嵌（JSON 载荷 {}）",
        doc.meshes.len(),
        doc.meshes.iter().map(|m| m.indices.len() / 3).sum::<usize>(),
        outcome.embedded_textures,
        outcome.dropped_textures.len(),
        util::format_bytes(outcome.payload_bytes as u64)
    ));
    lines.push(
        "说明：预览只画静态网格与材质，蒙皮/骨骼动画不参与渲染（动画轨道未载入，避免白占分片体积）；需要动画数据请用 export_asset。"
            .to_string(),
    );
    if !outcome.dropped_textures.is_empty() {
        lines.push(format!(
            "  未内嵌的贴图（只是预览页不内嵌，资产里的贴图还在；预览里显示为材质 baseColor，通常是白色，要看贴图请用 GUI 视口）：{}",
            outcome.dropped_textures.join("、")
        ));
    }
    for note in &outcome.notes {
        lines.push(format!("  - {note}"));
    }
    if !outcome.skipped_meshes.is_empty() {
        lines.push(format!("  省略的网格：{}", outcome.skipped_meshes.join("、")));
    }
    if !outcome.truncated_meshes.is_empty() {
        lines.push(format!(
            "  截断的网格：{}",
            outcome.truncated_meshes.join("、")
        ));
    }
    lines.push("操作：左键旋转、右键平移、滚轮缩放、R 重置视角。".to_string());
    Ok(lines.join("\n"))
}

fn split_dir_name(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

fn join_dir(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn list(host: &Host, args: &Value) -> Result<String, ToolError> {
    let dir = string_arg(args, "dir").unwrap_or_default();
    let recursive = bool_arg(args, "recursive", true)?;
    let limit = u64_arg(args, "limit", 200)? as usize;
    let start = host.normalize_path(&dir);

    // 宿主配额是 200 次/调用：目录访问数必须硬顶住，否则深目录树会把配额吃光，
    // 最后报成「宿主调用失败」，反而看不出是扫太宽。
    const MAX_DIRS: usize = 160;
    let mut queue = vec![(start.clone(), 0usize)];
    let mut visited = 0usize;
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut truncated = false;
    let mut unreadable: Vec<String> = Vec::new();

    // 起点目录本身读不到就直接报错：静默回 0 个文件，用户会以为目录里真的没有资产。
    if let Err(err) = host.list(&start) {
        return Err(ToolError::fail(format!(
            "目录 `{}` 不可访问：{}（检查路径是否存在、是否在工作区内）",
            if start.is_empty() { "<根>" } else { &start },
            err.message
        )));
    }

    'scan: while let Some((current, depth)) = queue.pop() {
        if visited >= MAX_DIRS {
            truncated = true;
            break;
        }
        visited += 1;
        let entries = match host.list(&current) {
            Ok(entries) => entries,
            Err(err) => {
                // 子目录读不了不必让整次扫描失败（权限、竞态删除都常见），但绝不能装没看见：
                // 记下来在结果里明说，否则「少列了几个资产」会被当成「目录里没有」。
                eprintln!("asset3d: 跳过目录 `{current}`：{}", err.message);
                unreadable.push(current);
                continue;
            }
        };
        for (path, is_dir, size) in entries {
            let name = path.rsplit('/').next().unwrap_or(&path);
            if is_dir {
                if recursive && depth < 12 && !is_noise_dir(name) {
                    queue.push((path, depth + 1));
                }
                continue;
            }
            if !is_asset_name(name) {
                continue;
            }
            // 配额满了立刻停：继续遍历只会白烧宿主调用，还拿不到新结果。
            if files.len() >= limit {
                truncated = true;
                break 'scan;
            }
            files.push((path, size));
        }
    }

    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let mut out = String::new();
    out.push_str(&format!(
        "在工作区 `{}` 下找到 {} 个 3D 资产{}：\n",
        if start.is_empty() { "<根>" } else { &start },
        files.len(),
        if truncated { "（结果已截断）" } else { "" }
    ));
    let mut total = 0u64;
    for (path, size) in &files {
        total += *size;
        out.push_str(&format!(
            "  {path}｜{}｜{}\n",
            util::format_bytes(*size),
            if path.to_lowercase().ends_with(".fbx") {
                "FBX"
            } else {
                "glTF"
            }
        ));
    }
    out.push_str(&format!("合计 {}\n", util::format_bytes(total)));
    if files.is_empty() {
        out.push_str("没有找到 .fbx / .glb / .gltf。\n");
    }
    if !unreadable.is_empty() {
        out.push_str(&format!(
            "另有 {} 个目录无法读取（已跳过，结果不完整）：\n",
            unreadable.len()
        ));
        for dir in unreadable.iter().take(10) {
            out.push_str(&format!("  {dir}\n"));
        }
        if unreadable.len() > 10 {
            out.push_str("  ……（其余略）\n");
        }
    }
    Ok(out)
}

/// 最近一次视图渲染的解析结果。
///
/// 面板每收到一次刷新事件（`workspace.changed` / 手动刷新）就会调一次
/// `view.render`，而一次 96 MB 的 FBX 解析要几百毫秒到几秒。缓存键取
/// `(相对路径, 宿主报的文件字节数)`：资产没换就复用同一份 [`AirDoc`]，
/// 换了文件字节数自然对不上，立刻重解析。**只缓存一份**——多留几份能把
/// 内存吃到一个 G，而用户一次只看一个资产。
static VIEW_CACHE: std::sync::Mutex<Option<(String, u64, crate::air::AirDoc)>> =
    std::sync::Mutex::new(None);

/// 面板视图入口（`aiide.view/1`，见 docs/24 §4.2）。
///
/// 与 `inspect_asset` 的差别是**输出形态**而不是语义：同一个导入流程，
/// 但这里折成描述树，且固定 `animations=false`（面板只列动画名字与时长，
/// 解析轨道纯属白烧 CPU）。
fn view_render(host: &Host, args: &Value) -> Result<String, ToolError> {
    let raw = string_arg(args, "path").unwrap_or_default();
    let tree = if raw.trim().is_empty() {
        let assets = scan_assets_shallow(host, 100);
        crate::ui::render_empty(&assets, assets.len() >= 100)
    } else {
        let rel = host.normalize_path(&raw);
        if rel.is_empty() {
            return Err(ToolError::param("path 不能为空"));
        }
        let size = host.file_size(&rel).unwrap_or(0);
        let cached = {
            let guard = VIEW_CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
            match guard.as_ref() {
                Some((cached_rel, cached_size, doc)) if cached_rel == &rel && *cached_size == size => {
                    Some(doc.clone())
                }
                _ => None,
            }
        };
        let doc = match cached {
            Some(doc) => doc,
            None => {
                let (_, doc) = load(host, &rel, false, false)?;
                let mut guard = VIEW_CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
                *guard = Some((rel.clone(), size, doc.clone()));
                doc
            }
        };
        crate::ui::render_doc(&rel, &doc)
    };

    // 一定是字符串回包：宿主 `result_text` 对字符串原样透传，前端 `JSON.parse`
    // 一次就能拿到视图对象。回对象会被再序列化一层，前端要多剥一层壳。
    serde_json::to_string(&tree).map_err(|err| ToolError::fail(format!("视图序列化失败：{err}")))
}

/// 浅扫工作区取资产清单，只给空态视图用。
///
/// 深度限 3、目录限 40：空态是「打开面板第一眼」的画面，不能为了列全而把
/// 200 次宿主配额吃光（`list_assets` 才是给完整清单用的）。扫不到就返回空表，
/// 让面板显示「工作区里没有资产」，而不是报错。
fn scan_assets_shallow(host: &Host, limit: usize) -> Vec<(String, u64)> {
    const MAX_DIRS: usize = 40;
    let mut queue = vec![(String::new(), 0usize)];
    let mut visited = 0usize;
    let mut files: Vec<(String, u64)> = Vec::new();
    while let Some((current, depth)) = queue.pop() {
        if visited >= MAX_DIRS || files.len() >= limit {
            break;
        }
        visited += 1;
        let Ok(entries) = host.list(&current) else {
            continue;
        };
        for (path, is_dir, size) in entries {
            let name = path.rsplit('/').next().unwrap_or(&path);
            if is_dir {
                if depth < 2 && !is_noise_dir(name) {
                    queue.push((path, depth + 1));
                }
                continue;
            }
            if is_asset_name(name) {
                files.push((path, size));
                if files.len() >= limit {
                    break;
                }
            }
        }
    }
    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    files
}

fn engine_loader(host: &Host, args: &Value) -> Result<String, ToolError> {
    let out = require_path_named(args, "out")?;
    let overwrite = bool_arg(args, "overwrite", false)?;
    let target = host.normalize_path(&out);
    if target.is_empty() {
        return Err(ToolError::param("out 不能为空"));
    }
    // 覆盖已有源码是危险动作：不覆盖就明确报错，让调用方自己决定。
    if !overwrite && host.file_size(&target).is_ok() {
        return Err(ToolError::fail(format!(
            "{target} 已存在；确认要覆盖再传 overwrite=true"
        )));
    }
    // 旧实现只替换 __GENERATED_FROM__，剩下三个占位符原样写进用户源码，
    // 生成结果既不能编译也看不出数据版本；现在统一渲染并做残留兜底。
    let source = render_loader_template()?;
    let written = host.write_text(&target, &source)?;
    let created = written
        .get("created")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(format!(
        "引擎加载器已写入：{target}｜{}｜{}\n配套：放进 r3e-asset（或依赖它的 crate）即可编译；AIR 的字段与校验口径全部来自 r3e_asset::air_import，本模板不再复刻结构体，也不需要额外加 serde / serde_json。",
        util::format_bytes(source.len() as u64),
        if created { "新建" } else { "覆盖" }
    ))
}

// ---------------------------------------------------------------------------
// 引擎加载器模板渲染
// ---------------------------------------------------------------------------

/// 加载器源码渲染委托给 [`crate::export_core::render_loader`]：视口导出用的是
/// 同一份模板与占位符替换逻辑，这里只做错误类型转换。
fn render_loader_template() -> Result<String, ToolError> {
    crate::export_core::render_loader().map_err(ToolError::fail)
}

// ---------------------------------------------------------------------------
// 导入主流程
// ---------------------------------------------------------------------------

fn load(
    host: &Host,
    raw_path: &str,
    animations: bool,
    embed_textures: bool,
) -> Result<(String, AirDoc), ToolError> {
    let rel = host.normalize_path(raw_path);
    if rel.is_empty() {
        return Err(ToolError::param("path 不能为空"));
    }
    host.progress(&format!("读取 {rel}"));
    let bytes = host.read_bytes(&rel)?;
    if bytes.is_empty() {
        return Err(ToolError::fail(format!("{rel} 是空文件")));
    }
    let container = util::sniff_container(&rel, &bytes).map_err(ToolError::fail)?;
    host.progress(&format!("解析 {}", container.label()));

    let mut doc = AirDoc::new(container.as_str(), &rel);
    match container {
        Container::FbxBinary | Container::FbxAscii => {
            crate::fbx::import(&mut doc, &bytes, &rel, animations, embed_textures)
                .map_err(ToolError::fail)?;
        }
        Container::Glb | Container::Gltf => {
            // glTF 的外部 buffer / 贴图是相对源文件目录的；插件自己没有磁盘权限，
            // 一律经宿主的 fs.read 读，读不到就让它留在 issues 里（不阻断导入）。
            let mut fetch = |path: &str| -> Option<Vec<u8>> {
                let normalized = host.normalize_path(path);
                host.read_bytes(&normalized).ok()
            };
            crate::gltf::import(
                &mut doc,
                &bytes,
                &rel,
                animations,
                embed_textures,
                &mut fetch,
            )
            .map_err(ToolError::fail)?;
        }
    }
    doc.finalize();
    Ok((rel, doc))
}

/// 回读自检、编码、加载器模板渲染全部在 [`crate::export_core`] 里实现：
/// 插件工具与视口（asset3d-gui 的导出按键）共用同一份代码，格式只定义一次。


pub fn estimate_binary_bytes(doc: &AirDoc) -> u64 {
    // 逐项对齐 air_bin 的真实落盘布局：容器头 + 每网格的几何段 + 每张内嵌贴图
    // + 头部 JSON（节点/材质/蒙皮/动画）。少算的部分最后都会变成「预估够、实际写不下」。
    let mut total = 0u64;
    // AIRB 容器头：magic(4) + version(4) + meta_len(4) + blob_len(4) + 8 字节对齐余量。
    total += 24;

    for mesh in &doc.meshes {
        total += mesh.positions.len() as u64 * 12;
        total += mesh.normals.len() as u64 * 12;
        total += mesh.uvs.len() as u64 * 8;
        total += mesh.indices.len() as u64 * 4;
        total += mesh.joints.len() as u64 * 8;
        total += mesh.weights.len() as u64 * 16;
    }

    // 内嵌贴图按 base64 原样进 JSON 头，长度就是 data_base64 的字节数。
    for texture in &doc.textures {
        if let Some(data) = &texture.data_base64 {
            total += data.len() as u64;
        }
    }

    // 每个网格在 JSON 头里有一段固定描述（名字、各属性字节偏移/数量、AABB、材质分段）。
    total += doc.meshes.len() as u64 * 256;
    // 每个节点：名字 + 变换（T/R/S 各 3 个 f32 的文本形式）+ parent/mesh 引用。
    total += doc.nodes.len() as u64 * 192;
    // 每个材质：名字、baseColor、metallic/roughness、各贴图槽引用。
    total += doc.materials.len() as u64 * 128;

    // 蒙皮：每关节 64 字节的逆绑定矩阵（IBM，4x4 f32 = 16 * 4）。
    // 之前完全漏算，骨骼稍多的资产一导出就会突破预算。
    for skin in &doc.skins {
        total += skin.joints.len() as u64 * 64;
        total += skin.joints.len() as u64 * 4; // 关节索引与其名字引用
        total += 64; // 蒙皮自身描述（名字、skeleton 根、IBM 段偏移）
    }

    // 动画：每轨道的采样时间（f32）+ 值（vec4 按 16 字节），另加每轨道/每动画的头部。
    for clip in &doc.animations {
        total += 96; // 动画名、时长、轨道数
        for track in &clip.tracks {
            total += 128; // 轨道目标节点/路径/插值方式
            total += track.times.len() as u64 * 4;
            total += track.values.len() as u64 * 16;
        }
    }

    // 统计与包围盒等零碎字段。
    total + 2048
}

// ---------------------------------------------------------------------------
// 参数与路径
// ---------------------------------------------------------------------------

fn require_path(args: &Value) -> Result<String, ToolError> {
    require_path_named(args, "path")
}

fn require_path_named(args: &Value, key: &str) -> Result<String, ToolError> {
    let value = args.get(key).and_then(Value::as_str).unwrap_or("").trim();
    if value.is_empty() {
        return Err(ToolError::param(format!("缺少参数 `{key}`")));
    }
    Ok(value.to_string())
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        // 模型偶尔会把布尔写成 "true"/"false"：宽容一点，别为这个报错。
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            other => Err(ToolError::param(format!(
                "参数 `{key}` 需要布尔值，收到 `{other}`"
            ))),
        },
        Some(other) => Err(ToolError::param(format!(
            "参数 `{key}` 需要布尔值，收到 {other}"
        ))),
    }
}

fn u64_arg(args: &Value, key: &str, default: u64) -> Result<u64, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => Ok(number.as_u64().unwrap_or(default)),
        Some(Value::String(text)) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| ToolError::param(format!("参数 `{key}` 需要整数，收到 `{text}`"))),
        Some(other) => Err(ToolError::param(format!(
            "参数 `{key}` 需要整数，收到 {other}"
        ))),
    }
}

fn default_output(rel: &str, suffix: &str) -> String {
    let (dir, name) = match rel.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", rel),
    };
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    if dir.is_empty() {
        format!("{stem}{suffix}")
    } else {
        format!("{dir}/{stem}{suffix}")
    }
}

/// 把 `model.air.json` / `model.preview.html` 这类「本工具产生的双扩展名」
/// 换成另一种产物后缀，避免出现 `model.air.air.bin`。
fn replace_extension(raw: &str, suffix: &str) -> String {
    let normalized = raw.trim().replace('\\', "/");
    let (dir, name) = match normalized.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), normalized.clone()),
    };
    let stripped = name
        .strip_suffix(".air.json")
        .or_else(|| name.strip_suffix(".air.bin"))
        .or_else(|| name.strip_suffix(".preview.html"))
        .or_else(|| name.rsplit_once('.').map(|(stem, _)| stem))
        .unwrap_or(&name);
    if dir.is_empty() {
        format!("{stripped}{suffix}")
    } else {
        format!("{dir}/{stripped}{suffix}")
    }
}

/// 按 format 强制产物扩展名：`out` 写错后缀时改写成规范后缀，并告诉调用方改过。
///
/// 旧实现里 `out` 给什么就写什么，`format=bin` + `out=a.json` 会写出一个后缀是
/// `.json`、内容是 AIRB 二进制包的文件，引擎按后缀分流时会直接读坏。
/// 返回 `(最终路径, 是否改过后缀)`。
fn force_extension(host: &Host, raw: &str, suffix: &str) -> (String, bool) {
    let path = host.normalize_path(raw);
    if path.to_ascii_lowercase().ends_with(suffix) {
        return (path, false);
    }
    let candidate = replace_extension(&path, suffix);
    // `out=m.json.txt` 这种多级后缀：换掉末段会留下 `m.json.json`，收掉多出来的一层，
    // 保证「后缀说了算、又不叠罗汉」。
    let doubled = format!("{suffix}{suffix}");
    if candidate.to_ascii_lowercase().ends_with(&doubled) {
        let trimmed = candidate.len() - suffix.len();
        return (candidate[..trimmed].to_string(), true);
    }
    (candidate, true)
}

fn is_asset_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".fbx") || lower.ends_with(".glb") || lower.ends_with(".gltf")
}

fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | "target" | ".git" | "dist" | "build" | ".next" | "vendor"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::air_bin;
    use crate::export_core::{civil_from_unix_days, find_placeholder, LOADER_PLACEHOLDERS};

    /// 缺陷⑥：估算必须逐项等于落盘布局，而不是"差不多"。
    ///
    /// 用同一份空文档做基线，分别加上蒙皮/动画，断言差值**正好**等于
    /// 64B/关节 IBM + 4B/关节索引 + 64B 蒙皮头、96B/动画 + 128B/轨道
    /// + 4B/时间点 + 16B/关键值。差值而不是绝对值，是为了不让 2048 的
    /// 零碎余量掩盖某一项漏算。
    #[test]
    fn estimate_counts_skin_and_animation_overhead() {
        let base = AirDoc::new("glb", "a.glb");
        let baseline = estimate_binary_bytes(&base);

        let mut skinned = base.clone();
        skinned.skins.push(crate::air::AirSkin {
            index: 0,
            name: "Rig".to_string(),
            joints: vec![1, 2, 3],
            inverse_bind_matrices: vec![[0.0; 16]; 3],
        });
        let skin_delta = estimate_binary_bytes(&skinned) - baseline;
        assert_eq!(
            skin_delta,
            3 * 64 + 3 * 4 + 64,
            "每个关节应计 64B IBM + 4B 索引，蒙皮自身再算 64B，实际差 {skin_delta}"
        );

        let mut animated = base.clone();
        animated.animations.push(crate::air::AirAnimation {
            name: "Walk".to_string(),
            duration: 1.0,
            tracks: vec![crate::air::AirTrack {
                node: 1,
                property: "rotation".to_string(),
                interpolation: crate::air::INTERPOLATION_LINEAR.to_string(),
                times: vec![0.0; 5],
                values: vec![[0.0, 0.0, 0.0, 1.0]; 4],
            }],
        });
        let anim_delta = estimate_binary_bytes(&animated) - baseline;
        assert_eq!(
            anim_delta,
            96 + 128 + 5 * 4 + 4 * 16,
            "动画头 96B + 轨道头 128B + 4B/时间点 + 16B/关键值，实际差 {anim_delta}"
        );
    }

    #[test]
    fn default_output_keeps_directory() {
        assert_eq!(default_output("a/b/c.glb", ".air.bin"), "a/b/c.air.bin");
        assert_eq!(default_output("c.glb", ".air.json"), "c.air.json");
        assert_eq!(default_output("dir\\c.glb", ".air.bin"), "dir\\c.air.bin");
    }

    #[test]
    fn replace_extension_swaps_suffix() {
        assert_eq!(
            replace_extension("out/model.air.json", ".air.bin"),
            "out/model.air.bin"
        );
        assert_eq!(
            replace_extension("out/model.air.bin", ".air.json"),
            "out/model.air.json"
        );
        assert_eq!(replace_extension("model", ".air.bin"), "model.air.bin");
    }

    #[test]
    fn asset_names_are_case_insensitive() {
        assert!(is_asset_name("A.FBX"));
        assert!(is_asset_name("b.Glb"));
        assert!(!is_asset_name("c.png"));
    }

    /// `ToolError` 刻意没实现 `Debug`（工具错误只走结构化回执），测试里手动解包。
    fn rendered_template() -> String {
        match render_loader_template() {
            Ok(source) => source,
            Err(_) => panic!("模板应能渲染，但 render_loader_template 返回了错误"),
        }
    }

    /// 回归：旧版只替换 __GENERATED_FROM__，其余三个占位符原样落盘。
    #[test]
    fn rendered_loader_leaves_no_placeholder() {
        let source = rendered_template();
        assert!(
            find_placeholder(&source).is_none(),
            "渲染后仍有残留占位符：{:?}",
            find_placeholder(&source)
        );
        for token in LOADER_PLACEHOLDERS {
            assert!(!source.contains(token), "残留占位符 {token}");
        }
    }

    #[test]
    fn rendered_loader_carries_versions_and_timestamp() {
        let source = rendered_template();
        assert!(source.contains(&format!("AIR v{}", crate::air::AIR_VERSION)));
        assert!(source.contains(&format!("AIRB 容器 v{}", air_bin::VERSION)));
        // 生成时间必须是 2026 起步的 UTC 时间戳，而不是占位符。
        assert!(source.contains("UTC"), "缺少生成时间");
        assert!(
            source.contains("20"),
            "生成时间应是四位年份，实际内容未包含 20xx"
        );
        // 口径一致：模板必须引用引擎实现，而不是自己复刻结构体。
        assert!(source.contains("r3e_asset::air_import"));
    }

    #[test]
    fn placeholder_guard_only_flags_upper_snake_tokens() {
        assert_eq!(
            find_placeholder("//! 生成时间：__GENERATED_AT__"),
            Some("__GENERATED_AT__".to_string())
        );
        assert_eq!(
            find_placeholder("let x = a__B__b;"),
            Some("__B__".to_string())
        );
        // 普通文本与正常标识符不该被误判。
        assert_eq!(find_placeholder("fn snake_case_name() {}"), None);
        assert_eq!(find_placeholder("AIR_SUFFIXES 与 __ 分隔符"), None);
        assert_eq!(find_placeholder("trailing __"), None);
        assert_eq!(find_placeholder("__lower__"), None);
    }

    #[test]
    fn civil_dates_match_known_timestamps() {
        assert_eq!(civil_from_unix_days(0), (1970, 1, 1));
        assert_eq!(civil_from_unix_days(19_000), (2022, 1, 8));
        // 2026-10-26 00:00:00 UTC
        assert_eq!(civil_from_unix_days(20_752), (2026, 10, 26));
        // 闰日附近：2024-02-29
        assert_eq!(civil_from_unix_days(19_782), (2024, 2, 29));
    }
}
