//! GPU motion analysis: dense flow readback, tracking, stabilization warp,
//! Speed Warp interpolation and scene-cut scores (skipped without an adapter).

use nle_core::eval::{Composition, Layer, LayerSource, RenderNode};
use nle_core::motion::{self, Mat3, MotionModel, Stabilization, Track};
use nle_core::{AssetId, ClipId, FrameBlendMode, FrameRate, Time};
use nle_media::decode::VideoDecoder;
use nle_media::{HwBackend, MediaError, PixelFormat, VideoFrame};
use nle_render::motion::{analyze, detect, estimate, stabilization_from, track, Frame, TrackEnd};
use nle_render::renderer::FlowQuality;
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};

/// These tests allocate several full-resolution flow pyramids each; run in
/// parallel they can exhaust VRAM ("Not enough memory left" on an 8 GB Arc),
/// so they take turns.
static GPU_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn gpu() -> Option<(std::sync::MutexGuard<'static, ()>, GpuContext)> {
    let lock = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match GpuContext::new_headless() {
        Ok(g) => Some((lock, g)),
        Err(e) => {
            eprintln!("skipping GPU test: {e}");
            None
        }
    }
}

fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (y as u32).wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0xffff) as f32 / 65535.0
}

/// Smooth value noise (two octaves) — textured enough for flow everywhere.
fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let oct = |x: f32, y: f32, cell: f32, seed: u32| {
        let (gx, gy) = (x / cell, y / cell);
        let (x0, y0) = (gx.floor(), gy.floor());
        let (fx, fy) = (gx - x0, gy - y0);
        let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let (ix, iy) = (x0 as i32, y0 as i32);
        let a = hash(ix, iy, seed) + (hash(ix + 1, iy, seed) - hash(ix, iy, seed)) * sx;
        let b = hash(ix, iy + 1, seed) + (hash(ix + 1, iy + 1, seed) - hash(ix, iy + 1, seed)) * sx;
        a + (b - a) * sy
    };
    0.65 * oct(x, y, 14.0, seed) + 0.35 * oct(x, y, 5.0, seed ^ 0x55)
}

/// RGBA image of noise sampled through `m` (output px → world px).
fn render_world(w: u32, h: u32, m: &Mat3, seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let p = motion::apply(m, [x as f32 + 0.5, y as f32 + 0.5]);
            let v = (noise(p[0], p[1], seed) * 255.0) as u8;
            out.extend_from_slice(&[v, v, v, 255]);
        }
    }
    out
}

/// Pixel-space matrix → normalised coordinates of a `w × h` frame.
fn px_to_norm(m: &Mat3, w: f32, h: f32) -> Mat3 {
    let s = [w, 0.0, 0.0, 0.0, h, 0.0, 0.0, 0.0, 1.0];
    let si = [1.0 / w, 0.0, 0.0, 0.0, 1.0 / h, 0.0, 0.0, 0.0, 1.0];
    motion::mul(&si, &motion::mul(m, &s))
}

fn frame(t: f64, w: u32, h: u32, rgba: Vec<u8>) -> Frame {
    Frame { t, width: w, height: h, rgba }
}

#[test]
fn gpu_flow_field_measures_shift() {
    let Some((_lock, gpu)) = gpu() else { return };
    let mut r = Renderer::new(gpu, 64, 64);
    let (w, h) = (320u32, 180u32);
    let a = render_world(w, h, &motion::IDENTITY, 1);
    // content moves +6, +3 px: b(p) = a(p - d)
    let b = render_world(w, h, &motion::translation(-6.0, -3.0), 1);
    for q in [FlowQuality::standard(2), FlowQuality::high()] {
        let f = r.flow_field(&a, &b, w, h, q);
        assert_eq!((f.width, f.height), (w, h));
        let mut xs: Vec<f32> = f.data.iter().map(|v| v[0]).collect();
        let mut ys: Vec<f32> = f.data.iter().map(|v| v[1]).collect();
        xs.sort_by(f32::total_cmp);
        ys.sort_by(f32::total_cmp);
        let (mx, my) = (xs[xs.len() / 2], ys[ys.len() / 2]);
        eprintln!("{q:?}: median flow ({mx:.3}, {my:.3})");
        assert!((mx - 6.0).abs() < 0.3 && (my - 3.0).abs() < 0.3, "median flow ({mx}, {my})");
    }
}

