// Output stage: working texture → 8-bit display texture (the egui viewer
// samples it through an sRGB view). NV12 export packing lives in nv12.wgsl.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var display: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 1)
fn to_display(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let c = textureLoad(src, vec2<i32>(id.xy), 0);
    textureStore(display, vec2<i32>(id.xy), vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
