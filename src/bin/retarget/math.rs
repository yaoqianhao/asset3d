//! 重定向用的最小数学层：局部 TRS ↔ 4x4 矩阵、四元数链式乘法与增量。
//!
//! 全部用 f64：骨骼链只有几十级，但四元数增量对舍入很敏感（源和目标都可能有
//! 0.9999… 这种近单位量），f32 在这个量级上做「差一个反演」会放大到 1e-4 级别，
//! 正好压在验收的数值比对上，所以这里不省精度。

use glam::{DMat3, DMat4, DQuat, DVec3};

/// 单个节点的局部变换，语义与 glTF node 的 TRS 一致（父空间 → 本节点）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalTrs {
    pub t: DVec3,
    pub r: DQuat,
    pub s: DVec3,
}

impl LocalTrs {
    pub const IDENTITY: LocalTrs = LocalTrs {
        t: DVec3::ZERO,
        r: DQuat::IDENTITY,
        s: DVec3::ONE,
    };

    pub fn new(t: DVec3, r: DQuat, s: DVec3) -> Self {
        Self { t, r, s }
    }

    /// 局部 TRS → 4x4（列主序与 glam 一致：平移在 w 轴）。
    pub fn matrix(&self) -> DMat4 {
        DMat4::from_scale_rotation_translation(self.s, self.r, self.t)
    }

    /// 4x4 → 局部 TRS。镜像（行列式为负）时把符号吃进 X 轴缩放，
    /// 保证四元数仍是纯旋转——glTF 里正常资产不会出现，但不能 panic。
    pub fn from_matrix(m: &DMat4) -> Self {
        let t = m.w_axis.truncate();
        let mut sx = m.x_axis.truncate().length();
        let mut sy = m.y_axis.truncate().length();
        let mut sz = m.z_axis.truncate().length();
        if sx < 1e-12 {
            sx = 1.0;
        }
        if sy < 1e-12 {
            sy = 1.0;
        }
        if sz < 1e-12 {
            sz = 1.0;
        }
        let mut basis = DMat3::from_cols(
            m.x_axis.truncate() / sx,
            m.y_axis.truncate() / sy,
            m.z_axis.truncate() / sz,
        );
        if basis.determinant() < 0.0 {
            sx = -sx;
            basis = DMat3::from_cols(
                m.x_axis.truncate() / sx,
                m.y_axis.truncate() / sy,
                m.z_axis.truncate() / sz,
            );
        }
        Self {
            t,
            r: DQuat::from_mat3(&basis).normalize(),
            s: DVec3::new(sx, sy, sz),
        }
    }
}

