//! Gaming 9:16 reframe on the GPU: a square in the 16:9 source must stay a
//! square in every layout on a 9:16 canvas (zero stretching), the layer must
//! fill the vertical frame, and the X pan must move the crop.

use nle_core::eval::{Composition, Layer, LayerSource, RenderNode};
use nle_core::{AssetId, ClipId, Effect, EffectKind, FrameBlendMode, FrameRate, Param, Time};
use nle_media::{PixelFormat, VideoFrame};
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};

const SW: u32 = 320;
const SH: u32 = 180;
const OW: u32 = 108;
const OH: u32 = 192;

/// Mid-grey 16:9 frame with a white 40×40 square at (x0, 70).
struct Square {
    x0: u32,
}

impl FrameSource for Square {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let mut data = Vec::with_capacity((SW * SH * 3 / 2) as usize);
        for y in 0..SH {
            for x in 0..SW {
                let inside = (self.x0..self.x0 + 40).contains(&x) && (70..110).contains(&y);
                data.push(if inside { 235 } else { 90 });
            }
        }
        data.extend(std::iter::repeat_n(128u8, (SW * SH / 2) as usize));
        Some(VideoFrame::new(SW, SH, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

fn reframe(layout: f64, pan: f64) -> Effect {
    let mut e = Effect::new(EffectKind::GamingReframe916);
    e.params[0] = Param::new(0.0); // HUD PiP off: measure the gameplay only
    e.params[6] = Param::new(layout);
    e.params[7] = Param::new(pan);
    e
}

fn render(r: &mut Renderer, src: &mut Square, fx: Effect) -> Vec<u8> {
    let mut l = Layer::new(ClipId(1), 0, LayerSource::Media { asset: AssetId(src.x0 as u64), source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame });
    l.effects = vec![fx.resolve(Time::ZERO)];
    r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), src, RenderOptions::default())
}

/// Bounding box (w, h, centre x) of the bright square in the output luma.
fn bright_box(nv12: &[u8]) -> (u32, u32, f32) {
    let (mut x0, mut x1, mut y0, mut y1) = (u32::MAX, 0, u32::MAX, 0);
    for y in 0..OH {
        for x in 0..OW {
            if nv12[(y * OW + x) as usize] > 170 {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    assert!(x0 <= x1, "square not visible");
    (x1 - x0 + 1, y1 - y0 + 1, (x0 + x1) as f32 * 0.5)
}

#[test]
fn reframe_layouts_never_stretch() {
    let gpu = match GpuContext::new_headless() {
        Ok(g) => g,
        Err(e) => return eprintln!("skipping GPU test: {e}"),
    };
    let mut r = Renderer::new(gpu, OW, OH);
    let mut src = Square { x0: 140 };
    for (layout, name) in [(0.0, "split stack"), (1.0, "centre fit"), (2.0, "full crop")] {
        let out = render(&mut r, &mut src, reframe(layout, 0.0));
        let (w, h, cx) = bright_box(&out);
        eprintln!("{name}: square {w}×{h} at x {cx:.1}");
        assert!(w.abs_diff(h) <= 1, "{name}: square stretched to {w}×{h}");
        assert!((cx - OW as f32 / 2.0).abs() <= 1.5, "{name}: centred crop");
    }
    // full crop fills the vertical frame: no black pillar/letterbox bars
    let out = render(&mut r, &mut src, reframe(2.0, 0.0));
    for (x, y) in [(1, 1), (OW - 2, 1), (1, OH - 2), (OW - 2, OH - 2)] {
        assert!(out[(y * OW + x) as usize] > 60, "corner ({x},{y}) is a bar");
    }
    // X pan: square at the source's right edge is out of the centred crop,
    // panning right brings it in (still square)
    let mut right = Square { x0: 260 };
    let centred = render(&mut r, &mut right, reframe(2.0, 0.0));
    assert!(centred[..(OW * OH) as usize].iter().all(|&v| v <= 170), "edge square outside centred crop");
    let (w, h, _) = bright_box(&render(&mut r, &mut right, reframe(2.0, 1.0)));
    assert!(w.abs_diff(h) <= 1, "panned square {w}×{h}");
}
