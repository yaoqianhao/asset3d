//! 骨骼绑定研究报告生成器（临时诊断，非产品代码）。
#[path = "../air.rs"]
#[allow(dead_code)]
mod air;
#[path = "../air_bin.rs"]
#[allow(dead_code)]
mod air_bin;
#[path = "../export_core.rs"]
#[allow(dead_code)]
mod export_core;
#[path = "../fbx.rs"]
#[allow(dead_code)]
mod fbx;
#[path = "../gltf.rs"]
#[allow(dead_code)]
mod gltf;
#[path = "../util.rs"]
#[allow(dead_code)]
mod util;
#[path = "../vrm.rs"]
#[allow(dead_code)]
mod vrm;

use std::collections::{HashMap, HashSet};
use std::io::Write;

fn trs(node: &air::AirNode) -> [f32; 16] {
    let [x, y, z, w] = node.rotation;
    let (x, y, z, w) = (x as f64, y as f64, z as f64, w as f64);
    let mut m = util::identity4();
    let r = [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
    ];
    for row in 0..3 {
        for col in 0..3 {
            m[col * 4 + row] = (r[row][col] * node.scale[col] as f64) as f32;
        }
    }
    m[12] = node.position[0];
    m[13] = node.position[1];
    m[14] = node.position[2];
    m
}

fn world_of(nodes: &[air::AirNode], index: usize) -> [f32; 16] {
    let node = &nodes[index];
    let local = trs(node);
    match node.parent {
        Some(p) => util::mat4_mul(&world_of(nodes, p as usize), &local),
        None => local,
    }
}

/// 3x3 旋转差异角（度）
fn rot_angle_deg(a: &[f32; 16], b: &[f32; 16]) -> f64 {
    // R = Ra^T * Rb 的 trace
    let mut tr = 0.0f64;
    for i in 0..3 {
        for k in 0..3 {
            tr += (a[k * 4 + i] as f64) * (b[k * 4 + i] as f64);
        }
    }
    let cos = ((tr - 1.0) / 2.0).clamp(-1.0, 1.0);
    cos.acos().to_degrees()
}

fn quat_angle_deg(a: [f32; 4], b: [f32; 4]) -> f64 {
    let dot = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]).abs() as f64;
    (2.0 * dot.clamp(-1.0, 1.0).acos()).to_degrees()
}

