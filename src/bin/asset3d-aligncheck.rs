//! temp diag: 验证 fbx.rs 的「骨架坐标系对齐」改动（骨架 AABB + 静态蒙皮恒等 + 动画帧）。
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

use air::*;

fn quat_to_mat4(q: [f32; 4]) -> [f32; 16] {
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);
    [
        1.0 - 2.0 * (yy + zz),
        2.0 * (xy + wz),
        2.0 * (xz - wy),
        0.0,
        2.0 * (xy - wz),
        1.0 - 2.0 * (xx + zz),
        2.0 * (yz + wx),
        0.0,
        2.0 * (xz + wy),
        2.0 * (yz - wx),
        1.0 - 2.0 * (xx + yy),
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
    ]
}

fn local_of(n: &AirNode) -> [f32; 16] {
    let r = quat_to_mat4(n.rotation);
    let s = n.scale;
    let mut m = r;
    for c in 0..3 {
        m[c * 4 + 0] *= s[0];
        m[c * 4 + 1] *= s[1];
        m[c * 4 + 2] *= s[2];
    }
    m[12] = n.position[0];
    m[13] = n.position[1];
    m[14] = n.position[2];
    m
}

fn world_of(nodes: &[AirNode], idx: u32) -> [f32; 16] {
    let mut acc = local_of(&nodes[idx as usize]);
    let mut cur = nodes[idx as usize].parent;
    let mut guard = 0;
    while let Some(p) = cur {
        acc = util::mat4_mul(&local_of(&nodes[p as usize]), &acc);
        cur = nodes[p as usize].parent;
        guard += 1;
        if guard > nodes.len() {
            break;
        }
    }
    acc
}

fn xf(m: &[f32; 16], p: [f32; 3]) -> [f32; 3] {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

#[derive(Clone, Copy)]
struct Aabb {
    min: [f32; 3],
    max: [f32; 3],
}

impl Aabb {
    fn new() -> Self {
        Self {
            min: [f32::MAX; 3],
            max: [f32::MIN; 3],
        }
    }
    fn add(&mut self, p: [f32; 3]) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(p[i]);
            self.max[i] = self.max[i].max(p[i]);
        }
    }
    fn contains(&self, p: [f32; 3], margin: f32) -> bool {
        (0..3).all(|i| p[i] >= self.min[i] - margin && p[i] <= self.max[i] + margin)
    }
    fn dist_out(&self, p: [f32; 3]) -> f32 {
        let mut d = 0.0f32;
        for i in 0..3 {
            let lo = p[i] - self.min[i];
            let hi = self.max[i] - p[i];
            let v = lo.min(hi);
            if v < 0.0 {
                d += v * v;
            }
        }
        d.sqrt()
    }
    fn size(&self) -> [f32; 3] {
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }
}

/// 对骨架按给定世界矩阵口径算 AABB，返回 (骨数, 在网格框内的骨数, 框外平均超距, 骨架框)
fn skeleton_stats(
    doc: &AirDoc,
    joint_worlds: &[[f32; 16]],
    mesh_box: &Aabb,
) -> (usize, usize, f32, Aabb) {
    let _ = doc;
    let mut box_ = Aabb::new();
    let mut inside = 0usize;
    let mut out_sum = 0.0f32;
    let mut out_n = 0usize;
    for m in joint_worlds {
        let p = [m[12], m[13], m[14]];
        box_.add(p);
        if mesh_box.contains(p, 0.02) {
            inside += 1;
        } else {
            out_sum += mesh_box.dist_out(p);
            out_n += 1;
        }
    }
    let avg_out = if out_n > 0 { out_sum / out_n as f32 } else { 0.0 };
    (joint_worlds.len(), inside, avg_out, box_)
}

/// 采样某个动画在 t 时刻的骨骼世界矩阵（轨道缺省时用节点静态 TRS）。
fn pose_worlds(doc: &AirDoc, anim: Option<(&AirAnimation, f32)>) -> Vec<[f32; 16]> {
    pose_worlds_override(doc, anim, None)
}

