// ResolveFX-style clip effects. Every entry point shares one binding
// interface: source image, auxiliary image (blurred highlights for
// glow / halation; the source itself otherwise), sampler, parameters and
// the output.
//
// Pixel-sized parameters are specified at 1080p and scaled by `k`.

struct Fx {
    a: vec4<f32>,     // effect parameters 0..3
    b: vec4<f32>,     // parameters 4..7
    c: vec4<f32>,     // parameters 8..11
    x: vec4<f32>,     // CPU-derived values (per entry point, see below)
    dims: vec2<f32>,  // output size in pixels
    time: f32,        // seconds (clip-local) — animates grain
    k: f32,           // output height / 1080
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var aux: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> u: Fx;
@group(0) @binding(4) var dst: texture_storage_2d<rgba16float, write>;

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

fn px_uv(id: vec3<u32>) -> vec2<f32> {
    return (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(textureDimensions(dst));
}

fn inside(id: vec3<u32>) -> bool {
    let d = textureDimensions(dst);
    return id.x < d.x && id.y < d.y;
}

fn hash3(p: vec3<f32>) -> f32 {
    let q = fract(p * vec3<f32>(0.1031, 0.1030, 0.0973));
    let r = q + dot(q, q.yzx + 33.33);
    return fract((r.x + r.y) * r.z);
}

// ----- glow / halation -------------------------------------------------------

// x.x = threshold, x.y = 0 excess-over-threshold (halation), 1 highlight
// mask (glow: highlights keep their full colour). Writes the highlights at
// half resolution, averaging 4 source taps so thin bright edges survive.
@compute @workgroup_size(8, 8, 1)
fn bright_pass(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let t = 0.25 / vec2<f32>(textureDimensions(dst));
    let c = 0.25 * (textureSampleLevel(src, samp, uv + vec2<f32>(-t.x, -t.y), 0.0).rgb
        + textureSampleLevel(src, samp, uv + vec2<f32>(t.x, -t.y), 0.0).rgb
        + textureSampleLevel(src, samp, uv + vec2<f32>(-t.x, t.y), 0.0).rgb
        + textureSampleLevel(src, samp, uv + vec2<f32>(t.x, t.y), 0.0).rgb);
    let cc = clamp(c, vec3<f32>(0.0), vec3<f32>(4.0));   // no fireflies from HDR spikes
    let l = dot(cc, LUMA);
    let thr = u.x.x;
    var contrib: f32;
    if (u.x.y > 0.5) {
        // threshold 1.0 must glow nothing, even on pure white
        contrib = smoothstep(thr - 0.08, thr + 0.02, l) * step(thr, 0.999);
    } else {
        let knee = 0.1;
        let soft = clamp(l - thr + knee, 0.0, 2.0 * knee);
        contrib = max(soft * soft / (4.0 * knee + 1e-4), l - thr) / max(l, 1e-4);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(cc * max(contrib, 0.0), 1.0));
}

// Separable Gaussian, 25 taps spread over ±3σ (x.x = σ in output pixels).
fn gauss(id: vec3<u32>, dir: vec2<f32>) {
    let uv = px_uv(id);
    let sigma = max(u.x.x, 0.5);
    let step = sigma * 3.0 / 12.0;
    let texel = dir / vec2<f32>(textureDimensions(src));
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var i = -12; i <= 12; i = i + 1) {
        let o = f32(i) * step;
        let w = exp(-o * o / (2.0 * sigma * sigma));
        acc = acc + textureSampleLevel(src, samp, uv + texel * o, 0.0).rgb * w;
        wsum = wsum + w;
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(acc / wsum, 1.0));
}

@compute @workgroup_size(8, 8, 1)
fn blur_h(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    gauss(id, vec2<f32>(1.0, 0.0));
}

@compute @workgroup_size(8, 8, 1)
fn blur_v(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    gauss(id, vec2<f32>(0.0, 1.0));
}

// x.rgb = tint × strength, x.w = 0 additive glow, 1 screen (halation).
@compute @workgroup_size(8, 8, 1)
fn combine(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let base = textureSampleLevel(src, samp, uv, 0.0);
    let g = textureSampleLevel(aux, samp, uv, 0.0).rgb * u.x.rgb;
    var out: vec3<f32>;
    if (u.x.w > 0.5) {
        // halation: red-orange light bleeding back into the image
        let b = clamp(base.rgb, vec3<f32>(0.0), vec3<f32>(1.0));
        out = base.rgb + g * (vec3<f32>(1.0) - b);
    } else {
        out = base.rgb + g;
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, base.a));
}

// ----- camera shake -----------------------------------------------------------

