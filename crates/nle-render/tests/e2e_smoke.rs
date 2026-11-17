//! Release smoke test: the editor's whole pipeline on real files, driven
//! through the same library calls the UI makes.
//!
//!   import mp4 + music → trim → Zoom Punch → Whisper auto-captions
//!   (voice clip re-roled to Dialogue) → look-ahead ducking → GPU export
//!   → ffprobe validation + "is the music actually ducked" measurement.
//!
//! Needs: Windows (speech is synthesized with System.Speech), ffmpeg, a GPU,
//! the Whisper base model in %LOCALAPPDATA%\RustCutPro\models, and a build
//! with Whisper:
//!
//!   cargo test -p nle-render --features nle-media/whisper-vulkan --test e2e_smoke -- --ignored --nocapture

use nle_core::captions::{speech_clips, CaptionTrack};
use nle_core::zoom_punch::{apply_punch, PunchAnchor, DEFAULT_INTENSITY};
use nle_core::{audio_spans, AudioRole, Clip, Command, History, Project, Time};
use nle_media::audio::{mixdown, needs_duck_envelope, to_mono_16k};
use nle_media::transcribe::{self, TranscribeOptions};
use nle_media::{encoder_probe, ffmpeg_cli, probe};
use nle_render::export::{run_export, working_presets, AudioExport, RateControl};
use nle_render::{ExportSettings, GpuContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const SPEECH: &str = "This is a quick test of the montage editor. The captions should show every word.";
/// Speech starts this far into voice.mp4; the trim removes the first 500 ms.
const SPEECH_DELAY_MS: u64 = 1500;
const MUSIC_HZ: f64 = 55.0;

fn ffmpeg(args: &[&str]) {
    let st = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().expect("run ffmpeg");
    assert!(st.success(), "ffmpeg {args:?} failed");
}

fn synth_speech(wav: &Path) {
    let script = format!(
        "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
         $s.Rate = -1; $s.SetOutputToWaveFile('{}'); $s.Speak('{}'); $s.Dispose()",
        wav.display(),
        SPEECH
    );
    let st = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).status().expect("run powershell");
    assert!(st.success() && wav.is_file(), "speech synthesis failed");
}

