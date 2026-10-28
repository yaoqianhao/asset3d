//! 视口的着色器（M-UI-3 第二刀 + 第三刀材质贴图）。
//!
//! 第二刀只做「把三角形画出来」；第三刀补上 baseColorFactor 与 baseColorTexture：
//! 采样贴图 × 材质基色，仍是一个方向光 + 一点边缘提亮，不做 PBR、法线贴图、
//! 金属度/粗糙度。判据是「能看出模型长什么样、花纹对不对」。
//!
//! 两套绑定：`group(0)` 是每帧不变的相机与光照，`group(1)` 是逐分区的材质——
//! 换材质只换 bind group，不重建管线。

struct Uniforms {
    view_proj: mat4x4<f32>,
    // xyz = 方向光（世界空间，指向光源），w 未用。用 vec4 是为了 16 字节对齐。
    light: vec4<f32>,
};

struct Material {
    // glTF 的 baseColorFactor / FBX 的 DiffuseColor，已 clamp 到 0..1。
    base_color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var<uniform> m: Material;
// 没有贴图的材质也走同一条路径：绑一张 1×1 白纹理，着色器里就不用分支了。
@group(1) @binding(1) var tex: texture_2d<f32>;
@group(1) @binding(2) var samp: sampler;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) world: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = u.view_proj * vec4<f32>(in.pos, 1.0);
    out.normal = in.normal;
    out.world = in.pos;
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let l = normalize(u.light.xyz);
    // 双面：法线朝背后时翻过来，否则背面全黑，看起来像破面。
    let facing = select(-n, n, dot(n, l) >= 0.0);
    let diff = max(dot(facing, l), 0.0);

    // 贴图是 sRGB 格式，采样时硬件已转成线性；baseColorFactor 在 glTF 里本就是
    // 线性值，两者直接相乘即物理上说的 albedo。
    let albedo = m.base_color.rgb * textureSample(tex, samp, in.uv).rgb;
    // 环境项给到 0.35：纯方向光下背光面全是死黑，资产细节看不出来。
    let lit = albedo * (0.35 + 0.65 * diff);
    // 一点点轮廓光，纯黑背景下更容易看清剪影。
    let rim = pow(1.0 - abs(dot(facing, normalize(-in.world))), 3.0) * 0.10;
    // alpha 恒为 1：本刀不做透明排序，半透明资产按不透明画（只取它的颜色）。
    return vec4<f32>(lit + vec3<f32>(rim, rim, rim), 1.0);
}
