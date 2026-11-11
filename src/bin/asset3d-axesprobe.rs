//! temp diag: 判定 mote3.fbx 的骨架/网格坐标系错位是「文件固有」还是「ufbx 空间转换造成的伪像」。
//! 用 4 种加载配置分别解析，对比：原始 axes、网格局部/世界 AABB、骨骼世界 AABB、G=bind×inv(node) 角度。
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
use ufbx::{CoordinateAxes, CoordinateAxis, SpaceConversion};

fn axes_str(a: &CoordinateAxes) -> String {
    let f = |x: CoordinateAxis| match x {
        CoordinateAxis::PositiveX => "+X",
        CoordinateAxis::NegativeX => "-X",
        CoordinateAxis::PositiveY => "+Y",
        CoordinateAxis::NegativeY => "-Y",
        CoordinateAxis::PositiveZ => "+Z",
        CoordinateAxis::NegativeZ => "-Z",
        CoordinateAxis::Unknown => "?",
    };
    format!("right={} up={} front={}", f(a.right), f(a.up), f(a.front))
}

fn rot_angle(m: &[f32; 16]) -> f64 {
    let mut scale = [0f64; 3];
    for c in 0..3 {
        let mut s2 = 0f64;
        for r in 0..3 {
            let v = m[c * 4 + r] as f64;
            s2 += v * v;
        }
        scale[c] = s2.sqrt().max(1e-12);
    }
    let trace = (m[0] as f64) / scale[0] + (m[5] as f64) / scale[1] + (m[10] as f64) / scale[2];
    ((trace - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
}

struct Aabb {
    min: [f64; 3],
    max: [f64; 3],
    n: usize,
}
impl Aabb {
    fn new() -> Self {
        Aabb { min: [f64::MAX; 3], max: [f64::MIN; 3], n: 0 }
    }
    fn add(&mut self, p: [f64; 3]) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(p[i]);
            self.max[i] = self.max[i].max(p[i]);
        }
        self.n += 1;
    }
    fn fmt(&self) -> String {
        if self.n == 0 {
            return "empty".into();
        }
        format!(
            "X[{:.3},{:.3}] Y[{:.3},{:.3}] Z[{:.3},{:.3}]",
            self.min[0], self.max[0], self.min[1], self.max[1], self.min[2], self.max[2]
        )
    }
    fn span(&self) -> [f64; 3] {
        [self.max[0] - self.min[0], self.max[1] - self.min[1], self.max[2] - self.min[2]]
    }
}

fn xform(m: &ufbx::Matrix, p: [f64; 3]) -> [f64; 3] {
    let t = util::mat4_from_ufbx(m);
    let (x, y, z) = (p[0] as f32, p[1] as f32, p[2] as f32);
    [
        (t[0] * x + t[4] * y + t[8] * z + t[12]) as f64,
        (t[1] * x + t[5] * y + t[9] * z + t[13]) as f64,
        (t[2] * x + t[6] * y + t[10] * z + t[14]) as f64,
    ]
}

fn tpos(m: &ufbx::Matrix) -> [f64; 3] {
    [m.m03 as f64, m.m13 as f64, m.m23 as f64]
}