/// `override_rot`：把某个节点的旋转替换成给定值（用于把对齐节点的旋转置单位阵，
/// 复现「不插对齐节点」时的旧行为）。
fn pose_worlds_override(
    doc: &AirDoc,
    anim: Option<(&AirAnimation, f32)>,
    override_rot: Option<(usize, [f32; 4])>,
) -> Vec<[f32; 16]> {
    let mut trs: Vec<([f32; 3], [f32; 4], [f32; 3])> = doc
        .nodes
        .iter()
        .map(|n| (n.position, n.rotation, n.scale))
        .collect();
    if let Some((idx, rot)) = override_rot {
        if idx < trs.len() {
            trs[idx].1 = rot;
        }
    }
    if let Some((clip, t)) = anim {
        for track in &clip.tracks {
            let idx = track.node as usize;
            if idx >= trs.len() || track.times.is_empty() {
                continue;
            }
            let (value, _) = sample_track(track, t);
            match track.property.as_str() {
                "translation" => trs[idx].0 = [value[0], value[1], value[2]],
                "rotation" => trs[idx].1 = [value[0], value[1], value[2], value[3]],
                "scale" => trs[idx].2 = [value[0], value[1], value[2]],
                _ => {}
            }
        }
    }
    let mut worlds = Vec::with_capacity(doc.nodes.len());
    for i in 0..doc.nodes.len() {
        let local = {
            let n = AirNode {
                position: trs[i].0,
                rotation: trs[i].1,
                scale: trs[i].2,
                ..doc.nodes[i].clone()
            };
            local_of(&n)
        };
        worlds.push(local);
    }
    // 沿父链累乘
    let mut out = vec![[0.0f32; 16]; doc.nodes.len()];
    for i in 0..doc.nodes.len() {
        let mut acc = worlds[i];
        let mut cur = doc.nodes[i].parent;
        let mut guard = 0;
        while let Some(p) = cur {
            acc = util::mat4_mul(&worlds[p as usize], &acc);
            cur = doc.nodes[p as usize].parent;
            guard += 1;
            if guard > doc.nodes.len() {
                break;
            }
        }
        out[i] = acc;
    }
    out
}

