//! temp diag: 确认骨骼根与网格节点的层级关系，并给出建议的骨架修正矩阵 M。
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
use ufbx::{CoordinateAxes, CoordinateAxis};

fn qmul(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    (
        a.3 * b.0 + a.0 * b.3 + a.1 * b.2 - a.2 * b.1,
        a.3 * b.1 - a.0 * b.2 + a.1 * b.3 + a.2 * b.0,
        a.3 * b.2 + a.0 * b.1 - a.1 * b.0 + a.2 * b.3,
        a.3 * b.3 - a.0 * b.0 - a.1 * b.1 - a.2 * b.2,
    )
}
fn qnorm(q: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let n = (q.0 * q.0 + q.1 * q.1 + q.2 * q.2 + q.3 * q.3).sqrt().max(1e-12);
    (q.0 / n, q.1 / n, q.2 / n, q.3 / n)
}
/// 两个四元数夹角（度）
fn qangle(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> f64 {
    let d = (a.0 * b.0 + a.1 * b.1 + a.2 * b.2 + a.3 * b.3).abs().clamp(0.0, 1.0);
    2.0 * d.acos().to_degrees()
}
fn mat_to_q(m: &[f32; 16]) -> (f64, f64, f64, f64) {
    // 列主序 3x3，先正交归一化
    let c0 = [m[0] as f64, m[1] as f64, m[2] as f64];
    let mut c1 = [m[4] as f64, m[5] as f64, m[6] as f64];
    let mut c2 = [m[8] as f64, m[9] as f64, m[10] as f64];
    let norm = |v: [f64; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let s0 = norm(c0).max(1e-12);
    let n0 = [c0[0] / s0, c0[1] / s0, c0[2] / s0];
    // Gram-Schmidt
    let d = n0[0] * c1[0] + n0[1] * c1[1] + n0[2] * c1[2];
    c1 = [c1[0] - d * n0[0], c1[1] - d * n0[1], c1[2] - d * n0[2]];
    let s1 = norm(c1).max(1e-12);
    let n1 = [c1[0] / s1, c1[1] / s1, c1[2] / s1];
    c2 = [
        n0[1] * n1[2] - n0[2] * n1[1],
        n0[2] * n1[0] - n0[0] * n1[2],
        n0[0] * n1[1] - n0[1] * n1[0],
    ];
    let m00 = n0[0];
    let m10 = n0[1];
    let m20 = n0[2];
    let m01 = n1[0];
    let m11 = n1[1];
    let m21 = n1[2];
    let m02 = c2[0];
    let m12 = c2[1];
    let m22 = c2[2];
    let trace = m00 + m11 + m22;
    let q = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        ((m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, 0.25 * s)
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        (0.25 * s, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s)
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        ((m01 + m10) / s, 0.25 * s, (m12 + m21) / s, (m02 - m20) / s)
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        ((m02 + m20) / s, (m12 + m21) / s, 0.25 * s, (m10 - m01) / s)
    };
    qnorm(q)
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let out_path = std::env::args().nth(2).unwrap();
    let opts = ufbx::LoadOpts {
        target_axes: CoordinateAxes {
            right: CoordinateAxis::PositiveX,
            up: CoordinateAxis::PositiveY,
            front: CoordinateAxis::PositiveZ,
        },
        target_unit_meters: 1.0,
        ..Default::default()
    };
    let scene = ufbx::load_file(&path, opts).unwrap();
    let mut o = String::new();
    let mut idx_of: HashMap<u32, usize> = HashMap::new();
    for (i, n) in scene.nodes.iter().enumerate() {
        idx_of.insert(n.element.element_id, i);
    }
    let mut name_of: Vec<String> = scene.nodes.iter().map(|n| n.element.name.to_string()).collect();
    for (i, nm) in name_of.iter_mut().enumerate() {
        if nm.is_empty() {
            *nm = format!("node{i}");
        }
    }
    let parent_of: Vec<Option<usize>> = scene
        .nodes
        .iter()
        .map(|n| n.parent.as_ref().and_then(|p| idx_of.get(&p.element.element_id).copied()))
        .collect();

    // mesh 节点父链
    o.push_str("== mesh nodes ==\n");
    for n in scene.nodes.iter() {
        if n.mesh.is_none() {
            continue;
        }
        let me = idx_of[&n.element.element_id];
        let mut chain = Vec::new();
        let mut cur = Some(me);
        while let Some(c) = cur {
            chain.push(name_of[c].clone());
            cur = parent_of[c];
        }
        o.push_str(&format!("mesh `{}` ({}) chain: {}\n", name_of[me], n.element.name, chain.join(" < ")));
    }

    // 骨骼集合
    let mut bones: HashSet<usize> = HashSet::new();
    let mut gs: Vec<((f64, f64, f64, f64), [f64; 3], f64)> = Vec::new();
    for m in scene.meshes.iter() {
        for def in m.skin_deformers.iter() {
            for c in def.clusters.iter() {
                let Some(b) = c.bone_node.as_ref() else { continue };
                let bi = idx_of[&b.element.element_id];
                bones.insert(bi);
                let nw = util::mat4_from_ufbx(&b.node_to_world);
                let bw = util::mat4_from_ufbx(&c.bind_to_world);
                let g = util::mat4_mul(&bw, &util::mat4_invert(&nw));
                gs.push((mat_to_q(&g), [g[12] as f64, g[13] as f64, g[14] as f64], 0.0));
            }
        }
    }
    // 骨骼根：骨骼节点里父不在骨骼集合的
    let mut roots: Vec<usize> = bones
        .iter()
        .copied()
        .filter(|i| match parent_of[*i] {
            Some(p) => !bones.contains(&p),
            None => true,
        })
        .collect();
    roots.sort();
    o.push_str(&format!("\n== bones: {} roots: {:?} ==\n", bones.len(), roots.iter().map(|r| name_of[*r].clone()).collect::<Vec<_>>()));
    for r in &roots {
        let chain: Vec<String> = {
            let mut v = Vec::new();
            let mut cur = Some(*r);
            while let Some(c) = cur {
                v.push(name_of[c].clone());
                cur = parent_of[c];
            }
            v
        };
        // 该根子树里是否含 mesh 节点
        let mut subtree_meshes = Vec::new();
        for n in scene.nodes.iter() {
            if n.mesh.is_none() {
                continue;
            }
            let me = idx_of[&n.element.element_id];
            let mut cur = Some(me);
            while let Some(c) = cur {
                if c == *r {
                    subtree_meshes.push(name_of[me].clone());
                    break;
                }
                cur = parent_of[c];
            }
        }
        o.push_str(&format!("root `{}` chain: {} | subtree meshes: {:?}\n", name_of[*r], chain.join(" < "), subtree_meshes));
    }

    // medoid 旋转
    if !gs.is_empty() {
        let qs: Vec<(f64, f64, f64, f64)> = gs.iter().map(|g| g.0).collect();
        let mut best = 0usize;
        let mut best_cost = f64::MAX;
        for i in 0..qs.len() {
            let cost: f64 = qs.iter().map(|q| qangle(qs[i], *q)).sum();
            if cost < best_cost {
                best_cost = cost;
                best = i;
            }
        }
        let med = qs[best];
        let tmed = {
            let mut t = [0.0f64; 3];
            for g in &gs {
                for k in 0..3 {
                    t[k] += g.1[k];
                }
            }
            [t[0] / gs.len() as f64, t[1] / gs.len() as f64, t[2] / gs.len() as f64]
        };
        o.push_str(&format!(
            "\nM(medoid) q=({:.5},{:.5},{:.5},{:.5}) angle_vs_all: mean={:.2} max={:.2}\n",
            med.0, med.1, med.2, med.3,
            gs.iter().map(|g| qangle(med, g.0)).sum::<f64>() / gs.len() as f64,
            gs.iter().map(|g| qangle(med, g.0)).fold(0.0, f64::max)
        ));
        o.push_str(&format!("t mean=({:.6},{:.6},{:.6}) spread max={:.6}\n", tmed[0], tmed[1], tmed[2],
            gs.iter().map(|g| ((g.1[0]-tmed[0]).powi(2)+(g.1[1]-tmed[1]).powi(2)+(g.1[2]-tmed[2]).powi(2)).sqrt()).fold(0.0, f64::max)));
        // 旋转轴/角
        let w = med.3.clamp(-1.0, 1.0);
        let ang = 2.0 * w.acos().to_degrees();
        let s = (1.0 - w * w).sqrt();
        let axis = if s < 1e-6 { [0.0, 0.0, 0.0] } else { [med.0 / s, med.1 / s, med.2 / s] };
        o.push_str(&format!("axis=({:.4},{:.4},{:.4}) angle={:.2}deg\n", axis[0], axis[1], axis[2], ang));
        // 若 M 前乘到根，骨骼世界位置会到哪
        o.push_str(&format!("suggest: root-local <- M * root-local ; IBM = inv(M * bone_world) * mesh_node_world\n"));
        // 也给出 root 节点现在的 local（用 ufbx node_to_parent）
        for r in &roots {
            let n = &scene.nodes[*r];
            let l = util::mat4_from_ufbx(if n.parent.is_some() { &n.node_to_parent } else { &n.node_to_world });
            o.push_str(&format!("root `{}` local t=({:.4},{:.4},{:.4}) decompose()={:?}\n", name_of[*r], l[12], l[13], l[14], util::decompose(&l)));
        }
    }
    std::fs::File::create(&out_path).unwrap().write_all(o.as_bytes()).unwrap();
    println!("ok");
}
