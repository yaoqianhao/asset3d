//! 可视化预览：把 AIR 文档渲染成「一个薄 HTML + 若干数据分片」。
//!
//! 为什么不是单文件：宿主 `host/fs.write` 是整文件塞进一帧、帧上限 1 MiB
//! （见 `rpc::FRAME_LIMIT_BYTES`），单文件 HTML 一过约 1 MB 就直接写不出去。
//! 所以页面只留渲染模板、分片索引和元数据摘要，几何/材质/贴图拆成一串
//! `*.shard.NN.js`，每片都按 `rpc::payload_budget` 的实际预算切好；页面用
//! `<script src>` 顺序 include（`file://` 下也能加载，不受 fetch 的跨域限制），
//! viewer 侧合并后 `JSON.parse`，缺片/坏片都在页面上给提示。
//!
//! 能力边界（别在文案里吹大）：预览只画**静态网格 + 材质**，没有蒙皮矩阵、
//! 没有动画时间轴。joints/weights/skins/animations 只作为统计数字展示。

use crate::air::{AirDoc, AirMesh};

/// 单张贴图的 base64 上限：超了就不内嵌（避免一个 4K PBR 贴图把分片撑到几十 MB）。
const MAX_TEXTURE_BYTES: usize = 6 * 1024 * 1024;
/// 全部贴图 base64 之和的上限。
const MAX_TEXTURE_TOTAL: usize = 24 * 1024 * 1024;
/// 分片切分时在宿主预算之外再留的余量：保证每片**严格低于**闸门，而不是卡着线过。
const SHARD_MARGIN: usize = 1024;

/// 预览生成结果。
pub struct ViewOutcome {
    pub html: String,
    /// JSON 载荷总字节数（所有分片内容之和，未算 JS 包装）。
    pub payload_bytes: usize,
    /// 数据分片：`(文件名, 内容)`，文件名与 HTML 里的 `<script src>` 一致。
    pub shards: Vec<(String, String)>,
    /// 每片实际字节数，与 `shards` 同序（写盘前用于自检）。
    pub shard_bytes: Vec<usize>,
    /// 因为超过三角形预算而被整体丢弃的网格名。
    pub skipped_meshes: Vec<String>,
    /// 因为单网格就超预算而被截断的网格（`名字 原三角数 → 保留三角数`）。
    pub truncated_meshes: Vec<String>,
    /// 实际内嵌的贴图数量。
    pub embedded_textures: usize,
    /// 因为体积超限没有内嵌的贴图名。
    pub dropped_textures: Vec<String>,
    /// 生成期提示（会写进页面的 issues 面板）。
    pub notes: Vec<String>,
}

/// 渲染 HTML + 数据分片。
///
/// - `max_triangles`：预览的三角形预算。超了先整网格丢弃（大的先丢），只剩一个
///   网格还超就**截断这个网格的索引**——降级总比撞宿主写盘上限好。
/// - `shard_base`：分片文件名主干（不含目录），产出 `{base}.shard.01.js`。
/// - `shard_budget`：单片载荷上限（字节），由调用方按 `rpc::payload_budget` 算好。
pub fn render(
    doc: &AirDoc,
    max_triangles: u64,
    shard_base: &str,
    shard_budget: usize,
) -> Result<ViewOutcome, String> {
    let mut doc = doc.clone();
    let mut notes: Vec<String> = Vec::new();

    let (skipped, truncated) = apply_triangle_budget(&mut doc, max_triangles, &mut notes);
    let (embedded, dropped) = gate_textures(&mut doc, &mut notes);

    for note in &notes {
        doc.issues.push(note.clone());
    }

    let payload = serde_json::to_string(&doc)
        .map_err(|err| format!("序列化 AIR 失败（不该发生）：{err}"))?;
    // `</script>` 会把内嵌字符串提前截断；分片虽然是 .js 不是 HTML，但按同一套
    // 转义走，`</script>` 出现在字符串里也不会出事。
    let payload = payload.replace("</", "<\\/");

    let shards = split_payload(&payload, shard_base, shard_budget)?;
    let shard_bytes: Vec<usize> = shards.iter().map(|(_, text)| text.len()).collect();

    let title = doc
        .source
        .path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("AIR")
        .to_string();

    // 页面顶部要能一眼看出数据被拆成了哪些文件（也给 grep 留一条线索）。
    let listing = shards
        .iter()
        .zip(shard_bytes.iter())
        .map(|((name, _), bytes)| format!("{name}（{}）", crate::util::format_bytes(*bytes as u64)))
        .collect::<Vec<_>>()
        .join("、");
    let meta = serde_json::json!({
        "total": shards.len(),
        "bytes": payload.len(),
        "files": shards.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(),
    });
    let scripts = shards
        .iter()
        .map(|(name, _)| format!("<script src=\"{}\"></script>", html_escape(name)))
        .collect::<Vec<_>>()
        .join("\n");

    let html = include_str!("../assets/viewer.html")
        .replace("__AIR_TITLE__", &html_escape(&title))
        .replace("__AIR_SHARD_LIST__", &html_escape(&listing))
        .replace("__AIR_SHARD_META__", &meta.to_string())
        .replace("__AIR_SHARD_SCRIPTS__", &scripts);

    Ok(ViewOutcome {
        html,
        payload_bytes: payload.len(),
        shards,
        shard_bytes,
        skipped_meshes: skipped,
        truncated_meshes: truncated,
        embedded_textures: embedded,
        dropped_textures: dropped,
        notes,
    })
}

