// Video scopes, computed on the GPU from the final working frame (display-
// encoded BT.709 values). No full-frame readback: `accumulate` bins every
// sampled pixel into an atomic count buffer, then one `draw_*` pass per scope
// turns the counts into a small rgba8 image that egui shows directly.
//
// Count buffer layout (u32 each), mirrored by `scopes.rs`:
//   [WAVE]  4 channels (Y, R, G, B) × WAVE_COLS columns × LEVELS levels
//   [VEC]   VEC_N × VEC_N Cb/Cr bins (x = Cb, y = −Cr, centre = neutral)
//   [HIST]  4 channels × LEVELS
//   [MAX]   4 per-channel histogram maxima (Y, R, G, B)

struct ScopeParams {
    stride: u32,      // sample every `stride`-th pixel in x and y
    wave_mode: u32,   // waveform: 0 luma, 1 RGB overlay
    gain: f32,        // brightness of the traces
    vec_zoom: f32,    // vectorscope magnification (1 = 100 % saturation at the rim)
    samples: f32,     // number of sampled pixels
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: ScopeParams;
@group(0) @binding(2) var<storage, read_write> counts: array<atomic<u32>>;
@group(0) @binding(3) var dst: texture_storage_2d<rgba8unorm, write>;

const WAVE_COLS: u32 = 384u;
const LEVELS: u32 = 256u;
const VEC_N: u32 = 256u;
const VEC_OFF: u32 = 393216u;   // 4 * WAVE_COLS * LEVELS
const HIST_OFF: u32 = 458752u;  // VEC_OFF + VEC_N * VEC_N
const MAX_OFF: u32 = 459776u;   // HIST_OFF + 4 * LEVELS
const COUNT_LEN: u32 = 459780u; // MAX_OFF + 4

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);
const BG = vec3<f32>(0.035, 0.035, 0.04);
const GRID = vec3<f32>(0.32, 0.24, 0.12);
// Skin-tone ("flesh") line angle in the Cb/Cr plane, degrees from +Cb.
const SKIN_DEG: f32 = 123.0;

fn level(v: f32) -> u32 {
    return u32(clamp(v, 0.0, 1.0) * 255.0 + 0.5);
}

/// BT.709 colour-difference components, each in −0.5..0.5.
fn cbcr(c: vec3<f32>) -> vec2<f32> {
    let y = dot(c, LUMA);
    return vec2<f32>((c.b - y) / 1.8556, (c.r - y) / 1.5748);
}

/// Vectorscope pixel position (0..VEC_N) of a Cb/Cr pair.
fn vec_pos(cc: vec2<f32>) -> vec2<f32> {
    let n = f32(VEC_N);
    return vec2<f32>((cc.x * p.vec_zoom + 0.5) * n, (0.5 - cc.y * p.vec_zoom) * n);
}

fn trace(count: u32, reference: f32) -> f32 {
    return 1.0 - exp(-f32(count) * p.gain / max(reference, 1e-6));
}

@compute @workgroup_size(256, 1, 1)
fn clear(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x < COUNT_LEN) {
        atomicStore(&counts[id.x], 0u);
    }
}

@compute @workgroup_size(8, 8, 1)
fn accumulate(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(src);
    let s = id.xy * max(p.stride, 1u);
    if (s.x >= dims.x || s.y >= dims.y) {
        return;
    }
    let c = clamp(textureLoad(src, vec2<i32>(s), 0).rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    let y = dot(c, LUMA);
    let col = min(s.x * WAVE_COLS / dims.x, WAVE_COLS - 1u);
    let lv = vec4<u32>(level(y), level(c.r), level(c.g), level(c.b));
    for (var ch = 0u; ch < 4u; ch = ch + 1u) {
        atomicAdd(&counts[(ch * WAVE_COLS + col) * LEVELS + lv[ch]], 1u);
        atomicAdd(&counts[HIST_OFF + ch * LEVELS + lv[ch]], 1u);
    }
    let vp = clamp(vec2<i32>(floor(vec_pos(cbcr(c)))), vec2<i32>(0), vec2<i32>(i32(VEC_N) - 1));
    atomicAdd(&counts[VEC_OFF + u32(vp.y) * VEC_N + u32(vp.x)], 1u);
}

/// Per-channel histogram maximum (one workgroup of LEVELS threads).
@compute @workgroup_size(256, 1, 1)
fn hist_max(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= LEVELS) {
        return;
    }
    for (var ch = 0u; ch < 4u; ch = ch + 1u) {
        atomicMax(&counts[MAX_OFF + ch], atomicLoad(&counts[HIST_OFF + ch * LEVELS + id.x]));
    }
}

