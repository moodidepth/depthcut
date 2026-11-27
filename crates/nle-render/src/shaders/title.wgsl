// Title / Text+ rendering: a CPU-rasterised glyph mask (r = coverage,
// g = coverage of the highlighted word, b = coverage of upcoming dimmed words) becomes a full-frame RGBA layer with
// a rounded "pill" background, drop shadow, outline, word highlight and the
// animation state (scale about the text centre, offset, opacity, glitch RGB
// split + slice jitter), all on the GPU.

struct Title {
    text_color: vec4<f32>,
    shadow_color: vec4<f32>,
    pill_color: vec4<f32>,
    pill_rect: vec4<f32>,       // x0, y0, x1, y1 (output px)
    mask_rect: vec4<f32>,       // x, y, w, h where the mask sits (output px)
    outline_color: vec4<f32>,
    highlight_color: vec4<f32>,
    dim_color: vec4<f32>,       // upcoming (not yet spoken) caption words
    shadow_offset: vec2<f32>,   // px
    shadow_blur: f32,           // px
    pill_radius: f32,           // px
    anim_offset: vec2<f32>,     // px
    anim_scale: f32,
    opacity: f32,
    outline_width: f32,         // px
    glitch: f32,                // 0..1
    glitch_seed: f32,
    flags: u32,                 // bit0 shadow, bit1 pill, bit2 outline, bit3 word glow
};

@group(0) @binding(0) var mask: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> u: Title;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;

fn mask_rgb(p: vec2<f32>) -> vec3<f32> {
    let q = (p - u.mask_rect.xy) / u.mask_rect.zw;
    if (any(q < vec2<f32>(0.0)) || any(q > vec2<f32>(1.0))) {
        return vec3<f32>(0.0);
    }
    return textureSampleLevel(mask, samp, q, 0.0).rgb;
}

fn mask_at(p: vec2<f32>) -> vec2<f32> {
    return mask_rgb(p).rg;
}

fn coverage(p: vec2<f32>) -> f32 {
    return mask_at(p).r;
}

// coverage dilated by `r` pixels (outline)
fn dilated(p: vec2<f32>, r: f32) -> f32 {
    var m = coverage(p);
    for (var i = 0; i < 16; i = i + 1) {
        let a = f32(i) * 0.39269908;   // 2π / 16
        let d = vec2<f32>(cos(a), sin(a));
        m = max(m, coverage(p + d * r));
        m = max(m, coverage(p + d * r * 0.5));
    }
    return m;
}

// soft halo of the highlighted word (mask channel g) within `r` pixels
fn word_glow(p: vec2<f32>, r: f32) -> f32 {
    var s = 0.0;
    for (var i = 0; i < 12; i = i + 1) {
        let a = f32(i) * 0.52359878;   // 2π / 12
        let d = vec2<f32>(cos(a), sin(a));
        s = s + mask_at(p + d * r).g + mask_at(p + d * r * 0.5).g;
    }
    return clamp(s / 8.0, 0.0, 1.0);
}

fn rounded_box(p: vec2<f32>, lo: vec2<f32>, hi: vec2<f32>, r: f32) -> f32 {
    let c = (lo + hi) * 0.5;
    let h = (hi - lo) * 0.5;
    let q = abs(p - c) - h + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// premultiplied "over"
fn over(top: vec4<f32>, under: vec4<f32>) -> vec4<f32> {
    return top + under * (1.0 - top.a);
}

fn hash(x: f32) -> f32 {
    return fract(sin(x * 12.9898) * 43758.5453);
}

// premultiplied colour of the title at pre-animation position p
fn shade(p: vec2<f32>) -> vec4<f32> {
    var acc = vec4<f32>(0.0);
    let outline = (u.flags & 4u) != 0u && u.outline_width > 0.0;
    if ((u.flags & 2u) != 0u) {
        let d = rounded_box(p, u.pill_rect.xy, u.pill_rect.zw, u.pill_radius);
        let a = clamp(0.5 - d, 0.0, 1.0) * u.pill_color.a;
        acc = vec4<f32>(u.pill_color.rgb * a, a);
    }
    if ((u.flags & 1u) != 0u) {
        let sp = p - u.shadow_offset;
        let r = max(u.shadow_blur, 0.0) * 0.5;
        var s = 0.0;
        var n = 0.0;
        for (var y = -2; y <= 2; y = y + 1) {
            for (var x = -2; x <= 2; x = x + 1) {
                let w = 1.0 / (1.0 + f32(x * x + y * y));
                let q = sp + vec2<f32>(f32(x), f32(y)) * r;
                var c = coverage(q);
                if (outline) {
                    c = max(c, coverage(q + vec2<f32>(u.outline_width, 0.0)));
                    c = max(c, coverage(q - vec2<f32>(u.outline_width, 0.0)));
                }
                s = s + c * w;
                n = n + w;
            }
        }
        let a = (s / n) * u.shadow_color.a;
        acc = over(vec4<f32>(u.shadow_color.rgb * a, a), acc);
    }
    if ((u.flags & 8u) != 0u) {
        // CapCut warm glow: highlight colour at 35 % under the stroke
        let k = f32(min(textureDimensions(dst).x, textureDimensions(dst).y)) / 1080.0;
        let a = word_glow(p, 9.0 * k) * 0.35;
        acc = over(vec4<f32>(u.highlight_color.rgb * a, a), acc);
    }
    if (outline) {
        let a = dilated(p, u.outline_width) * u.outline_color.a;
        acc = over(vec4<f32>(u.outline_color.rgb * a, a), acc);
    }
    let m = mask_rgb(p);
    let hl = clamp(m.g / max(m.r, 1e-3), 0.0, 1.0);
    let dm = clamp(m.b / max(m.r, 1e-3), 0.0, 1.0);
    let col = mix(mix(u.text_color, u.dim_color, dm), u.highlight_color, hl);
    let ta = m.r * col.a;
    acc = over(vec4<f32>(col.rgb * ta, ta), acc);
    return acc;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (id.x >= dims.x || id.y >= dims.y) {
        return;
    }
    let p = vec2<f32>(id.xy) + vec2<f32>(0.5);
    // undo the animation transform (scale about the text centre, then offset)
    let centre = u.mask_rect.xy + u.mask_rect.zw * 0.5;
    var q = (p - u.anim_offset - centre) / max(u.anim_scale, 1e-3) + centre;
    var acc: vec4<f32>;
    if (u.glitch > 0.001) {
        let k = f32(min(dims.x, dims.y)) / 1080.0;
        let slice = floor(q.y / (14.0 * k));
        let h = hash(slice * 1.37 + u.glitch_seed * 7.1);
        if (h > 0.55) {
            q.x = q.x + (hash(slice + u.glitch_seed * 3.3) - 0.5) * u.glitch * 90.0 * k;
        }
        let split = vec2<f32>(u.glitch * 10.0 * k, 0.0);
        let r = shade(q + split);
        let g = shade(q);
        let b = shade(q - split);
        let a = max(r.a, max(g.a, b.a));
        acc = vec4<f32>(r.r, g.g, b.b, a);
    } else {
        acc = shade(q);
    }
    acc = acc * clamp(u.opacity, 0.0, 1.0);
    // store straight alpha for the compositor
    let rgb = select(vec3<f32>(0.0), acc.rgb / max(acc.a, 1e-5), acc.a > 0.0);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(rgb, acc.a));
}