fn probe(tag: &str, path: &str, opts: ufbx::LoadOpts) -> String {
    let mut out = String::new();
    let scene = match ufbx::load_file(path, opts) {
        Ok(s) => s,
        Err(e) => return format!("\n########## {tag}\nload failed: {e:?}\n"),
    };
    out.push_str(&format!("\n########## {tag}\n"));
    out.push_str(&format!(
        "exporter={:?} creator=`{}` original_app={} {}\n",
        scene.metadata.exporter, scene.metadata.creator,
        scene.metadata.original_application.name, scene.metadata.original_application.version
    ));
    out.push_str(&format!(
        "raw settings.axes: {} | original_axis_up={:?} | unit_meters={}\n",
        axes_str(&scene.settings.axes), scene.settings.original_axis_up, scene.settings.unit_meters
    ));
    out.push_str(&format!(
        "metadata: space_conversion={:?} root_rotation={:?} root_scale={} geometry_scale={} mirror={:?}\n",
        scene.metadata.space_conversion, scene.metadata.root_rotation, scene.metadata.root_scale,
        scene.metadata.geometry_scale, scene.metadata.mirror_axis
    ));
    // warnings
    let ws: Vec<String> = scene.metadata.warnings.iter().map(|w| format!("{:?}", w.type_)).collect();
    out.push_str(&format!("warnings({}): {:?}\n", ws.len(), ws));

    // 节点树顶层
    out.push_str("top-level nodes:\n");
    for nr in scene.nodes.iter() {
        let n = nr;
        if n.parent.is_none() {
            out.push_str(&format!(
                "  `{}` depth={} is_root={} adjust={} mesh={} bone={}\n",
                n.element.name, n.node_depth, n.is_root, n.has_adjust_transform,
                n.mesh.is_some(), n.bone.is_some()
            ));
        }
    }
    // 骨架 root 的父链（找名字 root 的节点）
    for nr in scene.nodes.iter() {
        let n = nr;
        if n.element.name == "root" || n.element.name == "group3" {
            let mut chain = Vec::new();
            let mut cur: Option<&ufbx::Node> = Some(n.as_ref());
            while let Some(c) = cur {
                chain.push(format!("{}#{}", c.element.name, c.node_depth));
                cur = c.parent.as_ref().map(|p| p.as_ref());
            }
            out.push_str(&format!("chain of `{}`: {}\n", n.element.name, chain.join(" -> ")));
        }
    }

    // 网格顶点：局部 AABB 与世界 AABB（第一个蒙皮网格 body）
    let mut local = Aabb::new();
    let mut world = Aabb::new();
    let mut picked = String::new();
    for mr in scene.meshes.iter() {
        let m = mr.as_ref();
        let Some(node) = m.element.instances.first().map(|r| r.as_ref()) else { continue };
        local = Aabb::new();
        world = Aabb::new();
        let pos = &m.vertex_position;
        for i in 0..m.num_vertices {
            let v = pos[i];
            local.add([v.x as f64, v.y as f64, v.z as f64]);
            world.add(xform(&node.node_to_world, [v.x as f64, v.y as f64, v.z as f64]));
        }
        picked = format!("`{}` verts={}", m.element.name, m.num_vertices);
        break;
    }
    out.push_str(&format!("mesh {} local AABB: {}\n", picked, local.fmt()));
    out.push_str(&format!("mesh {} world AABB: {}\n", picked, world.fmt()));

    // 骨骼世界 AABB + G 角度
    let mut bone_w = Aabb::new();
    let mut angles: Vec<f64> = Vec::new();
    let mut g_detail = String::new();
    'outer: for mr in scene.meshes.iter() {
        let m = mr.as_ref();
        let Some(def) = m.skin_deformers.first() else { continue };
        for c in def.clusters.iter() {
            let Some(bone) = c.bone_node.as_ref() else { continue };
            let nw = &bone.node_to_world;
            bone_w.add(tpos(nw));
            let bw = &c.bind_to_world;
            let nw4 = util::mat4_from_ufbx(nw);
            let bw4 = util::mat4_from_ufbx(bw);
            let g4 = util::mat4_mul(&bw4, &util::mat4_invert(&nw4));
            let a = rot_angle(&g4);
            angles.push(a);
            if g_detail.lines().count() < 4 && matches!(&*bone.element.name, "root" | "pelvis" | "spine_01" | "hand_l") {
                g_detail.push_str(&format!(
                    "  G[`{}`] = {:.2}deg t=({:.4},{:.4},{:.4})\n",
                    bone.element.name, a, g4[12], g4[13], g4[14]
                ));
            }
        }
        break;
    }
    out.push_str(&format!("bones world AABB: {}\n", bone_w.fmt()));
    out.push_str(&format!(
        "mesh span={:?} bone span={:?}\n",
        world.span().map(|v| (v * 1000.0).round() / 1000.0),
        bone_w.span().map(|v| (v * 1000.0).round() / 1000.0)
    ));
    out.push_str(&g_detail);
    angles.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if !angles.is_empty() {
        let n = angles.len();
        out.push_str(&format!(
            "G angle n={} min={:.2} p25={:.2} median={:.2} p75={:.2} max={:.2}\n",
            n, angles[0], angles[n / 4], angles[n / 2], angles[n * 3 / 4], angles[n - 1]
        ));
    }
    // 静态蒙皮自洽性：palette(bind)=node_world? 用 geometry_to_bone 检验
    if let Some(mr) = scene.meshes.iter().next() {
        let m = mr.as_ref();
        if let Some(def) = m.skin_deformers.first() {
            if let Some(c) = def.clusters.iter().next() {
                if let Some(bone) = c.bone_node.as_ref() {
                    let nw4 = util::mat4_from_ufbx(&bone.node_to_world);
                    let gb4 = util::mat4_from_ufbx(&c.geometry_to_bone);
                    let p = util::mat4_mul(&nw4, &gb4);
                    out.push_str(&format!(
                        "first cluster `{}` node_world*geometry_to_bone t=({:.4},{:.4},{:.4})\n",
                        bone.element.name, p[12], p[13], p[14]
                    ));
                }
            }
        }
    }
    out
}

fn main() {
    let path = std::env::args().nth(1).expect("need fbx path");
    let yup = CoordinateAxes {
        right: CoordinateAxis::PositiveX,
        up: CoordinateAxis::PositiveY,
        front: CoordinateAxis::PositiveZ,
    };
    let mut out = String::new();

    // A: 全部默认（不做任何空间转换）
    out.push_str(&probe("A: 默认（无 target_axes / 无单位转换）", &path, ufbx::LoadOpts::default()));

    // B: 只做单位转换（厘米 -> 米），不做轴向转换
    let b = ufbx::LoadOpts { target_unit_meters: 1.0, ..Default::default() };
    out.push_str(&probe("B: 仅 target_unit_meters=1.0", &path, b));

    // C: target_axes=Y-up + 单位，space_conversion 默认（TransformRoot）
    let c = ufbx::LoadOpts {
        target_axes: yup,
        target_unit_meters: 1.0,
        ..Default::default()
    };
    out.push_str(&probe("C: Y-up + 米（space_conversion=TransformRoot 默认）", &path, c));

    // D: Y-up + 米 + MODIFY_GEOMETRY
    let d = ufbx::LoadOpts {
        target_axes: yup,
        target_unit_meters: 1.0,
        space_conversion: SpaceConversion::ModifyGeometry,
        ..Default::default()
    };
    out.push_str(&probe("D: Y-up + 米 + space_conversion=ModifyGeometry", &path, d));

    // E: Y-up + 米 + ADJUST_TRANSFORMS
    let e = ufbx::LoadOpts {
        target_axes: yup,
        target_unit_meters: 1.0,
        space_conversion: SpaceConversion::AdjustTransforms,
        ..Default::default()
    };
    out.push_str(&probe("E: Y-up + 米 + space_conversion=AdjustTransforms", &path, e));

    let target = std::env::args().nth(2).unwrap_or_else(|| "axesprobe.txt".to_string());
    std::fs::File::create(&target).unwrap().write_all(out.as_bytes()).unwrap();
    println!("written to {target}");
}
