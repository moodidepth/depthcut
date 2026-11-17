// Primary colour correction: exposure, white balance, 3-way wheels
// (lift / gamma / gain), contrast, saturation and a .cube 3D LUT sampled
// with hardware trilinear filtering. Optional before/after split wipe.

struct Grade {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    temperature: f32,
    tint: f32,
    lut_mix: f32,
    wipe: f32,        // < 0: off; else fraction of width shown ungraded (left side)
    lut_size: f32,
    lift: vec4<f32>,
    gamma: vec4<f32>,
    gain: vec4<f32>,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var lut: texture_3d<f32>;
@group(0) @binding(2) var lut_sampler: sampler;
@group(0) @binding(3) var<uniform> g: Grade;
@group(0) @binding(4) var dst: texture_storage_2d<rgba16float, write>;

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

fn grade(c_in: vec3<f32>) -> vec3<f32> {
    // exposure + white balance in linear light
    var lin = pow(max(c_in, vec3<f32>(0.0)), vec3<f32>(2.4));
    lin = lin * exp2(g.exposure);
    lin = lin * vec3<f32>(1.0 + 0.2 * g.temperature, 1.0 - 0.2 * g.tint, 1.0 - 0.2 * g.temperature);
    var c = pow(max(lin, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4));

    // three-way wheels
    let lift = (g.lift.rgb + vec3<f32>(g.lift.a)) * 0.25;
    let gam = (g.gamma.rgb + vec3<f32>(g.gamma.a)) * 0.5;
    let gain = (g.gain.rgb + vec3<f32>(g.gain.a)) * 0.5;
    c = c * (vec3<f32>(1.0) + gain);
    c = c + lift * (vec3<f32>(1.0) - c);
    c = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0) / max(vec3<f32>(1.0) + gam, vec3<f32>(0.05)));

    // contrast (pivot 0.5) and saturation
    c = (c - vec3<f32>(0.5)) * (1.0 + g.contrast) + vec3<f32>(0.5);
    let l = dot(c, LUMA);
    c = mix(vec3<f32>(l), c, g.saturation);

    // 3D LUT, sampling texel centres
    if (g.lut_mix > 0.0) {
        let n = g.lut_size;
        let coord = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)) * ((n - 1.0) / n) + vec3<f32>(0.5 / n);
        let lc = textureSampleLevel(lut, lut_sampler, coord, 0.0).rgb;
        c = mix(c, lc, g.lut_mix);
    }
    return c;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let px = textureLoad(src, vec2<i32>(id.xy), 0);
    var out = grade(px.rgb);
    if (g.wipe >= 0.0) {
        let x = f32(id.x) / f32(dims.x);
        if (x < g.wipe) {
            out = px.rgb;
        }
        // thin divider line
        if (abs(x - g.wipe) * f32(dims.x) < 1.5) {
            out = vec3<f32>(1.0, 0.27, 0.23);
        }
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, px.a));
}
