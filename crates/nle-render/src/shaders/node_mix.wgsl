// Node compositor parallel mixer: one input's change from the base added to
// the running sum. out = acc + (input - base), premultiplied RGBA.

@group(0) @binding(0) var acc: texture_2d<f32>;
@group(0) @binding(1) var input: texture_2d<f32>;
@group(0) @binding(2) var base: texture_2d<f32>;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let p = vec2<i32>(id.xy);
    let a = textureLoad(acc, p, 0);
    let i = textureLoad(input, p, 0);
    let b = textureLoad(base, p, 0);
    let o = a + (i - b);
    textureStore(dst, p, vec4<f32>(o.rgb, clamp(o.a, 0.0, 1.0)));
}
