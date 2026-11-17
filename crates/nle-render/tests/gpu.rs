//! Shader validation (always) and GPU execution tests (when an adapter is
//! available — skipped gracefully on headless CI).

use nle_core::eval::{Composition, Layer, LayerSource, RenderNode, ResolvedColor, ResolvedTransition};
use nle_core::{AssetId, ClipId, Direction, Effect, EffectKind, FrameBlendMode, FrameRate, Time, TitleAnim, TitleParams, TransitionKind};
use nle_media::{PixelFormat, VideoFrame};
use nle_render::pipelines::ALL_SHADERS;
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
    let out = r.render_nv12(&comp, &mut src, RenderOptions { wipe: Some(0.5), color: true });
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
        data.extend(std::iter::repeat(128u8).take((MW * MH / 2) as usize));
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
            EffectKind::CameraShake | EffectKind::DirectionalBlur | EffectKind::ZoomBlur | EffectKind::MotionBlur | EffectKind::RgbSplitGlitch => {
                assert!((y - base_y).abs() < 4.0, "{kind:?} keeps a flat frame nearly flat: {y}")
            }
            EffectKind::Glow | EffectKind::Halation | EffectKind::ImpactFlash => assert!(y >= base_y - 2.0, "{kind:?} only adds light: {y}"),
            EffectKind::FilmGrain => assert!((y - base_y).abs() < 8.0, "grain is zero-mean: {y}"),
            EffectKind::NeonOutline => assert!(y <= base_y + 1.0, "flat frame has no edges: {y}"),
            EffectKind::ChromaKey => {}
        }
    }

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
