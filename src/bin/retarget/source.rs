//! 源动作读取：把 GLB / FBX 里的骨架与动作统一成「每骨骼每帧的局部 TRS」。
//!
//! 两条链路都落到同一份 [`Clip`]：
//!
//! - GLB：直接用本 crate 的 [`super::glb`] 读 nodes / animations，保留源采样时刻
//!   （不去重、不重采样、不降帧），只把所有通道的时间点做一次并集，方便逐帧对齐。
//! - FBX：走 `ufbx` 的 `bake_anim`，与既有 `src/fbx.rs` 完全同一套轴 / 单位选项，
//!   这样 `D:\share` 里的 UE 动作 `.fbx` 也能当源用（二次验证就靠它）。

use std::collections::BTreeMap;

use glam::{DQuat, DVec3};
use serde_json::Value;

use super::glb::Glb;
use super::math::LocalTrs;

/// 骨架：名字 + 父索引 + rest 局部变换。索引即 glTF node / FBX node 的下标。
pub struct Skeleton {
    pub names: Vec<String>,
    pub parent: Vec<Option<usize>>,
    pub rest: Vec<LocalTrs>,
}

impl Skeleton {
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// 父先于子的遍历序。父子链坏掉（成环 / 父索引越界）时把剩下的按原序补在尾部，
    /// 宁可算错某一根骨的链，也不能静默丢骨头。
    pub fn order(&self) -> Vec<usize> {
        let n = self.len();
        let mut order = Vec::with_capacity(n);
        let mut done = vec![false; n];
        loop {
            let mut progressed = false;
            for i in 0..n {
                if done[i] {
                    continue;
                }
                let ready = match self.parent[i] {
                    None => true,
                    Some(p) => p < n && done[p],
                };
                if ready {
                    done[i] = true;
                    order.push(i);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        for i in 0..n {
            if !done[i] {
                order.push(i);
            }
        }
        order
    }
}

/// 一段动作：时间轴（秒）+ 每骨骼每帧的局部 TRS。
pub struct Clip {
    pub name: String,
    pub skeleton: Skeleton,
    pub times: Vec<f64>,
    /// `frames[骨骼][帧]`
    pub frames: Vec<Vec<LocalTrs>>,
    /// 该骨骼在这段动作里是否有动画通道（没通道的骨在 `frames` 里是 rest 常量）。
    pub animated: Vec<bool>,
    /// 源骨架的蒙皮关节（glTF skin.joints 的节点下标）；FBX 源为空。
    pub joints: Vec<usize>,
    pub kind: &'static str,
}

impl Clip {
    pub fn duration(&self) -> f64 {
        match (self.times.first(), self.times.last()) {
            (Some(first), Some(last)) => last - first,
            _ => 0.0,
        }
    }

    pub fn frame_count(&self) -> usize {
        self.times.len()
    }

    /// 某根骨在整段动作里的局部旋转波动（弧度）。用于报告里说明这根骨到底动没动。
    pub fn rotation_swing(&self, bone: usize, from: usize, to: usize) -> f64 {
        let mut max = 0.0f64;
        let end = to.min(self.frame_count());
        for i in from..end {
            let a = self.frames[bone][i].r;
            for j in i..end {
                let ang = a.angle_between(self.frames[bone][j].r).abs();
                if ang > max {
                    max = ang;
                }
            }
        }
        max
    }
}

/// 一个动画通道：某节点的某个 path 的关键帧。
struct Track {
    path: String,
    #[allow(dead_code)]
    interp: String,
    times: Vec<f64>,
    values: Vec<[f64; 4]>,
    dims: usize,
}

impl Track {
    fn sample(&self, at: f64) -> [f64; 4] {
        if self.times.is_empty() {
            return [0.0; 4];
        }
        if self.times.len() == 1 || at <= self.times[0] {
            return self.values[0];
        }
        let last = self.times.len() - 1;
        if at >= self.times[last] {
            return self.values[last];
        }
        // 二分定位所在区间
        let mut lo = 0usize;
        let mut hi = last;
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.times[mid] <= at {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let t0 = self.times[lo];
        let t1 = self.times[hi];
        let span = t1 - t0;
        let alpha = if span > 0.0 {
            ((at - t0) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self.path == "rotation" {
            let a = DQuat::from_xyzw(
                self.values[lo][0],
                self.values[lo][1],
                self.values[lo][2],
                self.values[lo][3],
            );
            let b = DQuat::from_xyzw(
                self.values[hi][0],
                self.values[hi][1],
                self.values[hi][2],
                self.values[hi][3],
            );
            let q = a.slerp(b, alpha);
            [q.x, q.y, q.z, q.w]
        } else {
            let mut out = [0.0; 4];
            for c in 0..self.dims {
                out[c] = self.values[lo][c] + (self.values[hi][c] - self.values[lo][c]) * alpha;
            }
            out
        }
    }
}

fn node_rest(node: &Value) -> LocalTrs {
    // glTF 规范：node 带 matrix 时，TRS 字段必须被忽略。
    if let Some(matrix) = node.get("matrix").and_then(Value::as_array) {
        if matrix.len() == 16 {
            let mut cols = [0.0f64; 16];
            for (i, v) in matrix.iter().enumerate() {
                cols[i] = v.as_f64().unwrap_or(0.0);
            }
            return LocalTrs::from_matrix(&glam::DMat4::from_cols_array(&cols));
        }
    }
    let read = |key: &str, fallback: [f64; 4]| -> [f64; 4] {
        let mut out = fallback;
        if let Some(arr) = node.get(key).and_then(Value::as_array) {
            for (i, v) in arr.iter().take(4).enumerate() {
                out[i] = v.as_f64().unwrap_or(fallback[i]);
            }
        }
        out
    };
    let t = read("translation", [0.0, 0.0, 0.0, 0.0]);
    let r = read("rotation", [0.0, 0.0, 0.0, 1.0]);
    let s = read("scale", [1.0, 1.0, 1.0, 1.0]);
    LocalTrs::new(
        DVec3::new(t[0], t[1], t[2]),
        DQuat::from_xyzw(r[0], r[1], r[2], r[3]).normalize(),
        DVec3::new(s[0], s[1], s[2]),
    )
}

/// 从 GLB 字节里读骨架（含不是 joint 的中间节点，世界矩阵要用整条链）。
pub fn skeleton_from_glb(glb: &Glb) -> Result<Skeleton, String> {
    let nodes = glb
        .arr("nodes")
        .ok_or_else(|| "GLB 里没有 nodes".to_string())?;
    if nodes.is_empty() {
        return Err("GLB 的 nodes 为空".to_string());
    }
    let mut parent: Vec<Option<usize>> = vec![None; nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            for child in children {
                if let Some(idx) = child.as_u64() {
                    let idx = idx as usize;
                    if idx < nodes.len() {
                        parent[idx] = Some(i);
                    }
                }
            }
        }
    }
    let names = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            n.get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("node#{i}"))
        })
        .collect();
    let rest = nodes.iter().map(node_rest).collect();
    Ok(Skeleton {
        names,
        parent,
        rest,
    })
}

/// 从 GLB 读一段动作。`want` 为空则取第 0 段；`want` 非空时按名字精确匹配。
pub fn clip_from_glb(bytes: &[u8], want: Option<&str>) -> Result<Clip, String> {
    let glb = Glb::parse(bytes)?;
    let skeleton = skeleton_from_glb(&glb)?;
    let anims = glb
        .arr("animations")
        .ok_or_else(|| "源资产里没有 animations：这不是动作序列文件".to_string())?;
    if anims.is_empty() {
        return Err("源资产的 animations 数组是空的".to_string());
    }
    let index = match want {
        Some(name) => anims
            .iter()
            .position(|a| a.get("name").and_then(Value::as_str) == Some(name))
            .ok_or_else(|| {
                let list: Vec<String> = anims
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        format!(
                            "[{i}] {}",
                            a.get("name").and_then(Value::as_str).unwrap_or("(无名)")
                        )
                    })
                    .collect();
                format!("源资产里没有名为 {name} 的动作，现有：{}", list.join(", "))
            })?,
        None => 0,
    };
    let anim = &anims[index];
    let name = anim
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("(无名动作)")
        .to_string();

