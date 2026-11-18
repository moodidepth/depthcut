// One stacked colour correction node (colorspace.wgsl and mask_common.wgsl
// are prepended). Input and output are Rec.709 display values.
//
//   key   = HSL qualifier (of the node input) × shape mask × mix
//   graded: exposure / WB (linear) → working space: offset, lift / gamma /
//           gain, log wheels, contrast, saturation → display → tone curves
//           (master then R / G / B) → hue-vs-hue / sat / lum
//   out   = mix(input, graded, key)
// `highlight` previews the key: 1 = matched pixels in colour over grey,
// 2 = black / white matte.

struct Node {
    lift: vec4<f32>,
    gamma: vec4<f32>,
    gain: vec4<f32>,
    offset: vec4<f32>,
    log_sh: vec4<f32>,
    log_mid: vec4<f32>,
    log_hi: vec4<f32>,
    exposure: f32,
    temperature: f32,
    tint: f32,
    contrast: f32,
    pivot: f32,       // offset from the working space's 18 % grey
    saturation: f32,
    low_range: f32,
    high_range: f32,
    mix_amt: f32,
    working: u32,
    curves: u32,      // bit 0: tone curves, bit 1: hue curves
    highlight: u32,
    q_flags: u32,     // bit 0 hue, 1 sat, 2 lum, 3 invert, 4 qualifier on
    q_denoise: f32,   // matte blur radius in pixels (0 = off)
    _p0: f32,
    _p1: f32,
    q_hue: vec4<f32>, // centre, width, soft
    q_sat: vec4<f32>, // low, high, soft
    q_lum: vec4<f32>, // low, high, soft
    mask: Mask,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> n: Node;
@group(0) @binding(2) var<storage, read> curves: array<f32>; // 7 × 256
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;

const SAMPLES: u32 = 256u;

// Linear lookup in baked table `t` at x (0..1); values beyond 0..1 keep
// their distance from the end point so HDR headroom survives.
fn tone(t: u32, x: f32) -> f32 {
    let xc = clamp(x, 0.0, 1.0);
    let f = xc * f32(SAMPLES - 1u);
    let i = min(u32(f), SAMPLES - 2u);
    let a = curves[t * SAMPLES + i];
    let b = curves[t * SAMPLES + i + 1u];
    return mix(a, b, f - f32(i)) + (x - xc);
}

// Periodic lookup over hue (0..1).
fn hue_tab(t: u32, h: f32) -> f32 {
    let f = fract(h) * f32(SAMPLES);
    let i = u32(f) % SAMPLES;
    let j = (i + 1u) % SAMPLES;
    return mix(curves[t * SAMPLES + i], curves[t * SAMPLES + j], fract(f));
}

// Hue 0..1 (red 0, green 1/3, blue 2/3), saturation (chroma / max) and luma.
fn hsl(c: vec3<f32>) -> vec3<f32> {
    let mx = max(c.r, max(c.g, c.b));
    let mn = min(c.r, min(c.g, c.b));
    let d = mx - mn;
    var h = 0.0;
    if (d > 1e-6) {
        if (mx == c.r) {
            h = (c.g - c.b) / d;
        } else if (mx == c.g) {
            h = (c.b - c.r) / d + 2.0;
        } else {
            h = (c.r - c.g) / d + 4.0;
        }
        h = fract(h / 6.0 + 1.0);
    }
    var s = 0.0;
    if (mx > 1e-6) {
        s = d / mx;
    }
    return vec3<f32>(h, s, dot(c, LUMA));
}

fn band(v: f32, lo: f32, hi: f32, soft: f32) -> f32 {
    let s = max(soft, 1e-4);
    var k = smoothstep(lo - s, lo, v);
    if (hi < 1.0) {
        k = k * (1.0 - smoothstep(hi, hi + s, v));
    }
    return k;
}

fn qualify(c: vec3<f32>) -> f32 {
    let q = hsl(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)));
    var k = 1.0;
    if ((n.q_flags & 1u) != 0u) {
        let d = abs(fract(q.x - n.q_hue.x + 0.5) - 0.5);
        k = k * (1.0 - smoothstep(n.q_hue.y * 0.5, n.q_hue.y * 0.5 + max(n.q_hue.z, 1e-4), d));
    }
    if ((n.q_flags & 2u) != 0u) {
        k = k * band(q.y, n.q_sat.x, n.q_sat.y, n.q_sat.z);
    }
    if ((n.q_flags & 4u) != 0u) {
        k = k * band(q.z, n.q_lum.x, n.q_lum.y, n.q_lum.z);
    }
    return k;
}

