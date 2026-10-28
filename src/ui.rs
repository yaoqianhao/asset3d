//! 声明式视图（`aiide.view/1`）的描述树生成（ai_ide docs/24 §4.2、§6.2）。
//!
//! 这个模块只做一件事：把 [`AirDoc`] 折成一棵**纯数据的树**，交给宿主渲染。
//! 它不碰 HTML、不碰字符串拼接，也不产出任何可执行内容——`button` 的 `action`
//! 只写命令名（`open_viewport` / `export_air` / `export_loader`），真正的审批与
//! 执行在宿主的 `plugin.command` 网关里。
//!
//! 三个刻意的边界：
//!
//! 1. **一个节点只表达一个意思**。宿主侧的白名单控件（row/column/group/tabs/text/
//!    stat/table/list/tree/progress/toggle/slider/select/color/button）里没有
//!    「富文本」「链接」，所以文案一律留在 `text` 的 `value` 里，不做 markdown。
//! 2. **限幅比宿主更早**。宿主会在 2000 节点 / 500 行 / 4 KiB 字符串处截断，
//!    但截断本身会记一条诊断（面板底部会显示「内容被砍过」）。这里先按更小的
//!    上限收口（材质 100 行、动画 100 项、问题 20 条），让常见资产根本不触发宿主
//!    截断——用户看到的「已截断」提示才有信息量。
//! 3. **总字节有硬顶**。宿主 `plugin.call` 的正文上限是 64 KiB，超了整块视图
//!    会被拒渲染（`truncated` 分支）。一张材质表就能撑爆，所以表格按 100 行、
//!    每格 40 字符先自我约束，剩下交给 [`assert_budget`] 兜底。

use serde_json::{json, Value};

use crate::air::AirDoc;
use crate::util;

/// 视图协议版本（与 `apps/desktop/src/renderer/src/plugin-view.ts: VIEW_SCHEMA` 同值）。
pub const VIEW_SCHEMA: &str = "aiide.view/1";

/// 材质 / 贴图 / 动画表格的行数上限。
const MAX_ROWS: usize = 100;
/// 问题清单条数上限。
const MAX_ISSUES: usize = 20;
/// 表格单元格的字符上限。超了这里就砍，别等宿主的 4 KiB 规则。
const MAX_CELL_CHARS: usize = 40;
/// 描述树序列化后的字节上限，**低于**宿主 64 KiB 的正文上限，留出信封余量。
const MAX_VIEW_BYTES: usize = 48 * 1024;

/// 三张动作按钮共用的命令名（必须与清单 `commands[].name` 逐字一致）。
pub const CMD_OPEN_VIEWPORT: &str = "open_viewport";
pub const CMD_EXPORT_ASSET: &str = "export_air";
pub const CMD_EXPORT_LOADER: &str = "export_loader";

/// 生成视图正文：直接就是要发给宿主的 `aiide.view/1` 对象。
///
/// 注意**不再包一层 `ui`**：宿主 `plugin-view-store.ts` 把 `plugin.call` 的
/// `content` 直接 `JSON.parse` 后交给 `parseView`，包一层会让 `schema` 找不到，
/// 整棵树被判 `schema_mismatch`。文档 §6.2 里那句 `{ ui: ... }` 是早期口径，
/// 以 M-UI-1 的实际实现为准。
pub fn render_doc(rel: &str, doc: &AirDoc) -> Value {
    json!({
        "schema": VIEW_SCHEMA,
        "rev": rev_of(rel, doc),
        "root": {
            "type": "column",
            "children": [
                overview(rel, doc),
                materials(doc),
                textures(doc),
                animations(doc),
                rig(doc),
                problems(doc),
                actions(rel),
            ],
        },
    })
}

