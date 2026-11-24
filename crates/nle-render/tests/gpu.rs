//! Shader validation (always) and GPU execution tests (when an adapter is
//! available — skipped gracefully on headless CI).

use nle_core::eval::{Composition, Layer, LayerSource, RenderNode, ResolvedColor, ResolvedTransition};
use nle_core::{AssetId, ClipId, Direction, Effect, EffectKind, FrameBlendMode, FrameRate, Time, TitleAnim, TitleParams, TransitionKind};
use nle_media::{PixelFormat, VideoFrame};
use nle_render::pipelines::{ALL_SHADERS, EFFECT_ENTRY_POINTS, SCOPE_ENTRY_POINTS, SHADER_EFFECTS, SHADER_SCOPES};
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};
use std::collections::HashMap;

#[test]
fn all_wgsl_shaders_validate() {
    for (name, src, entry) in ALL_SHADERS {
        let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{name}.wgsl parse error:\n{}", e.emit_to_string(src)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{name}.wgsl validation error: {e:?}"));
        assert!(module.entry_points.iter().any(|ep| ep.name == *entry), "{name}: missing entry point {entry}");
    }
    // every effect pipeline the renderer builds must exist as a compute entry point
    let fx = naga::front::wgsl::parse_str(SHADER_EFFECTS).unwrap();
    for entry in EFFECT_ENTRY_POINTS {
        let ep = fx.entry_points.iter().find(|ep| ep.name == *entry).unwrap_or_else(|| panic!("effects.wgsl: missing entry point {entry}"));
        assert_eq!(ep.stage, naga::ShaderStage::Compute, "{entry}");
        assert_eq!(ep.workgroup_size, [8, 8, 1], "{entry}: the renderer dispatches 8×8 groups");
    }
    assert_eq!(fx.entry_points.len(), EFFECT_ENTRY_POINTS.len(), "every effects.wgsl entry point is listed");
    let sc = naga::front::wgsl::parse_str(SHADER_SCOPES).unwrap();
    for entry in SCOPE_ENTRY_POINTS {
        let ep = sc.entry_points.iter().find(|ep| ep.name == *entry).unwrap_or_else(|| panic!("scopes.wgsl: missing entry point {entry}"));
        assert_eq!(ep.stage, naga::ShaderStage::Compute, "{entry}");
    }
    assert_eq!(sc.entry_points.len(), SCOPE_ENTRY_POINTS.len(), "every scopes.wgsl entry point is listed");
}

// ---------------------------------------------------------------------------

const W: u32 = 64;
const H: u32 = 32;

/// Solid-colour NV12 frames per asset, specified as limited-range YUV.
struct Solid(HashMap<AssetId, (u8, u8, u8)>);

