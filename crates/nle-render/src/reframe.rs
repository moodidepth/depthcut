//! Gaming 9:16 reframe geometry: the CPU mirror of the `gaming_reframe_916`
//! shader, so the zero-stretch guarantee is unit-testable and the viewer
//! gizmos see the same layer size the compositor uses.
//!
//! Every region maps output pixels to source pixels with one uniform scale:
//! the source window always has the region's aspect ratio.

use nle_core::effects::{EffectKind, ResolvedEffect};

/// Source window `[x, y, w, h]` (uv) with aspect `rect_aspect` (w/h), at full
/// height (wide sources) or full width (tall sources), panned horizontally
/// by `pan` in −1..1.
pub fn aspect_window(rect_aspect: f32, src_aspect: f32, pan: f32) -> [f32; 4] {
    let (w, h) = if rect_aspect < src_aspect { (rect_aspect / src_aspect, 1.0) } else { (1.0, src_aspect / rect_aspect) };
    let ox = (1.0 - w) * 0.5 * (1.0 + pan.clamp(-1.0, 1.0));
    [ox, (1.0 - h) * 0.5, w, h]
}

/// Height of the split-stack top panel for an output `dims` (16:10 or 4:3).
pub fn split_top_height(dims: [f32; 2], four_three: bool) -> f32 {
    let ta = if four_three { 4.0 / 3.0 } else { 1.6 };
    (dims[0] / ta).min(dims[1] * 0.75)
}

/// The 9:16 layer size the reframe effect renders for a `src` frame on a
/// canvas `canvas_h` pixels tall (never below the source height, even width).
pub fn output_size(src: (u32, u32), canvas_h: u32) -> (u32, u32) {
    let h = canvas_h.max(src.1).clamp(16, 4096);
    let w = ((h as f32 * 9.0 / 16.0).round() as u32 + 1) & !1;
    (w.max(2), h)
}

/// Layer aspect after a clip's effect stack: `Some(9/16)` once a reframe
/// effect is active.
pub fn stack_output_size(effects: &[ResolvedEffect], src: (u32, u32), canvas_h: u32) -> (u32, u32) {
    if effects.iter().any(|e| e.kind == EffectKind::GamingReframe916) {
        output_size(src, canvas_h)
    } else {
        src
    }
}

/// Output-pixels-per-source-pixel `(sx, sy)` of a region of `rect` output
/// pixels showing `window` of a `src`-sized frame.
pub fn region_scale(rect: [f32; 2], window: [f32; 4], src: [f32; 2]) -> (f32, f32) {
    (rect[0] / (window[2] * src[0]), rect[1] / (window[3] * src[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: [f32; 2] = [1920.0, 1080.0];
    const OUT: [f32; 2] = [1080.0, 1920.0];

    fn uniform(s: (f32, f32)) -> bool {
        (s.0 - s.1).abs() < 1e-4 * s.0.max(s.1)
    }

    #[test]
    fn full_crop_is_608_wide_and_uniform() {
        let w = aspect_window(OUT[0] / OUT[1], SRC[0] / SRC[1], 0.0);
        assert!((w[2] * SRC[0] - 607.5).abs() < 0.5, "608 px source window: {}", w[2] * SRC[0]);
        let s = region_scale(OUT, w, SRC);
        assert!(uniform(s) && (s.0 - 1920.0 / 1080.0).abs() < 1e-3, "{s:?}");
        // pan reaches both edges without leaving the frame
        let l = aspect_window(OUT[0] / OUT[1], SRC[0] / SRC[1], -1.0);
        let r = aspect_window(OUT[0] / OUT[1], SRC[0] / SRC[1], 1.0);
        assert_eq!(l[0], 0.0);
        assert!((r[0] + r[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn split_stack_top_and_bottom_are_uniform() {
        for four_three in [false, true] {
            let th = split_top_height(OUT, four_three);
            assert_eq!(th, if four_three { 810.0 } else { 675.0 });
            let top = aspect_window(OUT[0] / th, SRC[0] / SRC[1], 0.4);
            assert!(uniform(region_scale([OUT[0], th], top, SRC)));
            let bh = OUT[1] - th;
            let bottom = aspect_window(OUT[0] / bh, SRC[0] / SRC[1], 0.0);
            assert!(uniform(region_scale([OUT[0], bh], bottom, SRC)));
        }
        // 16:10 from 16:9 keeps 90 % of the width
        assert!((aspect_window(1.6, 16.0 / 9.0, 0.0)[2] - 0.9).abs() < 1e-6);
    }

    #[test]
    fn centre_fit_is_1080x608() {
        let s = SRC[0] / SRC[1];
        let fit = [OUT[0], OUT[0] / s];
        assert_eq!(fit[1].round(), 608.0);
        assert!(uniform(region_scale(fit, [0.0, 0.0, 1.0, 1.0], SRC)));
    }

    #[test]
    fn output_is_9_16_at_canvas_height() {
        assert_eq!(output_size((1280, 720), 1920), (1080, 1920));
        assert_eq!(output_size((1920, 1080), 1080), (608, 1080));
        assert_eq!(output_size((1280, 720), 540), (406, 720));
    }
}