fn mirror(v: vec2<f32>) -> vec2<f32> {
    let m = abs(v - 2.0 * floor(v * 0.5));   // 0..2
    return vec2<f32>(1.0) - abs(vec2<f32>(1.0) - m);
}

fn wrap_uv(v: vec2<f32>, tile: bool) -> vec2<f32> {
    if (tile) {
        return fract(v);
    }
    return mirror(v);
}

// x = (dx, dy) in uv, rotation (radians), zoom.
// c = (tilt_deg, moblur_len 0..1, edge_wrap 0=mirror/1=tile, rgb_split px @1080p)
@compute @workgroup_size(8, 8, 1)
fn shake(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let dims = u.dims;
    let aspect = dims.x / max(dims.y, 1.0);
    let tile = u.c.z > 0.5;
    var p = px_uv(id) - vec2<f32>(0.5);
    p.x = p.x * aspect;
    let c = cos(u.x.z);
    let s = sin(u.x.z);
    p = vec2<f32>(c * p.x - s * p.y, s * p.x + c * p.y) / max(u.x.w, 0.01);
    p.x = p.x / aspect;
    let base_uv = p + vec2<f32>(0.5) + u.x.xy;
    let moblur = clamp(u.c.y, 0.0, 1.0);
    let blur_dir = u.x.xy * (moblur * 0.5);
    let split = u.c.w * u.k / max(dims.x, 1.0);
    var acc = vec4<f32>(0.0);
    let taps = select(1u, 6u, moblur > 0.01 && length(u.x.xy) > 0.0005);
    for (var i = 0u; i < taps; i = i + 1u) {
        let f = select(0.0, (f32(i) + 0.5) / f32(taps) - 0.5, taps > 1u);
        let cuv = base_uv - blur_dir * f;
        if (split > 0.0002) {
            let radial = normalize((px_uv(id) - vec2<f32>(0.5)) + vec2<f32>(1e-5, 0.0));
            let r = textureSampleLevel(src, samp, wrap_uv(cuv + radial * split, tile), 0.0).r;
            let g = textureSampleLevel(src, samp, wrap_uv(cuv, tile), 0.0).g;
            let ba = textureSampleLevel(src, samp, wrap_uv(cuv - radial * split, tile), 0.0);
            acc = acc + vec4<f32>(r, g, ba.b, ba.a);
        } else {
            acc = acc + textureSampleLevel(src, samp, wrap_uv(cuv, tile), 0.0);
        }
    }
    textureStore(dst, vec2<i32>(id.xy), acc / f32(taps));
}

// ----- Cineon film grain ----------------------------------------------------

fn vnoise(p: vec2<f32>, seed: f32, ch: f32) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let s = f * f * (3.0 - 2.0 * f);
    let a = hash3(vec3<f32>(i, seed + ch));
    let b = hash3(vec3<f32>(i + vec2<f32>(1.0, 0.0), seed + ch));
    let c = hash3(vec3<f32>(i + vec2<f32>(0.0, 1.0), seed + ch));
    let d = hash3(vec3<f32>(i + vec2<f32>(1.0, 1.0), seed + ch));
    return mix(mix(a, b, s.x), mix(c, d, s.x), s.y);
}

// two octaves, roughly Gaussian, zero mean
fn grain(p: vec2<f32>, seed: f32, ch: f32) -> f32 {
    let n = vnoise(p, seed, ch) * 0.65 + vnoise(p * 2.03 + vec2<f32>(17.0, 3.0), seed, ch + 0.5) * 0.35;
    return (n - 0.5) * 2.6;
}

// a = (grain size, strength, saturation, stock 0 = 35mm / 1 = 16mm)
@compute @workgroup_size(8, 8, 1)
fn film_grain(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let base = textureLoad(src, vec2<i32>(id.xy), 0);
    let s16 = u.a.w > 0.5;
    let size = max(u.a.x * u.k, 0.35) * select(1.0, 1.6, s16);
    let strength = u.a.y * select(1.0, 1.45, s16);
    let p = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / size;
    // new grain pattern every frame at 24 fps
    let seed = floor(u.time * 24.0) * 7.31;
    let mono = grain(p, seed, 0.0);
    let col = vec3<f32>(grain(p, seed, 1.0), grain(p, seed, 2.0), grain(p, seed, 3.0));
    let n = mix(vec3<f32>(mono), col, clamp(u.a.z, 0.0, 1.0));
    // Cineon-like response: strongest in the mid-tones, gentle in shadows/highlights
    let l = clamp(dot(base.rgb, LUMA), 0.0, 1.0);
    let response = 0.35 + 2.6 * l * (1.0 - l);
    let out = max(base.rgb + n * strength * 0.12 * response, vec3<f32>(0.0));
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, base.a));
}