impl FrameSource for Solid {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, asset: AssetId, index: i64) -> Option<VideoFrame> {
        let (y, u, v) = *self.0.get(&asset)?;
        let mut data = vec![y; (W * H) as usize];
        for _ in 0..(W * H / 4) {
            data.push(u);
            data.push(v);
        }
        Some(VideoFrame::new(W, H, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

fn gpu() -> Option<GpuContext> {
    match GpuContext::new_headless() {
        Ok(g) => {
            eprintln!("GPU: {} ({:?})", g.info.name, g.info.backend);
            Some(g)
        }
        Err(e) => {
            eprintln!("skipping GPU test: {e}");
            None
        }
    }
}

fn media_layer(asset: AssetId) -> Layer {
    Layer::new(ClipId(1), 0, LayerSource::Media { asset, source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame })
}

fn mean_y(nv12: &[u8], x0: u32, x1: u32) -> f32 {
    let mut s = 0u64;
    let mut n = 0u64;
    for y in 0..H {
        for x in x0..x1 {
            s += nv12[(y * W + x) as usize] as u64;
            n += 1;
        }
    }
    s as f32 / n as f32
}

/// A dark picture with a bright band over the third quarter of one axis.
struct Band(usize);

impl FrameSource for Band {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let mut data = Vec::new();
        for y in 0..H {
            for x in 0..W {
                let (at, len) = if self.0 == 0 { (x, W) } else { (y, H) };
                data.push(if (len / 2..len * 3 / 4).contains(&at) { 200u8 } else { 60 });
            }
        }
        data.resize((W * H * 3 / 2) as usize, 128);
        Some(VideoFrame::new(W, H, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

#[test]
fn a_background_lut_arrives_after_the_first_picture() {
    let Some(gpu) = gpu() else { return };
    struct RemoveOnDrop(std::path::PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let path = std::env::temp_dir().join(format!("rcp-lut-{}.cube", std::process::id()));
    let _cleanup = RemoveOnDrop(path.clone());
    std::fs::write(&path, "LUT_3D_SIZE 2\n".to_string() + &"1 0 0\n".repeat(8)).unwrap();
    let mut layer = Layer::new(ClipId(1), 0, LayerSource::Solid([0.5, 0.5, 0.5, 1.0]));
    layer.color.enabled = true;
    layer.color.lut = Some(nle_core::LutRef { path: path.to_string_lossy().into_owned(), intensity: 1.0 });
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(layer)]);
    let draw = |r: &mut Renderer| {
        r.render(&comp, &mut Band(0), RenderOptions::default());
        r.read_display()[..3].to_vec()
    };
    let mut blocking = Renderer::new(gpu.clone(), 16, 16);
    let wanted = draw(&mut blocking);

    let mut preview = Renderer::new(gpu, 16, 16);
    preview.load_luts_in_background();
    assert_ne!(draw(&mut preview), wanted, "the first picture waited for the file");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !preview.poll_luts() {
        assert!(std::time::Instant::now() < until, "the LUT never arrived");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(draw(&mut preview), wanted);
}

#[test]
fn flip_mirrors_the_picture_inside_its_crop() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    for axis in 0..2 {
        for flipped in [false, true] {
            let mut layer = media_layer(AssetId(9100 + axis as u64 * 2 + flipped as u64));
            layer.video.crop[axis * 2] = 0.5;
            layer.video.flip[axis] = flipped;
            let out = r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(layer)]), &mut Band(axis), RenderOptions::default());
            let len = if axis == 0 { W } else { H };
            let at = |p: u32| {
                let (x, y) = if axis == 0 { (p, H / 2) } else { (W / 2, p) };
                out[(y * W + x) as usize] as i32
            };
            let (near, far) = if flipped { (60, 200) } else { (200, 60) };
            for (p, want) in [(len / 8, 16), (len * 5 / 8, near), (len * 7 / 8, far)] {
                assert!((at(p) - want).abs() <= 3, "axis {axis}, flipped {flipped}, at {p}: {} for {want}", at(p));
            }
        }
    }
}

#[test]
fn gpu_pipeline_end_to_end() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let (a, b) = (AssetId(9001), AssetId(9002));
    let mut src = Solid(HashMap::from([(a, (180, 128, 128)), (b, (40, 128, 128))]));

    // 1. YUV → RGB → NV12 round trip preserves luma
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(media_layer(a))]);
    let out = r.render_nv12(&comp, &mut src, RenderOptions::default());
    assert_eq!(out.len(), (W * H * 3 / 2) as usize);
    let y = mean_y(&out, 0, W);
    assert!((y - 180.0).abs() <= 2.0, "luma round trip {y}");
    let chroma = out[(W * H) as usize];
    assert!((chroma as i32 - 128).abs() <= 2, "neutral chroma {chroma}");

    // 2. Cross dissolve at 50 % lands half-way between the clips
    let tr = RenderNode::Transition {
        from: Some(Box::new(media_layer(a))),
        to: Some(Box::new(media_layer(b))),
        transition: ResolvedTransition { kind: TransitionKind::CrossDissolve, direction: Direction::Left, softness: 0.1, progress: 0.5 },
    };
    let out = r.render_nv12(&Composition::new(Time::ZERO, vec![tr]), &mut src, RenderOptions::default());
    let y = mean_y(&out, 0, W);
    assert!((y - 110.0).abs() <= 3.0, "dissolve midpoint {y}");

    // 3. Exposure +1 stop brightens; the before/after wipe keeps the left half untouched
    let mut graded = media_layer(b);
    graded.color = ResolvedColor { enabled: true, exposure: 1.0, ..Default::default() };
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(graded)]);
    let out = r.render_nv12(&comp, &mut src, RenderOptions { wipe: Some(0.5), ..Default::default() });
    let left = mean_y(&out, 0, W / 2 - 4);
    let right = mean_y(&out, W / 2 + 4, W);
    assert!((left - 40.0).abs() <= 2.0, "ungraded side {left}");
    // +1 stop doubles linear light; with the 2.4 transfer that is x2^(1/2.4) in code values
    let expected = 16.0 + (40.0 - 16.0) * 2f32.powf(1.0 / 2.4);
    assert!((right - expected).abs() <= 2.0, "graded side {right}, expected {expected}");

    // 4. Transform: half scale leaves black borders, opacity mixes with what is below
    let mut small = media_layer(a);
    small.video.scale = [0.5, 0.5];
    let out = r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(small)]), &mut src, RenderOptions::default());
    assert!(out[0] <= 18, "corner is black: {}", out[0]);
    assert!((out[((H / 2) * W + W / 2) as usize] as i32 - 180).abs() <= 3);

    let mut top = media_layer(a);
    top.video.opacity = 0.5;
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(media_layer(b)), RenderNode::Layer(top)]);
    let y = mean_y(&r.render_nv12(&comp, &mut src, RenderOptions::default()), 0, W);
    assert!((y - 110.0).abs() <= 3.0, "50% opacity {y}");

    // 5. Slow-motion frame blending and optical-flow paths execute
    for mode in [FrameBlendMode::FrameBlend, FrameBlendMode::OpticalFlow, FrameBlendMode::WeightedAccumulation] {
        let mut l = media_layer(a);
        l.source = LayerSource::Media { asset: a, source_time: Time::from_millis(50), speed: 0.25, blend: mode };
        let y = mean_y(&r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), &mut src, RenderOptions::default()), 0, W);
        assert!((y - 180.0).abs() <= 3.0, "{mode:?} on a static frame is unchanged: {y}");
    }

    // 6. Every transition kind renders without validation errors
    for kind in TransitionKind::ALL {
        let tr = RenderNode::Transition {
            from: Some(Box::new(media_layer(a))),
            to: Some(Box::new(media_layer(b))),
            transition: ResolvedTransition { kind, direction: Direction::Right, softness: 0.2, progress: 0.3 },
        };
        let out = r.render_nv12(&Composition::new(Time::ZERO, vec![tr]), &mut src, RenderOptions::default());
        assert_eq!(out.len(), (W * H * 3 / 2) as usize);
    }

    // 7. Titles: white glyphs, centred, over black (needs a system font)
    let title = Layer { source: LayerSource::Title(Box::new(TitleParams { text: "Hi".into(), font_size: 900.0, ..Default::default() }), TitleAnim::default()), ..media_layer(a) };
    let out = r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(title)]), &mut src, RenderOptions::default());
    let lit: Vec<(u32, u32)> = (0..H).flat_map(|y| (0..W).map(move |x| (x, y))).filter(|(x, y)| out[(y * W + x) as usize] > 128).collect();
    eprintln!("title pixels lit: {}", lit.len());
    if nle_render::text::FontBook::system().has_fonts() {
        assert!(lit.len() >= 25, "title should light glyph pixels, got {}", lit.len());
        let cx = lit.iter().map(|p| p.0 as f32).sum::<f32>() / lit.len() as f32;
        let cy = lit.iter().map(|p| p.1 as f32).sum::<f32>() / lit.len() as f32;
        assert!((cx - W as f32 / 2.0).abs() < W as f32 * 0.2 && (cy - H as f32 / 2.0).abs() < H as f32 * 0.25, "title centred: ({cx}, {cy})");
        assert!(out[0] <= 20, "background stays black");
    }

    assert!(r.stats().cache_frames >= 1);
    // display path
    r.render(&comp, &mut src, RenderOptions::default());
}

