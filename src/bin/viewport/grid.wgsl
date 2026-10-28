// 参考地板（地面网格 + 坐标轴）的着色器：只吃顶点里的颜色，不做光照。
//
// 与 `shader.wgsl` 共用同一个 uniform 绑定（`view_proj`），因此不需要第二套
// bind group；线段没有面法线概念，光照在这里毫无意义，直接输出顶点色更省也更好看。

struct Uniforms {
    view_proj: mat4x4<f32>,
    light: vec4<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    // 顶点布局里这个位置是「法线」，网格复用成颜色（见 `scene::reference_grid`）。
    @location(0) color: vec3<f32>,
};

@vertex
fn vs_grid(@location(0) pos: vec3<f32>, @location(1) color: vec3<f32>) -> VsOut {
    var out: VsOut;
    out.clip = uniforms.view_proj * vec4<f32>(pos, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_grid(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
