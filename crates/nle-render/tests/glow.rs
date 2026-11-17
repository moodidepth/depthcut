//! GLOW bloom on the GPU: a bright square on a dark gameplay-like frame must
//! visibly bleed light into its surroundings, react to every parameter and
//! leave a frame without highlights untouched.

use nle_core::eval::{Composition, Layer, LayerSource, RenderNode};
use nle_core::{AssetId, ClipId, Effect, EffectKind, FrameBlendMode, FrameRate, Param, Time};
use nle_media::{PixelFormat, VideoFrame};
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};

const W: u32 = 320;
const H: u32 = 180;
/// Bright square: x 140..180, y 70..110.
const SQ: (u32, u32, u32, u32) = (140, 180, 70, 110);

/// Dark grey frame (Y 60) with a near-white square (Y 230), neutral chroma.
struct Pattern {
    bright: u8,
}

impl FrameSource for Pattern {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let mut data = Vec::with_capacity((W * H * 3 / 2) as usize);
        for y in 0..H {
            for x in 0..W {
                let inside = (SQ.0..SQ.1).contains(&x) && (SQ.2..SQ.3).contains(&y);
                data.push(if inside { self.bright } else { 60 });
            }
        }
        data.extend(std::iter::repeat_n(128u8, (W * H / 2) as usize));
        Some(VideoFrame::new(W, H, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

fn gpu() -> Option<GpuContext> {
    match GpuContext::new_headless() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipping GPU test: {e}");
            None
        }
    }
}

fn glow(values: &[f64]) -> Effect {
    let mut e = Effect::new(EffectKind::Glow);
    for (i, v) in values.iter().enumerate() {
        e.params[i] = Param::new(*v);
    }
    e
}

fn render(r: &mut Renderer, src: &mut Pattern, fx: Option<Effect>) -> Vec<u8> {
    let mut l = Layer::new(ClipId(1), 0, LayerSource::Media { asset: AssetId(1), source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame });
    l.effects = fx.into_iter().map(|e| e.resolve(Time::ZERO)).filter(|r| !r.is_noop()).collect();
    r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), src, RenderOptions::default())
}

fn luma(nv12: &[u8], x: u32, y: u32) -> f32 {
    nv12[(y * W + x) as usize] as f32
}

/// Mean luma of the 12 px ring just outside the square.
fn halo(nv12: &[u8]) -> f32 {
    let mut s = 0.0;
    let mut n = 0.0;
    for y in SQ.2 - 12..SQ.3 + 12 {
        for x in SQ.0 - 12..SQ.1 + 12 {
            let inside = (SQ.0..SQ.1).contains(&x) && (SQ.2..SQ.3).contains(&y);
            if !inside {
                s += luma(nv12, x, y);
                n += 1.0;
            }
        }
    }
    s / n
}

#[test]
fn glow_blooms_visibly_and_tracks_parameters() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let mut src = Pattern { bright: 230 };
    let base = render(&mut r, &mut src, None);
    let h0 = halo(&base);

    // the live project's preset values (Threshold 0.78, Radius 32 px, Strength 0.35)
    let live = render(&mut r, &mut src, Some(glow(&[0.78, 32.0, 0.35])));
    let h_live = halo(&live);
    eprintln!("halo: none {h0:.1}, live preset {h_live:.1}");
    assert!(h_live > h0 + 6.0, "preset glow must be visible around highlights: {h_live:.1} vs {h0:.1}");

    // stronger intensity → more light; larger radius → light reaches further
    let strong = halo(&render(&mut r, &mut src, Some(glow(&[0.78, 32.0, 1.5]))));
    assert!(strong > h_live + 4.0, "intensity scales the bloom: {strong:.1} vs {h_live:.1}");
    let far = |radius: f64, r: &mut Renderer, src: &mut Pattern| {
        let out = render(r, src, Some(glow(&[0.6, radius, 1.0])));
        luma(&out, SQ.1 + 30, (SQ.2 + SQ.3) / 2)
    };
    let (near_r, wide_r) = (far(8.0, &mut r, &mut src), far(90.0, &mut r, &mut src));
    eprintln!("30 px outside: radius 8 → {near_r:.1}, radius 90 → {wide_r:.1}");
    assert!(wide_r > near_r + 3.0, "radius widens the bloom");

    // threshold above every pixel → nothing glows
    let none = render(&mut r, &mut src, Some(glow(&[1.0, 32.0, 1.0])));
    assert!((halo(&none) - h0).abs() < 1.5, "threshold 1.0 leaves the frame alone");

    // glow colour tints the halo (red glow raises V / Cr around the square)
    let red = render(&mut r, &mut src, Some(glow(&[0.6, 32.0, 1.5, 1.0, 0.1, 0.1])));
    let cr = |nv12: &[u8]| {
        let (cx, cy) = ((SQ.1 + 6) / 2, ((SQ.2 + SQ.3) / 2) / 2);
        nv12[(W * H + cy * W + cx * 2 + 1) as usize] as f32
    };
    eprintln!("Cr beside the square: base {:.0}, red glow {:.0}", cr(&base), cr(&red));
    assert!(cr(&red) > cr(&base) + 4.0, "glow_color tints the bloom");
}