// ---------------------------------------------------------------------------
// Optical flow, motion blur, effects and animated titles

/// Frames with a bright square moving `step` px to the right per frame.
struct Moving {
    step: i64,
}

const MW: u32 = 128;
const MH: u32 = 64;

impl FrameSource for Moving {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let x0 = 16 + index * self.step;
        let mut data = vec![16u8; (MW * MH) as usize];
        for y in 16..48u32 {
            for x in 0..MW {
                let xi = x as i64;
                if xi >= x0 && xi < x0 + 24 {
                    data[(y * MW + x) as usize] = 235;
                }
            }
        }
        data.extend(std::iter::repeat_n(128u8, (MW * MH / 2) as usize));
        Some(VideoFrame::new(MW, MH, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

fn row_luma(nv12: &[u8], y: u32) -> Vec<u8> {
    nv12[(y * MW) as usize..((y + 1) * MW) as usize].to_vec()
}

/// Centre of mass (x) of bright pixels on a row.
fn centre_x(row: &[u8]) -> f32 {
    let (mut s, mut n) = (0.0, 0.0);
    for (x, &v) in row.iter().enumerate() {
        let w = (v as f32 - 16.0).max(0.0);
        s += x as f32 * w;
        n += w;
    }
    s / f32::max(n, 1e-3)
}

fn moving_layer(asset: AssetId, t: Time, blend: FrameBlendMode) -> Layer {
    Layer::new(ClipId(1), 0, LayerSource::Media { asset, source_time: t, speed: 1.0, blend })
}

#[test]
fn gpu_optical_flow_interpolation_and_motion_blur() {
    let Some(gpu) = gpu() else { return };
    eprintln!("flow format: {:?}", gpu.flow_format);
    let mut r = Renderer::new(gpu, MW, MH);
    let mut src = Moving { step: 8 };
    let a = AssetId(77);
    let frame = |n: i64| Time::from_frames(n, FrameRate::FPS_30);
    let mut render = |r: &mut Renderer, l: Layer| r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), &mut src, RenderOptions::default());

    // half-way between frames 2 and 3: the square should sit half-way (+4 px)
    let half = Time(frame(2).0 + (frame(3).0 - frame(2).0) / 2);
    let sharp_frame = render(&mut r, moving_layer(a, frame(2), FrameBlendMode::FloorFrame));
    let c2 = centre_x(&row_luma(&sharp_frame, 32));
    let mid = render(&mut r, moving_layer(a, half, FrameBlendMode::OpticalFlow));
    assert!(r.stats().flow_fields >= 2, "forward + backward flow computed");
    let cm = centre_x(&row_luma(&mid, 32));
    eprintln!("square centre: frame 2 {c2:.2}, interpolated {cm:.2}");
    assert!((cm - (c2 + 4.0)).abs() < 2.0, "interpolated square at {cm}, expected ~{}", c2 + 4.0);
    // optical flow keeps the square solid instead of a 50 % double image
    let peak = row_luma(&mid, 32).into_iter().max().unwrap();
    assert!(peak > 180, "interpolated square stays bright: {peak}");

    // vector motion blur: the moving square's edges are smeared, static rows stay sharp
    let mut blurred = moving_layer(a, frame(2), FrameBlendMode::FloorFrame);
    let mut mb = Effect::new(EffectKind::MotionBlur);
    mb.params[0] = nle_core::Param::new(360.0);
    blurred.effects = vec![mb.resolve(Time::ZERO)];
    let out = render(&mut r, blurred);
    let edge = |row: &[u8]| row.iter().filter(|&&v| v > 40 && v < 210).count();
    let (sharp, soft) = (row_luma(&sharp_frame, 32), row_luma(&out, 32));
    eprintln!("edge pixels: sharp {}, blurred {}", edge(&sharp), edge(&soft));
    assert!(edge(&soft) > edge(&sharp) + 2, "motion blur widens the moving edges");
    assert_eq!(row_luma(&out, 4), row_luma(&sharp_frame, 4), "static rows untouched");
}

#[test]
fn gpu_effects_and_animated_titles() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let a = AssetId(9101);
    let mut src = Solid(HashMap::from([(a, (180, 128, 128))]));
    let base_y = mean_y(&r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(media_layer(a))]), &mut src, RenderOptions::default()), 0, W);

    // every effect renders; shake / blurs of a flat frame keep it flat
    for kind in EffectKind::ALL {
        let mut l = media_layer(a);
        l.effects = vec![Effect::new(kind).resolve(Time::from_millis(400))];
        let out = r.render_nv12(&Composition::new(Time::from_millis(400), vec![RenderNode::Layer(l)]), &mut src, RenderOptions::default());
        let y = mean_y(&out, 0, W);
        eprintln!("{kind:?}: {y:.1}");
        match kind {
            // 9:16 reframe: geometry covered by tests/reframe.rs (pillarboxed here)
            EffectKind::GamingReframe916 => {}
            EffectKind::CameraShake
            | EffectKind::DirectionalBlur
            | EffectKind::ZoomBlur
            | EffectKind::MotionBlur
            | EffectKind::RgbSplitGlitch
            | EffectKind::GaussianBlur
            | EffectKind::RadialBlur
            | EffectKind::Sharpen
            | EffectKind::Pixelate
            | EffectKind::LensDistortion => {
                assert!((y - base_y).abs() < 4.0, "{kind:?} keeps a flat frame nearly flat: {y}")
            }
            // neutral defaults
            EffectKind::HueSatLightness | EffectKind::Levels => assert!((y - base_y).abs() < 2.0, "{kind:?} defaults are neutral: {y}"),
            EffectKind::Vignette => {
                let centre = mean_y(&out, W / 2 - 4, W / 2 + 4);
                let edge = mean_y(&out, 0, 4);
                assert!(edge < centre - 10.0 && (centre - base_y).abs() < 6.0, "vignette darkens the edges only: edge {edge}, centre {centre}");
            }
            EffectKind::Glow | EffectKind::Halation | EffectKind::ImpactFlash => assert!(y >= base_y - 2.0, "{kind:?} only adds light: {y}"),
            EffectKind::FilmGrain => assert!((y - base_y).abs() < 8.0, "grain is zero-mean: {y}"),
            EffectKind::NeonOutline => assert!(y <= base_y + 1.0, "flat frame has no edges: {y}"),
            EffectKind::ChromaKey => {}
            // no matte from this source: AI mattes pass through (tests/matte.rs)
            EffectKind::PersonMatte | EffectKind::DepthMap => assert!((y - base_y).abs() < 1.0, "{kind:?} without a matte: {y}"),
        }
    }

    // colour tools move the flat frame the expected way
    let with = |r: &mut Renderer, src: &mut Solid, kind: EffectKind, set: &[(usize, f64)]| {
        let mut e = Effect::new(kind);
        for (i, v) in set {
            e.params[*i].value = *v;
        }
        let mut l = media_layer(a);
        l.effects = vec![e.resolve(Time::ZERO)];
        r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), src, RenderOptions::default())
    };
    let y = mean_y(&with(&mut r, &mut src, EffectKind::HueSatLightness, &[(2, 50.0)]), 0, W);
    assert!(y > base_y + 10.0, "lightness +50 % brightens: {y} vs {base_y}");
    let y = mean_y(&with(&mut r, &mut src, EffectKind::HueSatLightness, &[(2, -50.0)]), 0, W);
    assert!(y < base_y - 10.0, "lightness -50 % darkens: {y} vs {base_y}");
    let out = with(&mut r, &mut src, EffectKind::HueSatLightness, &[(0, 120.0)]);
    let cb = out[(W * H) as usize] as i32;
    assert!((mean_y(&out, 0, W) - base_y).abs() < 3.0 && (cb - 128).abs() <= 3, "hue rotation keeps greys grey (y {}, cb {cb})", mean_y(&out, 0, W));
    let y = mean_y(&with(&mut r, &mut src, EffectKind::Levels, &[(1, 0.8)]), 0, W);
    assert!(y > base_y + 20.0, "input white 0.8 brightens: {y}");
    let y = mean_y(&with(&mut r, &mut src, EffectKind::Levels, &[(4, 0.5)]), 0, W);
    assert!(y < base_y - 20.0, "output white 0.5 darkens: {y}");
    let y = mean_y(&with(&mut r, &mut src, EffectKind::Levels, &[(2, 2.0)]), 0, W);
    assert!(y > base_y + 3.0, "gamma 2 lifts the mid-tones: {y}");
    // a wide Gaussian (σ ≈ 12 px here → pyramid path) and a lens fit still cover the whole frame
    let out = with(&mut r, &mut src, EffectKind::GaussianBlur, &[(0, 200.0), (1, 2.0)]);
    assert!((mean_y(&out, 0, 2) - base_y).abs() < 4.0, "pyramid blur keeps edges opaque: {}", mean_y(&out, 0, 2));
    let out = with(&mut r, &mut src, EffectKind::LensDistortion, &[(0, 1.0), (3, 1.0)]);
    assert!(out[0] > 100, "lens 'Fit' keeps the corners filled: {}", out[0]);
    let out = with(&mut r, &mut src, EffectKind::LensDistortion, &[(0, 1.0), (6, 1.0)]);
    assert!(out[0] <= 20, "'Transparent' edges show black behind a barrel distortion: {}", out[0]);

    // chroma key: a green layer over the grey one disappears
    let g = AssetId(9102);
    // limited-range BT.709 YUV of RGB (0, 0.69, 0.25)
    src.0.insert(g, (125, 104, 66));
    let mut keyed = Layer::new(ClipId(2), 1, LayerSource::Media { asset: g, source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame });
    keyed.effects = vec![Effect::new(EffectKind::ChromaKey).resolve(Time::ZERO)];
    let y = mean_y(&r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(media_layer(a)), RenderNode::Layer(keyed)]), &mut src, RenderOptions::default()), 0, W);
    assert!((y - base_y).abs() < 6.0, "keyed green layer is transparent: {y} vs {base_y}");

    // an adjustment layer with an effect processes everything beneath it
    let mut adj = Layer::new(ClipId(3), 1, LayerSource::Solid([0.0; 4]));
    adj.adjustment = true;
    adj.effects = vec![Effect::new(EffectKind::Glow).resolve(Time::ZERO)];
    let y = mean_y(&r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(media_layer(a)), RenderNode::Layer(adj)]), &mut src, RenderOptions::default()), 0, W);
    assert!(y > base_y + 1.0, "adjustment glow brightens the clip below: {y} vs {base_y}");

    // animated titles: invisible at opacity 0; outline + glitch + pop render
    if nle_render::text::FontBook::system().has_fonts() {
        let title = |anim: TitleAnim, p: TitleParams| Layer::new(ClipId(4), 2, LayerSource::Title(Box::new(p), anim));
        let p = TitleParams { text: "Hi".into(), font_size: 900.0, outline_enabled: true, ..Default::default() };
        let hidden = TitleAnim { opacity: 0.0, ..Default::default() };
        let out = r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(title(hidden, p.clone()))]), &mut src, RenderOptions::default());
        assert!(mean_y(&out, 0, W) < 20.0, "fully transparent title");
        let glitch = TitleAnim { glitch: 0.8, glitch_seed: 3.0, scale: 1.2, ..Default::default() };
        let out = r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(title(glitch, p))]), &mut src, RenderOptions::default());
        assert!(mean_y(&out, 0, W) > 20.0, "glitching title still renders");
    }
    assert!(r.stats().working_bytes > 0);
}

