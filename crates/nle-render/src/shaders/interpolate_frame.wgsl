// Optical-flow frame interpolation (smooth slow motion, frame-rate
// conversion): bidirectional motion-compensated warping with occlusion-aware
// blending.
//
// With forward flow F01 (A → B) and backward flow F10 (B → A), the flow from
// the in-between instant t back to each source frame is approximated as in
// Super-SloMo:
//     F_t0 = -(1 - t)·t·F01 + t²·F10
//     F_t1 = (1 - t)²·F01 - t·(1 - t)·F10
// A is sampled at x + F_t0 and B at x + F_t1. Each side is weighted by its
// temporal distance and by forward/backward flow consistency, so pixels that
// are occluded in one frame are taken from the other one (no ghosting).

struct Interp {
    t: f32,
    sigma: f32,     // consistency tolerance (flow pixels)
    _pad0: f32,
    _pad1: f32,
};

@group(0) @binding(0) var img_a: texture_2d<f32>;
@group(0) @binding(1) var img_b: texture_2d<f32>;
@group(0) @binding(2) var flow_fw: texture_2d<f32>;
@group(0) @binding(3) var flow_bw: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;
@group(0) @binding(5) var<uniform> u: Interp;
@group(0) @binding(6) var dst: texture_storage_2d<rgba16float, write>;

// flow at uv, converted to uv units
fn fw(uv: vec2<f32>, fd: vec2<f32>) -> vec2<f32> {
    return textureSampleLevel(flow_fw, samp, uv, 0.0).xy / fd;
}
fn bw(uv: vec2<f32>, fd: vec2<f32>) -> vec2<f32> {
    return textureSampleLevel(flow_bw, samp, uv, 0.0).xy / fd;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let t = clamp(u.t, 0.0, 1.0);
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(dims);
    let fd = vec2<f32>(textureDimensions(flow_fw));
    let f01 = fw(uv, fd);
    let f10 = bw(uv, fd);
    let ft0 = -(1.0 - t) * t * f01 + t * t * f10;
    let ft1 = (1.0 - t) * (1.0 - t) * f01 - t * (1.0 - t) * f10;
    let u0 = uv + ft0;
    let u1 = uv + ft1;
    let c0 = textureSampleLevel(img_a, samp, u0, 0.0);
    let c1 = textureSampleLevel(img_b, samp, u1, 0.0);

    // forward/backward consistency at the sampled positions (in flow pixels)
    let a0 = fw(u0, fd);
    let e0 = length((a0 + bw(u0 + a0, fd)) * fd);
    let b1 = bw(u1, fd);
    let e1 = length((b1 + fw(u1 + b1, fd)) * fd);
    let s2 = max(u.sigma * u.sigma, 1e-3);
    // pixels leaving the frame are invalid on that side
    let out0 = any(u0 < vec2<f32>(0.0)) || any(u0 > vec2<f32>(1.0));
    let out1 = any(u1 < vec2<f32>(0.0)) || any(u1 > vec2<f32>(1.0));
    var w0 = (1.0 - t) * exp(-e0 * e0 / s2) * select(1.0, 0.05, out0);
    var w1 = t * exp(-e1 * e1 / s2) * select(1.0, 0.05, out1);
    if (w0 + w1 < 1e-4) {
        w0 = 1.0 - t;
        w1 = t;
    }
    let c = (c0 * w0 + c1 * w1) / (w0 + w1);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(c.rgb, max(c.a, 0.0)));
}