fn main() {
    let path = std::env::args().nth(1).expect("需要 fbx 路径");
    let doc = export_core::import_doc(std::path::Path::new(&path)).expect("导入失败");
    let scene = ufbx::load_file(
        &path,
        ufbx::LoadOpts {
            target_axes: ufbx::CoordinateAxes {
                right: ufbx::CoordinateAxis::PositiveX,
                up: ufbx::CoordinateAxis::PositiveY,
                front: ufbx::CoordinateAxis::PositiveZ,
            },
            target_unit_meters: 1.0,
            load_external_files: false,
            ignore_missing_external_files: true,
            generate_missing_normals: true,
            evaluate_skinning: true,
            filename: ufbx::StringOpt::Ref(&path),
            ..Default::default()
        },
    )
    .expect("ufbx 加载失败");

    let mut out = String::new();
    let nodes = &doc.nodes;

    // ---------------------------------------------------------------- 总览
    out.push_str("## 0 总览\n");
    out.push_str(&format!(
        "nodes={} meshes={} skins={} anims={} unit_scale={} up_axis={}\n",
        doc.nodes.len(),
        doc.meshes.len(),
        doc.skins.len(),
        doc.animations.len(),
        doc.units.meters_per_unit,
        doc.units.up_axis
    ));
    for (i, s) in doc.skins.iter().enumerate() {
        out.push_str(&format!(
            "skin#{i} name=`{}` joints={} ibm={}\n",
            s.name,
            s.joints.len(),
            s.inverse_bind_matrices.len()
        ));
    }

    // ---------------------------------------------------------------- 1 骨架拓扑
    out.push_str("\n## 1 骨架拓扑\n");
    let mut bone_set: HashSet<u32> = HashSet::new();
    let mut bone_order: Vec<u32> = Vec::new();
    for s in &doc.skins {
        for j in &s.joints {
            if bone_set.insert(*j) {
                bone_order.push(*j);
            }
        }
    }
    out.push_str(&format!("唯一骨骼节点数 = {}\n", bone_set.len()));
    // 根骨骼（父不在骨骼集合内）
    let roots: Vec<u32> = bone_order
        .iter()
        .copied()
        .filter(|j| match nodes[*j as usize].parent {
            None => true,
            Some(p) => !bone_set.contains(&p),
        })
        .collect();
    out.push_str(&format!("骨骼根节点 {} 个：", roots.len()));
    for r in &roots {
        out.push_str(&format!("`{}` ", nodes[*r as usize].name));
    }
    out.push('\n');
    // 树
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for j in &bone_order {
        if let Some(p) = nodes[*j as usize].parent {
            if bone_set.contains(&p) {
                children.entry(p).or_default().push(*j);
            }
        }
    }
    fn dump_tree(
        nodes: &[air::AirNode],
        children: &HashMap<u32, Vec<u32>>,
        bone_set: &HashSet<u32>,
        idx: u32,
        depth: usize,
        out: &mut String,
    ) {
        let n = &nodes[idx as usize];
        let w = world_of(nodes, idx as usize);
        let scale = (w[0].powi(2) + w[1].powi(2) + w[2].powi(2)).sqrt();
        let deg = {
            let q = n.rotation;
            let dot = (q[3] as f64).abs().clamp(-1.0, 1.0);
            (2.0 * dot.acos()).to_degrees()
        };
        out.push_str(&format!(
            "{}- [{}] `{}` local_t=({:.4},{:.4},{:.4}) local_rot={:.2}° local_s=({:.4},{:.4},{:.4}) world=({:.4},{:.4},{:.4}) |basis_x|={:.5}\n",
            "  ".repeat(depth),
            idx,
            n.name,
            n.position[0], n.position[1], n.position[2],
            deg,
            n.scale[0], n.scale[1], n.scale[2],
            w[12], w[13], w[14],
            scale
        ));
        if let Some(cs) = children.get(&idx) {
            let mut cs = cs.clone();
            cs.sort();
            for c in cs {
                dump_tree(nodes, children, bone_set, c, depth + 1, out);
            }
        }
    }
    for r in &roots {
        dump_tree(nodes, &children, &bone_set, *r, 0, &mut out);
    }
    // 骨骼集合外的父链（骨骼的祖先里有没有非骨骼节点）
    let mut external_ancestors: Vec<String> = Vec::new();
    for j in &bone_order {
        let mut p = nodes[*j as usize].parent;
        while let Some(pi) = p {
            if !bone_set.contains(&pi) {
                external_ancestors.push(format!(
                    "`{}` 的祖先 `{}` 不在骨骼表内",
                    nodes[*j as usize].name, nodes[pi as usize].name
                ));
                break;
            }
            p = nodes[pi as usize].parent;
        }
    }
    external_ancestors.sort();
    external_ancestors.dedup();
    if !external_ancestors.is_empty() {
        out.push_str("骨骼祖先中的非关节节点：\n");
        for e in &external_ancestors {
            out.push_str(&format!("  - {e}\n"));
        }
    }

    // ---------------------------------------------------------------- 2 绑定自洽性
    out.push_str("\n## 2 绑定姿态自洽性（jointWorld × IBM × localPos vs nodeWorld × localPos）\n");
    for (mi, mesh) in doc.meshes.iter().enumerate() {
        let Some(skin_index) = mesh.skin else {
            out.push_str(&format!("mesh#{mi} `{}` 无蒙皮\n", mesh.name));
            continue;
        };
        let skin = &doc.skins[skin_index as usize];
        let node_index = nodes
            .iter()
            .position(|n| n.mesh == Some(mi as u32))
            .unwrap_or(0);
        let node_world = world_of(nodes, node_index);
        let palettes: Vec<[f32; 16]> = skin
            .joints
            .iter()
            .zip(skin.inverse_bind_matrices.iter())
            .map(|(j, ibm)| util::mat4_mul(&world_of(nodes, *j as usize), ibm))
            .collect();
        // palette 应与 node_world 完全一致才对
        let mut pal_max = 0.0f32;
        for p in &palettes {
            for k in 0..16 {
                pal_max = pal_max.max((p[k] - node_world[k]).abs());
            }
        }
        let mut max_err = 0.0f32;
        let mut sum = 0.0f64;
        let mut zero_w = 0usize;
        for (i, p) in mesh.positions.iter().enumerate() {
            let mut acc = [0.0f32; 3];
            let mut wsum = 0.0f32;
            for k in 0..4 {
                let w = mesh.weights[i][k];
                if w <= 0.0 {
                    continue;
                }
                wsum += w;
                let slot = mesh.joints[i][k] as usize;
                let q = util::transform_point(&palettes[slot], *p);
                acc[0] += q[0] * w;
                acc[1] += q[1] * w;
                acc[2] += q[2] * w;
            }
            if wsum <= 0.0 {
                zero_w += 1;
            }
            let e = util::transform_point(&node_world, *p);
            let d = ((acc[0] - e[0]).powi(2) + (acc[1] - e[1]).powi(2) + (acc[2] - e[2]).powi(2))
                .sqrt();
            max_err = max_err.max(d);
            sum += d as f64;
        }
        out.push_str(&format!(
            "mesh#{mi} `{}` verts={} skin#{} joints={} | palette≡nodeWorld maxdiff={:.3e} | 蒙皮vs静态 顶点误差 max={:.3e} mean={:.3e} 零权重顶点={}\n",
            mesh.name,
            mesh.positions.len(),
            skin_index,
            skin.joints.len(),
            pal_max,
            max_err,
            sum / mesh.positions.len().max(1) as f64,
            zero_w
        ));
    }

    // ---------------------------------------------------------------- 3 权重质量
    out.push_str("\n## 3 权重质量\n");
    let mut influence_hist = [0usize; 5];
    let mut bone_vertex_count: HashMap<(usize, u16), usize> = HashMap::new();
    for (mi, mesh) in doc.meshes.iter().enumerate() {
        let Some(skin_index) = mesh.skin else { continue };
        let (mut wmin, mut wmax, mut dev) = (f32::INFINITY, f32::NEG_INFINITY, 0usize);
        let mut hist = [0usize; 5];
        let mut dom_hist: HashMap<u16, usize> = HashMap::new();
        for i in 0..mesh.positions.len() {
            let ws = mesh.weights[i];
            let s: f32 = ws.iter().sum();
            wmin = wmin.min(s);
            wmax = wmax.max(s);
            if (s - 1.0).abs() > 1e-4 {
                dev += 1;
            }
            let n = ws.iter().filter(|w| **w > 0.0).count().min(4);
            hist[n] += 1;
            influence_hist[n] += 1;
            // 主导骨骼
            let mut best = (0usize, 0.0f32);
            for k in 0..4 {
                if ws[k] > best.1 {
                    best = (k, ws[k]);
                }
            }
            if ws[best.0] > 0.0 {
                *dom_hist.entry(mesh.joints[i][best.0]).or_insert(0) += 1;
                *bone_vertex_count
                    .entry((skin_index as usize, mesh.joints[i][best.0]))
                    .or_insert(0) += 1;
            }
        }
        let n = mesh.positions.len().max(1);
        out.push_str(&format!(
            "mesh#{mi} `{}` 顶点={} 权重和 [min={:.6}, max={:.6}] 偏离1的顶点={} 影响骨骼数分布 1:{:.1}% 2:{:.1}% 3:{:.1}% 4:{:.1}%\n",
            mesh.name, n,
            wmin, wmax, dev,
            hist[1] as f64 * 100.0 / n as f64,
            hist[2] as f64 * 100.0 / n as f64,
            hist[3] as f64 * 100.0 / n as f64,
            hist[4] as f64 * 100.0 / n as f64
        ));
        // 主导骨骼 top5
        let skin = &doc.skins[skin_index as usize];
        let mut tops: Vec<(u16, usize)> = dom_hist.into_iter().collect();
        tops.sort_by(|a, b| b.1.cmp(&a.1));
        out.push_str("   主导骨骼 top5: ");
        for (slot, cnt) in tops.iter().take(5) {
            let name = skin
                .joints
                .get(*slot as usize)
                .map(|j| nodes[*j as usize].name.clone())
                .unwrap_or_default();
            out.push_str(&format!("`{name}`={cnt} "));
        }
        out.push('\n');
        // 未被任何顶点主导的骨骼
        let unused: Vec<String> = skin
            .joints
            .iter()
            .enumerate()
            .filter(|(slot, _)| !tops.iter().any(|(s, _)| s == &(*slot as u16)))
            .map(|(_, j)| nodes[*j as usize].name.clone())
            .collect();
        if !unused.is_empty() {
            out.push_str(&format!("   无顶点主导的骨骼: {}\n", unused.join(", ")));
        }
    }

    // ---------------------------------------------------------------- 4 TransformLink 对照
    out.push_str("\n## 4 cluster.bind_to_world（TransformLink）vs 节点世界矩阵\n");
    for (mi, um) in scene.meshes.iter().enumerate() {
        let Some(def) = um.skin_deformers.first() else {
            continue;
        };
        let air_mesh = doc.meshes.get(mi);
        out.push_str(&format!(
            "mesh#{mi} `{}` clusters={} air_joints={}\n",
            um.element.name,
            def.clusters.len(),
            air_mesh.and_then(|m| m.skin).map(|s| doc.skins[s as usize].joints.len()).unwrap_or(0)
        ));
        let mut worst: Vec<(String, f64, f64)> = Vec::new();
        let mut angle_sum = 0.0f64;
        let mut trans_sum = 0.0f64;
        let mut cnt = 0usize;
        for c in def.clusters.iter() {
            let Some(bone) = c.bone_node.as_ref() else { continue };
            let bone_world = util::mat4_from_ufbx(&bone.node_to_world);
            let bind = util::mat4_from_ufbx(&c.bind_to_world);
            let ang = rot_angle_deg(&bone_world, &bind);
            let tr = ((bone_world[12] - bind[12]).powi(2)
                + (bone_world[13] - bind[13]).powi(2)
                + (bone_world[14] - bind[14]).powi(2))
            .sqrt() as f64;
            angle_sum += ang;
            trans_sum += tr;
            cnt += 1;
            worst.push((bone.element.name.to_string(), ang, tr));
        }
        worst.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        if cnt > 0 {
            out.push_str(&format!(
                "   平均角度差={:.2}° 平均平移差={:.4}m\n",
                angle_sum / cnt as f64,
                trans_sum / cnt as f64
            ));
            out.push_str("   角度差最大的 8 根：\n");
            for (name, ang, tr) in worst.iter().take(8) {
                out.push_str(&format!("     `{name}` 角度差={ang:.2}° 平移差={tr:.4}m\n"));
            }
        }
    }

    // ---------------------------------------------------------------- 5 动画与绑定姿态一致性
    out.push_str("\n## 5 动画 vs 绑定姿态\n");
    for anim in &doc.animations {
        let driven: HashSet<u32> = anim.tracks.iter().map(|t| t.node).collect();
        out.push_str(&format!(
            "anim `{}` duration={:.3}s tracks={} driven_nodes={} (其中骨骼={})\n",
            anim.name,
            anim.duration,
            anim.tracks.len(),
            driven.len(),
            driven.iter().filter(|d| bone_set.contains(d)).count()
        ));
        // t=0 的关键帧值 vs 静态 TRS
        let mut max_t = 0.0f32;
        let mut max_r = 0.0f64;
        let mut max_s = 0.0f32;
        for track in &anim.tracks {
            let Some(first) = track.values.first() else { continue };
            let node = &nodes[track.node as usize];
            match track.property.as_str() {
                "translation" => {
                    let d = ((first[0] - node.position[0]).powi(2)
                        + (first[1] - node.position[1]).powi(2)
                        + (first[2] - node.position[2]).powi(2))
                    .sqrt();
                    max_t = max_t.max(d);
                }
                "rotation" => {
                    let d = quat_angle_deg([first[0], first[1], first[2], first[3]], node.rotation);
                    max_r = max_r.max(d);
                }
                "scale" => {
                    let d = (first[0] - node.scale[0])
                        .abs()
                        .max((first[1] - node.scale[1]).abs())
                        .max((first[2] - node.scale[2]).abs());
                    max_s = max_s.max(d);
                }
                _ => {}
            }
        }
        out.push_str(&format!(
            "   首帧关键值 vs 静态 TRS：位移最大差={max_t:.4}m 旋转最大差={max_r:.2}° 缩放最大差={max_s:.5}\n"
        ));
    }

    // ---------------------------------------------------------------- 6 多帧骨骼拉伸
    out.push_str("\n## 6 播放中的骨长稳定性（父子骨骼世界距离 / 绑定姿态比值）\n");
    if let Some(anim) = doc.animations.first() {
        // 采样若干时刻，逐帧从静态 TRS 叠加轨道
        let sample_times = [0.0f32, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0];
        // 静态骨长
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        for j in &bone_order {
            if let Some(p) = nodes[*j as usize].parent {
                if bone_set.contains(&p) {
                    pairs.push((p, *j));
                }
            }
        }
        let bind_worlds: Vec<[f32; 16]> = bone_order
            .iter()
            .map(|j| world_of(nodes, *j as usize))
            .collect();
        let idx_of_bone: HashMap<u32, usize> = bone_order
            .iter()
            .enumerate()
            .map(|(i, j)| (*j, i))
            .collect();
        let bind_len: Vec<f32> = pairs
            .iter()
            .map(|(p, c)| {
                let a = &bind_worlds[idx_of_bone[p]];
                let b = &bind_worlds[idx_of_bone[c]];
                ((a[12] - b[12]).powi(2) + (a[13] - b[13]).powi(2) + (a[14] - b[14]).powi(2))
                    .sqrt()
            })
            .collect();
        for t in sample_times {
            // 求值：静态 TRS 叠加轨道
            let mut posed: Vec<air::AirNode> = nodes.clone();
            for track in &anim.tracks {
                let node = &mut posed[track.node as usize];
                // 找键区间
                let times = &track.times;
                if times.is_empty() {
                    continue;
                }
                let mut k = 0usize;
                while k + 1 < times.len() && times[k + 1] <= t {
                    k += 1;
                }
                let (t0, t1) = (times[k], *times.get(k + 1).unwrap_or(&times[k]));
                let v0 = track.values[k];
                let v1 = *track.values.get(k + 1).unwrap_or(&v0);
                let u = if t1 > t0 {
                    ((t - t0) / (t1 - t0)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let lerp = |a: f32, b: f32| a + (b - a) * u;
                match track.property.as_str() {
                    "translation" => {
                        node.position = [lerp(v0[0], v1[0]), lerp(v0[1], v1[1]), lerp(v0[2], v1[2])]
                    }
                    "scale" => {
                        node.scale = [lerp(v0[0], v1[0]), lerp(v0[1], v1[1]), lerp(v0[2], v1[2])]
                    }
                    "rotation" => {
                        let mut q0 = [v0[0], v0[1], v0[2], v0[3]];
                        let mut q1 = [v1[0], v1[1], v1[2], v1[3]];
                        if q0[0] * q1[0] + q0[1] * q1[1] + q0[2] * q1[2] + q0[3] * q1[3] < 0.0 {
                            for v in q1.iter_mut() {
                                *v = -*v;
                            }
                        }
                        let mut q = [
                            lerp(q0[0], q1[0]),
                            lerp(q0[1], q1[1]),
                            lerp(q0[2], q1[2]),
                            lerp(q0[3], q1[3]),
                        ];
                        q0 = util::normalize_quat(q);
                        q = q0;
                        node.rotation = q;
                    }
                    _ => {}
                }
            }
            let mut worst = (0.0f32, String::new());
            let mut over10 = 0usize;
            for (i, (p, c)) in pairs.iter().enumerate() {
                if bind_len[i] < 1e-6 {
                    continue;
                }
                let wa = world_of(&posed, *p as usize);
                let wb = world_of(&posed, *c as usize);
                let d = ((wa[12] - wb[12]).powi(2)
                    + (wa[13] - wb[13]).powi(2)
                    + (wa[14] - wb[14]).powi(2))
                .sqrt();
                let ratio = d / bind_len[i];
                let dev = (ratio - 1.0).abs();
                if dev > 0.1 {
                    over10 += 1;
                }
                if dev > worst.0 {
                    worst = (
                        dev,
                        format!(
                            "{}→{} ratio={ratio:.3}",
                            nodes[*p as usize].name, nodes[*c as usize].name
                        ),
                    );
                }
            }
            out.push_str(&format!(
                "   t={t:.2}s 最大偏离={:.4}（{}） 偏离>10%的骨对={}\n",
                worst.0, worst.1, over10
            ));
        }
    }

    // ---------------------------------------------------------------- 7 骨骼与网格的空间关系
    out.push_str("\n## 7 骨骼位置 vs 网格包围盒\n");
    let mut mesh_min = [f32::INFINITY; 3];
    let mut mesh_max = [f32::NEG_INFINITY; 3];
    for (mi, mesh) in doc.meshes.iter().enumerate() {
        let Some(_skin) = mesh.skin else { continue };
        let node_index = nodes.iter().position(|n| n.mesh == Some(mi as u32)).unwrap_or(0);
        let nw = world_of(nodes, node_index);
        for p in mesh.positions.iter() {
            let w = util::transform_point(&nw, *p);
            for a in 0..3 {
                mesh_min[a] = mesh_min[a].min(w[a]);
                mesh_max[a] = mesh_max[a].max(w[a]);
            }
        }
    }
    out.push_str(&format!(
        "蒙皮网格世界 AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3})\n",
        mesh_min[0], mesh_min[1], mesh_min[2], mesh_max[0], mesh_max[1], mesh_max[2]
    ));
    let mut inside = 0usize;
    let mut outside: Vec<String> = Vec::new();
    for j in &bone_order {
        let w = world_of(nodes, *j as usize);
        let pos = [w[12], w[13], w[14]];
        let ok = (0..3).all(|a| pos[a] >= mesh_min[a] && pos[a] <= mesh_max[a]);
        if ok {
            inside += 1;
        } else {
            outside.push(format!(
                "`{}` ({:.3},{:.3},{:.3})",
                nodes[*j as usize].name, pos[0], pos[1], pos[2]
            ));
        }
    }
    out.push_str(&format!(
        "骨骼世界坐标落在网格 AABB 内：{}/{}",
        inside,
        bone_order.len()
    ));
    if !outside.is_empty() {
        out.push_str(&format!("；越界 {} 根：{}", outside.len(), outside.join(", ")));
    }
    out.push('\n');

    // ---------------------------------------------------------------- 8 动画轨道覆盖
    out.push_str("\n## 8 动画驱动覆盖\n");
    if let Some(anim) = doc.animations.first() {
        let driven: HashSet<u32> = anim.tracks.iter().map(|t| t.node).collect();
        let undriven: Vec<String> = bone_order
            .iter()
            .filter(|b| !driven.contains(b))
            .map(|b| nodes[*b as usize].name.clone())
            .collect();
        out.push_str(&format!(
            "骨骼总数={} 被动画驱动={} 未驱动={}\n",
            bone_order.len(),
            bone_order.iter().filter(|b| driven.contains(b)).count(),
            undriven.len()
        ));
        if !undriven.is_empty() {
            out.push_str(&format!("未驱动骨骼：{}\n", undriven.join(", ")));
        }
        // 轨道属性分布
        let mut props: HashMap<String, usize> = HashMap::new();
        for t in &anim.tracks {
            *props.entry(t.property.clone()).or_insert(0) += 1;
        }
        out.push_str(&format!("轨道属性分布：{props:?}\n"));
        // 关键帧数
        let total_keys: usize = anim.tracks.iter().map(|t| t.times.len()).sum();
        out.push_str(&format!(
            "关键帧总数={} 平均每轨={:.1}\n",
            total_keys,
            total_keys as f64 / anim.tracks.len().max(1) as f64
        ));
    }

    let target = r"C:\Users\Nero\AppData\Roaming\Tencent\Marvis\User\oAN1i2dpCCuZq-FgvYnflZ6SBnQA\workspace\conv_776800051b6f46619c993787b3340447\temp\skinreport.txt";
    let mut f = std::fs::File::create(target).unwrap();
    f.write_all(out.as_bytes()).unwrap();
    println!("report written");
}
