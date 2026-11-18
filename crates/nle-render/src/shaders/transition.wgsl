// GPU transitions between two full-frame images.
// 0 Cross Dissolve · 1 Dip to Black · 2 Dip to White · 3 Whip Pan
// 4 Slide · 5 Push · 6 Luma Wipe · 7 Zoom Punch

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
            var acc = vec3<f32>(0.0);
            for (var i = 0; i < 24; i = i + 1) {
                let o = (f32(i) / 23.0 - 0.5) * blur;
                acc = acc + strip(uv + t.dir * o, e);
            }
            out = acc / 24.0;
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
            var acc = vec3<f32>(0.0);
            for (var i = 0; i < 12; i = i + 1) {
                let s = 1.0 - blur * (f32(i) / 11.0);
                let quv = clamp(c + ((uv - c) / zoom) * s, vec2<f32>(0.0), vec2<f32>(1.0));
                let ruv = clamp(quv + (uv - c) * split, vec2<f32>(0.0), vec2<f32>(1.0));
                let buv = clamp(quv - (uv - c) * split, vec2<f32>(0.0), vec2<f32>(1.0));
                if (is_b) {
                    acc = acc + vec3<f32>(sb(ruv).r, sb(quv).g, sb(buv).b);
                } else {
                    acc = acc + vec3<f32>(sa(ruv).r, sa(quv).g, sa(buv).b);
                }
            }
            out = acc / 12.0;
        }
        default: {
            out = mix(sa(uv), sb(uv), p);
        }
    }
    // coverage cross-fades (exact for dissolves; opaque renders stay at 1)
    let alpha = mix(textureSampleLevel(a_tex, samp, uv, 0.0).a, textureSampleLevel(b_tex, samp, uv, 0.0).a, p);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, alpha));
}