/// Grey frame halves: left = `.0`, right = `.1` (limited-range luma).
struct Halves(u8, u8);

impl FrameSource for Halves {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let mut data: Vec<u8> = (0..H).flat_map(|_| (0..W).map(|x| if x < W / 2 { self.0 } else { self.1 })).collect();
        data.extend(std::iter::repeat_n(128u8, (W * H / 2) as usize));
        Some(VideoFrame::new(W, H, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

#[test]
fn gpu_sharpen_blur_and_pixelate_on_an_edge() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let mut src = Halves(60, 180);
    let run = |r: &mut Renderer, src: &mut Halves, kind: EffectKind, set: &[(usize, f64)]| {
        let mut e = Effect::new(kind);
        for (i, v) in set {
            e.params[*i].value = *v;
        }
        let mut l = media_layer(AssetId(1));
        l.effects = vec![e.resolve(Time::ZERO)];
        r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), src, RenderOptions::default())
    };
    let row = |o: &[u8]| o[(H / 2 * W) as usize..(H / 2 * W + W) as usize].to_vec();
    let (l, rr) = ((W / 2 - 1) as usize, (W / 2) as usize);
    // Levels at its defaults is skipped as a no-op: the untouched frame
    let p = row(&run(&mut r, &mut src, EffectKind::Levels, &[]));
    let (dl, dr) = (p[l], p[rr]);
    assert!(dl < 70 && dr > 170, "hard edge {dl} → {dr}");
    // unsharp mask overshoots on both sides of the edge
    let s = row(&run(&mut r, &mut src, EffectKind::Sharpen, &[(0, 2.0), (1, 2.0)]));
    eprintln!("sharpen {:?}", &s[28..36]);
    assert!(s[l] < dl && s[rr] > dr, "sharpen overshoot {} / {}", s[l], s[rr]);
    assert!((s[2] as i32 - p[2] as i32).abs() <= 2, "flat areas untouched");
    // Gaussian softens the edge; horizontal 0 leaves a vertical edge hard
    let g = row(&run(&mut r, &mut src, EffectKind::GaussianBlur, &[(0, 60.0)]));
    eprintln!("blur {:?}", &g[28..36]);
    assert!(g[l] > dl + 10 && g[rr] < dr - 10, "blurred edge {} / {}", g[l], g[rr]);
    let g = row(&run(&mut r, &mut src, EffectKind::GaussianBlur, &[(0, 60.0), (1, 0.0)]));
    assert!(g[l] < dl + 8 && g[rr] > dr - 8, "vertical-only blur keeps a vertical edge: {} / {}", g[l], g[rr]);
    // the 200 px @1080p maximum is ~5.9 px blocks here: runs of equal pixels,
    // and the block straddling the edge averages both sides
    let px = row(&run(&mut r, &mut src, EffectKind::Pixelate, &[(0, 200.0)]));
    eprintln!("pixelate {px:?}");
    let changes = px.windows(2).filter(|w| w[0].abs_diff(w[1]) > 1).count();
    assert!(changes <= 6, "pixelate makes runs of equal pixels ({changes} changes)");
    assert!(px[30] > dl + 10 && px[30] < dr - 10, "the block over the edge mixes both sides: {}", px[30]);
}