// ----- directional / zoom blur ------------------------------------------------

// a = (angle°, length px @1080p, samples)
@compute @workgroup_size(8, 8, 1)
fn directional_blur(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let ang = radians(u.a.x);
    let len = u.a.y * u.k;
    let d = vec2<f32>(cos(ang), -sin(ang)) * len / u.dims;
    let n = max(u32(u.a.z), 2u);
    var acc = vec4<f32>(0.0);
    for (var i = 0u; i < n; i = i + 1u) {
        let s = (f32(i) + 0.5) / f32(n) - 0.5;
        acc = acc + textureSampleLevel(src, samp, uv + d * s, 0.0);
    }
    textureStore(dst, vec2<i32>(id.xy), acc / f32(n));
}

// a = (centre x, centre y (-0.5..0.5), amount, samples)
@compute @workgroup_size(8, 8, 1)
fn zoom_blur(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let c = vec2<f32>(0.5) + u.a.xy;
    let n = max(u32(u.a.w), 2u);
    var acc = vec4<f32>(0.0);
    for (var i = 0u; i < n; i = i + 1u) {
        let s = 1.0 - u.a.z * 0.5 * f32(i) / f32(n - 1u);
        acc = acc + textureSampleLevel(src, samp, c + (uv - c) * s, 0.0);
    }
    textureStore(dst, vec2<i32>(id.xy), acc / f32(n));
}

// ----- chroma key -------------------------------------------------------------

fn cbcr(c: vec3<f32>) -> vec2<f32> {
    let y = dot(c, LUMA);
    return vec2<f32>((c.b - y) / 1.8556, (c.r - y) / 1.5748);
}

// a = (key r, key g, key b, tolerance), b = (edge feather, spill suppression)
@compute @workgroup_size(8, 8, 1)
fn chroma_key(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let base = textureLoad(src, vec2<i32>(id.xy), 0);
    let key = u.a.rgb;
    let c = clamp(base.rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    let kc = cbcr(key);
    let pc = cbcr(c);
    // chroma distance, normalised by the key's own saturation
    let dist = length(pc - kc) / max(length(kc), 0.05);
    let tol = u.a.w;
    let feather = max(u.b.x, 1e-3);
    let alpha = smoothstep(tol, tol + feather, dist);
    // spill suppression: limit the key's dominant channel near the key colour
    var rgb = base.rgb;
    let spill = u.b.y * (1.0 - smoothstep(tol, tol + feather + 0.6, dist));
    if (key.g >= key.r && key.g >= key.b) {
        rgb.g = mix(rgb.g, min(rgb.g, max(rgb.r, rgb.b)), spill);
    } else if (key.b >= key.r) {
        rgb.b = mix(rgb.b, min(rgb.b, max(rgb.r, rgb.g)), spill);
    } else {
        rgb.r = mix(rgb.r, min(rgb.r, max(rgb.g, rgb.b)), spill);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(rgb, base.a * alpha));
}

// ----- RGB split & digital glitch --------------------------------------------

// a = (intensity px @1080p, angle deg, edge_falloff, glitch_burst)
@compute @workgroup_size(8, 8, 1)
fn rgb_split_glitch(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv0 = px_uv(id);
    let burst = clamp(u.a.w, 0.0, 1.0);
    var uv = uv0;
    if (burst > 0.005) {
        let tq = floor(u.time * 30.0);
        let row = floor(uv.y * 28.0);
        let blk = floor(uv.y * 9.0);
        let line_n = hash3(vec3<f32>(row, tq, 1.0)) * 2.0 - 1.0;
        let blk_n = hash3(vec3<f32>(blk, tq, 7.0));
        let shift = line_n * burst * 0.025 + select(0.0, (blk_n - 0.5) * burst * 0.10, blk_n > 0.72);
        uv.x = mirror(vec2<f32>(uv.x + shift, uv.y)).x;
    }
    let ang = radians(u.a.y);
    let dvec = vec2<f32>(cos(ang), -sin(ang));
    let d_from_c = uv - vec2<f32>(0.5);
    let radial_scale = mix(1.0, length(d_from_c) * 2.0, clamp(u.a.z, 0.0, 1.0));
    let dir = mix(dvec, normalize(d_from_c + vec2<f32>(1e-5, 0.0)), clamp(u.a.z, 0.0, 1.0));
    let off = dir * (u.a.x * u.k * radial_scale) / max(u.dims, vec2<f32>(1.0));
    let r = textureSampleLevel(src, samp, mirror(uv + off), 0.0).r;
    let ga = textureSampleLevel(src, samp, mirror(uv), 0.0);
    let b = textureSampleLevel(src, samp, mirror(uv - off), 0.0).b;
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(r, ga.g, b, ga.a));
}