/// 三角形预算：先整网格丢弃，再对剩下的超大网格做索引截断。
///
/// 旧实现只在 `meshes.len() > 1` 时丢网格，单个 40 万三角形的网格会原样进页面，
/// 直接把预览撞到写盘上限——所以这里补上「单网格也降级」这条路径。
fn apply_triangle_budget(
    doc: &mut AirDoc,
    max_triangles: u64,
    notes: &mut Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let mut skipped: Vec<String> = Vec::new();
    let mut truncated: Vec<String> = Vec::new();
    if max_triangles == 0 {
        return (skipped, truncated);
    }
    let triangles = |mesh: &crate::air::AirMesh| mesh.indices.len() as u64 / 3;
    let total: u64 = doc.meshes.iter().map(triangles).sum();
    if total <= max_triangles || doc.meshes.is_empty() {
        return (skipped, truncated);
    }

    let order: Vec<usize> = {
        let mut order: Vec<(usize, u64)> = doc
            .meshes
            .iter()
            .enumerate()
            .map(|(index, mesh)| (index, triangles(mesh)))
            .collect();
        order.sort_by(|a, b| b.1.cmp(&a.1));
        order.into_iter().map(|(index, _)| index).collect()
    };

    // `None` = 整网格丢弃，`Some(n)` = 只保留 n 个三角形。
    let mut plan: Vec<Option<u64>> = doc.meshes.iter().map(|mesh| Some(triangles(mesh))).collect();
    let mut live: u64 = total;

    // 第一刀：整网格丢弃，但至少留一个网格（不然页面就是空的）。
    for index in &order {
        if live <= max_triangles || plan.iter().filter(|slot| slot.is_some()).count() <= 1 {
            break;
        }
        let tris = plan[*index].unwrap_or(0);
        plan[*index] = None;
        live -= tris;
        skipped.push(mesh_label(doc, *index));
    }

    // 第二刀：只剩一个网格还超预算 → 截断索引（单网格也降级）。
    if live > max_triangles {
        let mut left = max_triangles;
        for index in &order {
            let Some(tris) = plan[*index] else {
                continue;
            };
            if left >= tris {
                left -= tris;
                continue;
            }
            // 至少留 1 个三角形，否则这个网格等于被丢弃却还占着位置。
            let keep = left.max(1).min(tris);
            if keep < tris {
                plan[*index] = Some(keep);
                truncated.push(format!(
                    "{} {} → {}",
                    mesh_label(doc, *index),
                    tris,
                    keep
                ));
            }
            left = left.saturating_sub(keep);
        }
    }

    let any_drop = plan.iter().any(Option::is_none);
    let any_truncate = !truncated.is_empty();
    if !any_drop && !any_truncate {
        return (skipped, truncated);
    }

    // ---- 应用计划：截断索引 + 重映射节点 ----
    for (index, mesh) in doc.meshes.iter_mut().enumerate() {
        let Some(keep) = plan[index] else {
            continue;
        };
        let have = triangles(mesh);
        if keep < have {
            mesh.indices.truncate((keep * 3) as usize);
            let mut clipped: Vec<crate::air::MaterialPart> = Vec::new();
            let mut consumed = 0u32;
            for part in mesh.material_parts.drain(..) {
                if consumed as u64 >= keep {
                    break;
                }
                let start = part.triangle_offset.max(consumed);
                let end = (part.triangle_offset + part.triangle_count).min(keep as u32);
                if end > start {
                    let mut part = part;
                    part.triangle_offset = start - consumed;
                    part.triangle_count = end - start;
                    consumed = end;
                    clipped.push(part);
                }
            }
            mesh.material_parts = clipped;
            // 只截索引、留着全部顶点等于白降级：13456 个顶点照样把载荷顶到 1 MB
            // 以上。所以顺手把没用到的顶点压掉，统计与 AABB 一起重算。
            compact_vertices(mesh);
            if mesh.material_parts.is_empty() {
                mesh.material_parts.push(crate::air::MaterialPart {
                    material: None,
                    material_name: String::new(),
                    triangle_offset: 0,
                    triangle_count: keep as u32,
                });
            }
        }
    }

    if any_drop {
        let mut remap: Vec<Option<u32>> = Vec::with_capacity(plan.len());
        let mut next = 0u32;
        for slot in &plan {
            if slot.is_some() {
                remap.push(Some(next));
                next += 1;
            } else {
                remap.push(None);
            }
        }
        let mut keep_meshes = Vec::with_capacity(next as usize);
        for (index, mesh) in std::mem::take(&mut doc.meshes).into_iter().enumerate() {
            if plan[index].is_some() {
                keep_meshes.push(mesh);
            }
        }
        doc.meshes = keep_meshes;
        for node in doc.nodes.iter_mut() {
            if let Some(mesh) = node.mesh {
                node.mesh = remap.get(mesh as usize).copied().flatten();
            }
        }
    }

    // 统计要跟页面里真实存在的数据对齐，否则 HUD 上的三角形数与画面不符。
    refresh_geometry_stats(doc);

    if any_drop {
        notes.push(format!(
            "预览省略了 {} 个网格（原 {} 个三角形，预算 {}）：{}",
            skipped.len(),
            total,
            max_triangles,
            skipped.join("、")
        ));
    }
    if !truncated.is_empty() {
        notes.push(format!(
            "单网格超出预览预算，已截断索引（{}）；需要完整几何请用 export_asset",
            truncated.join("；")
        ));
    }
    (skipped, truncated)
}