/// Normalised working value of a neutral limited-range luma code, and back.
fn norm(y: u8) -> f32 {
    (y as f32 - 16.0) / 219.0
}

fn code(v: f32) -> f32 {
    16.0 + 219.0 * v.clamp(0.0, 1.0)
}

/// CPU reference of each blend mode on a grey value (W3C separable formulas).
fn blend_ref(mode: nle_core::BlendMode, b: f32, l: f32) -> f32 {
    use nle_core::BlendMode::*;
    let hard = |b: f32, l: f32| if l < 0.5 { 2.0 * b * l } else { 1.0 - 2.0 * (1.0 - b) * (1.0 - l) };
    match mode {
        Normal | Luminosity => l,
        Multiply => b * l,
        Screen => 1.0 - (1.0 - b) * (1.0 - l),
        Overlay => hard(l, b),
        Add => b + l,
        SoftLight => {
            if l <= 0.5 {
                b - (1.0 - 2.0 * l) * b * (1.0 - b)
            } else {
                let d = if b <= 0.25 { ((16.0 * b - 12.0) * b + 4.0) * b } else { b.sqrt() };
                b + (2.0 * l - 1.0) * (d - b)
            }
        }
        HardLight => hard(b, l),
        ColorDodge => {
            if l >= 1.0 {
                1.0
            } else {
                (b / (1.0 - l)).min(1.0)
            }
        }
        ColorBurn => {
            if l <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - b) / l).min(1.0)
            }
        }
        Darken => b.min(l),
        Lighten => b.max(l),
        Difference => (b - l).abs(),
        Exclusion => b + l - 2.0 * b * l,
    }
}

