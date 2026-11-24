//! Export delivers what the preview shows, or says that it could not.
//! Pictures and sound are read back from real exports, never from counters.
//! Skips itself without FFmpeg or a GPU.

use nle_core::eval::{self, Composition, Layer, LayerSource, RenderNode};
use nle_core::{AssetId, Clip, ClipId, Command, FrameBlendMode, FrameRate, MediaAsset, Project, Time, Transition, TransitionKind};
use nle_media::{ffmpeg_cli, probe, FrameServer, VideoFrame};
use nle_render::export::{run_export, AudioExport, ExportPreset, ExportSettings};
use nle_render::{FrameKey, FrameSource, GpuContext, RenderOptions, Renderer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const W: u32 = 320;
const H: u32 = 180;

/// A GPU and a directory of its own, removed on drop.
struct Rig {
    gpu: GpuContext,
    dir: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Option<Rig> {
        if !ffmpeg_cli::available() {
            eprintln!("skipping: ffmpeg not found");
            return None;
        }
        let gpu = GpuContext::new_headless().map_err(|e| eprintln!("skipping: {e}")).ok()?;
        let dir = std::env::temp_dir().join(format!("rcp-align-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Some(Rig { gpu, dir })
    }

    fn media(&self, name: &str, picture: &str, secs: &str, codec: &[&str]) -> MediaAsset {
        let path = self.dir.join(name);
        let video = format!("{picture}:duration={secs}");
        let sound = format!("sine=frequency=440:sample_rate=48000:duration={secs}");
        let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", &video, "-f", "lavfi", "-i", &sound])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-colorspace", "bt709", "-color_range", "tv", "-c:a", "aac"])
            .args(codec)
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "could not create {name}");
        probe::probe(&path).unwrap()
    }

    fn settings(&self, name: &str, preset: ExportPreset, project: &Project) -> ExportSettings {
        let mut s = ExportSettings::new(self.dir.join(name), preset, project);
        s.audio = AudioExport::None;
        s
    }

    /// Export with nothing decoded beforehand, as for media not yet analysed.
    fn export(&self, project: &Project, settings: &ExportSettings) -> Result<(), String> {
        run_export(project, settings, self.gpu.clone(), &HashMap::new(), &AtomicBool::new(false), |_| {}).map_err(|e| e.to_string())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn project(rate: FrameRate) -> Project {
    let mut p = Project::new("Alignment");
    p.timeline.format.width = W;
    p.timeline.format.height = H;
    p.timeline.format.rate = rate;
    p
}

/// Every picture of a file or numbered sequence, as RGBA.
fn pictures(path: &Path) -> Vec<Vec<u8>> {
    let out = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-fps_mode", "passthrough", "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks((W * H * 4) as usize).map(<[u8]>::to_vec).collect()
}

fn difference(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum::<u64>() as f64 / a.len() as f64
}

/// Records which source frame the renderer asks for.
struct Asked(FrameRate, Vec<i64>);

impl FrameSource for Asked {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        self.0
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        self.1.push(index);
        None
    }
}

#[test]
fn the_renderer_asks_for_the_frame_that_starts_then() {
    let Some(rig) = Rig::new("index") else { return };
    let mut renderer = Renderer::new(rig.gpu.clone(), 16, 16);
    // frame starts whose seconds, times the rate, land a hair under the integer
    for (rate, frames) in [(FrameRate::FPS_30, [123, 245, 490]), (FrameRate::new(30000, 1001), [15, 25, 30]), (FrameRate::new(25, 1), [29, 57, 113])] {
        for n in frames {
            let source = LayerSource::Media { asset: AssetId(1), source_time: Time::from_frames(n, rate), speed: 1.0, blend: FrameBlendMode::FloorFrame };
            let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(Layer::new(ClipId(1), 0, source))]);
            let mut asked = Asked(rate, Vec::new());
            renderer.render(&comp, &mut asked, RenderOptions::default());
            assert_eq!(asked.1.first(), Some(&n), "frame {n} at {rate:?}");
        }
    }
}

