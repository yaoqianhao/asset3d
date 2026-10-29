//! 视口的图形侧：surface + 一条网格管线 + 一份深度缓冲。
//!
//! M-UI-3 第一刀只做「窗口能出像素」（清屏）；第二刀补上静态网格与相机：顶点/索引
//! 缓冲一次性上传，相机每帧只改 uniform；第三刀补上材质贴图：场景按材质切成若干
//! 分区，每个分区一份 `baseColor` uniform + 一张贴图，换分区只换 bind group。
//!
//! 仍然不做骨骼动画、不做透明排序、不做 PBR——判据是「打开资产能看出长什么样」，
//! 不是「还原渲染引擎」。贴图与材质 uniform 的生命周期跟着 `GpuScene` 走：换资产时
//! 整批替换，显存不会随打开次数增长。
//!
//! 渲染只在被要求时发生（`request_redraw`）：静止时 GPU 空闲，笔记本风扇不会一直转。

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use winit::dpi::PhysicalSize;
use winit::window::Window;

use super::camera::Orbit;
use super::scene::{reference_grid, Scene, TexImage, Vertex};

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

/// 逐分区的材质参数。16 字节对齐（`vec4`）是 uniform 布局的硬要求，
/// 所以哪怕只有一个 `baseColor` 也用 4 个 f32。
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MaterialUniform {
    base_color: [f32; 4],
}

/// 参考地板的顶点：位置 + 顶点色。与模型顶点分开布局——网格线不需要 UV，
/// 硬塞进 32 字节的 `Vertex` 只是白白多传一份数据，还得让网格着色器声明一个
/// 用不到的输入。
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GridVertex {
    pos: [f32; 3],
    color: [f32; 3],
}

/// 上传到 GPU 的一个分区：索引区间 + 它自己的材质 bind group。
struct GpuPart {
    index_start: u32,
    index_count: u32,
    /// 多个分区共用同一材质时指向同一份 bind group。
    bind: Arc<wgpu::BindGroup>,
}

/// 上传到 GPU 的一份场景。
struct GpuScene {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    parts: Vec<GpuPart>,
    /// 蒙皮矩阵（`jointWorld × inverseBindMatrix`）的 storage buffer。
    /// 一份场景一份，顶点里的关节索引直接指进来。
    bones: wgpu::BindGroup,
    /// 索引总数：分区之外的兜底绘制用（正常情况下分区覆盖全部三角形）。
    index_count: u32,
    /// 各材质的 uniform 缓冲。只为持有生命周期，渲染时不直接读。
    #[allow(dead_code)]
    material_uniforms: Vec<wgpu::Buffer>,
    /// 骨骼 storage buffer 本体：bind group 会持有它，但显式留一份引用，
    /// 避免「绑定还在、缓冲先被回收」这类只在特定后端出现的时序问题。
    #[allow(dead_code)]
    bone_buffer: wgpu::Buffer,
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
    /// 材质绑定的布局与两件「兜底资源」：1×1 白纹理、线性重复采样器。
    /// 没有贴图的材质也照样绑一组，着色器里就不需要 `has_texture` 分支。
    material_layout: wgpu::BindGroupLayout,
    white_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    /// 默认材质的 bind group：场景没有分区（或分区不完整）时直接用它。
    fallback_bind: wgpu::BindGroup,
    /// 骨骼绑定（group 2）的布局，以及「一份恒等矩阵」的兜底绑定：
    /// 管线要求 group 2 必须绑上，静态资产也得占个位。
    bone_layout: wgpu::BindGroupLayout,
    fallback_bones: wgpu::BindGroup,
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

