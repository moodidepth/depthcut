// GPU transitions between two full-frame images.
// 0 Cross Dissolve · 1 Dip to Black · 2 Dip to White · 3 Whip Pan
// 4 Slide · 5 Push · 6 Luma Wipe · 7 Zoom Punch · 8 Wipe · 9 Iris
// 10 Clock Wipe · 11 Barn Door · 12 Cross Zoom · 13 Blur Dissolve

struct Tr {
    dir: vec2<f32>,
    progress: f32,
    softness: f32,
    kind: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var a_tex: texture_2d<f32>;
@group(0) @binding(1) var b_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> t: Tr;
@group(0) @binding(4) var dst: texture_storage_2d<rgba16float, write>;

fn in_unit(uv: vec2<f32>) -> bool {
    return all(uv >= vec2<f32>(0.0)) && all(uv <= vec2<f32>(1.0));
}

fn sa(uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(a_tex, samp, uv, 0.0).rgb;
}

fn sb(uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(b_tex, samp, uv, 0.0).rgb;
}

// A and B laid side by side along `dir`, viewed through a window that has
// moved by `shift` (0 → all A, 1 → all B).
fn strip(uv: vec2<f32>, shift: f32) -> vec3<f32> {
    let q = uv - t.dir * shift;
    if (in_unit(q)) {
        return sa(q);
    }
    return sb(q + t.dir);
}

// Reveal mask: 1 where B shows. `e` grows from 0 (first revealed) to 1;
// the soft edge (width s) sweeps past both ends so p = 0 / 1 are exact.
fn reveal(e: f32, p: f32, s: f32) -> f32 {
    let edge = p * (1.0 + s);
    return 1.0 - smoothstep(edge - s, edge, e);
}

// Interleaved gradient noise in [0, 1): offsets each pixel's blur taps so a
// long blur reads as fine grain instead of a comb of discrete copies.
fn ign(px: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(px, vec2<f32>(0.06711056, 0.00583715))));
}

