// Export packers for the high-bit-depth paths (the 8-bit one is nv12.wgsl).
//
// * `p010`:   working RGBA → P010 (BT.709 limited range, 10 bits in the top
//             of each little-endian u16). One invocation per 2×2 block, so
//             every write is a whole u32. Width and height must be even.
// * `rgba64`: working RGBA → RGBA 16-bit little-endian (FFmpeg `rgba64le`),
//             the source for ProRes / DNxHR / PNG / TIFF / EXR. Flags:
//             bit 0 = linearise (BT.1886 gamma 2.4, for EXR),
//             bit 1 = keep alpha (and un-premultiply), otherwise alpha = 1.

struct Pack {
    width: u32,
    height: u32,
    flags: u32,
    _pad: u32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> s: Pack;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;

const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

fn rgb(x: u32, y: u32) -> vec3<f32> {
    return clamp(textureLoad(src, vec2<i32>(i32(x), i32(y)), 0).rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn y10(c: vec3<f32>) -> u32 {
    return u32(round(64.0 + 876.0 * dot(c, LUMA)));
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
    let ym = dot(m, LUMA);
    let cb = u32(round(512.0 + 896.0 * (m.b - ym) / 1.8556));
    let cr = u32(round(512.0 + 896.0 * (m.r - ym) / 1.5748));
    let uv_base = s.width * s.height / 2u;
    dst[uv_base + (y0 / 2u) * wpr + id.x] = lo_hi(cb, cr);
}

@compute @workgroup_size(8, 8, 1)
fn rgba64(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= s.width || id.y >= s.height) {
        return;
    }
    var c = clamp(textureLoad(src, vec2<i32>(id.xy), 0), vec4<f32>(0.0), vec4<f32>(1.0));
    if ((s.flags & 2u) != 0u) {
        // the compositor works premultiplied
        if (c.a > 1e-5) {
            c = vec4<f32>(clamp(c.rgb / c.a, vec3<f32>(0.0), vec3<f32>(1.0)), c.a);
        } else {
            c = vec4<f32>(0.0);
        }
    } else {
        c.a = 1.0;
    }
    if ((s.flags & 1u) != 0u) {
        c = vec4<f32>(pow(c.rgb, vec3<f32>(2.4)), c.a);
    }
    let q = vec4<u32>(round(c * 65535.0));
    let i = (id.y * s.width + id.x) * 2u;
    dst[i] = q.r | (q.g << 16u);
    dst[i + 1u] = q.b | (q.a << 16u);
}