impl Default for LocalTrs {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[cfg(test)]
/// 局部空间增量：`dst_local_anim = dst_local_rest * (src_local_rest^-1 * src_local_anim)`。
///
/// 要求两侧骨骼的局部轴定义一致（同一套绑定骨架或严格同构的骨架）才等价于世界增量，
/// 因此 `plan.rs` 用的是它的世界空间等价形式，这里保留是为了对照算法定义与单测。
pub fn local_delta(src_rest: DQuat, src_anim: DQuat, dst_rest: DQuat) -> DQuat {
    dst_rest * (src_rest.inverse() * src_anim)
}

#[cfg(test)]
/// 世界空间增量：`delta = src_world_anim * src_world_rest^-1`。
///
/// 与 [`local_delta`] 是同一个式子的另一种写法：把「相对父骨骼」的增量换成
/// 「相对世界」的增量后，源骨架（UE：+X 指向子骨）与目标骨架（Tripo：+Y 指向子骨）
/// 的局部轴约定差异会被自动消掉，这正是跨绑定重定向能对上的原因。
pub fn world_delta(src_world_rest: DQuat, src_world_anim: DQuat) -> DQuat {
    src_world_anim * src_world_rest.inverse()
}

#[cfg(test)]
/// 两个旋转之间「差了多少」的弦长度量，取代 `angle_between` 做数值断言。
///
/// `angle_between` 内部是 acos：在 1 附近 acos 的导数发散，两个只差 1e-16 的四元数
/// 与差 1e-8 的四元数会算出同一个 0（f64 上 acos 的分辨率天花板约 1.5e-8 弧度）。
/// 所以「两套独立实现必须算出同一个旋转」这种断言只能用它判死会误判；弦长
/// `|a ∓ b|` 在小角度上对角度是线性的，正好是能分辨 1e-16 的那把尺子。
pub fn quat_chord(a: DQuat, b: DQuat) -> f64 {
    let a = [a.x, a.y, a.z, a.w];
    let b = [b.x, b.y, b.z, b.w];
    // q 与 -q 是同一个旋转，两种符号各算一次取小的
    let same: f64 = a.iter().zip(&b).map(|(x, y)| (x - y) * (x - y)).sum();
    let flipped: f64 = a.iter().zip(&b).map(|(x, y)| (x + y) * (x + y)).sum();
    same.min(flipped).sqrt()
}

/// 把单位向量 `from` 转到 `to` 的最短弧旋转（swing，不含 twist）。
///
/// 用途：两套绑定的 rest 骨朝向往往差很远（UE 人形是 T-pose，Tripo 生成的角色多是
/// 手臂下垂的 A-pose）。世界增量只描述「相对各自 rest 转了多少」，直接乘到目标
/// rest 上时，下垂的手臂会把整段增量吃掉——源把手臂抬平 90°，目标却只从下垂态
/// 转到水平偏向，看起来像没抬手。先用最短弧把目标 rest 骨朝向旋到源 rest 骨朝向，
/// 增量就落在同一个基准上，同为 `delta * align * rest` 的形式。
///
/// 用最短弧而不是任意对齐矩阵：避开绕骨轴方向的自由度，目标绑定自身的 twist 得以
/// 保留，不会凭空给四肢加扭转。两向量反向（180°）时最短弧不唯一，退化为绕 X 轴。
pub fn shortest_arc(from: DVec3, to: DVec3) -> DQuat {
    let a = from.normalize_or_zero();
    let b = to.normalize_or_zero();
    if a.length_squared() < 0.5 || b.length_squared() < 0.5 {
        return DQuat::IDENTITY;
    }
    let d = a.dot(b).clamp(-1.0, 1.0);
    if d > 1.0 - 1e-9 {
        return DQuat::IDENTITY;
    }
    if d < -1.0 + 1e-9 {
        // 反向：任取一条与 a 垂直的轴转 180°
        let axis = if a.x.abs() < 0.9 {
            DVec3::X.cross(a)
        } else {
            DVec3::Y.cross(a)
        };
        return DQuat::from_axis_angle(axis.normalize(), std::f64::consts::PI);
    }
    let axis = a.cross(b);
    DQuat::from_axis_angle(axis.normalize(), d.acos())
}

/// 四元数取与参考同半球的那个：逐帧写关键帧时避免 q / -q 抖动导致插值绕远路。
pub fn same_hemisphere(q: DQuat, reference: DQuat) -> DQuat {
    if q.dot(reference) < 0.0 {
        DQuat::from_xyzw(-q.x, -q.y, -q.z, -q.w)
    } else {
        q
    }
}





#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_3;

    fn q(axis: DVec3, angle: f64) -> DQuat {
        DQuat::from_axis_angle(axis.normalize(), angle)
    }

    /// 同一套绑定时，局部增量法必须精确复现源动作，且与世界增量法同解。
    ///
    /// 这条边界值得钉死：`dst_local_anim = dst_local_rest · (src_local_rest⁻¹ ·
    /// src_local_anim)` 成立的前提是两侧骨骼的局部轴一致（即同一套绑定）。
    /// 一旦目标 rest ≠ 源 rest（跨绑定，本工具的真实场景），局部增量法就不再等价于
    /// 世界增量法——所以 plan.rs 走的是「世界增量 + 目标 rest 重投影」，这里用单测
    /// 把两条路只在同绑定时重合这件事写清楚，避免以后有人把它当通用公式搬回去。
    #[test]
    fn local_delta_reproduces_source_on_the_same_rig() {
        let rest_p = q(DVec3::X, 0.3);
        let rest_b = q(DVec3::Y, -0.2);
        let anim_p = q(DVec3::X, 0.45);
        let anim_b = q(DVec3::Z, 0.7);

        let world_rest_b = rest_p * rest_b;
        let world_anim_b = anim_p * anim_b;

        // 局部路：父骨、子骨各自套自己的增量，再合成世界
        let dst_anim_p = local_delta(rest_p, anim_p, rest_p);
        let dst_anim_b = local_delta(rest_b, anim_b, rest_b);
        assert!(quat_chord(dst_anim_p, anim_p) < 1e-15);
        assert!(quat_chord(dst_anim_b, anim_b) < 1e-15);
        let local_way = dst_anim_p * dst_anim_b;

        // 世界路：子骨的世界增量直接乘回目标世界 rest
        let world_way = world_delta(world_rest_b, world_anim_b) * world_rest_b;

        assert!(quat_chord(world_way, world_anim_b) < 1e-15);
        assert!(quat_chord(local_way, world_way) < 1e-15);
    }

