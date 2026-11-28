// Slow-motion in-between frame synthesis.
// mode 1: frame blending (cross-fade of the neighbouring source frames)
// mode 2: motion-compensated interpolation using the block motion field
//         (warp A forward by t·d, B backward by (1-t)·d, blend).

struct Blend {
    t: f32,
    mode: u32,
    block: f32,
    _pad: u32,
};

@group(0) @binding(0) var a_tex: texture_2d<f32>;
@group(0) @binding(1) var b_tex: texture_2d<f32>;
@group(0) @binding(2) var mv_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> u: Blend;
@group(0) @binding(5) var dst: texture_storage_2d<rgba16float, write>;

// Blending runs on premultiplied colour so RGBA media keeps its coverage and a
// transparent texel never darkens the in-between frame.
fn pm(s: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(s.rgb * s.a, s.a);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(a_tex);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let size = vec2<f32>(dims);
    let px = vec2<f32>(id.xy) + vec2<f32>(0.5);
    let uv = px / size;
    var out: vec4<f32>;
    if (u.mode == 3u) {
        // HLAE-style multi-tap weighted frame accumulation using dense flow vectors
        let fd = vec2<f32>(textureDimensions(mv_tex));
        let d = textureSampleLevel(mv_tex, samp, uv, 0.0).xy / fd * size;
        var acc = vec4<f32>(0.0);
        var wsum = 0.0;
        for (var i = 0; i < 8; i = i + 1) {
            let sub = clamp(u.t + (f32(i) / 7.0 - 0.5) * 0.35, 0.0, 1.0);
            let dist = (f32(i) / 7.0 - 0.5) * 2.0;
            let w = exp(-dist * dist * 1.8);
            let ua = (px - d * sub) / size;
            let ub = (px + d * (1.0 - sub)) / size;
            let ca = pm(textureSampleLevel(a_tex, samp, ua, 0.0));
            let cb = pm(textureSampleLevel(b_tex, samp, ub, 0.0));
            acc = acc + mix(ca, cb, sub) * w;
            wsum = wsum + w;
        }
        out = acc / max(wsum, 1e-4);
    } else if (u.mode == 2u) {
        // bilinear motion vector at this pixel (field is per-block)
        let mv_uv = px / (vec2<f32>(textureDimensions(mv_tex)) * u.block);
        let d = textureSampleLevel(mv_tex, samp, mv_uv, 0.0).xy;
        let ua = (px - d * u.t) / size;
        let ub = (px + d * (1.0 - u.t)) / size;
        let ca = pm(textureSampleLevel(a_tex, samp, ua, 0.0));
        let cb = pm(textureSampleLevel(b_tex, samp, ub, 0.0));
        out = mix(ca, cb, u.t);
    } else {
        out = mix(pm(textureSampleLevel(a_tex, samp, uv, 0.0)), pm(textureSampleLevel(b_tex, samp, uv, 0.0)), u.t);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out.rgb / max(out.a, 1e-6), out.a));
}
