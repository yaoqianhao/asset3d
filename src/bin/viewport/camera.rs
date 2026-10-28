//! 轨道相机（M-UI-3 第二刀）。
//!
//! 只做看图需要的最小交互：左键拖动转、滚轮推拉。没有平移、没有 gizmo——这一刀
//! 的判据是「打开资产能看清形状」，不是「可编辑的检视器」。
//!
//! 单位与 AIR 一致（米）；`fit` 用包围球算距离，避免镜头怼在模型内部或飞得太远。

use glam::{Mat4, Vec3};

/// 45° 视场角：与人眼阅读 3D 模型的习惯接近，透不过分夸张。
const FOV_Y: f32 = 45.0;

/// 俯仰限制：留 0.05 弧度余量避开正上/正下的万向锁。
const PITCH_LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.05;

pub struct Orbit {
    pub target: Vec3,
    pub distance: f32,
    pub yaw: f32,
    pub pitch: f32,
    /// 包围球半径：定远近裁剪面用，裁剪面太近会让模型被切掉，太远则深度精度不够。
    radius: f32,
}

impl Orbit {
    /// 按包围球自动取景：距离让球正好落在 45° 视锥里，再放 20% 余量。
    pub fn fit(center: Vec3, radius: f32) -> Self {
        let radius = radius.max(1e-3);
        let distance = radius / (FOV_Y.to_radians() * 0.5).tan() * 1.2;
        Self {
            target: center,
            distance,
            // 默认右前上方 3/4 视角：比正视更能看出体积。
            yaw: 0.6,
            pitch: 0.35,
            radius,
        }
    }

    /// 鼠标拖动：像素增量 → 弧度。灵敏度 0.01 rad/px，接近其他看模工具的体感。
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.01;
        self.pitch = (self.pitch + dy * 0.01).clamp(-PITCH_LIMIT, PITCH_LIMIT);
        // yaw 不做取模：连续转十几圈后 f32 精度会掉，滚一圈就归一化回来。
        if self.yaw.abs() > std::f32::consts::TAU * 100.0 {
            self.yaw %= std::f32::consts::TAU;
        }
    }

    /// 滚轮：`delta` 为滚动格数，正数放大。指数缩放让远近手感一致。
    pub fn zoom(&mut self, delta: f32) {
        let factor = (-delta * 0.12).exp();
        let min = self.radius * 0.02;
        let max = self.radius * 200.0;
        self.distance = (self.distance * factor).clamp(min.max(1e-4), max);
    }

    pub fn eye(&self) -> Vec3 {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        self.target
            + Vec3::new(cos_pitch * sin_yaw, sin_pitch, cos_pitch * cos_yaw) * self.distance
    }

    /// 视图投影矩阵。wgpu 的裁剪空间深度是 0..1，用 glam 的 `perspective_rh`
    /// （不是 `perspective_rh_gl`，那个是 -1..1，会让近处被裁掉）。
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let near = (self.distance * 0.01).max(self.radius * 1e-4).max(1e-5);
        let far = self.distance + self.radius * 8.0 + 10.0;
        let projection = Mat4::perspective_rh(FOV_Y.to_radians(), aspect.max(0.01), near, far);
        let view = Mat4::look_at_rh(self.eye(), self.target, Vec3::Y);
        projection * view
    }
}