#[test]
fn gpu_estimates_similarity_and_perspective() {
    let Some((_lock, gpu)) = gpu() else { return };
    let mut r = Renderer::new(gpu, 64, 64);
    let (w, h) = (384u32, 216u32);
    let (wf, hf) = (w as f32, h as f32);
    let a = render_world(w, h, &motion::IDENTITY, 7);
    // camera rolls 2° and zooms 3 % about the centre: content moves by S
    let s_px = motion::similarity_about([wf / 2.0, hf / 2.0], 1.03, 2f32.to_radians(), [3.0, -2.0]);
    let b = render_world(w, h, &motion::invert(&s_px).unwrap(), 7);
    let (m, ratio) = estimate(&mut r, &frame(0.0, w, h, a.clone()), &frame(0.1, w, h, b), None, MotionModel::Similarity).expect("similarity");
    let truth = px_to_norm(&s_px, wf, hf);
    eprintln!("similarity inliers {ratio:.2}\n  got {m:?}\n want {truth:?}");
    let (s, rot) = motion::local_similarity(&m, [0.5, 0.5], wf / hf);
    assert!((s - 1.03).abs() < 0.006 && (rot.to_degrees() - 2.0).abs() < 0.3, "scale {s} rot {}", rot.to_degrees());
    let c = motion::apply(&m, [0.5, 0.5]);
    let ct = motion::apply(&truth, [0.5, 0.5]);
    assert!((c[0] - ct[0]).abs() * wf < 0.6 && (c[1] - ct[1]).abs() * hf < 0.6, "{c:?} vs {ct:?}");

    // a planar (keystone) change
    let h_px: Mat3 = [1.0, 0.02, 2.0, 0.01, 1.01, -1.0, 4e-5, -2e-5, 1.0];
    let b = render_world(w, h, &motion::invert(&h_px).unwrap(), 7);
    let (m, _) = estimate(&mut r, &frame(0.0, w, h, a), &frame(0.1, w, h, b), None, MotionModel::Perspective).expect("homography");
    let truth = px_to_norm(&h_px, wf, hf);
    for p in [[0.1, 0.1], [0.9, 0.1], [0.9, 0.9], [0.1, 0.9], [0.5, 0.5]] {
        let (q, qt) = (motion::apply(&m, p), motion::apply(&truth, p));
        let err = ((q[0] - qt[0]) * wf).hypot((q[1] - qt[1]) * hf);
        assert!(err < 0.8, "corner {p:?}: {err} px");
    }
}

/// Synthetic footage: a textured background panning/shaking and a textured
/// square subject moving across it; a hard cut to other content at `cut`.
struct Scene {
    w: u32,
    h: u32,
    rate: FrameRate,
    cut: Option<i64>,
    shake: bool,
}

impl Scene {
    fn frame_rgba(&self, n: i64) -> Vec<u8> {
        let (w, h) = (self.w, self.h);
        let seed = if self.cut.is_some_and(|c| n >= c) { 99 } else { 3 };
        let cam = if self.shake { [n as f32 * 0.8 + if n % 2 == 0 { 2.5 } else { -2.5 }, if n % 3 == 0 { 1.5 } else { -1.0 }] } else { [n as f32 * 0.5, 0.0] };
        // subject: 48 px square moving 3 px/frame right, 1 px down
        let (sx, sy) = (60.0 + 3.0 * n as f32, 50.0 + n as f32);
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let v = if (sx..sx + 48.0).contains(&fx) && (sy..sy + 48.0).contains(&fy) {
                    0.2 + 0.8 * noise(fx - sx + 500.0, fy - sy + 500.0, 42)
                } else {
                    0.6 * noise(fx + cam[0], fy + cam[1], seed)
                };
                let b = (v * 255.0) as u8;
                out.extend_from_slice(&[b, b, b, 255]);
            }
        }
        out
    }
}