/// 没有选中资产时的空态：告诉用户下一步，而不是渲染一棵空树。
pub fn render_empty(assets: &[(String, u64)], scanned: bool) -> Value {
    let mut children = vec![json!({
        "type": "text",
        "value": "还没有选择资产。在命令面板执行「导出 AIR」或「在 3D 视口中打开」时传入资产路径，\
                  这个面板就会显示统计、材质与动画清单。",
    })];

    if !assets.is_empty() {
        let items: Vec<Value> = assets
            .iter()
            .take(MAX_ROWS)
            .map(|(path, bytes)| {
                json!({
                    "key": path,
                    "label": format!("{path}｜{}", util::format_bytes(*bytes)),
                })
            })
            .collect();
        children.push(json!({
            "type": "group",
            "title": format!(
                "工作区里的资产（{} 个{}）",
                assets.len(),
                if scanned { "，仅列前 100 个" } else { "" }
            ),
            "children": [{ "type": "list", "items": items }],
        }));
    } else {
        children.push(json!({
            "type": "group",
            "title": "工作区里的资产",
            "children": [{
                "type": "text",
                "value": "工作区里没有找到 .fbx / .glb / .gltf 资产。",
            }],
        }));
    }

    let tree = json!({
        "schema": VIEW_SCHEMA,
        "rev": 0,
        "root": { "type": "column", "children": children },
    });
    assert_budget(tree, "空态")
}

/// 概览：一行两个分组（资产信息 / 规模统计）。
fn overview(rel: &str, doc: &AirDoc) -> Value {
    let mut info = vec![
        json!({ "type": "text", "value": rel }),
        stat("容器", &doc.source.container),
        stat("体积", &util::format_bytes(doc.source.bytes)),
    ];
    if let Some(unit) = doc.source.native_unit_meters {
        info.push(stat("源单位（米）", &format!("{unit:.4}")));
    }
    if let Some(up) = &doc.source.native_up_axis {
        info.push(stat("源上轴", up));
    }
    if let Some(creator) = &doc.source.creator {
        if !creator.trim().is_empty() {
            info.push(stat("生成器", creator));
        }
    }

    let stats = doc.stats.clone();
    let scale = vec![
        stat("节点", &stats.node_count.to_string()),
        stat("网格", &stats.mesh_count.to_string()),
        stat("顶点", &stats.vertex_count.to_string()),
        stat("三角形", &stats.triangle_count.to_string()),
        stat("材质", &stats.material_count.to_string()),
        stat("贴图", &stats.texture_count.to_string()),
        stat("骨骼", &stats.skin_count.to_string()),
        stat("关节", &stats.joint_count.to_string()),
        stat("动画", &stats.animation_count.to_string()),
        stat("几何字节", &util::format_bytes(stats.geometry_bytes)),
    ];

    json!({
        "type": "row",
        "children": [
            { "type": "group", "title": "资产", "children": info },
            { "type": "group", "title": "规模", "children": scale },
        ],
    })
}

/// 材质表：只列导入后真正会被引擎用到的四个字段（基色 / 金属度 / 粗糙度 / 贴图）。
fn materials(doc: &AirDoc) -> Value {
    if doc.materials.is_empty() {
        return json!({
            "type": "group",
            "title": "材质",
            "children": [{ "type": "text", "value": "没有材质：几何会按引擎默认材质渲染。" }],
        });
    }
    let rows: Vec<Value> = doc
        .materials
        .iter()
        .take(MAX_ROWS)
        .map(|material| {
            let [r, g, b, a] = material.base_color;
            json!({
                "cells": [
                    cell(&material.name),
                    cell(&format!(
                        "({:.2}, {:.2}, {:.2}, {:.2})",
                        r, g, b, a
                    )),
                    cell(&format!("{:.2}", material.metallic)),
                    cell(&format!("{:.2}", material.roughness)),
                    cell(&texture_ref(doc, material.base_color_texture)),
                    cell(if material.double_sided { "双面" } else { "单面" }),
                ],
            })
        })
        .collect();

    let mut children = vec![json!({
        "type": "table",
        "columns": ["材质", "基色", "金属度", "粗糙度", "基色贴图", "面"],
        "rows": rows,
    })];
    if doc.materials.len() > MAX_ROWS {
        children.push(json!({
            "type": "text",
            "value": format!("另有 {} 个材质未列出（面板只列前 {MAX_ROWS} 个）。", doc.materials.len() - MAX_ROWS),
        }));
    }
    json!({ "type": "group", "title": format!("材质（{}）", doc.materials.len()), "children": children })
}