fn blur_mix(uv: vec2<f32>, p: f32, r: f32) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    for (var j = -2; j <= 2; j = j + 1) {
        for (var i = -2; i <= 2; i = i + 1) {
            let o = vec2<f32>(f32(i), f32(j)) * r * 0.5;
            let q = clamp(uv + o, vec2<f32>(0.0), vec2<f32>(1.0));
            acc = acc + mix(sa(q), sb(q), p);
        }
    }
    return acc / 25.0;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(a_tex);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(dims);
    let p = clamp(t.progress, 0.0, 1.0);
    var out: vec3<f32>;
    switch t.kind {
        case 1u, 2u: {
            let mid = select(vec3<f32>(0.0), vec3<f32>(1.0), t.kind == 2u);
            if (p < 0.5) {
                out = mix(sa(uv), mid, p * 2.0);
            } else {
                out = mix(mid, sb(uv), (p - 0.5) * 2.0);
            }
        }
        case 3u: {
            // whip pan: push with a strong directional motion blur peaking mid-way
            let e = p * p * (3.0 - 2.0 * p);
            let blur = sin(3.14159265 * p) * 0.35;
            let jit = ign(vec2<f32>(id.xy));
            var acc = vec3<f32>(0.0);
            for (var i = 0; i < 64; i = i + 1) {
                let o = ((f32(i) + jit) / 64.0 - 0.5) * blur;
                acc = acc + strip(uv + t.dir * o, e);
            }
            out = acc / 64.0;
        }
        case 4u: {
            // slide: B moves in over a static A
            let q = uv + t.dir * (1.0 - p);
            if (in_unit(q)) {
                out = sb(q);
            } else {
                out = sa(uv);
            }
        }
        case 5u: {
            out = strip(uv, p);
        }
        case 6u: {
            // luma wipe: dark areas of A give way to B first
            let l = dot(sa(uv), vec3<f32>(0.2126, 0.7152, 0.0722));
            let s = max(t.softness, 0.001);
            let edge = p * (1.0 + s) - s;
            let m = smoothstep(edge, edge + s, l);
            out = mix(sb(uv), sa(uv), m);
        }
        case 7u: {
            // zoom punch: exponential 1.0 -> 2.4x push into A with radial blur & RGB split, snap back on B
            let c = vec2<f32>(0.5);
            let is_b = p >= 0.5;
            let u = select(p / 0.5, (p - 0.5) / 0.5, is_b);
            let zoom = select(1.0 + 1.4 * u * u, 1.0 + 0.9 * (1.0 - u) * (1.0 - u), is_b);
            let blur = sin(3.14159265 * p) * 0.25;
            let split = sin(3.14159265 * p) * 0.012;
            let jit = ign(vec2<f32>(id.xy));
            var acc = vec3<f32>(0.0);
            for (var i = 0; i < 24; i = i + 1) {
                let s = 1.0 - blur * ((f32(i) + jit) / 24.0);
                let quv = clamp(c + ((uv - c) / zoom) * s, vec2<f32>(0.0), vec2<f32>(1.0));
                let ruv = clamp(quv + (uv - c) * split, vec2<f32>(0.0), vec2<f32>(1.0));
                let buv = clamp(quv - (uv - c) * split, vec2<f32>(0.0), vec2<f32>(1.0));
                if (is_b) {
                    acc = acc + vec3<f32>(sb(ruv).r, sb(quv).g, sb(buv).b);
                } else {
                    acc = acc + vec3<f32>(sa(ruv).r, sa(quv).g, sa(buv).b);
                }
            }
            out = acc / 24.0;
        }
        case 8u: {
            // B enters from the side the direction points away from
            let e = 1.0 - (dot(uv - vec2<f32>(0.5), -t.dir) + 0.5);
            out = mix(sa(uv), sb(uv), reveal(e, p, max(t.softness, 0.001)));
        }
        case 9u: {
            let asp = f32(dims.x) / f32(dims.y);
            let rmax = length(vec2<f32>(asp, 1.0) * 0.5);
            let e = length((uv - vec2<f32>(0.5)) * vec2<f32>(asp, 1.0)) / rmax;
            out = mix(sa(uv), sb(uv), reveal(e, p, max(t.softness, 0.001)));
        }
        case 10u: {
            let d = uv - vec2<f32>(0.5);
            let e = fract(atan2(d.x, -d.y) / 6.2831853 + 1.0);
            out = mix(sa(uv), sb(uv), reveal(e, p, max(t.softness * 0.25, 0.001)));
        }
        case 11u: {
            var e = abs(uv.x - 0.5) * 2.0;
            if (abs(t.dir.y) > 0.5) {
                e = abs(uv.y - 0.5) * 2.0;
            }
            out = mix(sa(uv), sb(uv), reveal(e, p, max(t.softness, 0.001)));
        }
        case 12u: {
            let c = vec2<f32>(0.5);
            let k = sin(3.14159265 * p);
            let zoom = 1.0 + 1.2 * k;
            let w = smoothstep(0.4, 0.6, p);
            var acc = vec3<f32>(0.0);
            for (var i = 0; i < 16; i = i + 1) {
                let s = 1.0 - 0.35 * k * (f32(i) / 15.0);
                let q = clamp(c + (uv - c) / zoom * s, vec2<f32>(0.0), vec2<f32>(1.0));
                acc = acc + mix(sa(q), sb(q), w);
            }
            out = acc / 16.0;
        }
        case 13u: {
            out = blur_mix(uv, smoothstep(0.0, 1.0, p), sin(3.14159265 * p) * 0.02);
        }
        default: {
            out = mix(sa(uv), sb(uv), p);
        }
    }
    // coverage cross-fades (exact for dissolves; opaque renders stay at 1)
    let alpha = mix(textureSampleLevel(a_tex, samp, uv, 0.0).a, textureSampleLevel(b_tex, samp, uv, 0.0).a, p);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, alpha));
}
