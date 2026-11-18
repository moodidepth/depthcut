//! CPU mirror of `colorspace.wgsl`, the primary in `grade.wgsl` and the node
//! in `correct.wgsl` (without LUTs). The GPU tests compare the shaders with
//! these functions; the UI can use them too (e.g. for curve previews).

use nle_core::color::{ColorCorrection, ColorSpace, Qualifier, WorkingSpace, CURVE_SAMPLES};
use nle_core::eval::ResolvedColor;

pub type Rgb = [f32; 3];

pub const LUMA: Rgb = [0.2126, 0.7152, 0.0722];
const KNEE: f32 = 0.8;

type M3 = [[f32; 3]; 3];
pub const REC2020_TO_709: M3 = [[1.660491, -0.587641, -0.072850], [-0.124550, 1.132_9, -0.008349], [-0.018151, -0.100579, 1.118_73]];
pub const AWG3_TO_709: M3 = [[1.617523, -0.537287, -0.080237], [-0.070573, 1.334613, -0.264040], [-0.021102, -0.226954, 1.248056]];
pub const SGAMUT3CINE_TO_709: M3 = [[1.626947, -0.540139, -0.086809], [-0.178516, 1.417941, -0.239425], [-0.044436, -0.195920, 1.240356]];
pub const VGAMUT_TO_709: M3 = [[1.806576, -0.695697, -0.110879], [-0.170090, 1.305955, -0.135865], [-0.025206, -0.154468, 1.179674]];
pub const REC709_TO_AP1: M3 = [[0.613097, 0.339523, 0.047379], [0.070194, 0.916354, 0.013452], [0.020616, 0.109570, 0.869815]];
pub const AP1_TO_709: M3 = [[1.705051, -0.621792, -0.083259], [-0.130256, 1.140805, -0.010548], [-0.024003, -0.128969, 1.152972]];

fn mul(m: &M3, c: Rgb) -> Rgb {
    [0, 1, 2].map(|r| m[r][0] * c[0] + m[r][1] * c[1] + m[r][2] * c[2])
}

fn map(c: Rgb, f: impl Fn(f32) -> f32) -> Rgb {
    c.map(f)
}

pub fn dot_luma(c: Rgb) -> f32 {
    c[0] * LUMA[0] + c[1] * LUMA[1] + c[2] * LUMA[2]
}

