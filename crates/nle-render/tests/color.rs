//! GPU colour pipeline vs the CPU mirror in `nle_render::colormath`: input
//! transforms, working spaces, correction nodes (wheels, curves, hue
//! curves), HSL qualifier, shape masks, highlight preview, the wipe after
//! nodes and shape-masked effects. Skipped when no GPU adapter exists.

use nle_core::color::{ColorCorrection, ColorSpace, Curve, HueCurve, MaskShape, Qualifier, ShapeMask, WorkingSpace};
use nle_core::eval::{Composition, Layer, LayerSource, RenderNode, ResolvedColor};
use nle_core::{AssetId, ClipId, ColorWheel, Effect, EffectKind, FrameRate, Param, Time};
use nle_media::VideoFrame;
use nle_render::colormath::{self, Rgb};
use nle_render::{FrameSource, GpuContext, Highlight, RenderOptions, Renderer};

const W: u32 = 64;
const H: u32 = 32;

struct NoFrames;

impl FrameSource for NoFrames {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, _: i64) -> Option<VideoFrame> {
        None
    }
}

fn gpu() -> Option<GpuContext> {
    match GpuContext::new_headless() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipping GPU colour test: {e}");
            None
        }
    }
}

fn solid(id: u64, lane: i32, rgb: Rgb, color: ResolvedColor) -> Layer {
    let mut l = Layer::new(ClipId(id), lane, LayerSource::Solid([rgb[0], rgb[1], rgb[2], 1.0]));
    l.color = color;
    l
}

fn render(r: &mut Renderer, layers: Vec<Layer>, opts: RenderOptions) -> Vec<u8> {
    let comp = Composition::new(Time::ZERO, layers.into_iter().map(RenderNode::Layer).collect());
    r.render(&comp, &mut NoFrames, opts);
    r.read_display()
}

fn px(img: &[u8], x: u32, y: u32) -> Rgb {
    let i = ((y * W + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]].map(|v| v as f32 / 255.0)
}

fn close(gpu: Rgb, cpu: Rgb, tol: f32, what: &str) {
    let cpu = cpu.map(|v| v.clamp(0.0, 1.0));
    let ok = gpu.iter().zip(cpu).all(|(a, b)| (a - b).abs() <= tol);
    assert!(ok, "{what}: GPU {gpu:?} vs CPU {cpu:?}");
}

const TOL: f32 = 3.0 / 255.0;

#[test]
fn gpu_input_transforms_and_working_spaces_match_the_cpu_mirror() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    for space in ColorSpace::ALL {
        for working in WorkingSpace::ALL {
            for rgb in [[0.45, 0.40, 0.30], [0.62, 0.55, 0.70], [0.12, 0.2, 0.15]] {
                let c = ResolvedColor {
                    enabled: true,
                    input_space: space,
                    working,
                    exposure: 0.3,
                    contrast: 0.15,
                    saturation: 1.1,
                    temperature: 0.1,
                    lift: [0.02, 0.0, -0.02, 0.0],
                    gain: [0.0, 0.0, 0.0, 0.1],
                    ..Default::default()
                };
                let cpu = colormath::grade_primary(rgb, &c);
                let img = render(&mut r, vec![solid(1, 0, rgb, c)], RenderOptions::default());
                close(px(&img, W / 2, H / 2), cpu, TOL, &format!("{space:?} / {working:?} {rgb:?}"));
            }
        }
    }
}