fn key_at(p: vec2<i32>, dims: vec2<u32>) -> f32 {
    if ((n.q_flags & 16u) == 0u) {
        return 1.0;
    }
    var k: f32;
    if (n.q_denoise > 0.0) {
        // 3 × 3 taps spread over the radius
        let r = n.q_denoise;
        var sum = 0.0;
        for (var dy = -1; dy <= 1; dy = dy + 1) {
            for (var dx = -1; dx <= 1; dx = dx + 1) {
                let o = vec2<i32>(i32(round(f32(dx) * r)), i32(round(f32(dy) * r)));
                let q = clamp(p + o, vec2<i32>(0), vec2<i32>(dims) - vec2<i32>(1));
                sum = sum + qualify(textureLoad(src, q, 0).rgb);
            }
        }
        k = sum / 9.0;
    } else {
        k = qualify(textureLoad(src, p, 0).rgb);
    }
    if ((n.q_flags & 8u) != 0u) {
        k = 1.0 - k;
    }
    return k;
}

fn rotate_hue(c: vec3<f32>, turns: f32) -> vec3<f32> {
    // Rodrigues rotation about the neutral axis
    let k = vec3<f32>(0.57735027);
    let a = turns * 6.2831853;
    let ca = cos(a);
    let sa = sin(a);
    return c * ca + cross(k, c) * sa + k * dot(k, c) * (1.0 - ca);
}

fn correct(c_in: vec3<f32>) -> vec3<f32> {
    var lin = white_balance(display_decode(c_in) * exp2(n.exposure), n.temperature, n.tint);
    var c = to_working(lin, n.working);
    c = c + (n.offset.rgb + vec3<f32>(n.offset.a)) * 0.1;
    c = wheels(c, n.lift, n.gamma, n.gain);

    // log wheels: offsets weighted by tonal range of the working value
    let y = dot(c, LUMA);
    let ws = 1.0 - smoothstep(n.low_range - 0.1, n.low_range + 0.1, y);
    let wh = smoothstep(n.high_range - 0.1, n.high_range + 0.1, y);
    let wm = max(1.0 - ws - wh, 0.0);
    c = c + ((n.log_sh.rgb + vec3<f32>(n.log_sh.a)) * ws + (n.log_mid.rgb + vec3<f32>(n.log_mid.a)) * wm + (n.log_hi.rgb + vec3<f32>(n.log_hi.a)) * wh) * 0.2;

    let pivot = working_pivot(n.working) + n.pivot;
    c = (c - vec3<f32>(pivot)) * (1.0 + n.contrast) + vec3<f32>(pivot);
    c = mix(vec3<f32>(dot(c, LUMA)), c, n.saturation);
    c = display_encode(from_working(c, n.working));

    if ((n.curves & 1u) != 0u) {
        c = vec3<f32>(tone(1u, tone(0u, c.r)), tone(2u, tone(0u, c.g)), tone(3u, tone(0u, c.b)));
    }
    if ((n.curves & 2u) != 0u) {
        let q = hsl(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)));
        let l0 = dot(c, LUMA);
        c = rotate_hue(c, hue_tab(4u, q.x));
        let l1 = dot(c, LUMA);
        c = vec3<f32>(l1) + (c - vec3<f32>(l1)) * max(1.0 + hue_tab(5u, q.x), 0.0);
        // keep the luma the rotation moved, then apply hue-vs-lum on saturated colours
        c = c + vec3<f32>(l0 - l1 + hue_tab(6u, q.x) * 0.5 * q.y);
    }
    return c;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let p = vec2<i32>(id.xy);
    let px = textureLoad(src, p, 0);
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(dims);
    let key = key_at(p, dims) * mask_alpha(n.mask, uv);
    var out: vec3<f32>;
    if (n.highlight == 1u) {
        let grey = vec3<f32>(dot(px.rgb, LUMA) * 0.35 + 0.3);
        out = mix(grey, px.rgb, key);
    } else if (n.highlight == 2u) {
        out = vec3<f32>(key);
    } else {
        out = mix(px.rgb, correct(px.rgb), key * n.mix_amt);
    }
    textureStore(dst, p, vec4<f32>(out, px.a));
}