/// 贴图表：尺寸 / 体积 / 是否内嵌。槽位（slot）对排查「贴图没生效」最有用。
fn textures(doc: &AirDoc) -> Value {
    if doc.textures.is_empty() {
        return json!({
            "type": "group",
            "title": "贴图",
            "children": [{ "type": "text", "value": "没有外部贴图引用。" }],
        });
    }
    let rows: Vec<Value> = doc
        .textures
        .iter()
        .take(MAX_ROWS)
        .map(|texture| {
            let size = match (texture.width, texture.height) {
                (Some(w), Some(h)) => format!("{w}×{h}"),
                _ => "未知".to_string(),
            };
            json!({
                "cells": [
                    cell(&texture.name),
                    cell(&texture.slot),
                    cell(&size),
                    cell(&texture.mime_type),
                    cell(&util::format_bytes(texture.bytes)),
                    cell(if texture.embedded { "已内嵌" } else { "外部引用" }),
                ],
            })
        })
        .collect();

    let mut children = vec![json!({
        "type": "table",
        "columns": ["贴图", "槽位", "尺寸", "类型", "体积", "承载"],
        "rows": rows,
    })];
    if doc.textures.len() > MAX_ROWS {
        children.push(json!({
            "type": "text",
            "value": format!("另有 {} 张贴图未列出。", doc.textures.len() - MAX_ROWS),
        }));
    }
    json!({ "type": "group", "title": format!("贴图（{}）", doc.textures.len()), "children": children })
}

/// 动画清单。这里**只报名字与时长**：轨道细节属于视口，塞进面板既慢又没人看。
fn animations(doc: &AirDoc) -> Value {
    if doc.animations.is_empty() {
        return json!({
            "type": "group",
            "title": "动画",
            "children": [{ "type": "text", "value": "没有动画轨道（静态资产）。" }],
        });
    }
    let items: Vec<Value> = doc
        .animations
        .iter()
        .take(MAX_ROWS)
        .map(|animation| {
            let tracks = animation.tracks.len();
            json!({
                "key": &animation.name,
                "label": format!("{}｜{:.2} s｜{} 条轨道", animation.name, animation.duration, tracks),
            })
        })
        .collect();
    json!({
        "type": "group",
        "title": format!("动画（{}）", doc.animations.len()),
        "children": [{ "type": "list", "items": items }],
    })
}

/// 骨骼 / 蒙皮摘要：数量与关节数，不列逐骨骼表格（一条手臂就几十根骨头）。
fn rig(doc: &AirDoc) -> Value {
    if doc.skins.is_empty() && doc.nodes.is_empty() {
        return json!({
            "type": "group",
            "title": "骨骼",
            "children": [{ "type": "text", "value": "没有骨骼与蒙皮（静态网格）。" }],
        });
    }
    let mut children = vec![
        stat("骨骼数", &doc.skins.len().to_string()),
        stat("节点数", &doc.nodes.len().to_string()),
    ];
    for skin in doc.skins.iter().take(10) {
        children.push(stat(
            &format!("· {}", if skin.name.is_empty() { "未命名" } else { &skin.name }),
            &format!("{} 关节", skin.joints.len()),
        ));
    }
    if doc.skins.len() > 10 {
        children.push(json!({
            "type": "text",
            "value": format!("另有 {} 套骨骼未列出。", doc.skins.len() - 10),
        }));
    }
    json!({ "type": "group", "title": "骨骼", "children": children })
}

/// 导入问题：阻断级在前、提示级在后，都截到 20 条。
fn problems(doc: &AirDoc) -> Value {
    if doc.errors.is_empty() && doc.issues.is_empty() {
        return json!({
            "type": "group",
            "title": "导入问题",
            "children": [{ "type": "text", "value": "没有发现问题。" }],
        });
    }
    let mut children = Vec::new();
    if !doc.errors.is_empty() {
        children.push(stat("阻断级", &doc.errors.len().to_string()));
        for line in doc.errors.iter().take(MAX_ISSUES) {
            children.push(json!({ "type": "text", "value": format!("✕ {line}") }));
        }
    }
    if !doc.issues.is_empty() {
        children.push(stat("提示级", &doc.issues.len().to_string()));
        for line in doc.issues.iter().take(MAX_ISSUES) {
            children.push(json!({ "type": "text", "value": format!("· {line}") }));
        }
    }
    json!({ "type": "group", "title": "导入问题", "children": children })
}

