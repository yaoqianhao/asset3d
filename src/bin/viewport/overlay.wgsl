// 屏幕空间覆盖层：时间轴的槽与进度条。
//
// 顶点直接就是裁剪空间坐标（x/y ∈ [-1, 1]），不经 view_proj——覆盖层要是跟着
// 相机转，就失去了「UI」的意义。没有深度参与，也不读任何 bind group。

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) color: vec3<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_overlay(in: VsIn) -> VsOut {
    var out: VsOut;
    // 复用 GridVertex（pos + color 的 3+3 布局），z 当透明度用：省得为了一个
    // 覆盖层再定义一套顶点结构。
    out.clip = vec4<f32>(in.pos.xy, 0.0, 1.0);
    out.color = vec4<f32>(in.color, in.pos.z);
    return out;
}

@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
