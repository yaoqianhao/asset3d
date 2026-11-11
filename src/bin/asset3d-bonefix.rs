//! temp diag: 验证「骨架轴修正 M + IBM=inv(bind_to_world)*mesh_world」这一改法的正确性。
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

type Q = (f64, f64, f64, f64);

fn qnorm(q: Q) -> Q {
    let n = (q.0 * q.0 + q.1 * q.1 + q.2 * q.2 + q.3 * q.3).sqrt().max(1e-12);
    (q.0 / n, q.1 / n, q.2 / n, q.3 / n)
}
fn qangle(a: Q, b: Q) -> f64 {
    let d = (a.0 * b.0 + a.1 * b.1 + a.2 * b.2 + a.3 * b.3).abs().clamp(0.0, 1.0);
    2.0 * d.acos().to_degrees()
}
fn qmul(a: Q, b: Q) -> Q {
    (
        a.3 * b.0 + a.0 * b.3 + a.1 * b.2 - a.2 * b.1,
        a.3 * b.1 - a.0 * b.2 + a.1 * b.3 + a.2 * b.0,
        a.3 * b.2 + a.0 * b.1 - a.1 * b.0 + a.2 * b.3,
        a.3 * b.3 - a.0 * b.0 - a.1 * b.1 - a.2 * b.2,
    )
}
fn qrot(q: Q, v: [f64; 3]) -> [f64; 3] {
    let (x, y, z, w) = q;
    let t = [
        2.0 * (y * v[2] - z * v[1]),
        2.0 * (z * v[0] - x * v[2]),
        2.0 * (x * v[1] - y * v[0]),
    ];
    [
        v[0] + w * t[0] + (y * t[2] - z * t[1]),
        v[1] + w * t[1] + (z * t[0] - x * t[2]),
        v[2] + w * t[2] + (x * t[1] - y * t[0]),
    ]
}
fn mat_to_q_rot(m: &[f32; 16]) -> Q {
    let c0 = [m[0] as f64, m[1] as f64, m[2] as f64];
    let mut c1 = [m[4] as f64, m[5] as f64, m[6] as f64];
    let norm = |v: [f64; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let s0 = norm(c0).max(1e-12);
    let n0 = [c0[0] / s0, c0[1] / s0, c0[2] / s0];
    let d = n0[0] * c1[0] + n0[1] * c1[1] + n0[2] * c1[2];
    c1 = [c1[0] - d * n0[0], c1[1] - d * n0[1], c1[2] - d * n0[2]];
    let s1 = norm(c1).max(1e-12);
    let n1 = [c1[0] / s1, c1[1] / s1, c1[2] / s1];
    let n2 = [
        n0[1] * n1[2] - n0[2] * n1[1],
        n0[2] * n1[0] - n0[0] * n1[2],
        n0[0] * n1[1] - n0[1] * n1[0],
    ];
    let (m00, m10, m20) = (n0[0], n0[1], n0[2]);
    let (m01, m11, m21) = (n1[0], n1[1], n1[2]);
    let (m02, m12, m22) = (n2[0], n2[1], n2[2]);
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
fn aabb_of(points: impl Iterator<Item = [f64; 3]>) -> ([f64; 3], [f64; 3], usize) {
    let mut mn = [f64::MAX; 3];
    let mut mx = [f64::MIN; 3];
    let mut n = 0;
    for p in points {
        for k in 0..3 {
            mn[k] = mn[k].min(p[k]);
            mx[k] = mx[k].max(p[k]);
        }
        n += 1;
    }
    (mn, mx, n)
}
fn inside(mn: &[f64; 3], mx: &[f64; 3], p: [f64; 3]) -> bool {
    (0..3).all(|k| p[k] >= mn[k] - 1e-4 && p[k] <= mx[k] + 1e-4)
}
fn dist_box(mn: &[f64; 3], mx: &[f64; 3], p: [f64; 3]) -> f64 {
    let mut s = 0.0;
    for k in 0..3 {
        let d = if p[k] < mn[k] {
            mn[k] - p[k]
        } else if p[k] > mx[k] {
            p[k] - mx[k]
        } else {
            0.0
        };
        s += d * d;
    }
    s.sqrt()
}

struct Cl {
    bone: String,
    node_world: [f32; 16],
    bind: [f32; 16],
    weight_sum: f64,
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let out_path = std::env::args().nth(2).unwrap();
    let mut o = String::new();
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

    let mut idx_of: HashMap<u32, usize> = HashMap::new();
    for (i, n) in scene.nodes.iter().enumerate() {
        idx_of.insert(n.element.element_id, i);
    }
    let name_of: Vec<String> = scene.nodes.iter().enumerate().map(|(i, n)| {
        let s = n.element.name.to_string();
        if s.is_empty() { format!("node{i}") } else { s }
    }).collect();
    let parent_of: Vec<Option<usize>> = scene
        .nodes
        .iter()
        .map(|n| n.parent.as_ref().and_then(|p| idx_of.get(&p.element.element_id).copied()))
        .collect();

    // ---- cluster 收集 ----
    let mut cls: Vec<Cl> = Vec::new();
    for m in scene.meshes.iter() {
        for def in m.skin_deformers.iter() {
            for c in def.clusters.iter() {
                let Some(b) = c.bone_node.as_ref() else { continue };
                cls.push(Cl {
                    bone: b.element.name.to_string(),
                    node_world: util::mat4_from_ufbx(&b.node_to_world),
                    bind: util::mat4_from_ufbx(&c.bind_to_world),
                    weight_sum: c.weights.iter().sum(),
                });
            }
        }
    }
    o.push_str(&format!("clusters = {}\n", cls.len()));

    // ---- 加权中位四元数 M ----
    let qs: Vec<(Q, f64)> = cls
        .iter()
        .map(|c| (mat_to_q_rot(&util::mat4_mul(&c.bind, &util::mat4_invert(&c.node_world))), c.weight_sum))
        .collect();
    let mut best = 0usize;
    let mut best_cost = f64::MAX;
    for i in 0..qs.len() {
        let cost: f64 = qs.iter().map(|(q, w)| qangle(qs[i].0, *q) * w).sum();
        if cost < best_cost {
            best_cost = cost;
            best = i;
        }
    }
    let med = qs[best].0;
    let w_med = med.3.clamp(-1.0, 1.0);
    let ang = 2.0 * w_med.acos().to_degrees();
    let s = (1.0 - w_med * w_med).sqrt().max(1e-12);
    o.push_str(&format!(
        "\nM(weighted medoid) q=({:.6},{:.6},{:.6},{:.6}) axis=({:.3},{:.3},{:.3}) angle={:.3} deg\n",
        med.0, med.1, med.2, med.3, med.0 / s, med.1 / s, med.2 / s, ang
    ));
    let mut all: Vec<f64> = qs.iter().map(|(q, _)| qangle(med, *q)).collect();
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut wq: Vec<f64> = qs.iter().filter(|(_, w)| *w > 1e-6).map(|(q, _)| qangle(med, *q)).collect();
    wq.sort_by(|a, b| a.partial_cmp(b).unwrap());
    o.push_str(&format!(
        "M vs all clusters(n={}): med={:.2} p90={:.2} max={:.2}\nM vs weighted clusters(n={}): med={:.2} p90={:.2} max={:.2}\n",
        all.len(), all[all.len() / 2], all[all.len() * 9 / 10], all[all.len() - 1],
        wq.len(), wq[wq.len() / 2], wq[wq.len() * 9 / 10], wq[wq.len() - 1]
    ));

    // ---- 网格世界 AABB / 骨骼 AABB ----
    let mut mesh_pts: Vec<[f64; 3]> = Vec::new();
    for n in scene.nodes.iter() {
        if n.mesh.is_none() {
            continue;
        }
        let xf = util::mat4_from_ufbx(&n.node_to_world);
        if let Some(m) = n.mesh.as_ref() {
            for v in m.vertices.iter() {
                let p = util::transform_point(&xf, [v.x as f32, v.y as f32, v.z as f32]);
                mesh_pts.push([p[0] as f64, p[1] as f64, p[2] as f64]);
            }
        }
    }
    let ma = aabb_of(mesh_pts.iter().copied());
    o.push_str(&format!(
        "\nmesh world AABB: min=({:.4},{:.4},{:.4}) max=({:.4},{:.4},{:.4}) verts={}\n",
        ma.0[0], ma.0[1], ma.0[2], ma.1[0], ma.1[1], ma.1[2], ma.2
    ));

    let mut uniq: HashMap<String, [f64; 3]> = HashMap::new();
    for c in cls.iter() {
        uniq.insert(c.bone.clone(), [c.node_world[12] as f64, c.node_world[13] as f64, c.node_world[14] as f64]);
    }
    let orig_pts: Vec<[f64; 3]> = uniq.values().copied().collect();
    let oa = aabb_of(orig_pts.iter().copied());
    let fix_pts: Vec<[f64; 3]> = orig_pts.iter().map(|p| qrot(med, *p)).collect();
    let fa = aabb_of(fix_pts.iter().copied());
    o.push_str(&format!(
        "bone AABB orig  : min=({:.4},{:.4},{:.4}) max=({:.4},{:.4},{:.4}) n={}\n",
        oa.0[0], oa.0[1], oa.0[2], oa.1[0], oa.1[1], oa.1[2], oa.2
    ));
    o.push_str(&format!(
        "bone AABB M-fix : min=({:.4},{:.4},{:.4}) max=({:.4},{:.4},{:.4})\n",
        fa.0[0], fa.0[1], fa.0[2], fa.1[0], fa.1[1], fa.1[2]
    ));
    let in_o = orig_pts.iter().filter(|p| inside(&ma.0, &ma.1, **p)).count();
    let in_f = fix_pts.iter().filter(|p| inside(&ma.0, &ma.1, **p)).count();
    o.push_str(&format!(
        "bones inside mesh AABB: orig {}/{}, M-fixed {}/{}\n",
        in_o, orig_pts.len(), in_f, orig_pts.len()
    ));
    let od: Vec<f64> = orig_pts.iter().map(|p| dist_box(&ma.0, &ma.1, *p)).collect();
    let fd: Vec<f64> = fix_pts.iter().map(|p| dist_box(&ma.0, &ma.1, *p)).collect();
    let avg = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
    let mxv = |v: &Vec<f64>| v.iter().fold(0.0f64, |a, b| a.max(*b));
    o.push_str(&format!(
        "avg/max dist to mesh box: orig {:.4}/{:.4} | M-fixed {:.4}/{:.4}\n",
        avg(&od), mxv(&od), avg(&fd), mxv(&fd)
    ));
    // 竖直方向确认：骨 AABB 哪个轴最长
    let ext = |a: &([f64; 3], [f64; 3], usize)| [a.1[0] - a.0[0], a.1[1] - a.0[1], a.1[2] - a.0[2]];
    o.push_str(&format!(
        "extent orig x/y/z = {:.4}/{:.4}/{:.4} | M-fixed = {:.4}/{:.4}/{:.4}\n",
        ext(&oa)[0], ext(&oa)[1], ext(&oa)[2], ext(&fa)[0], ext(&fa)[1], ext(&fa)[2]
    ));

    // ---- M*node_world vs bind 残差（仅有权重骨骼）----
    let mut ra = Vec::new();
    let mut rp = Vec::new();
    for c in cls.iter().filter(|c| c.weight_sum > 1e-6) {
        let nwq = mat_to_q_rot(&c.node_world);
        let bq = mat_to_q_rot(&c.bind);
        ra.push(qangle(qmul(med, nwq), bq));
        let p1 = qrot(med, [c.node_world[12] as f64, c.node_world[13] as f64, c.node_world[14] as f64]);
        let p2 = [c.bind[12] as f64, c.bind[13] as f64, c.bind[14] as f64];
        rp.push(((p1[0] - p2[0]).powi(2) + (p1[1] - p2[1]).powi(2) + (p1[2] - p2[2]).powi(2)).sqrt());
    }
    let st = |v: &Vec<f64>| {
        let mut s = v.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (s[s.len() / 2], s[s.len() * 9 / 10], s[s.len() - 1])
    };
    let (a1, a2, a3) = st(&ra);
    let (p1, p2, p3) = st(&rp);
    o.push_str(&format!(
        "\nM*node_world vs bind_to_world (weighted n={}):\n  angle med={:.3} p90={:.3} max={:.3} deg\n  pos   med={:.4} p90={:.4} max={:.4} m\n",
        ra.len(), a1, a2, a3, p1, p2, p3
    ));
    // 未修正时（M=I）的对照
    let mut ra0: Vec<f64> = Vec::new();
    for c in cls.iter().filter(|c| c.weight_sum > 1e-6) {
        ra0.push(qangle(mat_to_q_rot(&c.node_world), mat_to_q_rot(&c.bind)));
    }
    let (z1, z2, z3) = st(&ra0);
    o.push_str(&format!("  baseline (no M) angle med={:.2} p90={:.2} max={:.2} deg\n", z1, z2, z3));

    // ---- 静态蒙皮恒等式：palette(bind pose)=bind*inv(bind)*mesh_world=mesh_world ----
    let mut maxdev = 0.0f64;
    for n in scene.nodes.iter() {
        if n.mesh.is_none() {
            continue;
        }
        let mesh_world = util::mat4_from_ufbx(&n.node_to_world);
        if let Some(m) = n.mesh.as_ref() {
            for def in m.skin_deformers.iter() {
                for c in def.clusters.iter() {
                    let bind = util::mat4_from_ufbx(&c.bind_to_world);
                    let ibm = util::mat4_mul(&util::mat4_invert(&bind), &mesh_world);
                    let pal = util::mat4_mul(&bind, &ibm);
                    for k in 0..16 {
                        maxdev = maxdev.max((pal[k] - mesh_world[k]).abs() as f64);
                    }
                }
            }
        }
    }
    o.push_str(&format!("static skin identity max abs dev = {:.3e}\n", maxdev));

    // ---- 骨骼根层级 ----
    let mut boneset: HashSet<usize> = HashSet::new();
    for c in cls.iter() {
        for (i, nm) in name_of.iter().enumerate() {
            if *nm == c.bone {
                boneset.insert(i);
            }
        }
    }
    let mut roots: Vec<usize> = boneset
        .iter()
        .copied()
        .filter(|i| match parent_of[*i] {
            Some(pp) => !boneset.contains(&pp),
            None => true,
        })
        .collect();
    roots.sort();
    o.push_str(&format!(
        "\nskeleton roots: {:?} (parent={:?})\n",
        roots.iter().map(|r| name_of[*r].clone()).collect::<Vec<_>>(),
        roots.iter().map(|r| parent_of[*r].map(|p| name_of[p].clone())).collect::<Vec<_>>()
    ));
    for r in &roots {
        let mut chain = Vec::new();
        let mut cur = Some(*r);
        while let Some(c) = cur {
            chain.push(name_of[c].clone());
            cur = parent_of[c];
        }
        o.push_str(&format!("root chain: {}\n", chain.join(" < ")));
    }

    // ---- 动画帧检查 ----
    o.push_str("\n== animation frames ==\n");
    let root_id = scene.nodes.iter().find(|x| x.element.name == "root").map(|x| x.element.element_id);
    for stack in scene.anim_stacks.iter() {
        let bopts = ufbx::BakeOpts {
            resample_rate: 30.0,
            trim_start_time: true,
            key_reduction_enabled: true,
            key_reduction_threshold: 1e-4,
            ..Default::default()
        };
        let baked = match ufbx::bake_anim(&scene, &stack.anim, bopts) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut max_keys = 0usize;
        let mut max_t: f64 = 0.0;
        for n in baked.nodes.iter() {
            if !n.rotation_keys.is_empty() {
                max_keys = max_keys.max(n.rotation_keys.len());
                if let Some(k) = n.rotation_keys.last() {
                    max_t = max_t.max(k.time);
                }
            }
            if !n.translation_keys.is_empty() {
                max_keys = max_keys.max(n.translation_keys.len());
                if let Some(k) = n.translation_keys.last() {
                    max_t = max_t.max(k.time);
                }
            }
        }
        o.push_str(&format!(
            "stack `{}`: baked_nodes={} max_keys={} max_time={:.4}s\n",
            stack.element.name, baked.nodes.len(), max_keys, max_t
        ));
        if let Some(rid) = root_id {
            if let Some(bn) = baked.nodes.iter().find(|n| n.element_id == rid) {
                o.push_str(&format!(
                    "  skeleton root `root`: rot_keys={} trans_keys={} scale_keys={}\n",
                    bn.rotation_keys.len(), bn.translation_keys.len(), bn.scale_keys.len()
                ));
                if !bn.rotation_keys.is_empty() {
                    let q0 = (bn.rotation_keys[0].value.x as f64, bn.rotation_keys[0].value.y as f64, bn.rotation_keys[0].value.z as f64, bn.rotation_keys[0].value.w as f64);
                    let mut amax: f64 = 0.0;
                    for k in bn.rotation_keys.iter() {
                        let q = (k.value.x as f64, k.value.y as f64, k.value.z as f64, k.value.w as f64);
                        amax = amax.max(qangle(q0, q));
                    }
                    o.push_str(&format!("  root rot variation max={:.4} deg\n", amax));
                }
                if !bn.translation_keys.is_empty() {
                    let t0 = &bn.translation_keys[0].value;
                    let mut dmax: f64 = 0.0;
                    for k in bn.translation_keys.iter() {
                        dmax = dmax.max(((k.value.x - t0.x).powi(2) + (k.value.y - t0.y).powi(2) + (k.value.z - t0.z).powi(2)).sqrt() as f64);
                    }
                    o.push_str(&format!("  root trans variation max={:.6} m\n", dmax));
                }
            }
        }
        let mut max_angle: f64 = 0.0;
        let mut max_pos: f64 = 0.0;
        let mut moved = 0usize;
        for n in baked.nodes.iter() {
            if n.rotation_keys.len() >= 2 {
                let q0 = (n.rotation_keys[0].value.x as f64, n.rotation_keys[0].value.y as f64, n.rotation_keys[0].value.z as f64, n.rotation_keys[0].value.w as f64);
                let mut lm: f64 = 0.0;
                for k in n.rotation_keys.iter() {
                    let q = (k.value.x as f64, k.value.y as f64, k.value.z as f64, k.value.w as f64);
                    lm = lm.max(qangle(q0, q));
                }
                if lm > 1e-3 {
                    moved += 1;
                }
                max_angle = max_angle.max(lm);
            }
            if n.translation_keys.len() >= 2 {
                let t0 = n.translation_keys[0].value;
                let mut lm: f64 = 0.0;
                for k in n.translation_keys.iter() {
                    lm = lm.max(((k.value.x - t0.x).powi(2) + (k.value.y - t0.y).powi(2) + (k.value.z - t0.z).powi(2)).sqrt() as f64);
                }
                if lm > 1e-5 {
                    moved += 1;
                }
                max_pos = max_pos.max(lm);
            }
        }
        o.push_str(&format!(
            "  whole clip: nodes_with_motion={} max_rot_delta={:.4} deg max_trans_delta={:.6} m\n",
            moved, max_angle, max_pos
        ));
    }

    std::fs::File::create(&out_path).unwrap().write_all(o.as_bytes()).unwrap();
    println!("ok");
}