    let channels = anim
        .get("channels")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("动作 {name} 没有 channels"))?;
    let samplers = anim
        .get("samplers")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("动作 {name} 没有 samplers"))?;

    let mut per_node: BTreeMap<usize, Vec<Track>> = BTreeMap::new();
    for (ci, channel) in channels.iter().enumerate() {
        let target = channel.get("target").unwrap_or(&Value::Null);
        let node = target.get("node").and_then(Value::as_u64);
        let path = target.get("path").and_then(Value::as_str).unwrap_or("");
        let (Some(node), true) = (node, matches!(path, "rotation" | "translation" | "scale")) else {
            continue; // weights 之类的通道与骨架无关，跳过
        };
        let node = node as usize;
        if node >= skeleton.len() {
            continue;
        }
        let sampler_idx = channel
            .get("sampler")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("动作 {name} 的 channels[{ci}] 缺 sampler"))?
            as usize;
        let sampler = samplers
            .get(sampler_idx)
            .ok_or_else(|| format!("动作 {name} 的 samplers[{sampler_idx}] 不存在"))?;
        let interp = sampler
            .get("interpolation")
            .and_then(Value::as_str)
            .unwrap_or("LINEAR")
            .to_string();
        let input = sampler
            .get("input")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("动作 {name} 的 samplers[{sampler_idx}] 缺 input"))?
            as usize;
        let output = sampler
            .get("output")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("动作 {name} 的 samplers[{sampler_idx}] 缺 output"))?
            as usize;

        let (time_dims, time_data) = glb.read_accessor(input)?;
        if time_dims != 1 {
            return Err(format!(
                "动作 {name} 的 samplers[{sampler_idx}].input 应该是 SCALAR，实际维度 {time_dims}"
            ));
        }
        let (out_dims, out_data) = glb.read_accessor(output)?;
        let want_dims = if path == "rotation" { 4 } else { 3 };
        if out_dims != want_dims {
            return Err(format!(
                "动作 {name} 的 samplers[{sampler_idx}] path={path} 输出维度应为 {want_dims}，实际是 {out_dims}"
            ));
        }
        let mut times = time_data;
        let mut values: Vec<[f64; 4]> = out_data
            .chunks_exact(out_dims)
            .map(|chunk| {
                let mut row = [0.0f64; 4];
                row[..out_dims].copy_from_slice(chunk);
                row
            })
            .collect();
        if interp == "CUBICSPLINE" {
            // 三次样条的关键帧是 (入切线, 值, 出切线) 三元组；这里只取值，
            // 因为重定向要的是姿态本身，切线在两侧骨架不同构时会失真。
            values = values.into_iter().skip(1).step_by(3).collect();
            if values.len() != times.len() {
                return Err(format!(
                    "动作 {name} 的 samplers[{sampler_idx}] 是 CUBICSPLINE 但关键帧数对不上（{} vs {}）",
                    values.len(),
                    times.len()
                ));
            }
        }
        if values.len() != times.len() {
            return Err(format!(
                "动作 {name} 的 samplers[{sampler_idx}] 输入 {} 帧、输出 {} 帧，对不上",
                times.len(),
                values.len()
            ));
        }
        per_node.entry(node).or_default().push(Track {
            path: path.to_string(),
            interp,
            times: std::mem::take(&mut times),
            values,
            dims: out_dims,
        });
    }

    // 时间轴：所有通道的采样时刻求并集（源采样时刻原样保留，不重采样）。
    let mut union: Vec<f64> = per_node
        .values()
        .flat_map(|tracks| tracks.iter().flat_map(|t| t.times.iter().copied()))
        .collect();
    union.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    union.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    if union.is_empty() {
        return Err(format!("动作 {name} 里没有任何可用的关键帧"));
    }

    let frame_count = union.len();
    let mut frames: Vec<Vec<LocalTrs>> = skeleton
        .rest
        .iter()
        .map(|rest| vec![*rest; frame_count])
        .collect();
    let mut animated = vec![false; skeleton.len()];
    for (node, tracks) in &per_node {
        for track in tracks {
            if track.path == "scale" {
                // 缩放不重定向：源骨的缩放曲线直接忽略，目标保持 rest 缩放。
                continue;
            }
            animated[*node] = true;
            let mut prev: Option<DQuat> = None;
            for (fi, at) in union.iter().enumerate() {
                let raw = track.sample(*at);
                match track.path.as_str() {
                    "rotation" => {
                        let mut q = DQuat::from_xyzw(raw[0], raw[1], raw[2], raw[3]);
                        if q.length_squared() < 1e-12 {
                            continue; // 坏关键帧（全零四元数）留着会把整帧姿态打成 NaN
                        }
                        q = q.normalize();
                        if let Some(last) = prev {
                            if q.dot(last) < 0.0 {
                                q = DQuat::from_xyzw(-q.x, -q.y, -q.z, -q.w);
                            }
                        }
                        prev = Some(q);
                        frames[*node][fi].r = q;
                    }
                    "translation" => {
                        frames[*node][fi].t = DVec3::new(raw[0], raw[1], raw[2]);
                    }
                    _ => {}
                }
            }
        }
    }

    let track_count: usize = per_node.values().map(Vec::len).sum();
    if track_count == 0 {
        return Err(format!("动作 {name} 里没有作用于骨架的 TRS 通道"));
    }

    let joints: Vec<usize> = glb
        .arr("skins")
        .and_then(|skins| skins.first())
        .and_then(|skin| skin.get("joints"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_u64)
                .map(|v| v as usize)
                .filter(|v| *v < skeleton.len())
                .collect()
        })
        .unwrap_or_default();

    Ok(Clip {
        name,
        skeleton,
        times: union,
        frames,
        animated,
        joints,
        kind: "GLB",
    })
}

