//! temp diag 5: per-cluster TransformLink vs node_to_world difference analysis.
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

fn rot_angle(m: &[f32; 16]) -> f64 {
    let s = |col: usize| {
        let (x, y, z) = (m[col * 4] as f64, m[col * 4 + 1] as f64, m[col * 4 + 2] as f64);
        (x * x + y * y + z * z).sqrt().max(1e-12)
    };
    let (sx, sy, sz) = (s(0), s(1), s(2));
    let trace = (m[0] as f64) / sx + (m[5] as f64) / sy + (m[10] as f64) / sz;
    ((trace - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
}

fn main() {
    let path = std::env::args().nth(1).expect("need fbx path");
    let opts = ufbx::LoadOpts {
        target_axes: ufbx::CoordinateAxes {
            right: ufbx::CoordinateAxis::PositiveX,
            up: ufbx::CoordinateAxis::PositiveY,
            front: ufbx::CoordinateAxis::PositiveZ,
        },
        target_unit_meters: 1.0,
        ..Default::default()
    };
    let scene = ufbx::load_file(&path, opts).expect("load failed");
    let mut out = String::new();
    let mut angles: Vec<f64> = Vec::new();
    // 绕 X 轴 -90 度：把 Z-up 骨骼空间映射到 Y-up
    let rx90: [f32; 16] = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];

    for mesh in &scene.meshes {
        let Some(deformer) = mesh.skin_deformers.first() else { continue };
        out.push_str(&format!(
            "\nmesh `{}` clusters={}\n",
            mesh.element.name,
            deformer.clusters.len()
        ));
        for cluster in &deformer.clusters {
            let Some(bone) = cluster.bone_node.as_ref() else { continue };
            let nw = util::mat4_from_ufbx(&bone.node_to_world);
            let bw = util::mat4_from_ufbx(&cluster.bind_to_world);
            let g = util::mat4_mul(&bw, &util::mat4_invert(&nw));
            let g2 = util::mat4_mul(&util::mat4_mul(&rx90, &bw), &util::mat4_invert(&nw));
            let ang = rot_angle(&g);
            angles.push(ang);
            out.push_str(&format!(
                "  `{}` G={:.2}deg t=({:.3},{:.3},{:.3}) |  afterRx-90: {:.2}deg\n",
                bone.element.name,
                ang,
                g[12],
                g[13],
                g[14],
                rot_angle(&g2)
            ));
        }
        break; // 只看第一个 mesh（同骨架共享）
    }
    angles.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if !angles.is_empty() {
        let n = angles.len();
        out.push_str(&format!(
            "\nG angle stats: n={n} min={:.2} p25={:.2} median={:.2} p75={:.2} max={:.2}\n",
            angles[0],
            angles[n / 4],
            angles[n / 2],
            angles[n * 3 / 4],
            angles[n - 1]
        ));
    }

    let target = r"C:\Users\Nero\AppData\Roaming\Tencent\Marvis\User\oAN1i2dpCCuZq-FgvYnflZ6SBnQA\workspace\conv_776800051b6f46619c993787b3340447\temp\skinreport5.txt";
    std::fs::File::create(target).unwrap().write_all(out.as_bytes()).unwrap();
    println!("ok");
}