#[test]
fn gpu_correction_nodes_match_the_cpu_mirror_and_masks_limit_them() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let rgb = [0.7, 0.35, 0.25];
    let nodes = [
        ColorCorrection { saturation: 0.0, exposure: 0.5, ..Default::default() },
        ColorCorrection {
            lift: ColorWheel { r: 0.0, g: 0.05, b: 0.1, master: 0.0 },
            offset: ColorWheel { r: 0.05, g: 0.0, b: 0.0, master: 0.0 },
            log_shadows: ColorWheel { r: 0.0, g: 0.0, b: 0.2, master: 0.0 },
            log_highlights: ColorWheel { r: 0.1, g: 0.0, b: 0.0, master: 0.0 },
            contrast: 0.3,
            pivot: 0.05,
            ..Default::default()
        },
        ColorCorrection {
            curve_master: Curve { points: vec![[0.0, 0.1], [0.5, 0.6], [1.0, 0.95]] },
            curve_b: Curve { points: vec![[0.0, 0.0], [0.4, 0.2], [1.0, 1.0]] },
            ..Default::default()
        },
        ColorCorrection {
            hue_vs_hue: HueCurve { points: vec![[0.03, 0.08], [0.5, 0.0]] },
            hue_vs_sat: HueCurve { points: vec![[0.03, -0.5], [0.5, 0.0]] },
            hue_vs_lum: HueCurve { points: vec![[0.03, 0.4], [0.5, 0.0]] },
            ..Default::default()
        },
    ];
    for working in WorkingSpace::ALL {
        for n in &nodes {
            let c = ResolvedColor { enabled: true, working, nodes: vec![n.clone()], ..Default::default() };
            let primary = colormath::grade_primary(rgb, &c);
            let cpu = colormath::correct_node(primary, n, working);
            let img = render(&mut r, vec![solid(1, 0, rgb, c)], RenderOptions::default());
            close(px(&img, W / 2, H / 2), cpu, TOL, &format!("{working:?} node {n:?}"));
        }
    }

    // a hard ellipse mask in the centre: inside graded, corners untouched
    let mask = ShapeMask { shape: MaskShape::Ellipse, size: [0.3, 0.3], feather: 0.0, ..Default::default() };
    let n = ColorCorrection { saturation: 0.0, mask: Some(mask.clone()), ..Default::default() };
    let c = ResolvedColor { enabled: true, nodes: vec![n.clone()], ..Default::default() };
    let img = render(&mut r, vec![solid(1, 0, rgb, c.clone())], RenderOptions::default());
    close(px(&img, W / 2, H / 2), colormath::correct_node(rgb, &n, WorkingSpace::Rec709Gamma), TOL, "inside the mask");
    close(px(&img, 1, 1), rgb, TOL, "outside the mask");
    // the GPU coverage follows the CPU mask at every pixel
    let aspect = W as f32 / H as f32;
    for y in 0..H {
        for x in 0..W {
            let a = mask.alpha([(x as f32 + 0.5) / W as f32, (y as f32 + 0.5) / H as f32], aspect);
            if a > 0.0 && a < 1.0 {
                continue;
            }
            let p = px(&img, x, y);
            let grey = (p[0] - p[2]).abs() < 0.02;
            assert_eq!(grey, a >= 1.0, "pixel ({x}, {y}) coverage {a}");
        }
    }
    // mix 0.5 lands halfway
    let half = ColorCorrection { mix: 0.5, ..n.clone() };
    let img = render(&mut r, vec![solid(1, 0, rgb, ResolvedColor { enabled: true, nodes: vec![half], ..Default::default() })], RenderOptions::default());
    let full = colormath::correct_node(rgb, &n, WorkingSpace::Rec709Gamma);
    close(px(&img, W / 2, H / 2), [0, 1, 2].map(|i| (rgb[i] + full[i]) * 0.5), TOL, "mix 0.5");
}

#[test]
fn gpu_qualifier_keys_one_colour_and_highlight_shows_the_matte() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let red = [0.8, 0.2, 0.2];
    let blue = [0.2, 0.3, 0.8];
    // red everywhere, blue over the right half, then an adjustment layer whose
    // node desaturates only reds
    let bottom = solid(1, 0, red, ResolvedColor::default());
    let mut right = solid(2, 1, blue, ResolvedColor::default());
    right.video.crop = [0.5, 0.0, 0.0, 0.0];
    let q = Qualifier { hue_center: 0.0, hue_width: 0.08, hue_soft: 0.04, use_sat: true, sat_low: 0.2, sat_high: 1.0, sat_soft: 0.05, use_lum: false, ..Default::default() };
    let node = ColorCorrection { saturation: 0.0, qualifier: Some(q), ..Default::default() };
    let mut adj = solid(3, 2, [0.0; 3], ResolvedColor { enabled: true, nodes: vec![node.clone()], ..Default::default() });
    adj.adjustment = true;
    let layers = vec![bottom, right, adj];

    assert!(colormath::qualify(&q, red) > 0.99 && colormath::qualify(&q, blue) < 0.01);
    let img = render(&mut r, layers.clone(), RenderOptions::default());
    close(px(&img, 8, H / 2), colormath::correct_node(red, &node, WorkingSpace::Rec709Gamma), TOL, "keyed red is desaturated");
    close(px(&img, W - 8, H / 2), blue, TOL, "blue is outside the key");

    // highlight: matte (white = keyed) and colour-over-grey
    let hl = |matte| RenderOptions { highlight: Some(Highlight { clip: ClipId(3), node: node.id, matte }), ..Default::default() };
    let img = render(&mut r, layers.clone(), hl(true));
    close(px(&img, 8, H / 2), [1.0; 3], TOL, "matte white on the key");
    close(px(&img, W - 8, H / 2), [0.0; 3], TOL, "matte black off the key");
    let img = render(&mut r, layers.clone(), hl(false));
    close(px(&img, 8, H / 2), red, TOL, "highlight keeps keyed pixels in colour");
    let p = px(&img, W - 8, H / 2);
    assert!((p[0] - p[2]).abs() < 0.02, "highlight greys the rest: {p:?}");
    // a highlight for another clip changes nothing
    let other = RenderOptions { highlight: Some(Highlight { clip: ClipId(99), node: node.id, matte: true }), ..Default::default() };
    let img = render(&mut r, layers.clone(), other);
    close(px(&img, W - 8, H / 2), blue, TOL, "other clip's highlight");

    // denoise widens nothing on flat areas and still keys
    let mut dn = node.clone();
    dn.qualifier.as_mut().unwrap().denoise = 1.0;
    let mut adj2 = layers[2].clone();
    adj2.color.nodes = vec![dn];
    let img = render(&mut r, vec![layers[0].clone(), layers[1].clone(), adj2], RenderOptions::default());
    close(px(&img, 8, H / 2), colormath::correct_node(red, &node, WorkingSpace::Rec709Gamma), TOL, "denoised key on a flat area");
}