pub fn srgb_decode(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

pub fn pq_decode(v: f32) -> f32 {
    let p = v.clamp(0.0, 1.0).powf(1.0 / 78.84375);
    let y = ((p - 0.8359375).max(0.0) / (18.851_563 - 18.6875 * p)).powf(1.0 / 0.159_301_76);
    y * 10000.0 / 203.0
}

pub fn hlg_decode(v: f32) -> f32 {
    let e = if v <= 0.5 { v * v / 3.0 } else { (((v - 0.559_910_7) / 0.178_832_77).exp() + 0.284_668_92) / 12.0 };
    e / 0.264_962_56
}

pub fn logc3_decode(t: f32) -> f32 {
    if t > 0.149658 {
        (10f32.powf((t - 0.385537) / 0.247190) - 0.052272) / 5.555556
    } else {
        (t - 0.092809) / 5.367655
    }
}

pub fn slog3_decode(t: f32) -> f32 {
    if t >= 171.210_3 / 1023.0 {
        10f32.powf((t * 1023.0 - 420.0) / 261.5) * 0.19 - 0.01
    } else {
        (t * 1023.0 - 95.0) * 0.01125 / (171.210_3 - 95.0)
    }
}

pub fn vlog_decode(t: f32) -> f32 {
    if t < 0.181 {
        (t - 0.125) / 5.6
    } else {
        10f32.powf((t - 0.598206) / 0.241514) - 0.00873
    }
}

/// Decoded code values → linear light, Rec.709 primaries.
pub fn to_linear709(c: Rgb, space: ColorSpace) -> Rgb {
    match space {
        ColorSpace::Rec709 => map(c, |v| v.max(0.0).powf(2.4)),
        ColorSpace::Srgb => map(c, srgb_decode),
        ColorSpace::Linear709 => c,
        ColorSpace::Rec2100Pq => mul(&REC2020_TO_709, map(c, pq_decode)),
        ColorSpace::Rec2100Hlg => mul(&REC2020_TO_709, map(c, hlg_decode)),
        ColorSpace::ArriLogC3 => mul(&AWG3_TO_709, map(c, logc3_decode)),
        ColorSpace::SonySLog3 => mul(&SGAMUT3CINE_TO_709, map(c, slog3_decode)),
        ColorSpace::PanasonicVLog => mul(&VGAMUT_TO_709, map(c, vlog_decode)),
    }
}

pub fn tonemap(lin: Rgb) -> Rgb {
    let c = lin.map(|v| v.max(0.0));
    let m = c[0].max(c[1]).max(c[2]);
    if m <= KNEE {
        return c;
    }
    let mm = KNEE + (1.0 - KNEE) * (1.0 - (-(m - KNEE) / (1.0 - KNEE)).exp());
    c.map(|v| v * mm / m)
}

pub fn to_working(lin: Rgb, ws: WorkingSpace) -> Rgb {
    match ws {
        WorkingSpace::AcesCct => mul(&REC709_TO_AP1, lin).map(nle_core::color::acescct_encode),
        WorkingSpace::Rec709Gamma => display_encode(lin),
    }
}

pub fn from_working(c: Rgb, ws: WorkingSpace) -> Rgb {
    match ws {
        WorkingSpace::AcesCct => mul(&AP1_TO_709, c.map(nle_core::color::acescct_decode)),
        WorkingSpace::Rec709Gamma => display_decode(c),
    }
}

pub fn display_encode(lin: Rgb) -> Rgb {
    lin.map(|v| v.max(0.0).powf(1.0 / 2.4))
}

pub fn display_decode(c: Rgb) -> Rgb {
    c.map(|v| v.max(0.0).powf(2.4))
}

pub fn wheels(c: Rgb, lift: [f32; 4], gamma: [f32; 4], gain: [f32; 4]) -> Rgb {
    [0, 1, 2].map(|i| {
        let l = (lift[i] + lift[3]) * 0.25;
        let g = (gamma[i] + gamma[3]) * 0.5;
        let k = (gain[i] + gain[3]) * 0.5;
        let mut v = c[i] * (1.0 + k);
        v += l * (1.0 - v);
        v.max(0.0).powf(1.0 / (1.0 + g).max(0.05))
    })
}

fn white_balance(lin: Rgb, temperature: f32, tint: f32) -> Rgb {
    [lin[0] * (1.0 + 0.2 * temperature), lin[1] * (1.0 - 0.2 * tint), lin[2] * (1.0 - 0.2 * temperature)]
}

fn contrast_sat(c: Rgb, pivot: f32, contrast: f32, saturation: f32) -> Rgb {
    let c = c.map(|v| (v - pivot) * (1.0 + contrast) + pivot);
    let l = dot_luma(c);
    c.map(|v| l + (v - l) * saturation)
}

/// The primary grade of `grade.wgsl` (no LUTs).
pub fn grade_primary(px: Rgb, g: &ResolvedColor) -> Rgb {
    let lin = to_linear709(px, g.input_space);
    let lin = white_balance(lin.map(|v| v * g.exposure.exp2()), g.temperature, g.tint);
    let c = wheels(to_working(lin, g.working), g.lift, g.gamma, g.gain);
    let c = contrast_sat(c, g.working.pivot(), g.contrast, g.saturation);
    let tm = g.input_space.is_scene_or_hdr();
    if g.working != WorkingSpace::Rec709Gamma || tm {
        let mut out = from_working(c, g.working);
        if tm {
            out = tonemap(out);
        }
        display_encode(out)
    } else {
        c
    }
}

/// Hue (0..1, red 0), saturation (chroma / max) and luma.
pub fn hsl(c: Rgb) -> Rgb {
    let mx = c[0].max(c[1]).max(c[2]);
    let mn = c[0].min(c[1]).min(c[2]);
    let d = mx - mn;
    let mut h = 0.0;
    if d > 1e-6 {
        h = if mx == c[0] {
            (c[1] - c[2]) / d
        } else if mx == c[1] {
            (c[2] - c[0]) / d + 2.0
        } else {
            (c[0] - c[1]) / d + 4.0
        };
        h = (h / 6.0 + 1.0).fract();
    }
    let s = if mx > 1e-6 { d / mx } else { 0.0 };
    [h, s, dot_luma(c)]
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn band(v: f32, lo: f32, hi: f32, soft: f32) -> f32 {
    let s = soft.max(1e-4);
    let mut k = smoothstep(lo - s, lo, v);
    if hi < 1.0 {
        k *= 1.0 - smoothstep(hi, hi + s, v);
    }
    k
}

/// Qualifier key of one pixel (no denoise).
pub fn qualify(q: &Qualifier, c: Rgb) -> f32 {
    let [h, s, l] = hsl(c.map(|v| v.clamp(0.0, 1.0)));
    let mut k = 1.0;
    if q.use_hue {
        let d = ((h - q.hue_center + 0.5).rem_euclid(1.0) - 0.5).abs();
        k *= 1.0 - smoothstep(q.hue_width * 0.5, q.hue_width * 0.5 + q.hue_soft.max(1e-4), d);
    }
    if q.use_sat {
        k *= band(s, q.sat_low, q.sat_high, q.sat_soft);
    }
    if q.use_lum {
        k *= band(l, q.lum_low, q.lum_high, q.lum_soft);
    }
    if q.invert {
        1.0 - k
    } else {
        k
    }
}

fn tone(t: &[f32], x: f32) -> f32 {
    let xc = x.clamp(0.0, 1.0);
    let f = xc * (CURVE_SAMPLES - 1) as f32;
    let i = (f as usize).min(CURVE_SAMPLES - 2);
    t[i] + (t[i + 1] - t[i]) * (f - i as f32) + (x - xc)
}

fn hue_tab(t: &[f32], h: f32) -> f32 {
    let f = h.rem_euclid(1.0) * CURVE_SAMPLES as f32;
    let i = (f as usize) % CURVE_SAMPLES;
    let j = (i + 1) % CURVE_SAMPLES;
    t[i] + (t[j] - t[i]) * f.fract()
}

fn rotate_hue(c: Rgb, turns: f32) -> Rgb {
    let k = 0.577_350_3f32;
    let a = turns * std::f32::consts::TAU;
    let (sa, ca) = a.sin_cos();
    let kc = k * (c[0] + c[1] + c[2]);
    // cross((k, k, k), c)
    let cr = [k * (c[2] - c[1]), k * (c[0] - c[2]), k * (c[1] - c[0])];
    [0, 1, 2].map(|i| c[i] * ca + cr[i] * sa + k * kc * (1.0 - ca))
}

/// The graded result of a node before keying / mix (`correct()` in correct.wgsl).
pub fn correct_node(px: Rgb, n: &ColorCorrection, ws: WorkingSpace) -> Rgb {
    let lin = white_balance(display_decode(px).map(|v| v * n.exposure.exp2()), n.temperature, n.tint);
    let mut c = to_working(lin, ws);
    let off = n.offset.as_array();
    c = [0, 1, 2].map(|i| c[i] + (off[i] + off[3]) * 0.1);
    c = wheels(c, n.lift.as_array(), n.gamma.as_array(), n.gain.as_array());
    let y = dot_luma(c);
    let wsh = 1.0 - smoothstep(n.low_range - 0.1, n.low_range + 0.1, y);
    let wh = smoothstep(n.high_range - 0.1, n.high_range + 0.1, y);
    let wm = (1.0 - wsh - wh).max(0.0);
    let (a, b, d) = (n.log_shadows.as_array(), n.log_midtones.as_array(), n.log_highlights.as_array());
    c = [0, 1, 2].map(|i| c[i] + ((a[i] + a[3]) * wsh + (b[i] + b[3]) * wm + (d[i] + d[3]) * wh) * 0.2);
    c = contrast_sat(c, ws.pivot() + n.pivot, n.contrast, n.saturation);
    c = display_encode(from_working(c, ws));
    if !n.curves_identity() {
        let t = n.bake_curves();
        let tab = |k: usize| &t[k * CURVE_SAMPLES..(k + 1) * CURVE_SAMPLES];
        let tone_on = ![&n.curve_master, &n.curve_r, &n.curve_g, &n.curve_b].iter().all(|c| c.is_identity());
        let hue_on = ![&n.hue_vs_hue, &n.hue_vs_sat, &n.hue_vs_lum].iter().all(|c| c.is_identity());
        if tone_on {
            c = [0, 1, 2].map(|i| tone(tab(i + 1), tone(tab(0), c[i])));
        }
        if hue_on {
            let q = hsl(c.map(|v| v.clamp(0.0, 1.0)));
            let l0 = dot_luma(c);
            c = rotate_hue(c, hue_tab(tab(4), q[0]));
            let l1 = dot_luma(c);
            let s = (1.0 + hue_tab(tab(5), q[0])).max(0.0);
            c = c.map(|v| l1 + (v - l1) * s);
            let dl = l0 - l1 + hue_tab(tab(6), q[0]) * 0.5 * q[1];
            c = c.map(|v| v + dl);
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_log_grey_cards_decode_to_18_percent() {
        // published 18 % grey code values
        assert!((logc3_decode(0.391007) - 0.18).abs() < 0.002);
        assert!((slog3_decode(420.0 / 1023.0) - 0.18).abs() < 0.002);
        assert!((vlog_decode(0.423) - 0.18).abs() < 0.003);
        // 203-nit PQ reference white ≈ code 0.5806; HLG reference white 0.75
        assert!((pq_decode(0.5806) - 1.0).abs() < 0.01);
        assert!((hlg_decode(0.75) - 1.0).abs() < 0.001);
        assert!((srgb_decode(0.5) - 0.214).abs() < 0.001);
        // continuity at the linear/log breaks
        assert!((logc3_decode(0.149658 - 1e-5) - logc3_decode(0.149658 + 1e-5)).abs() < 1e-3);
        assert!((slog3_decode(171.2103 / 1023.0 - 1e-5) - slog3_decode(171.2103 / 1023.0 + 1e-5)).abs() < 1e-3);
        assert!((vlog_decode(0.181 - 1e-5) - vlog_decode(0.181 + 1e-5)).abs() < 1e-3);
    }

    #[test]
    fn gamut_matrices_keep_white_and_ap1_round_trips() {
        for m in [&REC2020_TO_709, &AWG3_TO_709, &SGAMUT3CINE_TO_709, &VGAMUT_TO_709, &REC709_TO_AP1, &AP1_TO_709] {
            let w = mul(m, [1.0; 3]);
            assert!(w.iter().all(|v| (v - 1.0).abs() < 2e-4), "{w:?}");
        }
        let c = [0.3, 0.6, 0.1];
        let back = mul(&AP1_TO_709, mul(&REC709_TO_AP1, c));
        assert!(back.iter().zip(c).all(|(a, b)| (a - b).abs() < 1e-3), "{back:?}");
        let rt = from_working(to_working(c, WorkingSpace::AcesCct), WorkingSpace::AcesCct);
        assert!(rt.iter().zip(c).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn primary_is_the_legacy_math_for_rec709_and_identity_by_default() {
        let px = [0.2, 0.5, 0.8];
        let g = ResolvedColor::default();
        let out = grade_primary(px, &g);
        assert!(out.iter().zip(px).all(|(a, b)| (a - b).abs() < 1e-5), "{out:?}");
        // ACEScct with neutral settings round-trips too
        let a = grade_primary(px, &ResolvedColor { working: WorkingSpace::AcesCct, ..Default::default() });
        assert!(a.iter().zip(px).all(|(a, b)| (a - b).abs() < 2e-3), "{a:?}");
        // log grey card lands near display mid-grey, highlights roll off below 1
        let grey = grade_primary([0.41; 3], &ResolvedColor { input_space: ColorSpace::SonySLog3, ..Default::default() });
        assert!((grey[0] - 0.18f32.powf(1.0 / 2.4)).abs() < 0.02, "{grey:?}");
        let hot = grade_primary([0.9; 3], &ResolvedColor { input_space: ColorSpace::SonySLog3, ..Default::default() });
        assert!(hot[0] < 1.0 && hot[0] > 0.95, "{hot:?}");
    }

    #[test]
    fn node_ops_and_qualifier() {
        let n = ColorCorrection { saturation: 0.0, ..Default::default() };
        let out = correct_node([0.8, 0.2, 0.2], &n, WorkingSpace::Rec709Gamma);
        assert!((out[0] - out[1]).abs() < 1e-5 && (out[1] - out[2]).abs() < 1e-5);
        let id = correct_node([0.8, 0.2, 0.2], &ColorCorrection::default(), WorkingSpace::AcesCct);
        assert!((id[0] - 0.8).abs() < 2e-3 && (id[1] - 0.2).abs() < 2e-3);
        // hue-vs-hue of +1/3 turn moves red towards green
        let hh = ColorCorrection { hue_vs_hue: nle_core::color::HueCurve { points: vec![[0.0, 1.0 / 3.0]] }, ..Default::default() };
        let g = correct_node([0.8, 0.2, 0.2], &hh, WorkingSpace::Rec709Gamma);
        assert!(g[1] > g[0] && g[1] > g[2], "{g:?}");
        // a red qualifier keys red, not blue
        let q = Qualifier { hue_center: 0.0, hue_width: 0.1, hue_soft: 0.05, ..Default::default() };
        assert!(qualify(&q, [0.8, 0.2, 0.2]) > 0.99);
        assert!(qualify(&q, [0.2, 0.2, 0.8]) < 0.01);
        assert!(qualify(&Qualifier { invert: true, ..q }, [0.2, 0.2, 0.8]) > 0.99);
    }
}