#[test]
fn gpu_every_blend_mode_matches_the_reference() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    // two base / layer pairs so the dark and light branches are both exercised
    // (fresh asset ids per pair: the renderer caches decoded frames by asset)
    for (i, (yb, yl)) in [(120u8, 180u8), (180, 60)].into_iter().enumerate() {
        let (bg, fg) = (AssetId(9201 + 2 * i as u64), AssetId(9202 + 2 * i as u64));
        let mut src = Solid(HashMap::from([(bg, (yb, 128, 128)), (fg, (yl, 128, 128))]));
        for mode in nle_core::BlendMode::ALL {
            let mut top = Layer::new(ClipId(2), 1, LayerSource::Media { asset: fg, source_time: Time::ZERO, speed: 1.0, blend: FrameBlendMode::FloorFrame });
            top.video.blend = mode;
            let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(media_layer(bg)), RenderNode::Layer(top)]);
            let y = mean_y(&r.render_nv12(&comp, &mut src, RenderOptions::default()), 0, W);
            let want = code(blend_ref(mode, norm(yb), norm(yl)));
            eprintln!("{mode:?} ({yb},{yl}): {y:.1} want {want:.1}");
            assert!((y - want).abs() <= 2.0, "{mode:?} base {yb} layer {yl}: got {y}, want {want}");
        }
    }
}

