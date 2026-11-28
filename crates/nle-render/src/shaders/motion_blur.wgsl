// Vector motion blur (DaVinci Resolve style): smears each pixel along its
// optical-flow velocity for the fraction of the frame interval the virtual
// shutter is open. Pixels moving less than the threshold — static HUDs,
// titles, locked-off backgrounds — are left perfectly sharp.
//
// Quality 0 (Fast):   uniform line integral along the pixel's velocity.
// Quality 1 (Better): jittered samples weighted by velocity similarity, so
//                     moving subjects don't drag static surroundings with
//                     them and static pixels aren't smeared into motion.

struct Blur {
    shutter: f32,     // shutter angle / 360
    samples: u32,     // 8..32
    threshold: f32,   // minimum blur length in source pixels
    quality: u32,
    scale: f32,       // velocity multiplier (clip speed)
    amount: f32,      // RSMB blur amount (0.0..1.5, 0.5 = 1.0x nominal shutter)
    sensitivity: f32, // RSMB motion sensitivity (0..1; higher keeps static HUD sharper)
    _pad: f32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var flow: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> u: Blur;
@group(0) @binding(4) var dst: texture_storage_2d<rgba16float, write>;

fn hash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}

// velocity at uv in source pixels per frame
fn vel(uv: vec2<f32>, sd: vec2<f32>, fd: vec2<f32>) -> vec2<f32> {
    return textureSampleLevel(flow, samp, uv, 0.0).xy / fd * sd * u.scale;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let sd = vec2<f32>(textureDimensions(src));
    let fd = vec2<f32>(textureDimensions(flow));
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(dims);
    let center = textureSampleLevel(src, samp, uv, 0.0);
    let v = vel(uv, sd, fd);
    let gain = max(u.amount, 0.0) * 2.0;
    let blur = v * u.shutter * gain;
    let blen = length(blur);
    let min_thr = max(u.threshold, 0.25) * (1.0 + clamp(u.sensitivity, 0.0, 1.0) * 2.0);
    if (blen < min_thr || u.samples < 2u || gain <= 0.001) {
        textureStore(dst, vec2<i32>(id.xy), center);
        return;
    }
    let n = u.samples;
    let dir = blur / blen;
    let jitter = select(0.0, hash(vec2<f32>(id.xy)) - 0.5, u.quality == 1u);
    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    for (var i = 0u; i < n; i = i + 1u) {
        let s = (f32(i) + 0.5 + jitter) / f32(n) - 0.5;   // -0.5 .. 0.5
        let suv = uv + blur * s / sd;
        let c = textureSampleLevel(src, samp, suv, 0.0);
        var w = 1.0;
        if (u.quality == 1u) {
            let vs = vel(suv, sd, fd) * u.shutter * gain;
            let ls = length(vs);
            // samples that move like us contribute fully; static ones barely
            let similar = select(0.0, clamp(dot(vs / max(ls, 1e-4), dir), 0.0, 1.0), ls > 1e-4);
            let moving = smoothstep(min_thr * 0.5, max(min_thr, 0.5), ls);
            w = mix(0.10, 1.0, similar * moving);
        }
        // premultiplied, so transparent texels (RGBA media) add no colour
        acc = acc + vec4<f32>(c.rgb * c.a, c.a) * w;
        wsum = wsum + w;
    }
    // always keep some of the pixel itself (stable centre)
    acc = acc + vec4<f32>(center.rgb * center.a, center.a) * 0.5;
    wsum = wsum + 0.5;
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(acc.rgb / max(acc.a, 1e-6), acc.a / wsum));
}
