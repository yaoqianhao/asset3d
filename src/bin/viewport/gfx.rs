//! 视口的图形侧：一个 surface、一份 clear 通道。
//!
//! 这一刀（M-UI-3 第 1 刀）只做「窗口能出像素」：初始化 device/queue/surface，
//! 每帧拿一张纹理、清成视口的默认底色。静态网格的顶点/索引缓冲、相机、
//! 材质通道在下一刀加——先把「进程能起、窗口能开、管道能连」这三件事钉死，
//! 免得图形问题与进程问题混在一起查。

use std::sync::Arc;
use winit::dpi::PhysicalSize;
use winit::window::Window;

/// 视口底色：深蓝灰（sRGB）。比纯黑更容易看出「是视口，不是窗口没画」。
const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.055,
    g: 0.063,
    b: 0.078,
    a: 1.0,
};

/// 渲染器。`surface` 用 `'static`：持有一份 `Arc<Window>` 的克隆，
/// 生命周期与窗口本身一致，比到处传 `&'a Window` 干净。
pub struct Gfx {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// 当前帧是否已经渲染过——窗口最小化时 surface 尺寸为 0，配置会失败，
    /// 这期间跳过渲染而不是每帧报错刷日志。
    configured: bool,
}

impl Gfx {
    pub fn new(window: Arc<Window>) -> Result<Self, String> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|err| format!("创建 surface 失败：{err}"))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            // 集成显卡也能跑：视口只画一个网格，不需要独立 GPU。
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .ok_or_else(|| {
            "没有找到可用的图形适配器（GPU 驱动缺失或被禁用）：视口需要一块能跑 D3D12/Vulkan 的显卡。"
                .to_string()
        })?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("asset3d-viewport"),
                required_features: wgpu::Features::empty(),
                // 默认 limits 就够：顶点缓冲按 MB 级算，远没到上限。
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .map_err(|err| format!("创建 GPU 设备失败：{err}"))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            configured: size.width > 0 && size.height > 0,
        })
    }

    /// 窗口尺寸变化。宽或高归零（最小化）时不重配，留给下一次有效的 resize。
    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            self.configured = false;
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.configured = true;
    }

    /// 画一帧。返回 `Err` 表示这一帧没画成——调用方只记日志，不重试，
    /// 下一次 `request_redraw` 会自然再试。
    pub fn render(&mut self) -> Result<(), String> {
        if !self.configured {
            return Ok(());
        }
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                // surface 与窗口脱钩（拔显示器、改缩放）：重配一次即可。
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(wgpu::SurfaceError::OutOfMemory) => {
                return Err("显存不足：关掉其他 3D 程序后重开视口。".to_string());
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("asset3d-viewport-frame"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("asset3d-viewport-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // 网格通道在下一刀插进来，位置就是这里。
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}
