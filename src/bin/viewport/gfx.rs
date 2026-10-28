//! 视口的图形侧：surface + 一条网格管线 + 一份深度缓冲。
//!
//! M-UI-3 第一刀只做「窗口能出像素」（清屏）；第二刀补上静态网格与相机：顶点/索引
//! 缓冲一次性上传，相机每帧只改 uniform——不做骨骼动画、不采样贴图，材质通道留到
//! 后续刀（先让「打开资产能看出形状」这件事成立）。
//!
//! 渲染只在被要求时发生（`request_redraw`）：静止时 GPU 空闲，笔记本风扇不会一直转。

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use winit::dpi::PhysicalSize;
use winit::window::Window;

use super::camera::Orbit;
use super::scene::{reference_grid, Scene, Vertex};

/// 视口底色：深蓝灰（sRGB）。比纯黑更容易看出「是视口，不是窗口没画」。
const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.055,
    g: 0.063,
    b: 0.078,
    a: 1.0,
};

/// 深度格式：32 位比 24 位省心（不需要 pack 到 depth24plus-stencil8 的对齐要求）。
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// 方向光（世界空间，指向光源）。固定一个斜上方主光，够看清体积即可。
const LIGHT: [f32; 4] = [0.45, 0.8, 0.38, 0.0];

/// 着色器编进可执行文件：视口是单文件分发（`dist/asset3d-gui.exe`），
/// 外部 `.wgsl` 一旦漏打就是「开机黑屏 + 找不到文件」。
const SHADER: &str = include_str!("shader.wgsl");

/// 参考地板（地面网格 + 坐标轴）的着色器：不参与光照，顶点色直出。
const GRID_SHADER: &str = include_str!("grid.wgsl");

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    light: [f32; 4],
}

/// 上传到 GPU 的一份场景。
struct GpuScene {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

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
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    depth: Option<wgpu::TextureView>,
    scene: Option<GpuScene>,
    /// 参考地板：常驻的一条线段缓冲。没有模型的空窗口靠它证明「视口活着」，
    /// 有模型时它就是地面参照，所以一直画、不随 `clear_scene` 消失。
    grid_pipeline: wgpu::RenderPipeline,
    grid_vertices: wgpu::Buffer,
    grid_vertex_count: u32,
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

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("asset3d-viewport-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("asset3d-viewport-uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("asset3d-viewport-bind-group-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("asset3d-viewport-bind-group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("asset3d-viewport-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("asset3d-viewport-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: 12,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // 双面渲染：资产的 winding 各家不同（FBX 常见 CW），剔除会让人以为
                // 「模型少了一半」。着色器里靠 facing 修正光照。
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let grid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("asset3d-viewport-grid-shader"),
            source: wgpu::ShaderSource::Wgsl(GRID_SHADER.into()),
        });
        // 地面网格：线段，无光照，**不写深度**。写深度的后果是模型贴地的那层三角形
        // 会被网格线「提前占位」打洞（1 米一格的线正好穿过脚底）；只读深度的
        // `LessEqual` 让网格被模型挡住，又不影响后面的模型绘制。
        let grid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("asset3d-viewport-grid-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &grid_shader,
                entry_point: "vs_grid",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: 12,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &grid_shader,
                entry_point: "fs_grid",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::LessEqual,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let grid = reference_grid();
        let grid_vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("asset3d-viewport-grid-vertices"),
            contents: bytemuck::cast_slice(&grid.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let depth = create_depth(&device, &config);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            configured: size.width > 0 && size.height > 0,
            pipeline,
            uniforms,
            bind_group,
            depth: Some(depth),
            scene: None,
            grid_pipeline,
            grid_vertices,
            grid_vertex_count: grid.vertices.len() as u32,
        })
    }

    /// 把解析好的场景传上 GPU。旧缓冲在这里被替换，显存跟着最新的资产走。
    pub fn set_scene(&mut self, scene: &Scene) {
        let vertices = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("asset3d-viewport-vertices"),
                contents: bytemuck::cast_slice(&scene.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let indices = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("asset3d-viewport-indices"),
                contents: bytemuck::cast_slice(&scene.indices),
                usage: wgpu::BufferUsages::INDEX,
            });
        self.scene = Some(GpuScene {
            vertices,
            indices,
            index_count: scene.indices.len() as u32,
        });
    }

    pub fn clear_scene(&mut self) {
        self.scene = None;
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
        // 深度缓冲必须跟着 surface 一起重建，否则大小不一致会直接 panic。
        self.depth = Some(create_depth(&self.device, &self.config));
        self.configured = true;
    }

    /// 画一帧。返回 `Err` 表示这一帧没画成——调用方只记日志，不重试，
    /// 下一次 `request_redraw` 会自然再试。
    pub fn render(&mut self, camera: &Orbit) -> Result<(), String> {
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

        let aspect = self.config.width as f32 / self.config.height.max(1) as f32;
        let uniforms = Uniforms {
            view_proj: camera.view_proj(aspect).to_cols_array_2d(),
            light: LIGHT,
        };
        self.queue
            .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&uniforms));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("asset3d-viewport-frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("asset3d-viewport-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: self.depth.as_ref().map(|depth| {
                    wgpu::RenderPassDepthStencilAttachment {
                        view: depth,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // 先画地板再画模型：地板不写深度，顺序上放前面才能让模型正常遮挡它。
            if self.grid_vertex_count > 0 {
                pass.set_pipeline(&self.grid_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.grid_vertices.slice(..));
                pass.draw(0..self.grid_vertex_count, 0..1);
            }
            if let Some(scene) = &self.scene {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, scene.vertices.slice(..));
                pass.set_index_buffer(scene.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..scene.index_count, 0, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}

fn create_depth(device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("asset3d-viewport-depth"),
        size: wgpu::Extent3d {
            width: config.width.max(1),
            height: config.height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// 相机与投影的默认值：没有资产时也有一台「看着原点」的相机，避免 render 里
/// 到处 `Option` 判断。
///
/// 半径取 6 米而不是 1 米：空场景里画的是 20×20 的地板网格，相机得退到能看见
/// 一片格子的位置，否则屏幕上只有两条贴在脸上的线，看起来比清屏还像没启动。
pub fn idle_camera() -> Orbit {
    Orbit::fit(glam::Vec3::new(0.0, 0.8, 0.0), 6.0)
}