        // ---- 材质（group 1）：uniform + 贴图 + 采样器 ----
        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("asset3d-viewport-material-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        // 每个材质只传一张 2D 贴图，不做数组/立方体。
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let white_view = create_white_texture(&device, &queue);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("asset3d-viewport-sampler"),
            // Repeat：UV 在 0..1 之外时，贴图重复而不是夹边——提花类资产的 UV
            // 常故意超出 1，夹边会出现一条拉长的色带。
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let fallback_bind = {
            let material = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("asset3d-viewport-fallback-material"),
                contents: bytemuck::bytes_of(&MaterialUniform {
                    // 与第一刀的冷灰蓝一致：没有材质的资产看起来不该和上一版有差别。
                    base_color: [0.62, 0.68, 0.78, 1.0],
                }),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("asset3d-viewport-fallback-bind"),
                layout: &material_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: material.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&white_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            })
        };

        // ---- 骨骼（group 2）：只读 storage buffer，顶点着色器按索引取矩阵 ----
        // 为什么是 storage 而不是 uniform：uniform 数组在默认 limits 下只有 64 KiB，
        // 几百根骨头就到顶了；storage 一份 256 MiB 上限，蒙皮资产的天花板高得多。
        let bone_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("asset3d-viewport-bone-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let fallback_bones = {
            // 一份恒等矩阵：静态资产权重全零、根本不会读它，但 group 2 必须绑上，
            // 否则管线校验直接失败。
            let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("asset3d-viewport-fallback-bones"),
                contents: bytemuck::cast_slice(&[[
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
                    1.0f32,
                ]]),
                usage: wgpu::BufferUsages::STORAGE,
            });
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("asset3d-viewport-fallback-bones-bind"),
                layout: &bone_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            })
        };

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("asset3d-viewport-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout, &material_layout, &bone_layout],
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
                        // UV：偏移量跟着 `Vertex` 的字段顺序走（3+3 个 f32 = 24 字节）。
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 24,
                            shader_location: 2,
                        },
                        // 关节索引：u16×4（WGSL 里读成 vec4<u32>），偏移 32。
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Uint16x4,
                            offset: 32,
                            shader_location: 3,
                        },
                        // 关节权重：f32×4，偏移 40（u16×4 = 8 字节对齐后）。
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 40,
                            shader_location: 4,
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
        // 地板只吃 group 0：单独一份 layout，免得每帧为了「凑齐两组绑定」
        // 白绑一次材质——多绑一次也就多一次状态校验，但没必要。
        let grid_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("asset3d-viewport-grid-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        // 地面网格：线段，无光照，**不写深度**。写深度的后果是模型贴地的那层三角形
        // 会被网格线「提前占位」打洞（1 米一格的线正好穿过脚底）；只读深度的
        // `LessEqual` 让网格被模型挡住，又不影响后面的模型绘制。
        let grid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("asset3d-viewport-grid-pipeline"),
            layout: Some(&grid_layout),
            vertex: wgpu::VertexState {
                module: &grid_shader,
                entry_point: "vs_grid",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GridVertex>() as u64,
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
        // 场景侧的顶点是 32 字节的 `Vertex`（带 UV），地板只留位置 + 颜色。
        let packed: Vec<GridVertex> = grid
            .vertices
            .iter()
            .map(|vertex| GridVertex {
                pos: vertex.pos,
                color: vertex.normal,
            })
            .collect();
        let grid_vertex_count = packed.len() as u32;
        let grid_vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("asset3d-viewport-grid-vertices"),
            contents: bytemuck::cast_slice(&packed),
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
            material_layout,
            white_view,
            sampler,
            fallback_bind,
            bone_layout,
            fallback_bones,
            depth: Some(depth),
            scene: None,
            grid_pipeline,
            grid_vertices,
            grid_vertex_count,
        })
    }

    /// 把解析好的场景传上 GPU。旧缓冲在这里被替换，显存跟着最新的资产走。
    ///
    /// 细微处：材质按 `key` 去重。一个资产里 200 个网格共用 3 个材质是常态，
    /// 每个分区各建一份 bind group 会把同一张 2K 贴图上传 200 次——显存直接爆。
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

        // ---- 蒙皮矩阵：一张平铺的 storage buffer ----
        // 没有蒙皮资产时也建一份（1 个恒等矩阵），省掉渲染路径里的 Option 分支。
        let bone_buffer = {
            let contents: Vec<[f32; 16]> = if scene.joint_matrices.is_empty() {
                vec![[
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
                    1.0,
                ]]
            } else {
                scene.joint_matrices.clone()
            };
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("asset3d-viewport-bones"),
                    contents: bytemuck::cast_slice(&contents),
                    usage: wgpu::BufferUsages::STORAGE,
                })
        };
        let bones = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("asset3d-viewport-bones-bind"),
            layout: &self.bone_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: bone_buffer.as_entire_binding(),
            }],
        });

        // bind group 本身不实现 Clone（wgpu 22 的资源句柄都是唯一所有者），
        // 所以共享同一个材质时用 Arc 传引用，而不是复制一份 GPU 资源。
        let mut cache: HashMap<usize, Arc<wgpu::BindGroup>> = HashMap::new();
        let mut material_uniforms: Vec<wgpu::Buffer> = Vec::new();
        let mut parts: Vec<GpuPart> = Vec::with_capacity(scene.parts.len());
        for part in &scene.parts {
            let bind = match cache.get(&part.material.key) {
                Some(bind) => Arc::clone(bind),
                None => {
                    let uniform = self
                        .device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("asset3d-viewport-material"),
                            contents: bytemuck::bytes_of(&MaterialUniform {
                                base_color: part.material.base_color,
                            }),
                            usage: wgpu::BufferUsages::UNIFORM,
                        });
                    // 贴图视图只在建 bind group 时借出去一次，之后由 bind group
                    // 自己持有，所以这里不需要（也无法）clone 句柄。
                    let uploaded = part
                        .material
                        .texture
                        .as_ref()
                        .map(|image| upload_texture(&self.device, &self.queue, image));
                    // 没贴图就绑白纹理：`byte × 1.0` 等于只用 baseColor。
                    let view = uploaded.as_ref().unwrap_or(&self.white_view);
                    let bind = Arc::new(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        // 用材质名做调试标签：抓帧（RenderDoc / PIX）时能直接看出
                        // 哪一段是哪个材质，比 20 个同名 "material-bind" 强。
                        label: Some(if part.material.name.is_empty() {
                            "asset3d-viewport-material-bind"
                        } else {
                            &part.material.name
                        }),
                        layout: &self.material_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: uniform.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                        ],
                    }));
                    cache.insert(part.material.key, Arc::clone(&bind));
                    material_uniforms.push(uniform);
                    bind
                }
            };
            parts.push(GpuPart {
                index_start: part.index_start,
                index_count: part.index_count,
                bind,
            });
        }

        self.scene = Some(GpuScene {
            vertices,
            indices,
            parts,
            bones,
            index_count: scene.indices.len() as u32,
            // 缓冲句柄留着：bind group 会自己抓住资源，但保留一份引用能避免
            // 「绑定还在、缓冲先被回收」这种只在特定后端出现的时序问题。
            material_uniforms,
            bone_buffer,
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
                // group 2 是蒙皮矩阵：静态场景绑的是一份恒等矩阵占位，
                // 着色器按权重为 0 走静态分支，读都读不到它。
                pass.set_bind_group(2, &scene.bones, &[]);
                pass.set_vertex_buffer(0, scene.vertices.slice(..));
                pass.set_index_buffer(scene.indices.slice(..), wgpu::IndexFormat::Uint32);
                if scene.parts.is_empty() {
                    // 兜底：`scene::merge` 保证有三角形就至少一个分区，这条分支只是
                    // 防「将来改坏了 → 整块黑屏」的保险丝。
                    pass.set_bind_group(1, &self.fallback_bind, &[]);
                    pass.draw_indexed(0..scene.index_count, 0, 0..1);
                } else {
                    // 一个分区一个 draw call：换材质只换 bind group，不重建管线、
                    // 不重传缓冲。几十个材质的资产也就是几十次切换，肉眼无感。
                    for part in &scene.parts {
                        pass.set_bind_group(1, part.bind.as_ref(), &[]);
                        pass.draw_indexed(
                            part.index_start..part.index_start + part.index_count,
                            0,
                            0..1,
                        );
                    }
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}

/// 上传一张解码好的贴图。格式取 `Rgba8UnormSrgb`：像素字节原样（PNG 就是 sRGB），
/// 采样时由硬件转成线性，与着色器里的线性光照计算对齐。
fn upload_texture(device: &wgpu::Device, queue: &wgpu::Queue, image: &TexImage) -> wgpu::TextureView {
    let width = image.width.max(1);
    let height = image.height.max(1);
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("asset3d-viewport-texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // 不做 mipmap：没有多级渐远，远处贴图会有摩尔纹，但生成 mip 链要多一个
    // 计算管线 + 一次全链复制，对「看一眼资产」这个目标不划算。
    queue.write_texture(
        texture.as_image_copy(),
        &image.rgba,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        size,
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// 1×1 白纹理：没有贴图的材质也绑一张，着色器的采样路径就恒定不用分支。
fn create_white_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("asset3d-viewport-white"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // 必须真写进去：新建纹理的内容是未定义的，某些驱动上会全 0（材质全黑）。
    queue.write_texture(
        texture.as_image_copy(),
        &[255u8, 255, 255, 255],
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
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