// ----- Impact flash & sniper scope vignette ----------------------------------

// a = (flash_intensity, flash_decay, r, g), b = (b, vignette_darken, scope_zoom, 0)
@compute @workgroup_size(8, 8, 1)
fn impact_flash(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv0 = px_uv(id);
    let zoom = max(u.b.z, 1.0);
    let uv = mirror(vec2<f32>(0.5) + (uv0 - vec2<f32>(0.5)) / zoom);
    let base = textureSampleLevel(src, samp, uv, 0.0);
    let decay = select(1.0, exp(-max(u.time, 0.0) * u.a.y), u.a.y > 0.01);
    let flash = clamp(u.a.x * decay, 0.0, 2.5);
    let vig_amt = clamp(u.b.y, 0.0, 1.0);
    let r2 = dot(uv0 - vec2<f32>(0.5), uv0 - vec2<f32>(0.5)) * 4.0;
    let vig = 1.0 - vig_amt * smoothstep(0.35, 1.25, r2);
    let tint = vec3<f32>(u.a.z, u.a.w, u.b.x);
    let out = max(base.rgb * vig + tint * flash, vec3<f32>(0.0));
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, base.a));
}

// ----- Sobel neon / Saber character outline ----------------------------------

// a = (intensity, threshold, thickness px @1080p, edge_r), b = (edge_g, edge_b, base_dim, 0)
@compute @workgroup_size(8, 8, 1)
fn neon_outline(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let base = textureSampleLevel(src, samp, uv, 0.0);
    let step = (u.a.z * u.k) / max(u.dims, vec2<f32>(1.0));
    let l00 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(-step.x, -step.y), 0.0).rgb, LUMA);
    let l10 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(0.0, -step.y), 0.0).rgb, LUMA);
    let l20 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(step.x, -step.y), 0.0).rgb, LUMA);
    let l01 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(-step.x, 0.0), 0.0).rgb, LUMA);
    let l21 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(step.x, 0.0), 0.0).rgb, LUMA);
    let l02 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(-step.x, step.y), 0.0).rgb, LUMA);
    let l12 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(0.0, step.y), 0.0).rgb, LUMA);
    let l22 = dot(textureSampleLevel(src, samp, uv + vec2<f32>(step.x, step.y), 0.0).rgb, LUMA);
    let gx = (l20 + 2.0 * l21 + l22) - (l00 + 2.0 * l01 + l02);
    let gy = (l02 + 2.0 * l12 + l22) - (l00 + 2.0 * l10 + l20);
    let grad = length(vec2<f32>(gx, gy));
    let thr = clamp(u.a.y, 0.0, 0.99);
    let edge = smoothstep(thr, thr + 0.35, grad);
    let edge_col = vec3<f32>(u.a.w, u.b.x, u.b.y);
    let dimmed = base.rgb * (1.0 - clamp(u.b.z, 0.0, 0.9) * (1.0 - edge));
    let out = dimmed + edge_col * (edge * max(u.a.x, 0.0));
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, base.a));
}

// ----- Gaming 9:16 reframe (aspect-locked, zero stretch) -------------------
//
// Writes a vertical layout into `dst` (normally a 9:16 target, any aspect
// works). Every region maps output pixels to source pixels with ONE uniform
// scale (sx == sy): sources are cropped to the region's aspect, never
// squeezed. Mirrors `crate::reframe` (unit-tested on the CPU).
//
// a = (hud_enabled, hud_w, hud_h, pip_scale)      HUD/killfeed crop: top-right of the source
// b = (pip_y, corner_radius, layout, pan_x)        layout 0 split stack, 1 centre fit, 2 full crop
// c = (top_aspect 0=16:10 1=4:3, bg_darken, divider_px @1080 wide, bottom 0=HUD on blur 1=transparent)
// aux = blurred source (background fill)

fn src_aspect() -> f32 {
    let d = vec2<f32>(textureDimensions(src));
    return d.x / max(d.y, 1.0);
}

// Source window (uv origin.xy, size.zw) with aspect `a` (w/h) at full height
// or full width, panned horizontally by `pan` in -1..1.
fn aspect_window(a: f32, pan: f32) -> vec4<f32> {
    let s = src_aspect();
    var size = vec2<f32>(1.0, 1.0);
    if (a < s) {
        size.x = a / s;
    } else {
        size.y = s / a;
    }
    let ox = (1.0 - size.x) * 0.5 * (1.0 + clamp(pan, -1.0, 1.0));
    let oy = (1.0 - size.y) * 0.5;
    return vec4<f32>(ox, oy, size);
}

