//! temp diag 2: bone vs mesh coordinate system, animation skinning sanity.
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

use std::collections::HashSet;
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
    let local = trs(&nodes[index]);
    match nodes[index].parent {
        Some(p) => util::mat4_mul(&world_of(nodes, p as usize), &local),
        None => local,
    }
}

fn rot_angle_deg(a: &[f32; 16], b: &[f32; 16]) -> f64 {
    let mut tr = 0.0f64;
    for i in 0..3 {
        for k in 0..3 {
            tr += (a[k * 4 + i] as f64) * (b[k * 4 + i] as f64);
        }
    }
    ((tr - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
}

fn axis_of(m: &[f32; 16]) -> (f64, f64, f64) {
    let l = |i: usize| (m[i].powi(2) + m[i + 1].powi(2) + m[i + 2].powi(2)).sqrt() as f64;
    (l(0), l(4), l(8))
}

fn pose_at(nodes: &[air::AirNode], anim: &air::AirAnimation, t: f32) -> Vec<air::AirNode> {
    let mut posed = nodes.to_vec();
    for track in &anim.tracks {
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
        let u = if t1 > t0 { ((t - t0) / (t1 - t0)).clamp(0.0, 1.0) } else { 0.0 };
        let lerp = |a: f32, b: f32| a + (b - a) * u;
        let node = &mut posed[track.node as usize];
        match track.property.as_str() {
            "translation" => node.position = [lerp(v0[0], v1[0]), lerp(v0[1], v1[1]), lerp(v0[2], v1[2])],
            "scale" => node.scale = [lerp(v0[0], v1[0]), lerp(v0[1], v1[1]), lerp(v0[2], v1[2])],
            "rotation" => {
                let mut q1 = [v1[0], v1[1], v1[2], v1[3]];
                if v0[0] * q1[0] + v0[1] * q1[1] + v0[2] * q1[2] + v0[3] * q1[3] < 0.0 {
                    for v in q1.iter_mut() {
                        *v = -*v;
                    }
                }
                let q = [lerp(v0[0], q1[0]), lerp(v0[1], q1[1]), lerp(v0[2], q1[2]), lerp(v0[3], q1[3])];
                node.rotation = util::normalize_quat(q);
            }
            _ => {}
        }
    }
    posed
}

fn palettes_of(doc: &air::AirDoc, posed: &[air::AirNode], mesh_index: usize) -> Option<Vec<[f32; 16]>> {
    let mesh = &doc.meshes[mesh_index];
    let skin = &doc.skins[mesh.skin? as usize];
    Some(
        skin.joints
            .iter()
            .zip(skin.inverse_bind_matrices.iter())
            .map(|(j, ibm)| util::mat4_mul(&world_of(posed, *j as usize), ibm))
            .collect(),
    )
}

fn skinned_aabb(doc: &air::AirDoc, posed: &[air::AirNode], mesh_index: usize) -> Option<([f32; 3], [f32; 3])> {
    let mesh = &doc.meshes[mesh_index];
    let palettes = palettes_of(doc, posed, mesh_index)?;
    let mut mn = [f32::INFINITY; 3];
    let mut mx = [f32::NEG_INFINITY; 3];
    for (i, p) in mesh.positions.iter().enumerate() {
        let mut acc = [0.0f32; 3];
        for k in 0..4 {
            let w = mesh.weights[i][k];
            if w <= 0.0 {
                continue;
            }
            let q = util::transform_point(&palettes[mesh.joints[i][k] as usize], *p);
            acc[0] += q[0] * w;
            acc[1] += q[1] * w;
            acc[2] += q[2] * w;
        }
        for a in 0..3 {
            mn[a] = mn[a].min(acc[a]);
            mx[a] = mx[a].max(acc[a]);
        }
    }
    Some((mn, mx))
}

fn centroid_of_dominant(
    doc: &air::AirDoc,
    posed: &[air::AirNode],
    mesh_index: usize,
    bone_node: u32,
) -> Option<([f32; 3], usize)> {
    let mesh = &doc.meshes[mesh_index];
    let palettes = palettes_of(doc, posed, mesh_index)?;
    let skin = &doc.skins[mesh.skin? as usize];
    let slot = skin.joints.iter().position(|j| *j == bone_node)?;
    let mut acc = [0.0f64; 3];
    let mut n = 0usize;
    for (i, p) in mesh.positions.iter().enumerate() {
        let mut best = (0usize, 0.0f32);
        for k in 0..4 {
            if mesh.weights[i][k] > best.1 {
                best = (k, mesh.weights[i][k]);
            }
        }
        if mesh.joints[i][best.0] as usize != slot {
            continue;
        }
        let q = util::transform_point(&palettes[slot], *p);
        for a in 0..3 {
            acc[a] += q[a] as f64;
        }
        n += 1;
    }
    if n == 0 {
        return None;
    }
    Some((
        [
            (acc[0] / n as f64) as f32,
            (acc[1] / n as f64) as f32,
            (acc[2] / n as f64) as f32,
        ],
        n,
    ))
}

fn main() {
    let path = std::env::args().nth(1).expect("need fbx path");
    let doc = export_core::import_doc(std::path::Path::new(&path)).expect("import failed");
    let nodes = &doc.nodes;
    let mut out = String::new();

    if true {
        out.push_str("## nodes (first 20)\n");
        for (i, n) in nodes.iter().take(20).enumerate() {
            out.push_str(&format!(
                "[{}] `{}` parent={:?} mesh={:?} skin={:?} t=({:.4},{:.4},{:.4}) s=({:.4},{:.4},{:.4}) q={:?}\n",
                i, n.name, n.parent, n.mesh, n.skin, n.position[0], n.position[1], n.position[2],
                n.scale[0], n.scale[1], n.scale[2], n.rotation
            ));
        }
        out.push_str("\n## mesh node ancestor chain\n");
        for (i, n) in nodes.iter().enumerate() {
            if n.mesh.is_none() {
                continue;
            }
            out.push_str(&format!("mesh node [{}] `{}` mesh={:?}\n", i, n.name, n.mesh));
            let mut cur = Some(i as u32);
            while let Some(c) = cur {
                let w = world_of(nodes, c as usize);
                let (x, y, z) = axis_of(&w);
                out.push_str(&format!(
                    "    [{}] `{}` parent={:?} world_t=({:.4},{:.4},{:.4}) |basis|=({:.5},{:.5},{:.5})\n",
                    c, nodes[c as usize].name, nodes[c as usize].parent, w[12], w[13], w[14], x, y, z
                ));
                cur = nodes[c as usize].parent;
            }
        }
        out.push_str("\n## bone root ancestor chain\n");
        if let Some(r) = nodes.iter().position(|n| n.name == "root") {
            let mut cur = Some(r as u32);
            while let Some(c) = cur {
                let w = world_of(nodes, c as usize);
                let (x, y, z) = axis_of(&w);
                out.push_str(&format!(
                    "    [{}] `{}` parent={:?} world_t=({:.4},{:.4},{:.4}) |basis|=({:.5},{:.5},{:.5})\n",
                    c, nodes[c as usize].name, nodes[c as usize].parent, w[12], w[13], w[14], x, y, z
                ));
                cur = nodes[c as usize].parent;
            }
        }
    }

    let mut bone_set: HashSet<u32> = HashSet::new();
    for s in &doc.skins {
        for j in &s.joints {
            bone_set.insert(*j);
        }
    }

    out.push_str("\n## rotation diff: mesh node world vs bone world\n");
    for mi in 0..doc.meshes.len() {
        let Some(node_index) = nodes.iter().position(|n| n.mesh == Some(mi as u32)) else {
            continue;
        };
        let nw = world_of(nodes, node_index);
        let mut worst = (0.0f64, String::new());
        for b in &bone_set {
            let bw = world_of(nodes, *b as usize);
            let ang = rot_angle_deg(&nw, &bw);
            if ang > worst.0 {
                worst = (ang, nodes[*b as usize].name.clone());
            }
        }
        out.push_str(&format!(
            "mesh#{} node[{}] `{}` max rot diff vs bones = {:.2} deg (`{}`)\n",
            mi, node_index, nodes[node_index].name, worst.0, worst.1
        ));
    }

    out.push_str("\n## animation frame skinning check\n");
    if let Some(anim) = doc.animations.first() {
        for t in [0.0f32, 1.0, 2.0, 3.0, 4.0] {
            let posed = pose_at(nodes, anim, t);
            out.push_str(&format!("t={t:.2}s\n"));
            for mi in 0..doc.meshes.len() {
                if let Some((mn, mx)) = skinned_aabb(&doc, &posed, mi) {
                    out.push_str(&format!(
                        "  mesh#{mi} skinned AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3})\n",
                        mn[0], mn[1], mn[2], mx[0], mx[1], mx[2]
                    ));
                }
            }
            for bone_name in ["head", "hand_l", "foot_l", "pelvis"] {
                let Some(b) = nodes.iter().position(|n| n.name == bone_name) else { continue };
                let bw = world_of(&posed, b);
                let mut line = format!(
                    "  `{bone_name}` world=({:.3},{:.3},{:.3})",
                    bw[12], bw[13], bw[14]
                );
                for mi in 0..doc.meshes.len() {
                    if let Some((c, n)) = centroid_of_dominant(&doc, &posed, mi, b as u32) {
                        line.push_str(&format!(
                            " | m{} centroid=({:.3},{:.3},{:.3}) delta=({:.3},{:.3},{:.3}) n={n}",
                            mi, c[0], c[1], c[2],
                            c[0] - bw[12], c[1] - bw[13], c[2] - bw[14]
                        ));
                    }
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
    }

    out.push_str("\n## bind pose: bones vs skinned AABB\n");
    let mut mn = [f32::INFINITY; 3];
    let mut mx = [f32::NEG_INFINITY; 3];
    for mi in 0..doc.meshes.len() {
        if let Some((a, b)) = skinned_aabb(&doc, nodes, mi) {
            for k in 0..3 {
                mn[k] = mn[k].min(a[k]);
                mx[k] = mx[k].max(b[k]);
            }
        }
    }
    out.push_str(&format!(
        "all skinned AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3})\n",
        mn[0], mn[1], mn[2], mx[0], mx[1], mx[2]
    ));

    let target = r"C:\Users\Nero\AppData\Roaming\Tencent\Marvis\User\oAN1i2dpCCuZq-FgvYnflZ6SBnQA\workspace\conv_776800051b6f46619c993787b3340447\temp\skinreport2.txt";
    std::fs::File::create(target).unwrap().write_all(out.as_bytes()).unwrap();
    println!("ok");
}
