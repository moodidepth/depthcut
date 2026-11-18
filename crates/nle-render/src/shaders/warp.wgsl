// Projective warp (stabilization): every output pixel samples the source at
// M · (uv, 1), with M the output-uv → source-uv homography computed on the
// CPU (`nle_core::motion::Stabilization::correction`). Samples that land
// outside the source are transparent (auto-crop normally prevents that).

struct Warp {
    row0: vec4<f32>,
    row1: vec4<f32>,
    row2: vec4<f32>,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> u: Warp;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(dims);
    let p = vec3<f32>(uv, 1.0);
    let w = dot(u.row2.xyz, p);
    let s = vec2<f32>(dot(u.row0.xyz, p), dot(u.row1.xyz, p)) / select(w, 1e-6, abs(w) < 1e-6);
    // half-texel tolerance so the auto-cropped border never shows a seam
    let tol = 0.5 / vec2<f32>(textureDimensions(src));
    var c = textureSampleLevel(src, samp, clamp(s, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
    if (any(s < -tol) || any(s > vec2<f32>(1.0) + tol)) {
        c = vec4<f32>(0.0);
    }
    textureStore(dst, vec2<i32>(id.xy), c);
}
