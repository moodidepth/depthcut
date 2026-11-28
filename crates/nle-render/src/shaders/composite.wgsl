// Multi-layer compositing: transform (position/scale/rotation/anchor via an
// inverse affine matrix), crop, Ken Burns source rect, opacity and blend
// modes (indices = `BlendMode::shader_index`: Normal, Multiply, Screen,
// Overlay, Add, Soft Light, Hard Light, Color Dodge, Color Burn, Darken,
// Lighten, Difference, Exclusion, Luminosity). Reads the accumulator, writes
// the next accumulator (ping-pong).
//
// Blending happens on the display-encoded working values (as in other
// editors). Separable modes follow the W3C compositing spec. Modes defined only on
// 0..1 (soft light, dodge, burn) clamp their inputs; the others pass HDR
// values through.

struct Comp {
    inv_row0: vec4<f32>,   // output pixel (x, y, 1) → layer uv.x
    inv_row1: vec4<f32>,   // → layer uv.y
    crop: vec4<f32>,       // left, right, top, bottom (uv)
    src_rect: vec4<f32>,   // Ken Burns: x, y, w, h (uv)
    opacity: f32,
    blend: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var bg: texture_2d<f32>;
@group(0) @binding(1) var layer: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> c: Comp;
@group(0) @binding(4) var dst: texture_storage_2d<rgba16float, write>;

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

fn soft_light(b: f32, l: f32) -> f32 {
    if (l <= 0.5) {
        return b - (1.0 - 2.0 * l) * b * (1.0 - b);
    }
    var d = sqrt(b);
    if (b <= 0.25) {
        d = ((16.0 * b - 12.0) * b + 4.0) * b;
    }
    return b + (2.0 * l - 1.0) * (d - b);
}

fn color_dodge(b: f32, l: f32) -> f32 {
    if (b <= 0.0) {
        return 0.0;
    }
    if (l >= 1.0) {
        return 1.0;
    }
    return min(1.0, b / (1.0 - l));
}

fn color_burn(b: f32, l: f32) -> f32 {
    if (b >= 1.0) {
        return 1.0;
    }
    if (l <= 0.0) {
        return 0.0;
    }
    return 1.0 - min(1.0, (1.0 - b) / l);
}

// W3C ClipColor / SetLum (Rec. 709 weights, matching the working space).
fn clip_color(c: vec3<f32>) -> vec3<f32> {
    let l = dot(c, LUMA);
    let n = min(c.r, min(c.g, c.b));
    let x = max(c.r, max(c.g, c.b));
    var o = c;
    if (n < 0.0) {
        o = vec3<f32>(l) + (o - vec3<f32>(l)) * l / max(l - n, 1e-6);
    }
    if (x > 1.0 && l <= 1.0) {
        o = vec3<f32>(l) + (o - vec3<f32>(l)) * (1.0 - l) / max(x - l, 1e-6);
    }
    return o;
}

fn set_lum(c: vec3<f32>, l: f32) -> vec3<f32> {
    return clip_color(c + vec3<f32>(l - dot(c, LUMA)));
}

fn blend(b: vec3<f32>, l: vec3<f32>, mode: u32) -> vec3<f32> {
    let bc = clamp(b, vec3<f32>(0.0), vec3<f32>(1.0));
    let lc = clamp(l, vec3<f32>(0.0), vec3<f32>(1.0));
    switch mode {
        case 1u: { return b * l; }
        case 2u: { return vec3<f32>(1.0) - (vec3<f32>(1.0) - b) * (vec3<f32>(1.0) - l); }
        case 3u: {
            let lo = 2.0 * b * l;
            let hi = vec3<f32>(1.0) - 2.0 * (vec3<f32>(1.0) - b) * (vec3<f32>(1.0) - l);
            return select(hi, lo, b < vec3<f32>(0.5));
        }
        case 4u: { return b + l; }
        case 5u: { return vec3<f32>(soft_light(bc.r, lc.r), soft_light(bc.g, lc.g), soft_light(bc.b, lc.b)); }
        case 6u: {
            let lo = 2.0 * b * l;
            let hi = vec3<f32>(1.0) - 2.0 * (vec3<f32>(1.0) - b) * (vec3<f32>(1.0) - l);
            return select(hi, lo, l < vec3<f32>(0.5));
        }
        case 7u: { return vec3<f32>(color_dodge(bc.r, lc.r), color_dodge(bc.g, lc.g), color_dodge(bc.b, lc.b)); }
        case 8u: { return vec3<f32>(color_burn(bc.r, lc.r), color_burn(bc.g, lc.g), color_burn(bc.b, lc.b)); }
        case 9u: { return min(b, l); }
        case 10u: { return max(b, l); }
        case 11u: { return abs(b - l); }
        case 12u: { return b + l - 2.0 * b * l; }
        case 13u: { return set_lum(b, dot(l, LUMA)); }
        default: { return l; }
    }
}

// Bilinear filtering on premultiplied values. Layers hold straight alpha, so
// the hardware filter would pull the colour of fully transparent texels (often
// black) into a layer's edges and leave a dark fringe on keyed or RGBA media.
// Weighting each tap by its alpha avoids that; opaque layers filter exactly as
// before. Returns straight colour and coverage. Same edge clamping as the
// ClampToEdge sampler.
fn sample_layer(uv: vec2<f32>) -> vec4<f32> {
    let dims = vec2<i32>(textureDimensions(layer));
    let hi = dims - vec2<i32>(1);
    let t = uv * vec2<f32>(dims) - vec2<f32>(0.5);
    let t0 = floor(t);
    let f = t - t0;
    let i0 = clamp(vec2<i32>(t0), vec2<i32>(0), hi);
    let i1 = clamp(vec2<i32>(t0) + vec2<i32>(1), vec2<i32>(0), hi);
    let s00 = textureLoad(layer, i0, 0);
    let s10 = textureLoad(layer, vec2<i32>(i1.x, i0.y), 0);
    let s01 = textureLoad(layer, vec2<i32>(i0.x, i1.y), 0);
    let s11 = textureLoad(layer, i1, 0);
    let w00 = (1.0 - f.x) * (1.0 - f.y) * s00.a;
    let w10 = f.x * (1.0 - f.y) * s10.a;
    let w01 = (1.0 - f.x) * f.y * s01.a;
    let w11 = f.x * f.y * s11.a;
    let a = w00 + w10 + w01 + w11;
    let rgb = s00.rgb * w00 + s10.rgb * w10 + s01.rgb * w01 + s11.rgb * w11;
    return vec4<f32>(rgb / max(a, 1e-6), a);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(bg);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let base = textureLoad(bg, vec2<i32>(id.xy), 0);
    let p = vec3<f32>(f32(id.x) + 0.5, f32(id.y) + 0.5, 1.0);
    let uv = vec2<f32>(dot(c.inv_row0.xyz, p), dot(c.inv_row1.xyz, p));
    var out = base.rgb;
    // premultiplied "over": an opaque base stays opaque, a transparent one
    // (alpha exports) picks up the layer's coverage
    var alpha = base.a;
    let inside = uv.x >= c.crop.x && uv.x <= 1.0 - c.crop.y && uv.y >= c.crop.z && uv.y <= 1.0 - c.crop.w;
    if (inside) {
        let suv = c.src_rect.xy + uv * c.src_rect.zw;
        let l = sample_layer(suv);
        let a = clamp(l.a * c.opacity, 0.0, 1.0);
        // W3C Porter-Duff blend-mode compositing over a premultiplied base:
        // matches mix(base.rgb, blend(...), a) when base.a == 1 and stays
        // un-darkened on transparent canvases (ProRes 4444 / PNG alpha).
        let base_straight = select(vec3<f32>(0.0), base.rgb / base.a, base.a > 1e-6);
        let blended = blend(base_straight, l.rgb, c.blend);
        out = base.rgb * (1.0 - a) + l.rgb * (a * (1.0 - base.a)) + blended * (a * base.a);
        alpha = a + base.a * (1.0 - a);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, alpha));
}
