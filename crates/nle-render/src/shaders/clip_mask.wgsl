// Clip-level shape masks applied to a layer's straight alpha.
// mask_common.wgsl is prepended. Mirrors nle_core::mask::combine.

struct ClipMaskEntry {
    mask: Mask,
    mode: u32,    // 0 = Add, 1 = Subtract, 2 = Intersect
    opacity: f32,
    _p0: f32,
    _p1: f32,
};

struct ClipMasks {
    count: u32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
    entries: array<ClipMaskEntry, 8>,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> u: ClipMasks;
@group(0) @binding(2) var dst: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let c = textureLoad(src, vec2<i32>(id.xy), 0);
    let n = min(u.count, 8u);
    if (n == 0u) {
        textureStore(dst, vec2<i32>(id.xy), c);
        return;
    }
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(dims);
    var acc = select(1.0, 0.0, u.entries[0].mode == 0u);
    for (var i = 0u; i < n; i = i + 1u) {
        let e = u.entries[i];
        let m = mask_alpha(e.mask, uv) * e.opacity;
        if (e.mode == 0u) {
            acc = acc + m - acc * m;
        } else if (e.mode == 1u) {
            acc = acc * (1.0 - m);
        } else {
            acc = acc * m;
        }
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(c.rgb, c.a * clamp(acc, 0.0, 1.0)));
}