@compute @workgroup_size(8, 8, 1)
fn gaming_reframe_916(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let dims = u.dims;
    let p = vec2<f32>(id.xy) + vec2<f32>(0.5);
    let px = dims.x / 1080.0;
    let mode = u32(round(clamp(u.b.z, 0.0, 2.0)));
    let pan = u.b.w;
    let dark = 1.0 - clamp(u.c.y, 0.0, 0.95);
    var out = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var hud_top = clamp(u.b.x, 0.0, 0.5) * dims.y;
    var hud_bottom = dims.y;

    if (mode == 0u) {
        // split stack: aspect-locked gameplay crop on top, divider, bottom panel
        let ta = select(1.6, 4.0 / 3.0, u.c.x > 0.5);
        let th = min(dims.x / ta, dims.y * 0.75);
        let half_div = max(u.c.z, 0.0) * px * 0.5;
        if (p.y < th - half_div) {
            let w = aspect_window(dims.x / th, pan);
            out = textureSampleLevel(src, samp, w.xy + vec2<f32>(p.x / dims.x, p.y / th) * w.zw, 0.0);
        } else if (p.y < th + half_div) {
            out = vec4<f32>(0.03, 0.03, 0.035, 1.0);
        } else if (u.c.w > 0.5) {
            out = vec4<f32>(0.0);   // transparent: a cam / sub-clip track shows through
        } else {
            let bh = max(dims.y - th - half_div, 1.0);
            let w = aspect_window(dims.x / bh, 0.0);
            let lp = vec2<f32>(p.x / dims.x, (p.y - th - half_div) / bh);
            out = vec4<f32>(textureSampleLevel(aux, samp, w.xy + lp * w.zw, 0.0).rgb * dark, 1.0);
        }
        hud_top = th + half_div + clamp(u.b.x, 0.0, 0.5) * dims.y;
    } else if (mode == 1u) {
        // centre fit: whole 16:9 frame at full width over a blurred, darkened cover
        let s = src_aspect();
        var fs = vec2<f32>(dims.x, dims.x / s);
        if (fs.y > dims.y) {
            fs = vec2<f32>(dims.y * s, dims.y);
        }
        let o = (dims - fs) * 0.5;
        let q = (p - o) / fs;
        if (all(q >= vec2<f32>(0.0)) && all(q <= vec2<f32>(1.0))) {
            out = textureSampleLevel(src, samp, q, 0.0);
        } else {
            let w = aspect_window(dims.x / dims.y, 0.0);
            out = vec4<f32>(textureSampleLevel(aux, samp, w.xy + p / dims * w.zw, 0.0).rgb * dark, 1.0);
        }
        hud_bottom = o.y;   // the HUD PiP sits in the blurred band above the frame
    } else {
        // full crop: source scaled to the frame height, keyframeable X pan
        let w = aspect_window(dims.x / dims.y, pan);
        out = textureSampleLevel(src, samp, w.xy + p / dims * w.zw, 0.0);
    }

    // aspect-locked HUD / killfeed zoom PiP
    if (u.a.x > 0.5) {
        let kw = clamp(u.a.y, 0.05, 1.0);
        let kh = clamp(u.a.z, 0.05, 1.0);
        let hud_aspect = kw / kh * src_aspect();
        var pw = clamp(u.a.w, 0.1, 1.0) * dims.x;
        var ph = pw / hud_aspect;
        let avail = max(hud_bottom - hud_top - 4.0 * px, 8.0);
        if (ph > avail) {
            ph = avail;
            pw = ph * hud_aspect;
        }
        let half = vec2<f32>(pw, ph) * 0.5;
        let d = p - vec2<f32>(dims.x * 0.5, hud_top + half.y);
        let r = clamp(u.b.y * dims.x, 0.0, min(half.x, half.y) * 0.9);
        let q = abs(d) - (half - vec2<f32>(r));
        let sdf = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        let border = 3.0 * px;
        if (sdf <= 0.0) {
            let local = (d + half) / (2.0 * half);
            let h = textureSampleLevel(src, samp, vec2<f32>(1.0 - kw, 0.0) + local * vec2<f32>(kw, kh), 0.0);
            out = mix(out, vec4<f32>(h.rgb, 1.0), clamp(-sdf, 0.0, 1.0));
        } else if (sdf < border) {
            out = mix(out, vec4<f32>(1.0, 1.0, 1.0, 1.0), clamp(border - sdf, 0.0, 1.0) * 0.9);
        }
    }
    textureStore(dst, vec2<i32>(id.xy), out);
}

