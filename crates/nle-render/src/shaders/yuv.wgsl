// NV12 / P010 / RGBA8 → working-space RGBA (rgba16float), entirely on GPU.
// The decoded planes are uploaded verbatim into a storage buffer; this
// shader unpacks bytes/words, applies the YCbCr matrix and range expansion.

struct Params {
    width: u32,
    height: u32,
    uv_offset: u32,   // byte offset of the interleaved CbCr plane
    format: u32,      // 0 = NV12, 1 = P010, 2 = RGBA8
    matrix: u32,      // 0 = BT.709, 1 = BT.601, 2 = BT.2020
    full_range: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<uniform> p: Params;
@group(0) @binding(2) var dst: texture_storage_2d<rgba16float, write>;

fn byte_at(i: u32) -> u32 {
    return (src[i >> 2u] >> ((i & 3u) * 8u)) & 0xffu;
}

fn word_at(byte_index: u32) -> u32 {
    return (src[byte_index >> 2u] >> ((byte_index & 2u) * 8u)) & 0xffffu;
}

// Normalised sample `i` (in samples, not bytes) of the YUV planes.
fn sample(i: u32) -> f32 {
    if (p.format == 1u) {
        return f32(word_at(i * 2u)) / 65535.0;
    }
    return f32(byte_at(i)) / 255.0;
}

fn ycbcr_to_rgb(y_in: f32, u_in: f32, v_in: f32) -> vec3<f32> {
    var kr = 0.2126;
    var kb = 0.0722;
    if (p.matrix == 1u) {
        kr = 0.299;
        kb = 0.114;
    } else if (p.matrix == 2u) {
        kr = 0.2627;
        kb = 0.0593;
    }
    let kg = 1.0 - kr - kb;
    var y = y_in;
    var u = u_in - 0.5;
    var v = v_in - 0.5;
    if (p.full_range == 0u) {
        y = (y_in - 16.0 / 255.0) * (255.0 / 219.0);
        u = (u_in - 128.0 / 255.0) * (255.0 / 224.0);
        v = (v_in - 128.0 / 255.0) * (255.0 / 224.0);
    }
    let r = y + 2.0 * (1.0 - kr) * v;
    let b = y + 2.0 * (1.0 - kb) * u;
    let g = (y - kr * r - kb * b) / kg;
    return vec3<f32>(r, g, b);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= p.width || id.y >= p.height) {
        return;
    }
    var rgb: vec3<f32>;
    if (p.format == 2u) {
        let i = (id.y * p.width + id.x) * 4u;
        rgb = vec3<f32>(f32(byte_at(i)), f32(byte_at(i + 1u)), f32(byte_at(i + 2u))) / 255.0;
    } else {
        let bps = select(1u, 2u, p.format == 1u);
        let y = sample(id.y * p.width + id.x);
        let uv = p.uv_offset / bps + (id.y / 2u) * p.width + (id.x & 0xfffffffeu);
        rgb = ycbcr_to_rgb(y, sample(uv), sample(uv + 1u));
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
