//! temp diag 4: bone/mesh axis mismatch, corrected-skeleton proposal check.
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

fn skinned(doc: &air::AirDoc, posed: &[air::AirNode], mesh_index: usize) -> Vec<[f32; 3]> {
    let mesh = &doc.meshes[mesh_index];
    let Some(skin_index) = mesh.skin else { return Vec::new() };
    let skin = &doc.skins[skin_index as usize];
    let palettes: Vec<[f32; 16]> = skin
        .joints
        .iter()
        .zip(skin.inverse_bind_matrices.iter())
        .map(|(j, ibm)| util::mat4_mul(&world_of(posed, *j as usize), ibm))
        .collect();
    mesh.positions
        .iter()
        .enumerate()
        .map(|(i, p)| {
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
            acc
        })
        .collect()
}

fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

fn aabb(points: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut mn = [f32::INFINITY; 3];
    let mut mx = [f32::NEG_INFINITY; 3];
    for p in points {
        for a in 0..3 {
            mn[a] = mn[a].min(p[a]);
            mx[a] = mx[a].max(p[a]);
        }
    }
    (mn, mx)
}

fn main() {
    let path = std::env::args().nth(1).expect("need fbx path");
    let doc = export_core::import_doc(std::path::Path::new(&path)).expect("import failed");
    let nodes = &doc.nodes;
    let mut out = String::new();

    // 1 mesh local-space AABB (is geometry Y-up?)
    out.push_str("## mesh local-space AABB (geometry axis convention)\n");
    for (mi, mesh) in doc.meshes.iter().enumerate() {
        let (mn, mx) = aabb(&mesh.positions);
        out.push_str(&format!(
            "mesh#{mi} `{}` local min=({:.2},{:.2},{:.2}) max=({:.2},{:.2},{:.2}) size=({:.2},{:.2},{:.2})\n",
            mesh.name, mn[0], mn[1], mn[2], mx[0], mx[1], mx[2],
            mx[0] - mn[0], mx[1] - mn[1], mx[2] - mn[2]
        ));
    }

    // 2 bone world AABB vs mesh world AABB
    let mut bone_pts: Vec<[f32; 3]> = Vec::new();
    for s in &doc.skins {
        for j in &s.joints {
            let w = world_of(nodes, *j as usize);
            bone_pts.push([w[12], w[13], w[14]]);
        }
    }
    let (bmn, bmx) = aabb(&bone_pts);
    out.push_str(&format!(
        "\nbone world AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3})\n",
        bmn[0], bmn[1], bmn[2], bmx[0], bmx[1], bmx[2]
    ));
    let mut all: Vec<[f32; 3]> = Vec::new();
    for mi in 0..doc.meshes.len() {
        all.extend(skinned(&doc, nodes, mi));
    }
    let (smn, smx) = aabb(&all);
    out.push_str(&format!(
        "skinned mesh world AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3})\n",
        smn[0], smn[1], smn[2], smx[0], smx[1], smx[2]
    ));

    // 3 apply -90deg X rotation to root, recompute bone worlds
    let root_idx = nodes.iter().position(|n| n.name == "root").unwrap();
    let mut fixed = nodes.clone();
    let half = (-45.0f32 * std::f32::consts::PI / 180.0).sin();
    let c = (-45.0f32 * std::f32::consts::PI / 180.0).cos();
    let qx = [half, 0.0, 0.0, c];
    let q0 = fixed[root_idx].rotation;
    fixed[root_idx].rotation = quat_mul(q0, qx);
    let mut fpts: Vec<[f32; 3]> = Vec::new();
    for j in ["pelvis", "spine_03", "head", "hand_l", "foot_l", "ball_l"] {
        if let Some(i) = nodes.iter().position(|n| n.name == j) {
            let w = world_of(&fixed, i);
            fpts.push([w[12], w[13], w[14]]);
            out.push_str(&format!(
                "after -90X on root: `{j}` world=({:.3},{:.3},{:.3})\n",
                w[12], w[13], w[14]
            ));
        }
    }

    // 4 pivot test: pelvis +30deg local Z on original vs fixed skeleton
    let pelvis = nodes.iter().position(|n| n.name == "pelvis").unwrap();
    let base = skinned(&doc, nodes, 2);
    for (tag, base_nodes) in [("orig", nodes.clone()), ("fixed", fixed.clone())] {
        let mut posed = base_nodes.clone();
        let q0 = posed[pelvis].rotation;
        let half = (15.0f32 * std::f32::consts::PI / 180.0).sin();
        let c = (1.0 - half * half).sqrt();
        posed[pelvis].rotation = quat_mul(q0, [0.0, 0.0, half, c]);
        let moved = skinned(&doc, &posed, 2);
        let pw = world_of(&base_nodes, pelvis);
        let mut maxd = 0.0f32;
        let mut sumd = 0.0f64;
        for i in 0..base.len() {
            let d = ((moved[i][0] - base[i][0]).powi(2)
                + (moved[i][1] - base[i][1]).powi(2)
                + (moved[i][2] - base[i][2]).powi(2))
            .sqrt();
            maxd = maxd.max(d);
            sumd += d as f64;
        }
        let (mn, mx) = aabb(&moved);
        out.push_str(&format!(
            "{tag}: pelvis pivot=({:.3},{:.3},{:.3}) +30Z -> max vert move={:.3}m mean={:.4}m, AABB min=({:.2},{:.2},{:.2}) max=({:.2},{:.2},{:.2})\n",
            pw[12], pw[13], pw[14], maxd, sumd / base.len() as f64,
            mn[0], mn[1], mn[2], mx[0], mx[1], mx[2]
        ));
    }

    let target = r"C:\Users\Nero\AppData\Roaming\Tencent\Marvis\User\oAN1i2dpCCuZq-FgvYnflZ6SBnQA\workspace\conv_776800051b6f46619c993787b3340447\temp\skinreport4.txt";
    std::fs::File::create(target).unwrap().write_all(out.as_bytes()).unwrap();
    println!("ok");
}