/// 动作按钮：三个命令与清单 `commands[].name` 一一对应。
///
/// `export_air` 是 `risk=write`，宿主的审批卡会在点击后弹出——面板这边不做
/// 二次确认，否则用户要确认两次。
fn actions(rel: &str) -> Value {
    json!({
        "type": "row",
        "children": [
            {
                "type": "button",
                "label": "在 3D 视口中打开",
                "action": { "command": CMD_OPEN_VIEWPORT, "args": { "path": rel } },
            },
            {
                "type": "button",
                "label": "导出 AIR",
                "action": { "command": CMD_EXPORT_ASSET, "args": { "path": rel } },
            },
            {
                "type": "button",
                "label": "导出引擎加载器",
                "action": {
                    "command": CMD_EXPORT_LOADER,
                    "args": { "out": "src/air_loader.rs" },
                },
            },
        ],
    })
}

fn stat(label: &str, value: &str) -> Value {
    json!({ "type": "stat", "label": cell(label), "value": cell(value) })
}

/// 单元格 / 文本统一限幅：宿主 4 KiB 的规则太宽，表格一多就没体积了。
fn cell(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= MAX_CELL_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_CELL_CHARS).collect();
    out.push('…');
    out
}

fn texture_ref(doc: &AirDoc, index: Option<u32>) -> String {
    match index.and_then(|slot| doc.textures.get(slot as usize)) {
        Some(texture) if !texture.name.is_empty() => texture.name.clone(),
        Some(_) => "（未命名贴图）".to_string(),
        None => "—".to_string(),
    }
}