#[test]
fn gpu_wipe_after_nodes_shows_the_ungraded_source() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let rgb = [0.6, 0.4, 0.3];
    let c = ResolvedColor { enabled: true, exposure: 0.5, nodes: vec![ColorCorrection { saturation: 0.0, ..Default::default() }], ..Default::default() };
    let img = render(&mut r, vec![solid(1, 0, rgb, c.clone())], RenderOptions { wipe: Some(0.5), ..Default::default() });
    close(px(&img, 4, H / 2), rgb, TOL, "left of the wipe is the source");
    let p = px(&img, W - 4, H / 2);
    assert!((p[0] - p[2]).abs() < 0.02 && p[0] > rgb[2], "right of the wipe is graded: {p:?}");
    let line = px(&img, W / 2, H / 2);
    assert!(line[0] > 0.9 && line[1] < 0.4, "divider: {line:?}");
}

#[test]
fn gpu_masked_effect_only_changes_the_inside() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let rgb = [0.6, 0.6, 0.6];
    let mut e = Effect::new(EffectKind::HueSatLightness);
    e.params[2] = Param::new(-100.0);
    e.mask = Some(ShapeMask { shape: MaskShape::Rectangle, center: [0.25, 0.5], size: [0.2, 0.4], feather: 0.0, ..Default::default() });
    let mut l = solid(1, 0, rgb, ResolvedColor::default());
    l.effects = vec![e.resolve(Time::ZERO)];
    let img = render(&mut r, vec![l.clone()], RenderOptions::default());
    let inside = px(&img, W / 4, H / 2);
    assert!(inside[0] < 0.1, "darkened inside the mask: {inside:?}");
    close(px(&img, W - 4, H / 2), rgb, TOL, "outside the mask");
    // inverted: the other way round
    e.mask.as_mut().unwrap().invert = true;
    l.effects = vec![e.resolve(Time::ZERO)];
    let img = render(&mut r, vec![l], RenderOptions::default());
    close(px(&img, W / 4, H / 2), rgb, TOL, "inverted: inside untouched");
    assert!(px(&img, W - 4, H / 2)[0] < 0.1);
}

#[test]
fn brightness_lifts_every_channel_evenly_on_gpu_and_cpu() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    for working in WorkingSpace::ALL {
        for by in [-0.15, 0.2] {
            let rgb = [0.45, 0.40, 0.30];
            let plain = ResolvedColor { enabled: true, working, ..Default::default() };
            let base = colormath::grade_primary(rgb, &plain);
            let c = ResolvedColor { brightness: by, ..plain };
            let want = base.map(|v| v + by);
            close(colormath::grade_primary(rgb, &c), want, TOL, "CPU brightness");
            let img = render(&mut r, vec![solid(1, 0, rgb, c)], RenderOptions::default());
            close(px(&img, W / 2, H / 2), want, TOL, "GPU brightness");
        }
    }
}