#[test]
fn every_exported_frame_is_the_source_frame_of_that_time() {
    let Some(rig) = Rig::new("identity") else { return };
    let rate = FrameRate::new(30000, 1001);
    let asset = rig.media("in.mp4", "testsrc2=size=320x180:rate=30000/1001", "2.2", &[]);
    let source = pictures(&asset.path);
    let mut project = project(rate);
    project.timeline.append(Clip::from_asset_range(&asset, Time::ZERO, Time::from_frames(62, rate))).unwrap();
    project.add_asset(asset);

    let settings = rig.settings("out.png", ExportPreset::PngSequence, &project);
    rig.export(&project, &settings).unwrap();
    let out = pictures(&settings.output_target());
    assert_eq!(out.len(), 62);
    let nearest = |p: &[u8]| (0..source.len()).min_by(|&a, &b| difference(p, &source[a]).total_cmp(&difference(p, &source[b]))).unwrap();
    let wrong: Vec<_> = out.iter().enumerate().map(|(n, p)| (n, nearest(p))).filter(|(n, got)| n != got).collect();
    assert!(wrong.is_empty(), "(frame, picture exported): {wrong:?}");
}

/// The preview's source: frames are uploaded beforehand, never pulled.
struct Uploaded<'a>(&'a Project);

impl FrameSource for Uploaded<'_> {
    fn frame_rate(&self, asset: AssetId) -> FrameRate {
        self.0.asset(asset).map(|a| a.frame_rate()).unwrap_or_default()
    }
    fn frame(&mut self, _: AssetId, _: i64) -> Option<VideoFrame> {
        None
    }
    fn yuv_matrix(&self, asset: AssetId) -> Option<nle_core::YuvMatrix> {
        Some(self.0.asset(asset)?.video.as_ref()?.yuv_matrix())
    }
}

#[test]
fn preview_and_export_show_the_same_picture() {
    let Some(rig) = Rig::new("parity") else { return };
    let mut project = project(FrameRate::FPS_30);
    let first = rig.media("a.mp4", "testsrc2=size=320x180:rate=30", "3", &[]);
    let second = rig.media("b.mp4", "smptebars=size=320x180:rate=30", "3", &[]);

    let mut a = Clip::from_asset_range(&first, Time::from_secs_f64(0.5), Time::from_secs_f64(1.0));
    a.video.crop_left = 0.15.into();
    a.video.crop_top = 0.1.into();
    a.video.scale_x = 0.8.into();
    a.video.scale_y = 0.8.into();
    a.video.rotation = 12.0.into();
    a.video.position_x = 0.05.into();
    a.video.flip_horizontal = true;
    a.color.brightness = 0.1.into();
    a.color.saturation = 1.4.into();
    let mut b = Clip::from_asset_range(&second, Time::from_secs_f64(0.5), Time::from_secs_f64(1.0));
    b.video.opacity = 0.7.into();
    let (a_id, b_id) = (a.id, b.id);
    let timeline = &mut project.timeline;
    timeline.append(a).unwrap();
    timeline.append(b).unwrap();
    let dissolve = Transition::new(TransitionKind::CrossDissolve, Time::from_secs_f64(0.4));
    Command::SetTransition { id: a_id, at_end: true, transition: Some(dissolve) }.apply(timeline).unwrap();
    Command::SetSpeed { id: b_id, speed: 0.5 }.apply(timeline).unwrap();
    project.add_asset(first);
    project.add_asset(second);

    let settings = rig.settings("out.png", ExportPreset::PngSequence, &project);
    rig.export(&project, &settings).unwrap();
    let exported = pictures(&settings.output_target());
    assert_eq!(exported.len(), 90);

    let server = FrameServer::new(64, None);
    let mut renderer = Renderer::new(rig.gpu.clone(), W, H);
    for n in [0, 7, 15, 26, 30, 33, 47, 61, 89] {
        let t = Time::from_frames(n, FrameRate::FPS_30);
        let comp = eval::evaluate(&project.timeline, t);
        let mut wanted: Vec<(AssetId, Time, i64)> = Vec::new();
        for (id, st) in comp.media_requests() {
            wanted.push((id, st, st.frames_floor(project.asset(id).unwrap().frame_rate())));
        }
        for (id, st) in comp.next_frame_requests() {
            let rate = project.asset(id).unwrap().frame_rate();
            let next = st.frames_floor(rate) + 1;
            wanted.push((id, Time::from_frames(next, rate), next));
        }
        // one at a time: a worker holds a single request
        for (id, st, index) in wanted {
            let key = FrameKey { asset: id, index };
            let until = Instant::now() + Duration::from_secs(20);
            while !renderer.is_cached(key) {
                assert!(Instant::now() < until, "preview never decoded {index}");
                server.request(project.asset(id).unwrap(), st, false, false);
                std::thread::sleep(Duration::from_millis(15));
                for d in server.drain() {
                    renderer.upload_frame(FrameKey { asset: d.asset, index: d.index }, &d.frame);
                }
            }
        }
        renderer.render(&comp, &mut Uploaded(&project), RenderOptions::default());
        let off = difference(&renderer.read_display(), &exported[n as usize]);
        assert!(off < 1.5, "frame {n}: {off:.2} levels apart");
    }
}

