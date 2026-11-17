// Multi-layer compositing: transform (position/scale/rotation/anchor via an
// inverse affine matrix), crop, Ken Burns source rect, opacity and blend
// modes (Normal, Multiply, Screen, Overlay, Add). Reads the accumulator,
// writes the next accumulator (ping-pong).

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

fn blend(b: vec3<f32>, l: vec3<f32>, mode: u32) -> vec3<f32> {
    switch mode {
        case 1u: { return b * l; }
        case 2u: { return vec3<f32>(1.0) - (vec3<f32>(1.0) - b) * (vec3<f32>(1.0) - l); }
        case 3u: {
            let lo = 2.0 * b * l;
            let hi = vec3<f32>(1.0) - 2.0 * (vec3<f32>(1.0) - b) * (vec3<f32>(1.0) - l);
            return select(hi, lo, b < vec3<f32>(0.5));
        }
        case 4u: { return b + l; }
        default: { return l; }
    }
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
    let inside = uv.x >= c.crop.x && uv.x <= 1.0 - c.crop.y && uv.y >= c.crop.z && uv.y <= 1.0 - c.crop.w;
    if (inside) {
        let suv = c.src_rect.xy + uv * c.src_rect.zw;
        let l = textureSampleLevel(layer, samp, suv, 0.0);
        let a = clamp(l.a * c.opacity, 0.0, 1.0);
        out = mix(base.rgb, blend(base.rgb, l.rgb, c.blend), a);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, 1.0));
}
