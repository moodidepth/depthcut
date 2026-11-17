// Fill a working texture with a solid colour (accumulator clear).

struct Fill {
    color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> f: Fill;
@group(0) @binding(1) var dst: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    textureStore(dst, vec2<i32>(id.xy), f.color);
}
