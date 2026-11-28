//! RGBA media through the GPU compositor: decoded alpha reaches the layer,
//! straight alpha composites as "over", and filtering at a transparent edge
//! leaves no dark fringe (premultiplied sampling).

use nle_core::eval::{Composition, Layer, LayerSource, RenderNode};
use nle_core::keyframe::Param;
use nle_core::speed::FrameBlendMode;
use nle_core::{AssetId, ClipId, FrameRate, Time};
use nle_core::{Effect, EffectKind};
use nle_media::{PixelFormat, VideoFrame};
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};

const W: u32 = 64;
const H: u32 = 32;

/// One RGBA8 frame the size of the output, built per column by `col`.
struct Media(fn(u32) -> [u8; 4]);

impl FrameSource for Media {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let data = (0..H).flat_map(|_| (0..W).flat_map(|x| (self.0)(x))).collect();
        Some(VideoFrame::new(W, H, PixelFormat::Rgba8, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

fn gpu() -> Option<GpuContext> {
    match GpuContext::new_headless() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipping GPU alpha test: {e}");
            None
        }
    }
}

/// A solid `base` with the media layer on top, moved by `pos` (fractions of
/// the frame) and scaled by `scale`.
fn render(media: fn(u32) -> [u8; 4], base: [f32; 3], pos: [f32; 2], scale: f32) -> Vec<u8> {
    render_fx(media, base, pos, scale, None)
}

fn render_fx(media: fn(u32) -> [u8; 4], base: [f32; 3], pos: [f32; 2], scale: f32, fx: Option<Effect>) -> Vec<u8> {
    let gpu = gpu().expect("checked by the caller");
    let mut r = Renderer::new(gpu, W, H);
    let bg = Layer::new(ClipId(1), 0, LayerSource::Solid([base[0], base[1], base[2], 1.0]));
    let mut top = Layer::new(ClipId(2), 1, LayerSource::Media { asset: AssetId(7), source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame });
    top.video.position = pos;
    top.video.scale = [scale; 2];
    top.effects = fx.into_iter().map(|e| e.resolve(Time::ZERO)).collect();
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(bg), RenderNode::Layer(top)]);
    r.render(&comp, &mut Media(media), RenderOptions::default());
    r.read_display()
}

fn px(img: &[u8], x: u32) -> [u8; 3] {
    let i = (((H / 2) * W + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]]
}

fn near(got: [u8; 3], want: [u8; 3], tol: u8, what: &str) {
    assert!(got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= tol), "{what}: got {got:?}, want {want:?}");
}

/// Opaque red | half-transparent green | fully transparent (black RGB).
fn three_bands(x: u32) -> [u8; 4] {
    match x {
        0..=31 => [255, 0, 0, 255],
        32..=47 => [0, 255, 0, 128],
        _ => [0, 0, 0, 0],
    }
}

/// Opaque red left half, transparent black right half.
fn red_edge(x: u32) -> [u8; 4] {
    if x < W / 2 {
        [255, 0, 0, 255]
    } else {
        [0, 0, 0, 0]
    }
}

#[test]
fn gpu_rgba_media_composites_straight_alpha_over_the_base() {
    if gpu().is_none() {
        return;
    }
    let img = render(three_bands, [0.0, 0.0, 1.0], [0.0, 0.0], 1.0);
    near(px(&img, 12), [255, 0, 0], 2, "opaque pixels cover the base");
    // 128/255 green over blue
    near(px(&img, 40), [0, 128, 127], 3, "alpha 0.5 blends half way");
    near(px(&img, 58), [0, 0, 255], 1, "alpha 0 leaves the base untouched");
}

#[test]
fn gpu_transparent_edges_have_no_dark_fringe_under_subpixel_moves_and_scale() {
    if gpu().is_none() {
        return;
    }
    // Red over white keeps R at 255 everywhere; only G/B change with coverage.
    // Straight-alpha bilinear would mix the transparent black into the edge
    // (R ≈ 191 on a half-covered pixel).
    for (pos, scale) in [([0.5 / W as f32, 0.0], 1.0), ([0.37 / W as f32, 0.21 / H as f32], 0.73), ([-0.25 / W as f32, 0.0], 1.6)] {
        let img = render(red_edge, [1.0, 1.0, 1.0], pos, scale);
        let mut partial = 0;
        for x in 0..W {
            let [r, g, b] = px(&img, x);
            assert!(r >= 253, "dark fringe at x={x} (pos {pos:?}, scale {scale}): {:?}", [r, g, b]);
            assert!(g.abs_diff(b) <= 2, "edge stays a red/white mix at x={x}: {:?}", [r, g, b]);
            if (20..235).contains(&g) {
                partial += 1;
            }
        }
        assert!(partial >= 1, "the edge is filtered (pos {pos:?}, scale {scale})");
    }
}

#[test]
fn gpu_blur_over_a_transparent_edge_does_not_darken_it() {
    if gpu().is_none() {
        return;
    }
    // A horizontal directional blur smears the red edge into the transparent
    // half. Averaging straight colours would pull in the black RGB there.
    let mut blur = Effect::new(EffectKind::DirectionalBlur);
    // Length is in px at 1080p: 300 is ~9 px on this 32 px-tall frame.
    blur.params[1] = Param::new(300.0);
    let img = render_fx(red_edge, [1.0, 1.0, 1.0], [0.0, 0.0], 1.0, Some(blur));
    let mut partial = 0;
    for x in 0..W {
        let [r, g, b] = px(&img, x);
        assert!(r >= 252, "blur darkened the edge at x={x}: {:?}", [r, g, b]);
        if (20..235).contains(&g) {
            partial += 1;
        }
    }
    assert!(partial >= 4, "the blur spreads the edge over several pixels ({partial})");
}

#[test]
fn gpu_blend_mode_over_transparent_canvas_preserves_overlay_color() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let mut top = Layer::new(ClipId(1), 0, LayerSource::Solid([0.8, 0.2, 0.4, 0.5]));
    top.video.blend = nle_core::BlendMode::Multiply;
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(top)]);
    let packed = r.render_packed(&comp, &mut Media(red_edge), RenderOptions::default(), nle_render::PackFormat::Rgba64 { alpha: true, linear: false });
    let i = ((H / 2 * W + W / 2) * 8) as usize;
    let ch = |o: usize| u16::from_le_bytes([packed[i + o], packed[i + o + 1]]) as f32 / 65535.0;
    let (cr, cg, cb, ca) = (ch(0), ch(2), ch(4), ch(6));
    assert!((ca - 0.5).abs() < 0.02, "alpha must be 0.5, got {ca}");
    assert!((cr - 0.8).abs() < 0.03 && (cg - 0.2).abs() < 0.03 && (cb - 0.4).abs() < 0.03, "Multiply over transparent canvas must not darken to black: ({cr}, {cg}, {cb}, {ca})");
}