impl VideoDecoder for Scene {
    fn width(&self) -> u32 {
        self.w
    }
    fn height(&self) -> u32 {
        self.h
    }
    fn format(&self) -> PixelFormat {
        PixelFormat::Rgba8
    }
    fn frame_rate(&self) -> FrameRate {
        self.rate
    }
    fn backend(&self) -> HwBackend {
        HwBackend::Software
    }
    fn frame_at(&mut self, t: Time) -> Result<VideoFrame, MediaError> {
        let n = t.frames_floor(self.rate);
        Ok(VideoFrame::new(self.w, self.h, PixelFormat::Rgba8, self.frame_rgba(n), Time::from_frames(n, self.rate)))
    }
}

#[test]
fn gpu_region_tracker_follows_the_subject() {
    let Some((_lock, gpu)) = gpu() else { return };
    let mut r = Renderer::new(gpu, 64, 64);
    let mut dec = Scene { w: 320, h: 180, rate: FrameRate::FPS_30, cut: None, shake: false };
    let (w, h) = (320.0f32, 180.0f32);
    let t0 = 0.25 / 30.0;
    let region = [64.0 / w, 54.0 / h, 40.0 / w, 40.0 / h];
    let mut tr = Track::new("Subject", MotionModel::Translation, region, t0, w / h);
    let end = track(&mut r, &mut dec, &mut tr, t0, 20.0 / 30.0, |_, _| true);
    assert_eq!(end, TrackEnd::Done);
    assert!(tr.samples.len() >= 20, "{} samples", tr.samples.len());
    let start = tr.point_at(t0);
    let (_, t_end) = tr.range().unwrap();
    let p = tr.point_at(t_end);
    let n = (t_end * 30.0).floor() as f32;
    let (dx, dy) = ((p[0] - start[0]) * w, (p[1] - start[1]) * h);
    eprintln!("tracked {n} frames: moved ({dx:.2}, {dy:.2}) px, expected ({}, {})", 3.0 * n, n);
    assert!(n >= 19.0, "tracked to frame {n}");
    assert!((dx - 3.0 * n).abs() < 2.0 && (dy - n).abs() < 2.0);
}

#[test]
fn gpu_stabilizer_and_scene_cuts() {
    let Some((_lock, gpu)) = gpu() else { return };
    let mut r = Renderer::new(gpu, 64, 64);
    let mut dec = Scene { w: 320, h: 180, rate: FrameRate::FPS_30, cut: Some(24), shake: true };
    let a = analyze(&mut r, &mut dec, 0.0, 40.0 / 30.0, MotionModel::Translation, |_, _| true).expect("analysis");
    assert_eq!(a.deltas.len() + 1, a.times.len());
    // the camera moves world → frame by -cam: deltas measure the shake
    let d5 = a.deltas[5];
    let expect = -(0.8 + if 6 % 2 == 0 { 2.5 } else { -2.5 } - if 5 % 2 == 0 { 2.5 } else { -2.5 });
    eprintln!("delta 5→6: {:.2} px (expected {expect})", d5[2] * 320.0);
    assert!((d5[2] * 320.0 - expect).abs() < 0.6);
    // the hard cut at frame 24 is the only cut
    let cuts = detect(&a, 0.5);
    eprintln!("scores around the cut: {:?}", &a.scores[20..26]);
    assert_eq!(cuts.len(), 1, "{cuts:?}");
    let at_cut = a.scores[23].1;
    let others = a.scores.iter().enumerate().filter(|(i, _)| *i != 23).map(|(_, s)| s.1).fold(0.0f32, f32::max);
    assert!(at_cut > 0.2 && at_cut > 8.0 * others, "cut {at_cut} vs others {others}");
    assert!((cuts[0] * 30.0 - 24.25).abs() < 0.6, "{cuts:?}");
    // stabilize the pre-cut part: alternating ±2.5 px shake is removed
    let mut part = a.clone();
    part.times.truncate(22);
    part.deltas.truncate(21);
    let st = stabilization_from(&part, &Stabilization { model: MotionModel::Translation, smoothing: 0.2, ..Default::default() });
    assert!(st.crop_scale > 1.0 && st.crop_scale < 1.15, "crop {}", st.crop_scale);
    // world position of the output centre (frame-0 pixels) must move smoothly
    let world = |i: usize| {
        let m = st.correction(part.times[i]).unwrap();
        let src = motion::apply(&m, [0.5, 0.5]);
        let cam = i as f32 * 0.8 + if i.is_multiple_of(2) { 2.5 } else { -2.5 };
        src[0] * 320.0 + cam
    };
    for i in 4..17 {
        let accel = world(i - 1) - 2.0 * world(i) + world(i + 1);
        assert!(accel.abs() < 0.8, "frame {i}: {accel}");
    }
}

