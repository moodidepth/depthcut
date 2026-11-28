//! GPU tests for clip-level shape masks (`VideoMask`) and neighbour-layer track
//! mattes (`TrackMatte`: Alpha, Luma, AI Subject / "behind subject").

use nle_core::color::MaskShape;
use nle_core::eval::{Composition, Layer, LayerSource, RenderNode, ResolvedColor};
use nle_core::mask::{MaskMode, MatteChannel, MatteSource, TrackMatte, VideoMask};
use nle_core::{AssetId, ClipId, FrameBlendMode, FrameRate, MatteKind, Param, Time};
use nle_media::matte::Matte;
use nle_media::{PixelFormat, VideoFrame};
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};

const W: u32 = 240;
const H: u32 = 160;

struct MatteSrc {
    rgb: [u8; 3],
    matte_left_half: bool,
}

impl FrameSource for MatteSrc {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let mut data = Vec::with_capacity((W * H * 4) as usize);
        for _ in 0..(W * H) {
            data.extend_from_slice(&[self.rgb[0], self.rgb[1], self.rgb[2], 255]);
        }
        Some(VideoFrame::new(W, H, PixelFormat::Rgba8, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
    fn matte(&mut self, _: AssetId, _: i64, _: MatteKind) -> Option<Matte> {
        if !self.matte_left_half {
            return None;
        }
        let (mw, mh) = (60, 40);
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

fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

fn render_rgb(r: &mut Renderer, nodes: Vec<RenderNode>, src: &mut dyn FrameSource) -> Vec<u8> {
    r.render(&Composition::new(Time::ZERO, nodes), src, RenderOptions::default());
    r.read_display()
}

#[test]
fn clip_mask_inside_outside_feather_expand_invert_and_modes() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let mut src = MatteSrc { rgb: [0, 0, 0], matte_left_half: false };

    let bg = Layer::new(ClipId(1), 0, LayerSource::Solid([0.0, 0.0, 1.0, 1.0]));

    // 1. Sharp centered rectangle mask: red inside, blue outside
    let mut m = VideoMask::new(MaskShape::Rectangle);
    m.shape.center = [0.5, 0.5];
    m.shape.size = [0.25, 0.25];
    m.feather = Param::new(0.0);
    let mut fg = Layer::new(ClipId(2), 1, LayerSource::Solid([1.0, 0.0, 0.0, 1.0]));
    fg.masks = vec![m.resolve(Time::ZERO)];

    let px = render_rgb(&mut r, vec![RenderNode::Layer(bg.clone()), RenderNode::Layer(fg.clone())], &mut src);
    let inside = pixel(&px, W / 2, H / 2);
    let outside = pixel(&px, 10, H / 2);
    assert!(inside[0] > 245 && inside[2] < 10, "inside mask must be red: {inside:?}");
    assert!(outside[2] > 245 && outside[0] < 10, "outside mask must show blue bg: {outside:?}");

    // 2. Inverted mask flips inside and outside
    let mut inv_m = m.clone();
    inv_m.shape.invert = true;
    fg.masks = vec![inv_m.resolve(Time::ZERO)];
    let px = render_rgb(&mut r, vec![RenderNode::Layer(bg.clone()), RenderNode::Layer(fg.clone())], &mut src);
    let inside = pixel(&px, W / 2, H / 2);
    let outside = pixel(&px, 10, H / 2);
    assert!(inside[2] > 245 && inside[0] < 10, "inverted inside must show blue bg: {inside:?}");
    assert!(outside[0] > 245 && outside[2] < 10, "inverted outside must be red: {outside:?}");

    // 3. Feather creates a monotonic ramp across the right edge
    let mut soft = m.clone();
    soft.feather = Param::new(0.15);
    fg.masks = vec![soft.resolve(Time::ZERO)];
    let px = render_rgb(&mut r, vec![RenderNode::Layer(bg.clone()), RenderNode::Layer(fg.clone())], &mut src);
    let edge_x = ((0.5 + 0.25 / (W as f32 / H as f32)) * W as f32).round() as u32;
    let reds: Vec<u8> = ((edge_x - 10)..=(edge_x + 10)).map(|x| pixel(&px, x, H / 2)[0]).collect();
    assert!(reds.windows(2).all(|w| w[1] <= w[0] + 1), "feather must decrease monotonically: {reds:?}");
    assert!(reds.iter().any(|&v| v > 60 && v < 200), "feather must produce intermediate alpha: {reds:?}");

    // 4. Stacked Add + Subtract cuts a hole in the center
    let mut hole = VideoMask::new(MaskShape::Ellipse);
    hole.mode = MaskMode::Subtract;
    hole.shape.center = [0.5, 0.5];
    hole.shape.size = [0.1, 0.1];
    hole.feather = Param::new(0.0);
    let mut big = m.clone();
    big.expand = Param::new(0.05);
    fg.masks = vec![big.resolve(Time::ZERO), hole.resolve(Time::ZERO)];
    let px = render_rgb(&mut r, vec![RenderNode::Layer(bg), RenderNode::Layer(fg)], &mut src);
    let center = pixel(&px, W / 2, H / 2);
    let ring = pixel(&px, W / 2 + 30, H / 2);
    assert!(center[2] > 245 && center[0] < 10, "subtracted center must show blue bg: {center:?}");
    assert!(ring[0] > 245 && ring[2] < 10, "outer ring must stay red: {ring:?}");
}

#[test]
fn track_matte_behind_subject_and_above_layer_work() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let mut src = MatteSrc { rgb: [0, 220, 0], matte_left_half: true };

    // 1. Behind Subject (Below + Subject + Invert): green footage with person on left half;
    //    white overlay above with TrackMatte::behind_subject() shows only on the right half.
    let footage = Layer::new(ClipId(1), 0, LayerSource::Media { asset: AssetId(1), source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame });
    let mut title_overlay = Layer::new(ClipId(2), 1, LayerSource::Solid([1.0, 1.0, 1.0, 1.0]));
    title_overlay.track_matte = Some(TrackMatte::behind_subject());

    let px = render_rgb(&mut r, vec![RenderNode::Layer(footage.clone()), RenderNode::Layer(title_overlay)], &mut src);
    let on_subject = pixel(&px, W / 4, H / 2);
    let behind = pixel(&px, W * 3 / 4, H / 2);
    assert!(on_subject[1] > 200 && on_subject[0] < 20, "subject must stay unobstructed green: {on_subject:?}");
    assert!(behind[0] > 240 && behind[1] > 240 && behind[2] > 240, "background half must show white title: {behind:?}");

    // 2. Above Alpha matte (AE-style): blue bg lane 0, red layer lane 1 matted by masked yellow layer lane 2.
    //    Lane 2 is consumed (never drawn yellow); red appears inside lane 2's mask, blue outside.
    let bg = Layer::new(ClipId(10), 0, LayerSource::Solid([0.0, 0.0, 1.0, 1.0]));
    let mut red = Layer::new(ClipId(11), 1, LayerSource::Solid([1.0, 0.0, 0.0, 1.0]));
    red.track_matte = Some(TrackMatte { source: MatteSource::Above, channel: MatteChannel::Alpha, invert: false });
    let mut matte_layer = Layer::new(ClipId(12), 2, LayerSource::Solid([1.0, 1.0, 0.0, 1.0]));
    let mut shape = VideoMask::new(MaskShape::Rectangle);
    shape.shape.center = [0.5, 0.5];
    shape.shape.size = [0.2, 0.2];
    shape.feather = Param::new(0.0);
    matte_layer.masks = vec![shape.resolve(Time::ZERO)];

    let px = render_rgb(&mut r, vec![RenderNode::Layer(bg), RenderNode::Layer(red), RenderNode::Layer(matte_layer)], &mut src);
    let center = pixel(&px, W / 2, H / 2);
    let corner = pixel(&px, 10, 10);
    assert!(center[0] > 245 && center[1] < 10 && center[2] < 10, "inside matte must be red (not yellow matte): {center:?}");
    assert!(corner[2] > 245 && corner[0] < 10, "outside matte must be blue bg: {corner:?}");

    // 3. Adjustment layer with TrackMatte::behind_subject() grades only the background half.
    let mut adj = Layer::new(ClipId(20), 1, LayerSource::Solid([0.0; 4]));
    adj.adjustment = true;
    adj.color = ResolvedColor { enabled: true, brightness: -0.8, ..Default::default() };
    adj.track_matte = Some(TrackMatte::behind_subject());
    let px = render_rgb(&mut r, vec![RenderNode::Layer(footage), RenderNode::Layer(adj)], &mut src);
    let subj = pixel(&px, W / 4, H / 2);
    let bg_side = pixel(&px, W * 3 / 4, H / 2);
    assert!(subj[1] > 200, "subject untouched by behind-subject adjustment: {subj:?}");
    assert!(bg_side[1] < 80, "background darkened by behind-subject adjustment: {bg_side:?}");
}