// ---------------------------------------------------------------------------
// Video scopes

#[test]
fn gpu_scopes_bin_a_solid_frame_like_the_cpu_mirror() {
    use nle_render::scopes::{self, Channel};
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let mut src = Solid(HashMap::new());
    assert!(r.read_scope_counts().is_none(), "scopes are off by default");
    r.set_scopes_enabled(true);
    assert!(r.scopes_enabled());
    let rgb = [0.75f32, 0.25, 0.5];
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(Layer::new(ClipId(1), 0, LayerSource::Solid([rgb[0], rgb[1], rgb[2], 1.0])))]);
    r.render(&comp, &mut src, RenderOptions::default());
    let c = r.read_scope_counts().expect("scope counts");
    assert_eq!(c.len(), scopes::COUNT_LEN as usize);
    let n = W * H; // stride 1 at this size
    assert_eq!(scopes::sample_count(W, H, scopes::sample_stride(W)), n);

    // the histogram puts every sample into one bin per channel (±1 level of the CPU value)
    let peak = |ch: Channel| (0..scopes::LEVELS).max_by_key(|&lv| c[scopes::hist_index(ch, lv)]).unwrap();
    for (ch, v) in [(Channel::Luma, scopes::luma(rgb)), (Channel::Red, rgb[0]), (Channel::Green, rgb[1]), (Channel::Blue, rgb[2])] {
        let lv = peak(ch);
        eprintln!("{ch:?}: GPU level {lv}, CPU {}", scopes::level(v));
        assert!(lv.abs_diff(scopes::level(v)) <= 1, "{ch:?}");
        assert_eq!(c[scopes::hist_index(ch, lv)], n, "{ch:?}: all samples in one bin");
        assert_eq!(c[(scopes::MAX_OFF + ch as u32) as usize], n, "{ch:?}: histogram max");
    }
    // waveform: every sample counted once per channel, only in the peak level row
    for ch in [Channel::Luma, Channel::Red, Channel::Green, Channel::Blue] {
        let lv = peak(ch);
        let total: u32 = (0..scopes::WAVE_COLS).map(|col| c[scopes::wave_index(ch, col, lv)]).sum();
        assert_eq!(total, n, "{ch:?} waveform");
    }
    // vectorscope: one bin, matching the CPU mirror within a bin
    let vec: Vec<(u32, u32, u32)> = (0..scopes::VEC_N * scopes::VEC_N)
        .filter_map(|i| {
            let k = c[(scopes::VEC_OFF + i) as usize];
            (k > 0).then_some((i % scopes::VEC_N, i / scopes::VEC_N, k))
        })
        .collect();
    let (ex, ey) = scopes::vec_bin(rgb, 1.0);
    eprintln!("vectorscope bins {vec:?}, CPU ({ex}, {ey})");
    assert_eq!(vec.iter().map(|v| v.2).sum::<u32>(), n);
    assert!(vec.iter().all(|v| v.0.abs_diff(ex) <= 1 && v.1.abs_diff(ey) <= 1));

    // the drawn scopes show the trace where the bins are
    let px = |img: &[u8], w: u32, x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2]]
    };
    let bright = |p: [u8; 3]| p.iter().map(|&v| v as u32).sum::<u32>();
    let (vw, _) = nle_render::ScopeKind::Vectorscope.size();
    let vimg = r.read_scope_image(nle_render::ScopeKind::Vectorscope).unwrap();
    let at_trace = bright(px(&vimg, vw, ex, ey));
    let empty = bright(px(&vimg, vw, 255 - ex, 255 - ey));
    eprintln!("vectorscope trace {at_trace} vs opposite {empty}");
    assert!(at_trace > empty + 150, "vectorscope trace visible");
    let (ww, wh) = nle_render::ScopeKind::Waveform.size();
    let wimg = r.read_scope_image(nle_render::ScopeKind::Waveform).unwrap();
    let ly = wh - 1 - peak(Channel::Luma); // level 255 at the top
                                           // a 64 px frame fills every 6th of the 384 columns (col = x · 384 / 64)
    let lit = |y: u32| (0..W).map(|x| bright(px(&wimg, ww, x * scopes::WAVE_COLS / W, y))).sum::<u32>() / W;
    let (row, off) = (lit(ly), lit(20));
    eprintln!("waveform row {ly}: {row} vs row 20: {off}");
    assert!(row > off + 150, "waveform trace visible on the luma level");
    let himg = r.read_scope_image(nle_render::ScopeKind::Histogram).unwrap();
    assert!(himg.chunks(4).any(|p| p[0] > 100), "histogram draws red");
    let pimg = r.read_scope_image(nle_render::ScopeKind::Parade).unwrap();
    assert!(pimg.chunks(4).any(|p| p[0] > 100) && pimg.chunks(4).any(|p| p[2] > 100), "parade draws red and blue");

    // second frame in RGB-waveform mode: counts are cleared, not accumulated
    r.scope_settings.rgb_waveform = true;
    r.render(&comp, &mut src, RenderOptions::default());
    let c2 = r.read_scope_counts().unwrap();
    assert_eq!(c2[(scopes::MAX_OFF + Channel::Luma as u32) as usize], n);
    assert!(r.scope_view(nle_render::ScopeKind::Vectorscope).is_some());
    r.set_scopes_enabled(false);
    assert!(r.scope_view(nle_render::ScopeKind::Waveform).is_none());
}