#[test]
fn media_not_yet_decoded_is_still_heard() {
    let Some(rig) = Rig::new("sound") else { return };
    let asset = rig.media("in.mp4", "testsrc2=size=320x180:rate=30", "1", &[]);
    let mut project = project(FrameRate::FPS_30);
    project.timeline.append(Clip::from_asset(&asset)).unwrap();
    project.add_asset(asset);

    let mut settings = rig.settings("out.mp4", ExportPreset::H264Software, &project);
    settings.audio = AudioExport::Aac { kbps: 128 };
    rig.export(&project, &settings).unwrap();
    let sound = nle_media::audio::decode::decode_audio(&probe::probe(&settings.path).unwrap()).unwrap();
    let peak = sound.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.02, "silence: peak {peak}");
}

#[test]
fn missing_media_fails_the_export() {
    let Some(rig) = Rig::new("missing") else { return };
    for audio in [AudioExport::None, AudioExport::Aac { kbps: 128 }] {
        let asset = rig.media("in.mp4", "testsrc2=size=320x180:rate=30", "1", &[]);
        let path = asset.path.clone();
        let mut project = project(FrameRate::FPS_30);
        let mut clip = Clip::from_asset(&asset);
        clip.av.audio = audio != AudioExport::None;
        project.timeline.append(clip).unwrap();
        project.add_asset(asset);
        std::fs::remove_file(&path).unwrap();

        let mut settings = rig.settings("out.mp4", ExportPreset::H264Software, &project);
        settings.audio = audio;
        let result = rig.export(&project, &settings);
        assert!(result.as_ref().is_err_and(|e| e.contains("in.mp4")), "{audio:?}: {result:?}");
    }
}

#[test]
fn software_h265_exports_at_every_standard_rate() {
    let Some(rig) = Rig::new("x265") else { return };
    let preset = ExportPreset::from_encoder("libx265").expect("no software H.265 preset");
    let asset = rig.media("in.mp4", "testsrc2=size=320x180:rate=30", "1", &[]);
    let mut project = project(FrameRate::FPS_30);
    project.timeline.append(Clip::from_asset(&asset)).unwrap();
    project.add_asset(asset);

    for (fps, ten_bit) in [(24, false), (30, true), (60, false)] {
        let mut settings = rig.settings(&format!("out{fps}.mp4"), preset, &project);
        settings.frame_rate = FrameRate::new(fps, 1);
        settings.ten_bit = ten_bit;
        rig.export(&project, &settings).unwrap();
        let out = probe::probe(&settings.path).unwrap();
        let video = out.video.as_ref().unwrap();
        assert_eq!((video.codec.to_string().to_lowercase(), video.frame_rate), ("hevc".to_string(), FrameRate::new(fps, 1)));
        assert!((out.duration.as_secs_f64() - 1.0).abs() < 0.1, "{fps}: {:?}", out.duration);
    }
}

#[test]
fn a_cancelled_export_keeps_the_file_it_would_replace() {
    let Some(rig) = Rig::new("cancel") else { return };
    let asset = rig.media("in.mp4", "testsrc2=size=320x180:rate=30", "1", &[]);
    let mut project = project(FrameRate::FPS_30);
    project.timeline.append(Clip::from_asset(&asset)).unwrap();
    project.add_asset(asset);

    let settings = rig.settings("out.mp4", ExportPreset::H264Software, &project);
    std::fs::write(&settings.path, b"the previous export").unwrap();
    let cancel = AtomicBool::new(false);
    let result = run_export(&project, &settings, rig.gpu.clone(), &HashMap::new(), &cancel, |p| cancel.store(p.frame >= 3, std::sync::atomic::Ordering::Relaxed));
    assert!(matches!(result, Err(nle_render::export::ExportError::Cancelled)), "{result:?}");
    assert!(std::fs::read(&settings.path).unwrap() == b"the previous export", "the previous export was overwritten");
    assert_eq!(std::fs::read_dir(&rig.dir).unwrap().count(), 2, "a partial file was left behind");
}
