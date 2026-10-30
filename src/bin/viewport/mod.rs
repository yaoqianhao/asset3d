//! 视口进程的内聚模块（docs/24 §5）。
//!
//! - [`protocol`]：会话协议 `asset3d.session/1` 的编解码，纯函数、无 IO；
//! - [`scene`]：资产 → 可画的三角形（解析 AIR、烘节点变换、算包围球）；
//! - [`camera`]：轨道相机，只做转与推拉；
//! - [`gfx`]：wgpu 侧，只认「窗口 + 场景 + 相机 → 一帧」；
//! - [`win32`]：命名管道服务端、会话互斥体、管道名规范化、文件选择框，手写 FFI。
//! - [`workspace`]：工作区资产清单（后台线程扫描，带深度与条目上限）；
//! - [`ui`]：可点击 UI 的布局、命中测试与 GDI 位图绘制（中文文字不靠字形烘焙）。
//!
//! 前四个互不依赖，串起来的是 `asset3d-gui.rs`：它负责 argv 三形态、winit 事件循环、
//! 拖放与鼠标，以及「管道读线程 → 主线程渲染」这条跨线程通路。这样拆分是为了让
//! 每一块都能单独测：协议靠单测、管道靠冒烟、图形靠肉眼。

pub mod audit;
pub mod camera;
pub mod gfx;
pub mod protocol;
pub mod scene;
pub mod shot;
pub mod ui;
pub mod win32;
pub mod workspace;