// ----- Gaussian blur (RGBA, straight alpha) -----------------------------------
//
// Large radii run on a 2×2-box pyramid (`downsample`) so 25 taps stay dense;
// the final vertical pass upsamples bilinearly straight into the full-size
// target. Colour is weighted by alpha, so transparent pixels never bleed a
// dark fringe into the blurred edge.

@compute @workgroup_size(8, 8, 1)
fn downsample(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let t = 0.25 / vec2<f32>(textureDimensions(dst));
    let s0 = textureSampleLevel(src, samp, uv + vec2<f32>(-t.x, -t.y), 0.0);
    let s1 = textureSampleLevel(src, samp, uv + vec2<f32>(t.x, -t.y), 0.0);
    let s2 = textureSampleLevel(src, samp, uv + vec2<f32>(-t.x, t.y), 0.0);
    let s3 = textureSampleLevel(src, samp, uv + vec2<f32>(t.x, t.y), 0.0);
    let a = s0.a + s1.a + s2.a + s3.a;
    let rgb = s0.rgb * s0.a + s1.rgb * s1.a + s2.rgb * s2.a + s3.rgb * s3.a;
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(rgb / max(a, 1e-5), a * 0.25));
}

// x.x = σ in source pixels (< 0.3 = pass through).
fn gauss_rgba(id: vec3<u32>, dir: vec2<f32>) {
    let uv = px_uv(id);
    let sigma = u.x.x;
    if (sigma < 0.3) {
        textureStore(dst, vec2<i32>(id.xy), textureSampleLevel(src, samp, uv, 0.0));
        return;
    }
    let step = sigma * 3.0 / 12.0;
    let texel = dir / vec2<f32>(textureDimensions(src));
    var rgb = vec3<f32>(0.0);
    var a = 0.0;
    var wsum = 0.0;
    for (var i = -12; i <= 12; i = i + 1) {
        let o = f32(i) * step;
        let w = exp(-o * o / (2.0 * sigma * sigma));
        let s = textureSampleLevel(src, samp, uv + texel * o, 0.0);
        rgb = rgb + s.rgb * (s.a * w);
        a = a + s.a * w;
        wsum = wsum + w;
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(rgb / max(a, 1e-5), a / wsum));
}

@compute @workgroup_size(8, 8, 1)
fn gblur_h(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    gauss_rgba(id, vec2<f32>(1.0, 0.0));
}

@compute @workgroup_size(8, 8, 1)
fn gblur_v(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    gauss_rgba(id, vec2<f32>(0.0, 1.0));
}

// ----- radial (spin) blur -------------------------------------------------------

// a = (centre x, centre y (-0.5..0.5), angle°, samples)
@compute @workgroup_size(8, 8, 1)
fn radial_blur(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let aspect = u.dims.x / max(u.dims.y, 1.0);
    let c = vec2<f32>(0.5) + u.a.xy;
    var p = uv - c;
    p.x = p.x * aspect;
    let n = max(u32(u.a.w), 2u);
    let ang = radians(u.a.z);
    var acc = vec4<f32>(0.0);
    for (var i = 0u; i < n; i = i + 1u) {
        let t = ((f32(i) + 0.5) / f32(n) - 0.5) * ang;
        let cs = cos(t);
        let sn = sin(t);
        var q = vec2<f32>(cs * p.x - sn * p.y, sn * p.x + cs * p.y);
        q.x = q.x / aspect;
        acc = acc + textureSampleLevel(src, samp, c + q, 0.0);
    }
    textureStore(dst, vec2<i32>(id.xy), acc / f32(n));
}

// ----- vignette -----------------------------------------------------------------

// a = (amount -1..1 (negative = white), size, feather, roundness 0 = frame ellipse / 1 = circle),
// b = (centre x, centre y)
@compute @workgroup_size(8, 8, 1)
fn vignette(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let base = textureLoad(src, vec2<i32>(id.xy), 0);
    let aspect = u.dims.x / max(u.dims.y, 1.0);
    var d = (px_uv(id) - (vec2<f32>(0.5) + u.b.xy)) * 2.0;
    d.x = d.x * mix(1.0, aspect, clamp(u.a.w, 0.0, 1.0));
    let r = length(d);
    let m = smoothstep(u.a.y, u.a.y + max(u.a.z, 0.01), r);
    let amt = clamp(u.a.x, -1.0, 1.0);
    var rgb: vec3<f32>;
    if (amt >= 0.0) {
        rgb = base.rgb * (1.0 - amt * m);
    } else {
        rgb = mix(base.rgb, vec3<f32>(1.0), -amt * m);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(rgb, base.a));
}

// ----- sharpen (luma unsharp mask) -------------------------------------------

fn tap(uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(src, samp, uv, 0.0).rgb;
}

