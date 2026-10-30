// GUI 覆盖层：把 CPU 侧用 GDI 画好的位图当纹理整屏贴回去。
//
// 为什么整屏铺一张位图而不是每个控件一个 quad：控件的位置、文字、hover 态都在
// CPU 侧用真实字体度量算一次，GPU 只负责把结果贴上去——两者不会因为「格子算得
// 不一样」错位。命中测试用的是同一份矩形表（`ui::Layout`），点哪儿亮哪儿就是
// 必然的。
//
// 顶点不走顶点缓冲：一个全屏矩形六个角点，用 `vertex_index` 直接算最省事。
// 位图与窗口同尺寸，UV 0..1 正好一一对应，不做任何缩放采样。

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var ui_tex: texture_2d<f32>;
@group(0) @binding(1) var ui_sampler: sampler;

@vertex
fn vs_ui(@builtin(vertex_index) index: u32) -> VsOut {
    // 两个三角形拼成的矩形，UV 原点在左上（与 DIB 的行序一致）。
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let corner = corners[index];
    var out: VsOut;
    // 裁剪空间 y 朝上，位图 y 朝下：这里翻一次，后面所有坐标都按屏幕像素说话。
    out.clip = vec4<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_ui(in: VsOut) -> @location(0) vec4<f32> {
    // 透明度由 CPU 侧算好（不在任何控件矩形内的像素 alpha=0），这里直接透传。
    return textureSample(ui_tex, ui_sampler, in.uv);
}
