// Projects a straight-alpha layer into output space and modulates its alpha
// by a neighbour layer's output-space matte (Alpha, Luma or AI Subject).
// Output is straight RGBA ready for composite.wgsl with any blend mode.

struct TrackMatteParams {
    inv_row0: vec4<f32>,
    inv_row1: vec4<f32>,
    crop: vec4<f32>,
    src_rect: vec4<f32>,
    opacity: f32,
    channel: u32, // 0 = Alpha, 1 = Luma, 2 = Subject (R8 projected onto canvas)
    invert: u32,
    _pad: u32,
};

@group(0) @binding(0) var layer: texture_2d<f32>;
@group(0) @binding(1) var matte: texture_2d<f32>;
@group(0) @binding(2) var<uniform> u: TrackMatteParams;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;

const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

// Premultiplied bilinear tap over straight-alpha layer (mirrors composite.wgsl).
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
    let dims = textureDimensions(matte);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let p = vec3<f32>(f32(id.x) + 0.5, f32(id.y) + 0.5, 1.0);
    let uv = vec2<f32>(dot(u.inv_row0.xyz, p), dot(u.inv_row1.xyz, p));
    let inside = uv.x >= u.crop.x && uv.x <= 1.0 - u.crop.y && uv.y >= u.crop.z && uv.y <= 1.0 - u.crop.w;
    if (!inside) {
        textureStore(dst, vec2<i32>(id.xy), vec4<f32>(0.0));
        return;
    }
    let suv = u.src_rect.xy + uv * u.src_rect.zw;
    let l = sample_layer(suv);
    let mt = textureLoad(matte, vec2<i32>(id.xy), 0);
    var m = mt.a;
    if (u.channel == 1u) {
        m = clamp(dot(mt.rgb, LUMA), 0.0, 1.0);
    } else if (u.channel == 2u) {
        m = clamp(mt.r * mt.a, 0.0, 1.0);
    }
    if (u.invert != 0u) {
        m = 1.0 - m;
    }
    let a = clamp(l.a * u.opacity * m, 0.0, 1.0);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(l.rgb, a));
}