// a = (amount, radius px @1080p, threshold)
@compute @workgroup_size(8, 8, 1)
fn sharpen(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let base = textureSampleLevel(src, samp, uv, 0.0);
    let s = vec2<f32>(max(u.a.y * u.k, 0.5)) / max(u.dims, vec2<f32>(1.0));
    let sx = vec2<f32>(s.x, 0.0);
    let sy = vec2<f32>(0.0, s.y);
    // 3×3 tent kernel (1 2 1 / 2 4 2 / 1 2 1) at the radius
    let edges = tap(uv + sx) + tap(uv - sx) + tap(uv + sy) + tap(uv - sy);
    let corners = tap(uv + s) + tap(uv - s) + tap(uv + sx - sy) + tap(uv - sx + sy);
    let blur = (base.rgb * 4.0 + edges * 2.0 + corners) / 16.0;
    // luma only, so edges do not pick up colour fringes
    let d = dot(base.rgb - blur, LUMA);
    let dd = sign(d) * max(abs(d) - u.a.z, 0.0);
    let out = max(base.rgb + vec3<f32>(dd * u.a.x), vec3<f32>(0.0));
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(out, base.a));
}

// ----- pixelate -----------------------------------------------------------------

// a = (block px @1080p, shape 0 = square / 1 = dots)
@compute @workgroup_size(8, 8, 1)
fn pixelate(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let b = max(u.a.x * u.k, 1.0);
    let dims = max(u.dims, vec2<f32>(1.0));
    let p = vec2<f32>(id.xy) + vec2<f32>(0.5);
    let cc = (floor(p / b) + vec2<f32>(0.5)) * b;
    // average a 4×4 grid inside the block so the mosaic does not shimmer
    var rgb = vec3<f32>(0.0);
    var a = 0.0;
    for (var j = 0; j < 4; j = j + 1) {
        for (var i = 0; i < 4; i = i + 1) {
            let o = (vec2<f32>(f32(i), f32(j)) + vec2<f32>(0.5)) / 4.0 - vec2<f32>(0.5);
            let s = textureSampleLevel(src, samp, (cc + o * b) / dims, 0.0);
            rgb = rgb + s.rgb * s.a;
            a = a + s.a;
        }
    }
    var col = vec4<f32>(rgb / max(a, 1e-5), a / 16.0);
    if (u.a.y > 0.5) {
        let r = length(p - cc) / (0.5 * b);
        let m = 1.0 - smoothstep(1.0 - 1.5 / b, 1.0, r);
        col = vec4<f32>(col.rgb * m, col.a);
    }
    textureStore(dst, vec2<i32>(id.xy), col);
}

// ----- lens distortion ----------------------------------------------------------

// a = (k1, k2, zoom, chromatic aberration), b = (centre x, centre y, edges 0 fit / 1 transparent / 2 mirror)
fn lens_f(r2: f32) -> f32 {
    return 1.0 + u.a.x * r2 + u.a.y * r2 * r2;
}

fn lens_uv(uv: vec2<f32>, scale: f32) -> vec2<f32> {
    let aspect = u.dims.x / max(u.dims.y, 1.0);
    let c = vec2<f32>(0.5) + u.b.xy;
    var p = (uv - c) * 2.0;
    p.x = p.x * aspect;
    // r² = 1 at the frame corners
    let r2 = dot(p, p) / (aspect * aspect + 1.0);
    p = p * lens_f(r2) * scale / max(u.a.z, 0.01);
    p.x = p.x / aspect;
    return c + p * 0.5;
}

fn lens_sample(uv: vec2<f32>) -> vec4<f32> {
    let mode = u32(u.b.z + 0.5);
    if (mode == 2u) {
        return textureSampleLevel(src, samp, mirror(uv), 0.0);
    }
    if (mode == 1u && (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)))) {
        return vec4<f32>(0.0);
    }
    return textureSampleLevel(src, samp, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}

@compute @workgroup_size(8, 8, 1)
fn lens_distortion(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = px_uv(id);
    let ca = max(u.a.w, 0.0) * 0.02;
    var fit = 1.0;
    if (u32(u.b.z + 0.5) == 0u) {
        // pull the corners back inside the source (barrel distortion + red fringe)
        fit = 1.0 / max(lens_f(1.0) * (1.0 + ca), 1.0);
    }
    let g = lens_sample(lens_uv(uv, fit));
    var out = g;
    if (ca > 0.0) {
        out = vec4<f32>(lens_sample(lens_uv(uv, fit * (1.0 + ca))).r, g.g, lens_sample(lens_uv(uv, fit * (1.0 - ca))).b, g.a);
    }
    textureStore(dst, vec2<i32>(id.xy), out);
}

