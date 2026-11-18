// Mixes a processed image back over the original through a shape mask
// (effect masks), or applies the before/after wipe after correction nodes.
// mask_common.wgsl is prepended.

struct MaskMix {
    mask: Mask,
    wipe: f32,   // >= 0: wipe mode (left of `wipe` shows the original, plus a divider)
    _p0: f32,
    _p1: f32,
    _p2: f32,
};

@group(0) @binding(0) var orig: texture_2d<f32>;
@group(0) @binding(1) var processed: texture_2d<f32>;
@group(0) @binding(2) var<uniform> u: MaskMix;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn mask_mix(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(orig);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let o = textureLoad(orig, vec2<i32>(id.xy), 0);
    let p = textureLoad(processed, vec2<i32>(id.xy), 0);
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(dims);
    if (u.wipe >= 0.0) {
        var out = p;
        if (uv.x < u.wipe) {
            out = o;
        }
        if (abs(uv.x - u.wipe) * f32(dims.x) < 1.5) {
            out = vec4<f32>(1.0, 0.27, 0.23, o.a);
        }
        textureStore(dst, vec2<i32>(id.xy), out);
        return;
    }
    let a = mask_alpha(u.mask, uv);
    textureStore(dst, vec2<i32>(id.xy), mix(o, p, a));
}