fn fbx_load_opts(hint: &str) -> ufbx::LoadOpts<'_> {
    // 与 `src/fbx.rs` 的 load_opts 保持一致：+X right / +Y up / +Z front，
    // 这样 FBX 源和 GLB 源进到重定向里是同一套坐标系。
    ufbx::LoadOpts {
        target_axes: ufbx::CoordinateAxes {
            right: ufbx::CoordinateAxis::PositiveX,
            up: ufbx::CoordinateAxis::PositiveY,
            front: ufbx::CoordinateAxis::PositiveZ,
        },
        target_unit_meters: 1.0,
        load_external_files: false,
        ignore_missing_external_files: true,
        evaluate_skinning: false,
        use_blender_pbr_material: true,
        generate_missing_normals: true,
        filename: ufbx::StringOpt::Ref(hint),
        ..Default::default()
    }
}

/// 读 FBX 的一段动作。烘焙率可调：动作本身关键帧稀疏时，30 fps 足以还原 UE 动作的观感。
pub fn clip_from_fbx(bytes: &[u8], hint: &str, rate: f64) -> Result<Clip, String> {
    let scene = ufbx::load_memory(bytes, fbx_load_opts(hint)).map_err(|err| {
        format!(
            "FBX 解析失败（{:?}）：{}",
            err.type_,
            if err.description.is_empty() {
                "无更多信息"
            } else {
                &err.description
            }
        )
    })?;
    let nodes = scene.nodes.iter().collect::<Vec<_>>();
    if nodes.is_empty() {
        return Err("FBX 里没有任何节点".to_string());
    }
    let mut index_of: BTreeMap<u32, usize> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        index_of.insert(node.element.element_id, i);
    }
    let names: Vec<String> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            // ufbx::String 实现的是 Deref<Target = str>，借出来就是 &str。
            let name: &str = &n.element.name;
            if name.is_empty() {
                format!("node#{i}")
            } else {
                name.to_string()
            }
        })
        .collect();
    let parent: Vec<Option<usize>> = nodes
        .iter()
        .map(|n| {
            n.parent
                .as_ref()
                .and_then(|p| index_of.get(&p.element.element_id).copied())
        })
        .collect();
    let rest: Vec<LocalTrs> = nodes
        .iter()
        .map(|n| {
            let tr = n.local_transform;
            let r = DQuat::from_xyzw(tr.rotation.x, tr.rotation.y, tr.rotation.z, tr.rotation.w);
            LocalTrs::new(
                DVec3::new(tr.translation.x, tr.translation.y, tr.translation.z),
                if r.length_squared() < 1e-12 {
                    DQuat::IDENTITY
                } else {
                    r.normalize()
                },
                DVec3::new(tr.scale.x, tr.scale.y, tr.scale.z),
            )
        })
        .collect();
    let skeleton = Skeleton {
        names,
        parent,
        rest,
    };

    let stack = scene
        .anim_stacks
        .iter()
        .next()
        .ok_or_else(|| "FBX 里没有动画栈：这不是动作文件".to_string())?;
    // AnimStack 的名字挂在 scene element 上（`stack.element.name`），
    // `Ref<Anim>` 本身不带名字，所以只从 element 取，取不到就叫 fbx_anim。
    let anim_name = {
        let own: &str = &stack.element.name;
        if own.is_empty() {
            "fbx_anim".to_string()
        } else {
            own.to_string()
        }
    };
    let opts = ufbx::BakeOpts {
        resample_rate: rate,
        trim_start_time: false,
        key_reduction_enabled: false,
        ..Default::default()
    };
    let baked = ufbx::bake_anim(&scene, &stack.anim, opts)
        .map_err(|err| format!("FBX 动作烘焙失败（{:?}）", err.type_))?;

    let mut per_node: BTreeMap<usize, Vec<Track>> = BTreeMap::new();
    for node in baked.nodes.iter() {
        let Some(&idx) = index_of.get(&node.element_id) else {
            continue;
        };
        let mut push = |path: &str, times: Vec<f64>, values: Vec<[f64; 4]>, dims: usize| {
            if times.is_empty() || times.len() != values.len() {
                return;
            }
            per_node.entry(idx).or_default().push(Track {
                path: path.to_string(),
                interp: "LINEAR".to_string(),
                times,
                values,
                dims,
            });
        };
        let (t_times, t_values): (Vec<f64>, Vec<[f64; 4]>) = node
            .translation_keys
            .iter()
            .map(|k| (k.time, [k.value.x, k.value.y, k.value.z, 0.0]))
            .unzip();
        push("translation", t_times, t_values, 3);
        let (r_times, r_values): (Vec<f64>, Vec<[f64; 4]>) = node
            .rotation_keys
            .iter()
            .map(|k| (k.time, [k.value.x, k.value.y, k.value.z, k.value.w]))
            .unzip();
        push("rotation", r_times, r_values, 4);
        // 缩放通道有意不读：重定向不搬缩放，目标保持 rest。
    }

    if per_node.is_empty() {
        return Err(format!("FBX 动作 {anim_name} 烘焙后没有任何节点关键帧"));
    }

    let mut union: Vec<f64> = per_node
        .values()
        .flat_map(|tracks| tracks.iter().flat_map(|t| t.times.iter().copied()))
        .collect();
    union.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    union.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    let frame_count = union.len();

    let mut frames: Vec<Vec<LocalTrs>> = skeleton
        .rest
        .iter()
        .map(|rest| vec![*rest; frame_count])
        .collect();
    let mut animated = vec![false; skeleton.len()];
    for (node, tracks) in &per_node {
        for track in tracks {
            animated[*node] = true;
            let mut prev: Option<DQuat> = None;
            for (fi, at) in union.iter().enumerate() {
                let raw = track.sample(*at);
                if track.path == "rotation" {
                    let mut q = DQuat::from_xyzw(raw[0], raw[1], raw[2], raw[3]);
                    if q.length_squared() < 1e-12 {
                        continue;
                    }
                    q = q.normalize();
                    if let Some(last) = prev {
                        if q.dot(last) < 0.0 {
                            q = DQuat::from_xyzw(-q.x, -q.y, -q.z, -q.w);
                        }
                    }
                    prev = Some(q);
                    frames[*node][fi].r = q;
                } else {
                    frames[*node][fi].t = DVec3::new(raw[0], raw[1], raw[2]);
                }
            }
        }
    }

    Ok(Clip {
        name: anim_name,
        skeleton,
        times: union,
        frames,
        animated,
        joints: Vec::new(),
        kind: "FBX",
    })
}

