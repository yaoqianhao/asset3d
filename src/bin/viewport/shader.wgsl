//! 视口的最小着色器（M-UI-3 第二刀）。
//!
//! 只做「把三角形画出来」：一个方向光 + 一点边缘提亮，不采样贴图、不做 PBR。
//! 材质通道等 M-UI-3 后续刀再补，这一刀的判据是「能看出模型形状」。

struct Uniforms {
    view_proj: mat4x4<f32>,
    // xyz = 方向光（世界空间，指向光源），w 未用。用 vec4 是为了 16 字节对齐。
    light: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) world: vec3<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = u.view_proj * vec4<f32>(in.pos, 1.0);
    out.normal = in.normal;
    out.world = in.pos;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let l = normalize(u.light.xyz);
    // 双面：法线朝背后时翻过来，否则背面全黑，看起来像破面。
    let facing = select(-n, n, dot(n, l) >= 0.0);
    let diff = max(dot(facing, l), 0.0);
    let base = vec3<f32>(0.62, 0.68, 0.78);
    let lit = base * (0.22 + 0.78 * diff);
    // 一点点轮廓光，纯黑背景下更容易看清剪影。
    let rim = pow(1.0 - abs(dot(facing, normalize(-in.world))), 3.0) * 0.12;
    return vec4<f32>(lit + vec3<f32>(rim, rim, rim * 1.1), 1.0);
}