fn wave_count(ch: u32, col: u32, lv: u32) -> u32 {
    return atomicLoad(&counts[(ch * WAVE_COLS + col) * LEVELS + lv]);
}

/// Faint horizontal lines at 0, 25, 50, 75 and 100 %.
fn wave_grid(lv: u32) -> bool {
    return lv == 0u || lv == 64u || lv == 128u || lv == 191u || lv == 255u;
}

/// Waveform, WAVE_COLS × LEVELS: luma (white) or RGB overlay.
@compute @workgroup_size(8, 8, 1)
fn draw_waveform(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let col = min(id.x * WAVE_COLS / dims.x, WAVE_COLS - 1u);
    let lv = (LEVELS - 1u) - min(id.y * LEVELS / dims.y, LEVELS - 1u);
    // sampled pixels per column spread evenly over the levels
    let reference = p.samples / f32(WAVE_COLS * LEVELS) * 2.0;
    var c = BG;
    if (wave_grid(lv)) {
        c = c + GRID;
    }
    if (p.wave_mode == 0u) {
        c = c + vec3<f32>(0.86, 0.94, 0.88) * trace(wave_count(0u, col, lv), reference);
    } else {
        c = c + vec3<f32>(trace(wave_count(1u, col, lv), reference), trace(wave_count(2u, col, lv), reference), trace(wave_count(3u, col, lv), reference));
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}

/// RGB parade: R, G and B waveforms side by side.
@compute @workgroup_size(8, 8, 1)
fn draw_parade(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let third = max(dims.x / 3u, 1u);
    let ch = min(id.x / third, 2u);
    let col = min((id.x - ch * third) * WAVE_COLS / third, WAVE_COLS - 1u);
    let lv = (LEVELS - 1u) - min(id.y * LEVELS / dims.y, LEVELS - 1u);
    let reference = p.samples / f32(WAVE_COLS * LEVELS) * 2.0;
    var tint = vec3<f32>(1.0, 0.28, 0.28);
    if (ch == 1u) {
        tint = vec3<f32>(0.3, 1.0, 0.35);
    } else if (ch == 2u) {
        tint = vec3<f32>(0.35, 0.5, 1.0);
    }
    var c = BG;
    if (wave_grid(lv)) {
        c = c + GRID;
    }
    if (id.x - ch * third == 0u && ch > 0u) {
        c = c + vec3<f32>(0.25);
    }
    c = c + tint * trace(wave_count(ch + 1u, col, lv), reference);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}

fn seg_dist(q: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let ab = b - a;
    let t = clamp(dot(q - a, ab) / max(dot(ab, ab), 1e-6), 0.0, 1.0);
    return length(q - (a + ab * t));
}

/// Vectorscope, VEC_N × VEC_N: Cb right, Cr up, traces tinted by hue, with
/// the 100 % rim, 75 % colour-bar targets and the skin-tone line.
@compute @workgroup_size(8, 8, 1)
fn draw_vector(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let n = f32(VEC_N);
    let bin = min(id.xy * VEC_N / dims, vec2<u32>(VEC_N - 1u));
    let q = (vec2<f32>(id.xy) + 0.5) * n / vec2<f32>(dims);
    let centre = vec2<f32>(n * 0.5);
    var c = BG;
    // graticule: outer rim (100 % saturation at zoom 1), half rim, crosshair
    let r = length(q - centre);
    let rim = 0.5 * n * p.vec_zoom;
    if (abs(r - rim) < 0.7 || abs(r - rim * 0.5) < 0.5) {
        c = c + GRID;
    }
    if ((abs(q.x - centre.x) < 0.5 || abs(q.y - centre.y) < 0.5) && r < 6.0) {
        c = c + GRID;
    }
    // skin-tone line from the centre
    let a = radians(SKIN_DEG);
    let skin_end = centre + vec2<f32>(cos(a), -sin(a)) * n * 0.5;
    if (seg_dist(q, centre, skin_end) < 0.6) {
        c = c + vec3<f32>(0.45, 0.33, 0.22);
    }
    // 75 % colour-bar targets: R, Y, G, C, B, M
    var targets = array<vec3<f32>, 6>(
        vec3<f32>(0.75, 0.0, 0.0), vec3<f32>(0.75, 0.75, 0.0), vec3<f32>(0.0, 0.75, 0.0),
        vec3<f32>(0.0, 0.75, 0.75), vec3<f32>(0.0, 0.0, 0.75), vec3<f32>(0.75, 0.0, 0.75)
    );
    for (var i = 0u; i < 6u; i = i + 1u) {
        let t = vec_pos(cbcr(targets[i]));
        let d = abs(q - t);
        if (max(d.x, d.y) < 4.5 && max(d.x, d.y) > 3.5) {
            c = c + targets[i] * 0.7;
        }
    }
    // trace, tinted by the colour it represents
    let cnt = atomicLoad(&counts[VEC_OFF + bin.y * VEC_N + bin.x]);
    if (cnt > 0u) {
        let cc = vec2<f32>((f32(bin.x) + 0.5) / n - 0.5, 0.5 - (f32(bin.y) + 0.5) / n) / max(p.vec_zoom, 1e-3);
        let yv = 0.6;
        let rr = yv + 1.5748 * cc.y;
        let bb = yv + 1.8556 * cc.x;
        let gg = (yv - 0.2126 * rr - 0.0722 * bb) / 0.7152;
        let hue = clamp(vec3<f32>(rr, gg, bb), vec3<f32>(0.0), vec3<f32>(1.0));
        let reference = p.samples / f32(VEC_N * VEC_N) * 24.0;
        c = c + mix(vec3<f32>(1.0), hue, 0.65) * trace(cnt, reference);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}

/// Histogram: R, G and B overlaid (additive), luma as a thin outline.
@compute @workgroup_size(8, 8, 1)
fn draw_histogram(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let lv = min(id.x * LEVELS / dims.x, LEVELS - 1u);
    let h = 1.0 - (f32(id.y) + 0.5) / f32(dims.y);
    var c = BG;
    if (lv == 64u || lv == 128u || lv == 191u) {
        c = c + GRID * 0.6;
    }
    var tints = array<vec3<f32>, 3>(vec3<f32>(0.85, 0.2, 0.2), vec3<f32>(0.2, 0.8, 0.25), vec3<f32>(0.25, 0.35, 0.9));
    for (var ch = 1u; ch < 4u; ch = ch + 1u) {
        let m = f32(max(atomicLoad(&counts[MAX_OFF + ch]), 1u));
        let v = f32(atomicLoad(&counts[HIST_OFF + ch * LEVELS + lv])) / m;
        if (h <= v) {
            c = c + tints[ch - 1u] * 0.75;
        }
    }
    let my = f32(max(atomicLoad(&counts[MAX_OFF]), 1u));
    let vy = f32(atomicLoad(&counts[HIST_OFF + lv])) / my;
    if (abs(h - vy) < 1.5 / f32(dims.y)) {
        c = vec3<f32>(0.9);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