/// 按扩展名选链路。
pub fn load_clip(path: &std::path::Path, bytes: &[u8], want: Option<&str>) -> Result<Clip, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let hint = path.to_string_lossy().to_string();
    match ext.as_str() {
        "glb" => clip_from_glb(bytes, want),
        "fbx" => clip_from_fbx(bytes, &hint, 30.0),
        other => Err(format!(
            "源资产只支持 .glb / .fbx，收到 .{other}（`{}`）",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn clip_with_two_bones() -> Clip {
        let skeleton = Skeleton {
            names: vec!["a".into(), "b".into()],
            parent: vec![None, Some(0)],
            rest: vec![LocalTrs::IDENTITY, LocalTrs::IDENTITY],
        };
        Clip {
            name: "t".into(),
            skeleton,
            times: vec![0.0, 1.0],
            frames: vec![vec![LocalTrs::IDENTITY; 2]; 2],
            animated: vec![true, true],
            joints: Vec::new(),
            kind: "test",
        }
    }

    #[test]
    fn order_is_parent_first() {
        let clip = clip_with_two_bones();
        assert_eq!(clip.skeleton.order(), vec![0, 1]);
    }

    #[test]
    fn order_survives_broken_parent() {
        // 父子链成环：两根骨互为父，order 必须仍然覆盖所有骨骼（不丢骨）。
        let skeleton = Skeleton {
            names: vec!["a".into(), "b".into(), "c".into()],
            parent: vec![Some(1), Some(0), None],
            rest: vec![LocalTrs::IDENTITY; 3],
        };
        let order = skeleton.order();
        assert_eq!(order.len(), 3);
        assert_eq!(order[0], 2);
    }

    /// 关键帧区间内线性插值；区间外必须夹住两端（不是外推）。
    #[test]
    fn track_sampling_clamps_and_interpolates() {
        let track = Track {
            path: "translation".into(),
            interp: "LINEAR".into(),
            times: vec![0.0, 1.0],
            values: vec![[0.0, 0.0, 0.0, 0.0], [2.0, 4.0, 0.0, 0.0]],
            dims: 3,
        };
        assert_eq!(track.sample(-1.0), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(track.sample(0.5), [1.0, 2.0, 0.0, 0.0]);
        assert_eq!(track.sample(9.0), [2.0, 4.0, 0.0, 0.0]);
    }

    #[test]
    fn node_rest_prefers_matrix() {
        let node: Value = serde_json::from_str(
            r#"{"translation":[9,9,9],"matrix":[2,0,0,0, 0,2,0,0, 0,0,2,0, 1,2,3,1]}"#,
        )
        .unwrap();
        let rest = node_rest(&node);
        assert!((rest.t - DVec3::new(1.0, 2.0, 3.0)).length() < 1e-12);
        assert!((rest.s - DVec3::new(2.0, 2.0, 2.0)).length() < 1e-12);
    }
}
