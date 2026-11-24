// Primary colour correction with colour management (colorspace.wgsl is
// prepended):
//   input transform (camera log / HDR / sRGB → linear Rec.709, or an input LUT)
//   → exposure + white balance in linear light
//   → working space (Rec.709 gamma 2.4, or ACEScct over AP1)
//   → lift / gamma / gain wheels, contrast about 18 % grey, saturation
//   → back to Rec.709 display (highlight roll-off for scene / HDR sources)
//   → brightness (an even offset on the displayed values)
//   → creative .cube 3D LUT.
// Rec.709 input with the gamma working space is exactly the original
// (pre colour management) math. Optional before/after split wipe.

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
    input_space: u32,
    working: u32,
    in_lut_size: f32, // 0: no input LUT
    tonemap: u32,
    brightness: f32,
    headroom: f32,    // highest value kept by the tone map (1 = SDR)
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var lut: texture_3d<f32>;
@group(0) @binding(2) var lut_sampler: sampler;
@group(0) @binding(3) var<uniform> g: Grade;
@group(0) @binding(4) var dst: texture_storage_2d<rgba16float, write>;
@group(0) @binding(5) var in_lut: texture_3d<f32>;

fn sample_lut(t: texture_3d<f32>, c: vec3<f32>, n: f32) -> vec3<f32> {
    // sample texel centres
    let coord = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)) * ((n - 1.0) / n) + vec3<f32>(0.5 / n);
    return textureSampleLevel(t, lut_sampler, coord, 0.0).rgb;
}

fn grade(c_in: vec3<f32>) -> vec3<f32> {
    // input transform → linear Rec.709
    var lin: vec3<f32>;
    if (g.in_lut_size > 0.0) {
        lin = display_decode(sample_lut(in_lut, c_in, g.in_lut_size));
    } else {
        lin = to_linear709(c_in, g.input_space);
    }
    // exposure + white balance in linear light
    lin = white_balance(lin * exp2(g.exposure), g.temperature, g.tint);

    var c = to_working(lin, g.working);
    c = wheels(c, g.lift, g.gamma, g.gain);

    // contrast about 18 % grey and saturation
    let pivot = working_pivot(g.working);
    c = (c - vec3<f32>(pivot)) * (1.0 + g.contrast) + vec3<f32>(pivot);
    let l = dot(c, LUMA);
    c = mix(vec3<f32>(l), c, g.saturation);

    if (g.working != WS_GAMMA || g.tonemap != 0u) {
        var out_lin = from_working(c, g.working);
        if (g.tonemap != 0u) {
            let h = max(g.headroom, 1.0);
            out_lin = tonemap(out_lin / h) * h;
        }
        c = display_encode(out_lin);
    }
    c += vec3<f32>(g.brightness);

    if (g.lut_mix > 0.0) {
        c = mix(c, sample_lut(lut, c, g.lut_size), g.lut_mix);
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