/// 压掉索引截断后不再被引用的顶点，属性数组同步收缩。
///
/// 索引截断只保证「三角形数」降下来，顶点属性仍然全量序列化；对单个超大网格
/// 来说，真正占载荷的是 positions/normals/uvs，所以必须一起收。
fn compact_vertices(mesh: &mut AirMesh) {
    let old = mesh.positions.len();
    if old == 0 || mesh.indices.is_empty() {
        return;
    }

    let mut remap: Vec<u32> = vec![u32::MAX; old];
    let mut next: u32 = 0;
    for slot in mesh.indices.iter_mut() {
        let src = *slot as usize;
        if src >= old {
            // 越界索引由 `AirDoc::validate` 负责拦下，这里只保证不越界访问。
            *slot = 0;
            continue;
        }
        if remap[src] == u32::MAX {
            remap[src] = next;
            next += 1;
        }
        *slot = remap[src];
    }
    let kept = next as usize;
    if kept == old {
        return;
    }

    let mut positions = vec![mesh.positions[0]; kept];
    let mut normals = if mesh.normals.len() == old {
        Some(vec![[0.0f32, 0.0, 0.0]; kept])
    } else {
        None
    };
    let mut uvs = if mesh.uvs.len() == old {
        Some(vec![[0.0f32, 0.0]; kept])
    } else {
        None
    };
    let mut joints = if mesh.joints.len() == old {
        Some(vec![[0u16; 4]; kept])
    } else {
        None
    };
    let mut weights = if mesh.weights.len() == old {
        Some(vec![[0.0f32; 4]; kept])
    } else {
        None
    };

    for (index, slot) in remap.iter().enumerate() {
        if *slot == u32::MAX {
            continue;
        }
        let to = *slot as usize;
        positions[to] = mesh.positions[index];
        if let Some(target) = normals.as_mut() {
            target[to] = mesh.normals[index];
        }
        if let Some(target) = uvs.as_mut() {
            target[to] = mesh.uvs[index];
        }
        if let Some(target) = joints.as_mut() {
            target[to] = mesh.joints[index];
        }
        if let Some(target) = weights.as_mut() {
            target[to] = mesh.weights[index];
        }
    }

    mesh.positions = positions;
    if let Some(target) = normals {
        mesh.normals = target;
    }
    if let Some(target) = uvs {
        mesh.uvs = target;
    }
    if let Some(target) = joints {
        mesh.joints = target;
    }
    if let Some(target) = weights {
        mesh.weights = target;
    }

    // AABB 只覆盖留下来的顶点，不然页面上的包围盒会跟画面不符。
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for point in &mesh.positions {
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    if min[0].is_finite() && max[0].is_finite() {
        mesh.aabb_min = min;
        mesh.aabb_max = max;
    }
}

fn mesh_label(doc: &AirDoc, index: usize) -> String {
    let name = &doc.meshes[index].name;
    if name.is_empty() {
        format!("#{index}")
    } else {
        name.clone()
    }
}

/// 按真实几何重算 HUD 用到的几个统计字段。
fn refresh_geometry_stats(doc: &mut AirDoc) {
    let mut vertices = 0u64;
    let mut triangles = 0u64;
    let mut geometry_bytes = 0u64;
    for mesh in &doc.meshes {
        vertices += mesh.positions.len() as u64;
        triangles += mesh.indices.len() as u64 / 3;
        geometry_bytes += (mesh.positions.len() * 12
            + mesh.normals.len() * 12
            + mesh.uvs.len() * 8
            + mesh.indices.len() * 4
            + mesh.joints.len() * 8
            + mesh.weights.len() * 16) as u64;
    }
    doc.stats.mesh_count = doc.meshes.len() as u32;
    doc.stats.vertex_count = vertices;
    doc.stats.triangle_count = triangles;
    doc.stats.geometry_bytes = geometry_bytes;
}

/// 贴图体积闸门。
///
/// 关键点：`take()` 拿出来的数据，超限时**必须放回** `texture.data_base64`——
/// 预览不内嵌是体积取舍，不是把资产的数据吞掉；同时把丢掉的贴图名记下来，
/// 让页面与返回值都能说清「丢了哪张」，而不是只报一个数字。
fn gate_textures(doc: &mut AirDoc, notes: &mut Vec<String>) -> (usize, Vec<String>) {
    let mut embedded = 0usize;
    let mut embedded_bytes = 0usize;
    let mut dropped: Vec<String> = Vec::new();
    // (贴图下标, 原始 base64, 原始 embedded 标记)：序列化之后原样放回。
    let mut stash: Vec<(usize, String, bool)> = Vec::new();

    for (index, texture) in doc.textures.iter_mut().enumerate() {
        let Some(data) = texture.data_base64.take() else {
            continue;
        };
        if data.len() > MAX_TEXTURE_BYTES || embedded_bytes + data.len() > MAX_TEXTURE_TOTAL {
            let label = if texture.name.is_empty() {
                format!("#{index}")
            } else {
                texture.name.clone()
            };
            dropped.push(label);
            // 这一片 JSON 里确实没有它的数据，标记要与事实一致。
            let was_embedded = texture.embedded;
            texture.embedded = false;
            stash.push((index, data, was_embedded));
            continue;
        }
        embedded_bytes += data.len();
        embedded += 1;
        texture.data_base64 = Some(data);
    }

    if !dropped.is_empty() {
        notes.push(format!(
            "{} 张贴图因为体积超限没有内嵌，预览里退回材质的 baseColor：{}",
            dropped.len(),
            dropped.join("、")
        ));
    }
    if doc.textures.is_empty() {
        notes.push("该资产没有贴图，按材质的 baseColor/metallic/roughness 着色".to_string());
    }

    // 放回：预览页已经生成（不含这些数据），但 AIR 文档保持完整。
    for (index, data, was_embedded) in stash {
        if let Some(texture) = doc.textures.get_mut(index) {
            texture.data_base64 = Some(data);
            texture.embedded = was_embedded;
        }
    }
    (embedded, dropped)
}

/// 把 JSON 载荷切成若干 `.js` 分片，每片都严格低于 `shard_budget`。
///
/// 分片内容是 `window.__AIR_SHARDS__.push({"i":序号,"t":"片段"});`，viewer 按序合并。
fn split_payload(
    payload: &str,
    shard_base: &str,
    shard_budget: usize,
) -> Result<Vec<(String, String)>, String> {
    let cap = shard_budget.saturating_sub(SHARD_MARGIN);
    if cap < 4096 {
        return Err(format!(
            "宿主帧预算只剩 {shard_budget} 字节，装不下分片文件头；请缩短输出路径"
        ));
    }

    let mut shards: Vec<(String, String)> = Vec::new();
    let mut index = 1usize;
    let mut chunk = String::new();
    let mut used = 0usize; // 已写入 chunk 的转义后字节数

    for ch in payload.chars() {
        let escaped = escape_js_char(ch);
        let room = cap.saturating_sub(shard_frame_len(index));
        if used + escaped.len() > room && !chunk.is_empty() {
            shards.push((shard_file_name(shard_base, index), shard_text(index, &chunk)));
            index += 1;
            chunk.clear();
            used = 0;
        }
        used += escaped.len();
        chunk.push_str(&escaped);
    }
    // 空载荷也要留一片，否则页面拿不到 total 与 files 的一致性。
    if !chunk.is_empty() || shards.is_empty() {
        shards.push((shard_file_name(shard_base, index), shard_text(index, &chunk)));
    }

    for (name, text) in &shards {
        if text.len() > cap {
            return Err(format!(
                "分片 `{name}` 有 {} 字节，超过宿主单帧预算 {shard_budget} 字节",
                text.len()
            ));
        }
    }
    Ok(shards)
}

fn shard_frame_len(index: usize) -> usize {
    // `window.__AIR_SHARDS__.push({"i":N,"t":""});\n` 的固定开销。
    "window.__AIR_SHARDS__.push({\"i\":,\"t\":\"\"});\n".len() + index.to_string().len()
}

fn shard_file_name(shard_base: &str, index: usize) -> String {
    format!("{shard_base}.shard.{index:02}.js")
}

fn shard_text(index: usize, chunk: &str) -> String {
    format!(
        "window.__AIR_SHARDS__.push({{\"i\":{index},\"t\":\"{chunk}\"}});\n"
    )
}

/// 单个字符转成「JS 字符串字面量里安全」的形态。
fn escape_js_char(ch: char) -> String {
    match ch {
        '"' => "\\\"".to_string(),
        '\\' => "\\\\".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        '\u{2028}' => "\\u2028".to_string(),
        '\u{2029}' => "\\u2029".to_string(),
        c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
        c => c.to_string(),
    }
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::air::{AirDoc, AirMesh, AirTexture};

    fn doc_with(meshes: Vec<AirMesh>) -> AirDoc {
        let mut doc = AirDoc::new("big.glb", "big.glb");
        doc.meshes = meshes;
        doc.nodes.push(crate::air::AirNode {
            name: "n0".to_string(),
            mesh: Some(0),
            ..Default::default()
        });
        doc
    }

    fn grid_mesh(triangles: usize) -> AirMesh {
        let mut mesh = AirMesh {
            name: "grid".to_string(),
            ..Default::default()
        };
        for i in 0..(triangles + 2) {
            mesh.positions.push([i as f32, 0.0, 0.0]);
        }
        for i in 0..(triangles * 3) {
            mesh.indices.push((i % (triangles + 2)) as u32);
        }
        mesh
    }

    /// 缺陷④：单个网格超预算必须截断，而不是原样塞进页面把写盘撞死。
    #[test]
    fn single_huge_mesh_is_truncated_instead_of_dropped() {
        let mut doc = doc_with(vec![grid_mesh(1000)]);
        let mut notes = Vec::new();
        let (skipped, truncated) = apply_triangle_budget(&mut doc, 100, &mut notes);
        assert!(skipped.is_empty(), "只有一个网格，不许丢光");
        assert_eq!(truncated.len(), 1);
        assert!(truncated[0].contains("1000 → 100"), "{truncated:?}");
        assert_eq!(doc.meshes.len(), 1);
        assert_eq!(doc.meshes[0].indices.len(), 300, "索引要真的被截到 100 个三角形");
        assert_eq!(
            doc.meshes[0].positions.len(),
            300,
            "用不到的顶点要一起压掉，否则载荷还是顶不住"
        );
        assert_eq!(doc.stats.triangle_count, 100, "统计要跟着截断后的几何走");
        assert_eq!(doc.stats.vertex_count, 300);

        // 整条链路也得能出页面，且 HUD 数字对齐。
        let outcome = render(&doc, 100, "big", 256 * 1024).expect("render");
        assert!(!outcome.html.is_empty());
        assert!(outcome.dropped_textures.is_empty());
    }

    /// 缺陷③：贴图超限是「预览不内嵌」，不是把资产数据吞掉；名字必须被记下来。
    #[test]
    fn oversized_texture_is_restored_and_named() {
        let mut doc = doc_with(vec![grid_mesh(2)]);
        doc.textures.push(AirTexture {
            name: "huge_albedo".to_string(),
            mime_type: "image/png".to_string(),
            embedded: true,
            data_base64: Some("A".repeat(MAX_TEXTURE_BYTES + 1)),
            ..Default::default()
        });

        let mut notes = Vec::new();
        let (embedded, dropped) = gate_textures(&mut doc, &mut notes);
        assert_eq!(embedded, 0);
        assert_eq!(dropped, vec!["huge_albedo".to_string()]);
        let data = doc.textures[0].data_base64.as_ref().expect("take() 之后必须放回");
        assert_eq!(data.len(), MAX_TEXTURE_BYTES + 1, "放回的数据不能被动过");
        assert!(notes.iter().any(|note| note.contains("huge_albedo")));
    }

    /// 缺陷①：每片都严格低于预算，且真的被切成了多片。
    #[test]
    fn shards_stay_strictly_below_budget() {
        let doc = doc_with(vec![grid_mesh(20_000)]);
        let budget = 32 * 1024;
        let outcome = render(&doc, 400_000, "big", budget).expect("render");
        assert!(outcome.shards.len() > 1, "载荷远超预算时必须分片");
        for (name, text) in &outcome.shards {
            assert!(
                text.len() < budget,
                "分片 {name} 有 {} 字节，必须严格低于预算 {budget}",
                text.len()
            );
        }
        // HTML 只留骨架：不含几何，且顶部列出分片清单。
        assert!(outcome.html.len() < 64 * 1024, "HTML 不该再长大");
        assert!(outcome.html.contains("shard.01.js"));
        for placeholder in ["__AIR_SHARD_LIST__", "__AIR_SHARD_META__", "__AIR_SHARD_SCRIPTS__"] {
            assert!(!outcome.html.contains(placeholder), "占位符 {placeholder} 未被替换");
        }
    }

    /// 预算边界的两侧：刚好放得下时既不截断也不提示，多一个三角形才动手。
    /// 闸门写成 `<` 或 `<=` 全在这里体现，差一个三角形就白裁掉用户的网格。
    #[test]
    fn triangle_budget_only_clips_when_exceeded() {
        let mut fits = doc_with(vec![grid_mesh(100)]);
        let mut notes = Vec::new();
        let (skipped, truncated) = apply_triangle_budget(&mut fits, 100, &mut notes);
        assert!(skipped.is_empty() && truncated.is_empty(), "刚好等于预算不该被裁");
        assert_eq!(fits.meshes[0].indices.len(), 300);
        assert!(notes.is_empty(), "没超预算就不该有 issue");

        let mut over = doc_with(vec![grid_mesh(101)]);
        let mut notes = Vec::new();
        let (_, truncated) = apply_triangle_budget(&mut over, 100, &mut notes);
        assert_eq!(truncated.len(), 1, "多一个三角形就必须裁并说明");
        assert_eq!(over.meshes[0].indices.len(), 300);
    }
}
