//! AI matte effects on the GPU with a synthetic matte source: Person Matte
//! cuts out the subject / background or shows the matte, Depth Map shows
//! depth, fogs the far side and keeps a range, and both pass through when no
//! matte was generated.

use nle_core::eval::{Composition, Layer, LayerSource, RenderNode};
use nle_core::{AssetId, ClipId, Effect, EffectKind, FrameBlendMode, FrameRate, MatteKind, Param, Time};
use nle_media::matte::Matte;
use nle_media::{PixelFormat, VideoFrame};
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};

const W: u32 = 320;
const H: u32 = 180;

/// Flat grey frame (Y `luma`). The matte (lower resolution, like a model's
/// output) is 255 on the left half and 0 on the right.
struct Src {
    luma: u8,
    matte: bool,
}

impl FrameSource for Src {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let mut data = vec![self.luma; (W * H) as usize];
        data.extend(std::iter::repeat_n(128u8, (W * H / 2) as usize));
        Some(VideoFrame::new(W, H, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
    fn matte(&mut self, _: AssetId, _: i64, _: MatteKind) -> Option<Matte> {
        if !self.matte {
            return None;
        }
        let (mw, mh) = (64, 36);
        Matte::new(mw, mh, (0..mw * mh).map(|i| if i % mw < mw / 2 { 255 } else { 0 }).collect())
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

fn effect(kind: EffectKind, values: &[(usize, f64)]) -> Effect {
    let mut e = Effect::new(kind);
    for &(i, v) in values {
        e.params[i] = Param::new(v);
    }
    e
}

fn render(r: &mut Renderer, src: &mut Src, fx: Effect) -> Vec<u8> {
    let mut l = Layer::new(ClipId(1), 0, LayerSource::Media { asset: AssetId(1), source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame });
    l.effects = vec![fx.resolve(Time::ZERO)];
    r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), src, RenderOptions::default())
}

/// Luma well inside the left / right halves.
fn sides(nv12: &[u8]) -> (f32, f32) {
    let at = |x: u32| nv12[(H / 2 * W + x) as usize] as f32;
    (at(W / 4), at(W * 3 / 4))
}

#[test]
fn person_matte_and_depth_map_follow_the_matte() {
    let Some(gpu) = gpu() else { return };
    // frames and mattes are cached per renderer: one renderer per source
    let mut r = Renderer::new(gpu.clone(), W, H);
    let mut src = Src { luma: 180, matte: true };
    let black = 16.0 + 4.0;

    let (l, rt) = sides(&render(&mut r, &mut src, effect(EffectKind::PersonMatte, &[])));
    assert!(l > 150.0 && rt < black, "cut out subject: {l} {rt}");
    let (l, rt) = sides(&render(&mut r, &mut src, effect(EffectKind::PersonMatte, &[(0, 1.0)])));
    assert!(l < black && rt > 150.0, "cut out background: {l} {rt}");
    let (l, rt) = sides(&render(&mut r, &mut src, effect(EffectKind::PersonMatte, &[(0, 2.0)])));
    assert!(l > 230.0 && rt < black, "show matte: {l} {rt}");

    let (l, rt) = sides(&render(&mut r, &mut src, effect(EffectKind::DepthMap, &[])));
    assert!(l > 230.0 && rt < black, "show depth (white = near): {l} {rt}");
    // dark scene, light fog: the far (right) side fogs up, the near side stays
    let mut dark = Src { luma: 50, matte: true };
    let mut r2 = Renderer::new(gpu.clone(), W, H);
    let (l, rt) = sides(&render(&mut r2, &mut dark, effect(EffectKind::DepthMap, &[(0, 1.0), (4, 1.0)])));
    assert!((l - 50.0).abs() < 4.0 && rt > 150.0, "depth fog: {l} {rt}");
    let (l, rt) = sides(&render(&mut r, &mut src, effect(EffectKind::DepthMap, &[(0, 2.0)])));
    assert!(l > 150.0 && rt < black, "keep near range: {l} {rt}");

    // no generated matte: both pass through
    let mut none = Src { luma: 180, matte: false };
    let mut r3 = Renderer::new(gpu, W, H);
    for kind in [EffectKind::PersonMatte, EffectKind::DepthMap] {
        let (l, rt) = sides(&render(&mut r3, &mut none, effect(kind, &[(0, 2.0)])));
        assert!((l - 180.0).abs() < 3.0 && (rt - 180.0).abs() < 3.0, "{kind:?} without a matte: {l} {rt}");
    }
}