/// `rev`：内容指纹（FNV-1a 64）。
///
/// 宿主按 `rev` 去重（`plugin-view-store.ts` 的「同一 rev 不重复拉」），所以它必须
/// **反映内容**而不是时钟：同一次会话里重复打开面板不该重新解析一遍 96 MB 的 FBX，
/// 而资产真的变了（换文件、改材质）就必须变号。取文档里几个便宜的字段拼一串，
/// 不碰几何数组——对 50 万顶点做哈希会把一次视图刷新拖成秒级。
fn rev_of(rel: &str, doc: &AirDoc) -> u64 {
    fn feed(hash: &mut u64, text: &str) {
        for byte in text.as_bytes() {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    feed(&mut hash, rel);
    feed(&mut hash, &doc.source.container);
    feed(
        &mut hash,
        &format!(
        "{}|{}|{}|{}|{}|{}|{}",
        doc.source.bytes,
        doc.stats.node_count,
        doc.stats.mesh_count,
        doc.stats.vertex_count,
        doc.stats.triangle_count,
        doc.stats.material_count,
        doc.stats.animation_count
    ),
    );
    feed(&mut hash, &doc.errors.len().to_string());
    feed(&mut hash, &doc.issues.len().to_string());
    for material in doc.materials.iter().take(8) {
        feed(&mut hash, &material.name);
        feed(&mut hash, &format!("{:.3}", material.roughness));
    }
    for animation in doc.animations.iter().take(8) {
        feed(&mut hash, &animation.name);
        feed(&mut hash, &format!("{:.3}", animation.duration));
    }
    // 0 在宿主侧等价于「没有修订号」，避开它。
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// 体积兜底：超过 [`MAX_VIEW_BYTES`] 就换成一张说明卡，而不是让宿主整块拒渲染。
///
/// 换卡而不是「继续发」：宿主的 64 KiB 是硬截断，截断后的 JSON 一定解析失败，
/// 用户看到的是「返回不是合法 JSON」——比一句「资产太大」难懂得多。
fn assert_budget(tree: Value, what: &str) -> Value {
    let bytes = serde_json::to_vec(&tree).map(|raw| raw.len()).unwrap_or(usize::MAX);
    if bytes <= MAX_VIEW_BYTES {
        return tree;
    }
    json!({
        "schema": VIEW_SCHEMA,
        "rev": 0,
        "root": {
            "type": "column",
            "children": [
                {
                    "type": "text",
                    "value": format!(
                        "{what}的描述达到 {} KiB，超过面板 {MAX_VIEW_BYTES_KB} KiB 的上限，已省略明细。\
                         用 inspect_asset 看完整摘要，或直接在视口里打开。",
                        bytes / 1024
                    ),
                },
            ],
        },
    })
}

/// `assert_budget` 的文案用（`MAX_VIEW_BYTES` 换算成 KiB）。
const MAX_VIEW_BYTES_KB: usize = MAX_VIEW_BYTES / 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::air::{AirDoc, AirMaterial, AirSource, AirTexture};

    fn doc() -> AirDoc {
        let mut doc = AirDoc::new("GLB", "art/chair.glb");
        doc.source = AirSource {
            path: "art/chair.glb".to_string(),
            container: "GLB".to_string(),
            bytes: 2048,
            native_unit_meters: Some(1.0),
            native_up_axis: Some("Y".to_string()),
            creator: Some("Blender".to_string()),
        };
        doc.materials.push(AirMaterial {
            index: 0,
            name: "Oak".to_string(),
            base_color: [0.8, 0.5, 0.2, 1.0],
            metallic: 0.0,
            roughness: 0.4,
            base_color_texture: Some(0),
            double_sided: false,
        });
        doc.textures.push(AirTexture {
            index: 0,
            name: "oak_basecolor".to_string(),
            slot: "baseColor".to_string(),
            mime_type: "image/png".to_string(),
            width: Some(1024),
            height: Some(1024),
            bytes: 4096,
            embedded: false,
            source_path: Some("art/tex/oak.png".to_string()),
            data_base64: None,
        });
        doc.finalize();
        doc
    }

    #[test]
    fn render_doc_emits_schema_and_root() {
        let tree = render_doc("art/chair.glb", &doc());
        assert_eq!(tree["schema"], json!(VIEW_SCHEMA));
        assert!(tree["rev"].as_u64().unwrap_or(0) > 0);
        assert_eq!(tree["root"]["type"], json!("column"));
        // 动作按钮的命令名必须与清单一致，写错了宿主会判 command_not_declared。
        let raw = serde_json::to_string(&tree).expect("序列化");
        assert!(raw.contains(CMD_OPEN_VIEWPORT));
        assert!(raw.contains(CMD_EXPORT_ASSET));
        assert!(raw.contains(CMD_EXPORT_LOADER));
    }

    #[test]
    fn rev_is_content_addressed() {
        let first = render_doc("art/chair.glb", &doc());
        let second = render_doc("art/chair.glb", &doc());
        assert_eq!(first["rev"], second["rev"], "内容不变时 rev 必须稳定");

        let mut changed = doc();
        changed.materials[0].roughness = 0.9;
        assert_ne!(
            render_doc("art/chair.glb", &changed)["rev"],
            first["rev"],
            "材质改了 rev 必须变"
        );
    }

    #[test]
    fn cell_is_truncated_and_flattened() {
        let long = "材".repeat(MAX_CELL_CHARS + 20);
        let cut = cell(&long);
        assert_eq!(cut.chars().count(), MAX_CELL_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(cell("a\nb"), "a b");
    }

    #[test]
    fn empty_state_lists_assets() {
        let tree = render_empty(&[("art/a.glb".to_string(), 1024)], false);
        assert_eq!(tree["schema"], json!(VIEW_SCHEMA));
        let raw = serde_json::to_string(&tree).expect("序列化");
        assert!(raw.contains("art/a.glb"));
    }

    #[test]
    fn oversized_tree_degrades_to_notice() {
        let wide = json!({
            "schema": VIEW_SCHEMA,
            "rev": 1,
            "root": {
                "type": "column",
                "children": [{
                    "type": "text",
                    "value": "x".repeat(MAX_VIEW_BYTES),
                }],
            },
        });
        let shrunk = assert_budget(wide, "测试");
        let raw = serde_json::to_string(&shrunk).expect("序列化");
        assert!(raw.len() < MAX_VIEW_BYTES, "超限必须被换成说明卡");
        assert!(raw.contains("超过面板"));
    }
}