fn sample_track(track: &AirTrack, t: f32) -> ([f32; 4], bool) {
    if track.times.is_empty() || track.values.is_empty() {
        return ([0.0; 4], false);
    }
    if t <= track.times[0] {
        return (track.values[0], true);
    }
    let last = track.times.len().min(track.values.len()) - 1;
    if t >= track.times[last] {
        return (track.values[last], true);
    }
    for i in 0..last {
        if t >= track.times[i] && t <= track.times[i + 1] {
            let span = (track.times[i + 1] - track.times[i]).max(1e-9);
            let u = (t - track.times[i]) / span;
            let a = track.values[i];
            let b = track.values[i + 1];
            let mut v = [0.0f32; 4];
            for k in 0..4 {
                v[k] = a[k] + (b[k] - a[k]) * u;
            }
            if track.property == "rotation" {
                let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2] + v[3] * v[3]).sqrt();
                if n > 1e-9 {
                    for k in 0..4 {
                        v[k] /= n;
                    }
                }
            }
            return (v, true);
        }
    }
    (track.values[last], true)
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "mote3.fbx".into());
    let out_path = std::env::args().nth(2);
    let bytes = std::fs::read(&path).expect("read fbx");
    let mut lines: Vec<String> = Vec::new();
    macro_rules! p {
        ($($a:tt)*) => { lines.push(format!($($a)*)) };
    }

    let mut doc = AirDoc::new("fbx", &path);
    match fbx::import(&mut doc, &bytes, &path, true, false) {
        Ok(()) => {}
        Err(err) => {
            println!("import failed: {err}");
            return;
        }
    }
    doc.finalize();

    p!("# source: {}", path);
    p!("# nodes={} meshes={} skins={} anims={}", doc.nodes.len(), doc.meshes.len(), doc.skins.len(), doc.animations.len());
    p!("## notes (align 相关)");
    for issue in &doc.issues {
        if issue.contains("对齐") || issue.contains("骨架") {
            p!("  - {}", issue);
        }
    }
    p!("## errors: {}", doc.errors.len());
    for e in &doc.errors {
        p!("  ! {}", e);
    }

    // 新增的对齐节点
    for n in &doc.nodes {
        if n.name.contains("__axis_fix") {
            p!(
                "align node #{} name={} path={} parent={:?} rot=({:.4},{:.4},{:.4},{:.4})",
                n.index,
                n.name,
                n.path,
                n.parent,
                n.rotation[0],
                n.rotation[1],
                n.rotation[2],
                n.rotation[3]
            );
        }
    }

    let rest = pose_worlds(&doc, None);

    // 全身合并 AABB：单件网格（腰带/牙齿/眼球）自己的框只有几厘米，用它判断
    // 「骨骼是否落在模型体内」没有意义，统一用整个角色的包围盒。
    let mut body_box = Aabb::new();
    for (mi, mesh) in doc.meshes.iter().enumerate() {
        if let Some(node_idx) = doc.nodes.iter().position(|n| n.mesh == Some(mi as u32)) {
            let w = rest[node_idx];
            for pos in &mesh.positions {
                body_box.add(xf(&w, *pos));
            }
        }
    }
    p!(
        "\n## 全身合并 AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3}) size=({:.3},{:.3},{:.3})",
        body_box.min[0], body_box.min[1], body_box.min[2],
        body_box.max[0], body_box.max[1], body_box.max[2],
        body_box.size()[0], body_box.size()[1], body_box.size()[2]
    );

    // 骨轴检查：每根骨指向其子骨的方向，应与骨骼世界矩阵的某条主轴平行；
    // 骨架若与网格正交，这个夹角会顶到 90°。
    {
        let mut angles: Vec<f32> = Vec::new();
        for (i, node) in doc.nodes.iter().enumerate() {
            // 找出以 i 为父的骨（有 joint 身份的节点）
            let children: Vec<u32> = doc
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.parent == Some(i as u32))
                .map(|(ci, _)| ci as u32)
                .collect();
            if children.is_empty() {
                continue;
            }
            let w = rest[i];
            // 取骨长方向与骨骼世界矩阵三轴中最小的夹角：UE 骨架的骨长轴不一定是 Y，
            // 这个指标只反映「骨架内部是否正交自洽」，与整体坐标系无关。
            for c in children.iter().take(1) {
                let cw = rest[*c as usize];
                let d = [cw[12] - w[12], cw[13] - w[13], cw[14] - w[14]];
                let dl = (d[0].powi(2) + d[1].powi(2) + d[2].powi(2)).sqrt();
                if dl < 1e-4 {
                    continue;
                }
                let mut best = 90.0f32;
                for axis in 0..3 {
                    let a = [w[axis * 4], w[axis * 4 + 1], w[axis * 4 + 2]];
                    let al = (a[0].powi(2) + a[1].powi(2) + a[2].powi(2)).sqrt();
                    if al < 1e-6 {
                        continue;
                    }
                    let cos = ((d[0] * a[0] + d[1] * a[1] + d[2] * a[2]) / (dl * al))
                        .abs()
                        .clamp(0.0, 1.0);
                    best = best.min(cos.acos().to_degrees());
                }
                angles.push(best);
            }
        }
        angles.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if !angles.is_empty() {
            let med = angles[angles.len() / 2];
            let p90 = angles[(angles.len() * 9 / 10).min(angles.len() - 1)];
            p!(
                "骨长方向 vs 骨骼世界主轴夹角（骨架内部自洽性）：n={} med={:.1}° p90={:.1}° max={:.1}°",
                angles.len(), med, p90, angles[angles.len() - 1]
            );
        }
    }

    // ---- 对照：把对齐节点的旋转置单位阵，等价于本轮改动之前的旧口径 ----
    if let Some(align_idx) = doc.nodes.iter().position(|n| n.name.contains("__axis_fix")) {
        let legacy = pose_worlds_override(&doc, None, Some((align_idx, [0.0, 0.0, 0.0, 1.0])));
        let mut leg_box = Aabb::new();
        let mut inside = 0usize;
        let mut total = 0usize;
        for skin in &doc.skins {
            for j in &skin.joints {
                let w = legacy[*j as usize];
                let pos = [w[12], w[13], w[14]];
                leg_box.add(pos);
                total += 1;
                if body_box.contains(pos, 0.02) {
                    inside += 1;
                }
            }
        }
        p!(
            "\n## 旧口径对照（不插对齐节点）骨架 AABB size=({:.3},{:.3},{:.3}) 骨在全身AABB内={}/{}",
            leg_box.size()[0], leg_box.size()[1], leg_box.size()[2], inside, total
        );
    }

    for (mi, mesh) in doc.meshes.iter().enumerate() {
        // 网格世界矩阵
        let node_idx = doc.nodes.iter().position(|n| n.mesh == Some(mi as u32));
        let Some(node_idx) = node_idx else {
            p!("mesh#{} {}: 无挂载节点", mi, mesh.name);
            continue;
        };
        let mesh_world = rest[node_idx];
        let mut mesh_box = Aabb::new();
        for pos in &mesh.positions {
            mesh_box.add(xf(&mesh_world, *pos));
        }
        p!(
            "\n## mesh#{} `{}` node#{} `{}` verts={} world-AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3}) size=({:.3},{:.3},{:.3})",
            mi,
            mesh.name,
            node_idx,
            doc.nodes[node_idx].path,
            mesh.positions.len(),
            mesh_box.min[0],
            mesh_box.min[1],
            mesh_box.min[2],
            mesh_box.max[0],
            mesh_box.max[1],
            mesh_box.max[2],
            mesh_box.size()[0],
            mesh_box.size()[1],
            mesh_box.size()[2]
        );

        let Some(skin_idx) = mesh.skin else {
            p!("  (无 skin)");
            continue;
        };
        let skin = &doc.skins[skin_idx as usize];
        let joint_worlds: Vec<[f32; 16]> = skin.joints.iter().map(|j| rest[*j as usize]).collect();
        let (n, inside, avg_out, skel_box) = skeleton_stats(&doc, &joint_worlds, &body_box);
        let (_, inside_own, _, _) = skeleton_stats(&doc, &joint_worlds, &mesh_box);
        p!(
            "  skin#{} joints={} 骨在全身AABB内={}/{}（仅本网格框内={}） 框外平均超距={:.4}m 骨架AABB size=({:.3},{:.3},{:.3})",
            skin_idx,
            n,
            inside,
            n,
            inside_own,
            avg_out,
            skel_box.size()[0],
            skel_box.size()[1],
            skel_box.size()[2]
        );

        // 静态蒙皮恒等： Σ w*(joint_world*IBM*localPos)  vs  mesh_world*localPos
        let mut max_err = 0.0f32;
        let mut skinned_box = Aabb::new();
        let mut checked = 0usize;
        for (vi, pos) in mesh.positions.iter().enumerate() {
            let target = xf(&mesh_world, *pos);
            let mut acc = [0.0f32; 3];
            let w = mesh.weights.get(vi);
            let j = mesh.joints.get(vi);
            if let (Some(w), Some(j)) = (w, j) {
                for k in 0..4 {
                    if w[k] <= 0.0 {
                        continue;
                    }
                    let slot = j[k] as usize;
                    if slot >= skin.joints.len() {
                        continue;
                    }
                    let m = util::mat4_mul(&joint_worlds[slot], &skin.inverse_bind_matrices[slot]);
                    let q = xf(&m, *pos);
                    acc[0] += q[0] * w[k];
                    acc[1] += q[1] * w[k];
                    acc[2] += q[2] * w[k];
                }
            }
            skinned_box.add(acc);
            let err = ((acc[0] - target[0]).powi(2)
                + (acc[1] - target[1]).powi(2)
                + (acc[2] - target[2]).powi(2))
            .sqrt();
            if err > max_err {
                max_err = err;
            }
            checked += 1;
        }
        p!(
            "  静态蒙皮最大偏差={:.3e}m (checked {}) 蒙皮后AABB min=({:.3},{:.3},{:.3}) max=({:.3},{:.3},{:.3})",
            max_err,
            checked,
            skinned_box.min[0],
            skinned_box.min[1],
            skinned_box.min[2],
            skinned_box.max[0],
            skinned_box.max[1],
            skinned_box.max[2]
        );
    }

    // 动画帧检查
    p!("\n## animations");
    for (ai, clip) in doc.animations.iter().enumerate() {
        p!(
            "anim#{} `{}` duration={:.3}s tracks={}",
            ai,
            clip.name,
            clip.duration,
            clip.tracks.len()
        );
        let samples = [0.0f32, 0.25, 0.5, 0.75, 1.0];
        let mut prev: Option<Vec<[f32; 16]>> = None;
        for u in samples {
            let t = clip.duration * u;
            let worlds = pose_worlds(&doc, Some((clip, t)));
            let mut max_dev = 0.0f32;
            let mut moved = 0usize;
            for (i, w) in worlds.iter().enumerate() {
                let d = ((w[12] - rest[i][12]).powi(2)
                    + (w[13] - rest[i][13]).powi(2)
                    + (w[14] - rest[i][14]).powi(2))
                .sqrt();
                if d > 1e-4 {
                    moved += 1;
                }
                if d > max_dev {
                    max_dev = d;
                }
            }
            let step = match &prev {
                Some(pv) => {
                    let mut m = 0.0f32;
                    for i in 0..worlds.len() {
                        let d = ((worlds[i][12] - pv[i][12]).powi(2)
                            + (worlds[i][13] - pv[i][13]).powi(2)
                            + (worlds[i][14] - pv[i][14]).powi(2))
                        .sqrt();
                        if d > m {
                            m = d;
                        }
                    }
                    m
                }
                None => 0.0,
            };
            p!(
                "  t={:.2}s 节点位移 vs 静态: max={:.4}m 变化节点数={} ；vs 上一采样 max={:.4}m",
                t,
                max_dev,
                moved,
                step
            );
            prev = Some(worlds);
        }
    }

    let text = lines.join("\n");
    println!("{text}");
    if let Some(out) = out_path {
        std::fs::write(out, text).ok();
    }
}