/// NV12 frames with a bright square moving 8 px per frame (Speed Warp).
struct Moving;
const MW: u32 = 128;
const MH: u32 = 64;
impl FrameSource for Moving {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let x0 = 16 + index * 8;
        let mut data = vec![16u8; (MW * MH) as usize];
        for y in 16..48u32 {
            for x in 0..MW as i64 {
                if x >= x0 && x < x0 + 24 {
                    data[(y * MW) as usize + x as usize] = 235;
                }
            }
        }
        data.extend(std::iter::repeat_n(128u8, (MW * MH / 2) as usize));
        Some(VideoFrame::new(MW, MH, PixelFormat::Nv12, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

fn centre_x(nv12: &[u8], y: u32) -> f32 {
    let row = &nv12[(y * MW) as usize..((y + 1) * MW) as usize];
    let (mut s, mut n) = (0.0, 0.0);
    for (x, &v) in row.iter().enumerate() {
        let w = (v as f32 - 16.0).max(0.0);
        s += x as f32 * w;
        n += w;
    }
    s / f32::max(n, 1e-3)
}

#[test]
fn gpu_speed_warp_and_stabilize_warp() {
    let Some((_lock, gpu)) = gpu() else { return };
    let mut r = Renderer::new(gpu, MW, MH);
    let mut src = Moving;
    let f = |n: i64| Time::from_frames(n, FrameRate::FPS_30);
    let layer = |t: Time, blend| Layer::new(ClipId(1), 0, LayerSource::Media { asset: AssetId(5), source_time: t, speed: 1.0, blend });
    let half = Time(f(2).0 + (f(3).0 - f(2).0) / 2);
    let c2 = centre_x(&r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(layer(f(2), FrameBlendMode::FloorFrame))]), &mut src, RenderOptions::default()), 32);
    let mid = r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(layer(half, FrameBlendMode::SpeedWarp))]), &mut src, RenderOptions::default());
    let cm = centre_x(&mid, 32);
    eprintln!("speed warp: frame 2 at {c2:.2}, in-between at {cm:.2}");
    assert!((cm - (c2 + 4.0)).abs() < 1.5);
    assert!(mid[(32 * MW) as usize..((33) * MW) as usize].iter().copied().max().unwrap() > 180, "solid, not a double image");

    // stabilize warp: sampling 8 px to the right moves the content 8 px left
    let mut l = layer(f(2), FrameBlendMode::FloorFrame);
    l.video.stabilize = Some(motion::translation(8.0 / MW as f32, 0.0));
    let out = r.render_nv12(&Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]), &mut src, RenderOptions::default());
    let cs = centre_x(&out, 32);
    eprintln!("stabilize warp: {c2:.2} → {cs:.2}");
    assert!((cs - (c2 - 8.0)).abs() < 0.75);
}