/// Magnitude of `hz` in interleaved stereo `s` over [t0, t1) seconds (Goertzel, left channel).
fn tone_level(buf: &nle_media::AudioBuffer, hz: f64, t0: f64, t1: f64) -> f64 {
    let sr = buf.sample_rate as f64;
    let ch = buf.channels as usize;
    let (a, b) = ((t0 * sr) as usize, ((t1 * sr) as usize).min(buf.samples.len() / ch));
    let w = 2.0 * (2.0 * std::f64::consts::PI * hz / sr).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for i in a..b {
        let s0 = buf.samples[i * ch] as f64 + w * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    assert!(b > a, "empty window {t0:.2}..{t1:.2}s");
    let n = (b - a) as f64;
    (s1 * s1 + s2 * s2 - w * s1 * s2).sqrt() / n
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

#[test]
#[ignore = "release smoke test: needs Windows TTS, ffmpeg, a GPU and the Whisper base model"]
#[allow(clippy::assertions_on_constants)] // AVAILABLE is a build-feature flag; the assert explains a wrong build
fn e2e_import_trim_punch_captions_duck_export() {
    assert!(ffmpeg_cli::available(), "ffmpeg not found");
    assert!(transcribe::AVAILABLE, "build without Whisper: add --features nle-media/whisper-vulkan");
    let model = nle_media::paths::models_dir().join("ggml-base-q5_1.bin");
    assert!(model.is_file(), "missing {}", model.display());
    let gpu = GpuContext::new_headless().expect("GPU adapter");

    let dir = std::env::temp_dir().join(format!("rcp-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = |n: &str| -> PathBuf { dir.join(n) };

    // ---- media: 1280x720 gameplay-style clip with speech, and a music bed
    synth_speech(&p("speech.wav"));
    let delay = SPEECH_DELAY_MS.to_string();
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=60:duration=10",
        "-i",
        p("speech.wav").to_str().unwrap(),
        "-filter_complex",
        &format!("[1:a]adelay={delay}|{delay},apad,aformat=sample_rates=48000:channel_layouts=stereo[a]"),
        "-map",
        "0:v",
        "-map",
        "[a]",
        "-t",
        "10",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        p("voice.mp4").to_str().unwrap(),
    ]);
    ffmpeg(&["-f", "lavfi", "-i", &format!("sine=frequency={MUSIC_HZ}:sample_rate=48000:duration=10"), "-af", "volume=0.25", "-ac", "2", p("music_bed.wav").to_str().unwrap()]);

    // ---- import
    let mut project = Project::new("E2E Smoke");
    project.timeline.format.width = 1280;
    project.timeline.format.height = 720;
    let mut history = History::new(200);
    let voice = project.add_asset(probe::probe(&p("voice.mp4")).expect("probe voice"));
    let music = project.add_asset(probe::probe(&p("music_bed.wav")).expect("probe music"));
    let (voice, music) = (project.asset(voice).cloned().unwrap(), project.asset(music).cloned().unwrap());
    assert!(voice.has_video() && voice.has_audio());

    let vid = project.timeline.append(Clip::from_asset(&voice)).unwrap();
    // ---- trim: ripple the first 500 ms off the head (as the Q key does)
    project.timeline.ripple_trim_start_to(vid, Time::from_millis(500)).unwrap();
    let vclip = project.timeline.clip(vid).unwrap().clone();
    assert_eq!(vclip.duration, Time::from_millis(9500), "trimmed duration");

    // ---- music under the whole storyline; Music clips duck by default
    let mclip = Clip::from_asset_range(&music, Time::ZERO, Time::from_millis(9500));
    assert_eq!(mclip.audio.role, AudioRole::Music, "music_bed.wav guessed as Music");
    assert!(mclip.audio.ducking.enabled, "Music auto-ducks by default");
    let mid = project.timeline.connect(mclip, Time::ZERO, -1).unwrap();
    // the voice clip starts as General (gameplay audio): it must not trigger
    // ducking until the captions mark it as speech
    let mut v = project.timeline.clip(vid).unwrap().clone();
    v.audio.role = AudioRole::General;
    history.execute(&mut project.timeline, Command::UpdateClip { clip: Box::new(v) }).unwrap();

    // ---- Zoom Punch at 2.0 s (clip-local), centre anchor
    let mut v = project.timeline.clip(vid).unwrap().clone();
    let anchor = PunchAnchor::Center.resolve([0.0, 0.0], [1.0, 1.0]);
    assert!(apply_punch(&mut v, Time::from_millis(2000), DEFAULT_INTENSITY, false, anchor));
    history.execute(&mut project.timeline, Command::UpdateClip { clip: Box::new(v) }).unwrap();
    let v = project.timeline.clip(vid).unwrap();
    let peak = v.video.scale_x.at(Time::from_millis(2000) + Time::from_millis(100));
    let before = v.video.scale_x.at(Time::from_millis(500));
    eprintln!("zoom punch: scale {before:.3} → {peak:.3}");
    assert!(peak > before * 1.05, "punch raises scale ({before} → {peak})");

    // ---- audio sources (as the UI's media library holds them)
    let sources: HashMap<_, _> = project.assets.iter().map(|a| (a.id, Arc::new(nle_media::audio::decode::decode_audio(a).expect("decode audio")))).collect();

    // ---- Whisper auto-captions on the timeline mix (same steps as captions_ui)
    let extent = project.timeline.extent();
    let mix = mixdown(audio_spans(&project.timeline), &sources, Time::ZERO, extent);
    let mono16k = to_mono_16k(&mix);
    let mut opts = TranscribeOptions::new(model);
    opts.language = Some("en".into());
    let t0 = std::time::Instant::now();
    let tr = transcribe::transcribe(&mono16k, &opts, Arc::new(|_| {}), Arc::new(|s| eprintln!("whisper: {s}")), Arc::new(AtomicBool::new(false))).expect("transcribe");
    let text: String = tr.words.iter().map(|w| w.text.trim()).collect::<Vec<_>>().join(" ");
    eprintln!("whisper ({}, {:.1}s): {text}", tr.backend, t0.elapsed().as_secs_f32());
    // Known issue (2026-11-17, Arc A750, base-q5_1): on this exact mix the
    // Vulkan run drops "This is a" and "word." while the CPU run gets all 14
    // words. So this checks the pipeline, not the model's word accuracy;
    // RCP_E2E_DEBUG=1 prints the CPU transcript next to it.
    if std::env::var_os("RCP_E2E_DEBUG").is_some() {
        for w in &tr.words {
            eprintln!("  {:6.2}-{:6.2} {}", w.start.as_secs_f64(), w.end.as_secs_f64(), w.text);
        }
        let mut cpu = opts.clone();
        cpu.use_gpu = false;
        let ct = transcribe::transcribe(&mono16k, &cpu, Arc::new(|_| {}), Arc::new(|_| {}), Arc::new(AtomicBool::new(false))).unwrap();
        eprintln!("same mix on CPU: {}", ct.words.iter().map(|w| w.text.trim()).collect::<Vec<_>>().join(" "));
    }
    let lower = text.to_lowercase();
    for w in ["quick", "test", "montage", "editor", "captions", "should"] {
        assert!(lower.contains(w), "transcript is missing {w:?}: {text}");
    }
    assert!(tr.words.len() >= 8, "only {} words", tr.words.len());
    let first = tr.words.first().unwrap().start;
    // speech starts 1.5 s into the file (1.63 s measured), ≈1.1 s on the trimmed timeline
    assert!((first.as_secs_f64() - 1.0).abs() < 0.6, "first word at {:.2}s, expected ≈1.0s", first.as_secs_f64());

    let mut track = project.timeline.captions.clone();
    track.enabled = true;
    track.segments = CaptionTrack::phrases(&tr.words);
    let voiced = speech_clips(&project.timeline, &track);
    assert_eq!(voiced, vec![vid], "captions attribute the speech to the voice clip");
    let mut commands = vec![Command::SetCaptions { captions: Box::new(track) }];
    for id in &voiced {
        let mut c = project.timeline.clip(*id).unwrap().clone();
        c.audio.role = AudioRole::Dialogue;
        commands.push(Command::UpdateClip { clip: Box::new(c) });
    }
    history.execute(&mut project.timeline, Command::Batch { label: "Auto Captions", commands }).unwrap();
    eprintln!("captions: {} segments", project.timeline.captions.segments.len());
    assert!(project.timeline.captions.segments.len() >= 3);
    assert!(needs_duck_envelope(&audio_spans(&project.timeline)), "Dialogue + ducking Music needs the duck envelope");
    assert!(project.timeline.clip(mid).is_some());

    // ---- export (first working encoder: QSV on Arc, else x264)
    let presets = working_presets(encoder_probe::probe_blocking());
    let preset = *presets.first().expect("a working encoder");
    let out = p("e2e.mp4");
    let mut s = ExportSettings::new(&out, preset, &project);
    s.rate = RateControl::Quality(preset.default_quality());
    s.audio = AudioExport::Aac { kbps: 192 };
    let cancel = AtomicBool::new(false);
    let mut frames = 0;
    let t0 = std::time::Instant::now();
    run_export(&project, &s, gpu, &sources, &cancel, |p| frames = p.frame).expect("export");
    eprintln!("export via {}: {frames} frames in {:.1}s", preset.encoder(), t0.elapsed().as_secs_f32());

    // ---- ffprobe validation
    let info = probe::probe(&out).expect("ffprobe export");
    let vs = info.video.as_ref().expect("video stream");
    assert_eq!((vs.width, vs.height), (1280, 720));
    let dur = info.duration.as_secs_f64();
    assert!((dur - 9.5).abs() < 0.15, "duration {dur:.3}s, expected 9.5s");
    assert!(info.has_audio(), "audio stream");

    // ---- ducking really happened in the exported audio: the 55 Hz music
    // bed must drop while the voice speaks and recover after it.
    let exported = nle_media::audio::decode::decode_audio(&info).expect("decode exported audio");
    let last = tr.words.last().unwrap().end.as_secs_f64();
    let before = tone_level(&exported, MUSIC_HZ, 0.1, 0.7);
    let during = tone_level(&exported, MUSIC_HZ, first.as_secs_f64() + 0.5, (first.as_secs_f64() + 2.0).min(last));
    let after = tone_level(&exported, MUSIC_HZ, last.max(6.5) + 1.0, 9.3);
    eprintln!("music level: before {:.1} dB, during speech {:.1} dB, after {:.1} dB", db(before), db(during), db(after));
    assert!(db(before) - db(during) > 8.0, "music ducked under the voice");
    assert!((db(after) - db(before)).abs() < 3.0, "music recovered after the voice");

    eprintln!("E2E OK — {}", out.display());
    let _ = std::fs::remove_dir_all(&dir);
}