    /// 增量在 rest 处必须归零：源动画首帧就是 rest 姿态时，目标也不该被拧一下。
    #[test]
    fn world_delta_is_identity_at_rest() {
        let qw = q(DVec3::new(0.3, 1.0, -0.2), 0.8);
        assert!(world_delta(qw, qw).angle_between(DQuat::IDENTITY) < 1e-12);
    }

    /// 矩阵 ↔ TRS 往返：带平移、旋转、缩放时不能丢。
    #[test]
    fn trs_matrix_round_trip() {
        let trs = LocalTrs::new(
            DVec3::new(0.1, -0.25, 0.33),
            q(DVec3::new(0.2, 1.0, 0.1), FRAC_PI_3),
            DVec3::new(1.0, 0.5, 2.0),
        );
        let back = LocalTrs::from_matrix(&trs.matrix());
        assert!((back.t - trs.t).length() < 1e-12);
        // 旋转用弦长比：这条路的误差在 1e-16 量级，走 acos 的 angle_between 分辨不出来
        assert!(quat_chord(back.r, trs.r) < 1e-12);
        assert!((back.s - trs.s).length() < 1e-12);
    }

    /// 半球归一化：dot < 0 时取反，dot > 0 时原样。
    #[test]
    fn hemisphere_flip_only_when_needed() {
        let a = q(DVec3::Y, 0.3);
        let neg = DQuat::from_xyzw(-a.x, -a.y, -a.z, -a.w);
        assert!((same_hemisphere(neg, a).dot(a) - 1.0).abs() < 1e-12);
        assert!((same_hemisphere(a, a) - a).length() < 1e-12);
    }

    /// 最短弧必须把 from 精确转到 to，且不动绕轴方向的额外扭转。
    #[test]
    fn shortest_arc_maps_from_onto_to() {
        let cases = [
            (DVec3::NEG_Y, DVec3::X),                 // A-pose 下垂 → T-pose 平举
            (DVec3::X, DVec3::new(0.3, 1.0, -0.4)),   // 斜向
            (DVec3::new(1.0, 1.0, 1.0), DVec3::NEG_Z),// 任意方向
        ];
        for (from, to) in cases {
            let r = shortest_arc(from, to);
            let got = r * from.normalize();
            assert!(
                got.angle_between(to.normalize()) < 1e-12,
                "{from:?} → {to:?} 实际落到 {got:?}"
            );
            // 最短弧的转轴垂直于两端向量：转过去之后沿轴的分量不该变
            let axis = from.normalize().cross(to.normalize()).normalize_or_zero();
            if axis.length_squared() > 0.5 {
                let before = from.normalize().dot(axis);
                let after = got.dot(axis);
                assert!((before - after).abs() < 1e-12);
            }
        }
    }

    /// 退化输入：同向给单位旋转、反向给 180°、零向量给单位旋转。
    #[test]
    fn shortest_arc_handles_degenerate_inputs() {
        let same = shortest_arc(DVec3::X, DVec3::X);
        assert!(same.angle_between(DQuat::IDENTITY) < 1e-12);

        let opposite = shortest_arc(DVec3::X, DVec3::NEG_X);
        let flipped = opposite * DVec3::X;
        assert!((flipped - DVec3::NEG_X).length() < 1e-12);
        assert!((opposite.angle_between(DQuat::IDENTITY).to_degrees() - 180.0).abs() < 1e-9);

        // 零向量（骨长退化）不该产生 NaN
        let guard = shortest_arc(DVec3::ZERO, DVec3::X);
        assert!(guard.is_finite());
        assert!(guard.angle_between(DQuat::IDENTITY) < 1e-12);
    }
}
