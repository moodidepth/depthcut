// Export packers for the high-bit-depth paths (the 8-bit one is nv12.wgsl).
//
// * `p010`:   working RGBA → P010 (limited range, 10 bits in the top of each
//             little-endian u16). One invocation per 2×2 block, so every
//             write is a whole u32. Width and height must be even.
// * `rgba64`: working RGBA → RGBA 16-bit little-endian (FFmpeg `rgba64le`),
//             the source for ProRes / DNxHR / PNG / TIFF / EXR.
//
// Flags: bit 0 = linearise (BT.1886 gamma 2.4, for EXR; rgba64 only),
//        bit 1 = keep alpha (and un-premultiply), otherwise alpha = 1
//                (rgba64 only),
//        bit 2 = HDR10: BT.2020 primaries, PQ (SMPTE ST 2084),
//        bit 3 = HLG (ARIB STD-B67), BT.2020 primaries.
// Without bits 2/3 the output is BT.709 SDR clamped to 0..1. With them, the
// working values (display-referred, 1.0 = SDR reference white, highlights
// above 1 kept) are placed at 203 nits (PQ) / 75 % signal (HLG), and P010
// uses the BT.2020 non-constant-luminance matrix.

struct Pack {
    width: u32,
    height: u32,
    flags: u32,
    _pad: u32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> s: Pack;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;

const LUMA_709: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);
const LUMA_2020: vec3<f32> = vec3<f32>(0.2627, 0.6780, 0.0593);
const HDR_BITS: u32 = 12u;

fn pq_encode(l: vec3<f32>) -> vec3<f32> {
    // l: linear light, 1.0 = 10 000 nits
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let y = pow(clamp(l, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(m1));
    return pow((vec3<f32>(c1) + c2 * y) / (vec3<f32>(1.0) + c3 * y), vec3<f32>(m2));
}

fn hlg1(e: f32) -> f32 {
    let a = 0.17883277;
    let b = 0.28466892;
    let c = 0.55991073;
    let x = clamp(e, 0.0, 1.0);
    if (x <= 1.0 / 12.0) {
        return sqrt(3.0 * x);
    }
    return a * log(12.0 * x - b) + c;
}

// Working (display-encoded BT.709) → the output signal.
fn encode(c: vec3<f32>) -> vec3<f32> {
    if ((s.flags & HDR_BITS) == 0u) {
        return clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    let lin = pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.4));
    let l = vec3<f32>(
        dot(lin, vec3<f32>(0.6274, 0.3293, 0.0433)),
        dot(lin, vec3<f32>(0.0691, 0.9195, 0.0114)),
        dot(lin, vec3<f32>(0.0164, 0.0880, 0.8956)),
    );
    if ((s.flags & 4u) != 0u) {
        return pq_encode(l * (203.0 / 10000.0));
    }
    let e = l * 0.26496256;
    return vec3<f32>(hlg1(e.r), hlg1(e.g), hlg1(e.b));
}

fn rgb(x: u32, y: u32) -> vec3<f32> {
    return encode(textureLoad(src, vec2<i32>(i32(x), i32(y)), 0).rgb);
}

fn luma() -> vec3<f32> {
    if ((s.flags & HDR_BITS) != 0u) {
        return LUMA_2020;
    }
    return LUMA_709;
}

fn y10(c: vec3<f32>) -> u32 {
    return u32(round(64.0 + 876.0 * dot(c, luma())));
}

fn lo_hi(a: u32, b: u32) -> u32 {
    return ((a << 6u) & 0xffffu) | (((b << 6u) & 0xffffu) << 16u);
}

@compute @workgroup_size(8, 8, 1)
fn p010(@builtin(global_invocation_id) id: vec3<u32>) {
    let x0 = id.x * 2u;
    let y0 = id.y * 2u;
    if (x0 >= s.width || y0 >= s.height) {
        return;
    }
    let wpr = s.width / 2u;
    let c00 = rgb(x0, y0);
    let c10 = rgb(x0 + 1u, y0);
    let c01 = rgb(x0, y0 + 1u);
    let c11 = rgb(x0 + 1u, y0 + 1u);
    dst[y0 * wpr + id.x] = lo_hi(y10(c00), y10(c10));
    dst[(y0 + 1u) * wpr + id.x] = lo_hi(y10(c01), y10(c11));
    let m = (c00 + c10 + c01 + c11) * 0.25;
    let k = luma();
    let ym = dot(m, k);
    // Cb = (B - Y) / (2 (1 - Kb)), Cr = (R - Y) / (2 (1 - Kr))
    let cb = u32(round(512.0 + 896.0 * (m.b - ym) / (2.0 * (1.0 - k.b))));
    let cr = u32(round(512.0 + 896.0 * (m.r - ym) / (2.0 * (1.0 - k.r))));
    let uv_base = s.width * s.height / 2u;
    dst[uv_base + (y0 / 2u) * wpr + id.x] = lo_hi(cb, cr);
}

@compute @workgroup_size(8, 8, 1)
fn rgba64(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= s.width || id.y >= s.height) {
        return;
    }
    var c = textureLoad(src, vec2<i32>(id.xy), 0);
    c.a = clamp(c.a, 0.0, 1.0);
    if ((s.flags & 2u) != 0u) {
        // the compositor works premultiplied
        if (c.a > 1e-5) {
            c = vec4<f32>(c.rgb / c.a, c.a);
        } else {
            c = vec4<f32>(0.0);
        }
    } else {
        c.a = 1.0;
    }
    c = vec4<f32>(encode(c.rgb), c.a);
    if ((s.flags & 1u) != 0u && (s.flags & HDR_BITS) == 0u) {
        c = vec4<f32>(pow(c.rgb, vec3<f32>(2.4)), c.a);
    }
    let q = vec4<u32>(round(c * 65535.0));
    let i = (id.y * s.width + id.x) * 2u;
    dst[i] = q.r | (q.g << 16u);
    dst[i + 1u] = q.b | (q.a << 16u);
}