// ----- hue / saturation / lightness ---------------------------------------------

// a = (hue°, saturation %, lightness %)
@compute @workgroup_size(8, 8, 1)
fn hue_sat_lightness(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let base = textureLoad(src, vec2<i32>(id.xy), 0);
    var c = base.rgb;
    // hue: rotate around the grey axis (Rodrigues), which keeps greys grey
    let h = radians(u.a.x);
    let ax = vec3<f32>(0.57735027);
    let cs = cos(h);
    c = c * cs + cross(ax, c) * sin(h) + ax * dot(ax, c) * (1.0 - cs);
    let l = dot(c, LUMA);
    c = vec3<f32>(l) + (c - vec3<f32>(l)) * max(1.0 + u.a.y / 100.0, 0.0);
    let lt = clamp(u.a.z / 100.0, -1.0, 1.0);
    if (lt >= 0.0) {
        c = mix(c, vec3<f32>(1.0), lt);
    } else {
        c = c * (1.0 + lt);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(max(c, vec3<f32>(0.0)), base.a));
}

// ----- levels -------------------------------------------------------------------

// a = (input black, input white, gamma, output black), b = (output white, channel 0 RGB / 1 R / 2 G / 3 B).
// The working space holds display-encoded values, so the points act on them
// directly and match a display histogram.
fn level(x: f32) -> f32 {
    let t = max((x - u.a.x) / max(u.a.y - u.a.x, 1e-4), 0.0);
    let g = pow(t, 1.0 / max(u.a.z, 0.01));
    return max(u.a.w + g * (u.b.x - u.a.w), 0.0);
}

@compute @workgroup_size(8, 8, 1)
fn levels(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let base = textureLoad(src, vec2<i32>(id.xy), 0);
    let ch = u32(u.b.y + 0.5);
    var c = base.rgb;
    if (ch == 0u || ch == 1u) {
        c.r = level(c.r);
    }
    if (ch == 0u || ch == 2u) {
        c.g = level(c.g);
    }
    if (ch == 0u || ch == 3u) {
        c.b = level(c.b);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(c, base.a));
}

// ----- AI mattes ---------------------------------------------------------
// `aux` is the clip's cached per-frame matte (single channel in .r, sampled
// in the layer's source space).

// Person Matte. a = (output, choke, softness, -): output 0 cut out subject,
// 1 cut out background, 2 show matte. Softness 0.5 keeps the model's own
// edge; lower values harden it around the (choked) 50 % threshold.
@compute @workgroup_size(8, 8, 1)
fn person_matte(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let base = textureLoad(src, vec2<i32>(id.xy), 0);
    let raw = textureSampleLevel(aux, samp, px_uv(id), 0.0).r;
    let w = max(u.a.z * 2.0, 0.001);
    let m = clamp((raw - (0.5 + u.a.y - 0.5 * w)) / w, 0.0, 1.0);
    let mode = u32(u.a.x + 0.5);
    if (mode == 2u) {
        textureStore(dst, vec2<i32>(id.xy), vec4<f32>(vec3<f32>(m), base.a));
        return;
    }
    let keep = select(m, 1.0 - m, mode == 1u);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(base.rgb, base.a * keep));
}

// Depth Map. aux.r = relative nearness (1 = near). a = (output, near, far,
// softness), b = (fog amount, fog rgb): output 0 shows depth, 1 fades into
// fog from `near` (clear) to `far` (full fog), 2 keeps nearness in
// [far, near] (near ≥ 0.999 = no upper limit).
@compute @workgroup_size(8, 8, 1)
fn depth_map(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let base = textureLoad(src, vec2<i32>(id.xy), 0);
    let d = textureSampleLevel(aux, samp, px_uv(id), 0.0).r;
    let near = u.a.y;
    let far = u.a.z;
    let s = max(u.a.w, 0.001);
    let mode = u32(u.a.x + 0.5);
    if (mode == 0u) {
        textureStore(dst, vec2<i32>(id.xy), vec4<f32>(vec3<f32>(d), base.a));
        return;
    }
    if (mode == 1u) {
        let clear = smoothstep(min(far, near - 0.001), max(near, far + 0.001), d);
        let f = clamp(u.b.x, 0.0, 1.0) * (1.0 - clear);
        textureStore(dst, vec2<i32>(id.xy), vec4<f32>(mix(base.rgb, u.b.yzw, f), base.a));
        return;
    }
    let lo = smoothstep(far - s, far + s, d);
    let hi = select(1.0 - smoothstep(near - s, near + s, d), 1.0, near >= 0.999);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(base.rgb, base.a * lo * hi));
}
