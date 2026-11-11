//! temp diag 3: animation keyframe delta + axis mismatch proof.
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

fn main() {
    let path = std::env::args().nth(1).expect("need fbx path");
    let doc = export_core::import_doc(std::path::Path::new(&path)).expect("import failed");
    let nodes = &doc.nodes;
    let mut out = String::new();

    let anim = doc.animations.first().expect("no anim");
    out.push_str(&format!(
        "anim `{}` duration={} tracks={}\n",
        anim.name,
        anim.duration,
        anim.tracks.len()
    ));
    // 关键帧时间与值差异
    let mut max_rot = 0.0f64;
    let mut max_t = 0.0f32;
    let mut max_s = 0.0f32;
    let mut moving: Vec<String> = Vec::new();
    let mut sample: Vec<String> = Vec::new();
    for track in &anim.tracks {
        if track.values.len() < 2 {
            continue;
        }
        let v0 = track.values[0];
        let v1 = track.values[track.values.len() - 1];
        let name = nodes[track.node as usize].name.clone();
        match track.property.as_str() {
            "rotation" => {
                let dot = (v0[0] * v1[0] + v0[1] * v1[1] + v0[2] * v1[2] + v0[3] * v1[3]).abs() as f64;
                let ang = (2.0 * dot.clamp(-1.0, 1.0).acos()).to_degrees();
                if ang > 1.0 {
                    moving.push(format!("`{name}` rot {ang:.2} deg"));
                }
                max_rot = max_rot.max(ang);
            }
            "translation" => {
                let d = ((v0[0] - v1[0]).powi(2) + (v0[1] - v1[1]).powi(2) + (v0[2] - v1[2]).powi(2)).sqrt();
                if d > 0.5 {
                    moving.push(format!("`{name}` trans {d:.2}"));
                }
                max_t = max_t.max(d);
            }
            "scale" => {
                let d = (v0[0] - v1[0]).abs().max((v0[1] - v1[1]).abs()).max((v0[2] - v1[2]).abs());
                if d > 1e-4 {
                    moving.push(format!("`{name}` scale {d:.4}"));
                }
                max_s = max_s.max(d);
            }
            _ => {}
        }
        if sample.len() < 6 {
            sample.push(format!(
                "`{name}` {} times={:?} v0={:?} vN={:?}",
                track.property, track.times, v0, v1
            ));
        }
    }
    out.push_str(&format!(
        "首末关键帧最大差异: rot={max_rot:.3} deg, trans={max_t:.3}, scale={max_s:.5}\n"
    ));
    out.push_str(&format!("明显运动的轨道 {} 条（阈值 rot>1deg）\n", moving.len()));
    for m in moving.iter().take(30) {
        out.push_str(&format!("  {m}\n"));
    }
    out.push_str("sample tracks:\n");
    for s in &sample {
        out.push_str(&format!("  {s}\n"));
    }

    // 骨骼名单
    let mut bones: Vec<String> = Vec::new();
    if let Some(skin) = doc.skins.get(doc.meshes[2].skin.unwrap() as usize) {
        for j in &skin.joints {
            bones.push(nodes[*j as usize].name.clone());
        }
    }
    out.push_str(&format!("\nskin#2 joints({}): {}\n", bones.len(), bones.join(", ")));
    let mut ik: Vec<String> = nodes
        .iter()
        .filter(|n| n.name.starts_with("ik_"))
        .map(|n| n.name.clone())
        .collect();
    ik.sort();
    out.push_str(&format!("ik nodes: {}\n", ik.join(", ")));
    let skinned_bones: std::collections::HashSet<u32> = doc
        .skins
        .iter()
        .flat_map(|s| s.joints.iter().copied())
        .collect();
    out.push_str(&format!(
        "ik nodes in skin? {}\n",
        nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.name.starts_with("ik_"))
            .map(|(i, n)| format!("{}={}", n.name, skinned_bones.contains(&(i as u32))))
            .collect::<Vec<_>>()
            .join(", ")
    ));

    // 错轴验证：给 pelvis 施加绕其局部 Z 轴 +30 度，看网格顶点是绕世界哪个轴转
    let pelvis = nodes.iter().position(|n| n.name == "pelvis").unwrap();
    let mut posed = nodes.clone();
    let q0 = posed[pelvis].rotation;
    let half = (15.0f32 * std::f32::consts::PI / 180.0).sin();
    let (s, c) = (half, (1.0 - half * half).sqrt());
    let qz = [0.0f32, 0.0, s, c];
    let q = quat_mul(q0, qz);
    posed[pelvis].rotation = q;
    let base = skinned(&doc, nodes, 2);
    let moved = skinned(&doc, &posed, 2);
    // 统计顶点位移方向的平均叉积关系
    let mut sum_z = 0.0f64;
    let mut sum_y = 0.0f64;
    let mut sum_x = 0.0f64;
    let mut n = 0usize;
    let pelvis_world = world_of(nodes, pelvis);
    let center = [pelvis_world[12], pelvis_world[13], pelvis_world[14]];
    for i in 0..base.len().min(20000) {
        let d = [moved[i][0] - base[i][0], moved[i][1] - base[i][1], moved[i][2] - base[i][2]];
        if d[0].abs() + d[1].abs() + d[2].abs() < 1e-5 {
            continue;
        }
        let r = [base[i][0] - center[0], base[i][1] - center[1], base[i][2] - center[2]];
        // 绕 X 轴: (0,-rz,ry) 绕 Y: (rz,0,-rx) 绕 Z: (-ry,rx,0)
        let cross_x = norm(d, [0.0, -r[2], r[1]]);
        let cross_y = norm(d, [r[2], 0.0, -r[0]]);
        let cross_z = norm(d, [-r[1], r[0], 0.0]);
        sum_x += cross_x as f64;
        sum_y += cross_y as f64;
        sum_z += cross_z as f64;
        n += 1;
    }
    if n > 0 {
        out.push_str(&format!(
            "\npelvis +30deg around local Z: moved verts={n}, axis alignment X={:.3} Y={:.3} Z={:.3}\n",
            sum_x / n as f64, sum_y / n as f64, sum_z / n as f64
        ));
    }
    // 骨骼局部 Z 轴在世界里的方向
    let pw = pelvis_world;
    out.push_str(&format!(
        "pelvis world basis: x=({:.3},{:.3},{:.3}) y=({:.3},{:.3},{:.3}) z=({:.3},{:.3},{:.3})\n",
        pw[0], pw[1], pw[2], pw[4], pw[5], pw[6], pw[8], pw[9], pw[10]
    ));

    let target = r"C:\Users\Nero\AppData\Roaming\Tencent\Marvis\User\oAN1i2dpCCuZq-FgvYnflZ6SBnQA\workspace\conv_776800051b6f46619c993787b3340447\temp\skinreport3.txt";
    std::fs::File::create(target).unwrap().write_all(out.as_bytes()).unwrap();
    println!("ok");
}

fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

fn norm(a: [f32; 3], b: [f32; 3]) -> f32 {
    let la = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
    let lb = (b[0] * b[0] + b[1] * b[1] + b[2] * b[2]).sqrt();
    if la < 1e-9 || lb < 1e-9 {
        return 0.0;
    }
    (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) / (la * lb)
}
