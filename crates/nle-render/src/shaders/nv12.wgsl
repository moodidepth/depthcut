// Working RGBA → NV12 (BT.709, limited range) packed into a u32 storage
// buffer. Each invocation converts a 4×2 pixel block so every write is a
// whole 32-bit word (no atomics needed). Width must be a multiple of 4.

struct Size {
    width: u32,
    height: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> s: Size;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;

fn rgb(x: u32, y: u32) -> vec3<f32> {
    return clamp(textureLoad(src, vec2<i32>(i32(x), i32(y)), 0).rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn y_of(c: vec3<f32>) -> u32 {
    return u32(round(16.0 + 219.0 * dot(c, vec3<f32>(0.2126, 0.7152, 0.0722))));
}

fn pack4(a: u32, b: u32, c: u32, d: u32) -> u32 {
    return (a & 0xffu) | ((b & 0xffu) << 8u) | ((c & 0xffu) << 16u) | ((d & 0xffu) << 24u);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x0 = id.x * 4u;
    let y0 = id.y * 2u;
    if (x0 >= s.width || y0 >= s.height) {
        return;
    }
    let words_per_row = s.width / 4u;
    var uvs: array<u32, 4>;
    for (var row = 0u; row < 2u; row = row + 1u) {
        let y = min(y0 + row, s.height - 1u);
        let c0 = rgb(x0, y);
        let c1 = rgb(x0 + 1u, y);
        let c2 = rgb(x0 + 2u, y);
        let c3 = rgb(x0 + 3u, y);
        if (y0 + row < s.height) {
            dst[(y0 + row) * words_per_row + id.x] = pack4(y_of(c0), y_of(c1), y_of(c2), y_of(c3));
        }
        if (row == 0u) {
            let y1 = min(y0 + 1u, s.height - 1u);
            let l = (c0 + c1 + rgb(x0, y1) + rgb(x0 + 1u, y1)) * 0.25;
            let r = (c2 + c3 + rgb(x0 + 2u, y1) + rgb(x0 + 3u, y1)) * 0.25;
            let yl = dot(l, vec3<f32>(0.2126, 0.7152, 0.0722));
            let yr = dot(r, vec3<f32>(0.2126, 0.7152, 0.0722));
            uvs[0] = u32(round(128.0 + 224.0 * (l.b - yl) / 1.8556));
            uvs[1] = u32(round(128.0 + 224.0 * (l.r - yl) / 1.5748));
            uvs[2] = u32(round(128.0 + 224.0 * (r.b - yr) / 1.8556));
            uvs[3] = u32(round(128.0 + 224.0 * (r.r - yr) / 1.5748));
        }
    }
    let uv_base = s.width * s.height / 4u;
    dst[uv_base + (y0 / 2u) * words_per_row + id.x] = pack4(uvs[0], uvs[1], uvs[2], uvs[3]);
}
