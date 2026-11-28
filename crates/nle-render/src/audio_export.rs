//! Audio-only delivery: the full mix and / or one stem per mixer bus, as
//! WAV (24-bit or 32-bit float), FLAC or AAC. Stems are tapped after each
//! bus's fader, before the master chain, so the master's processing and
//! limiters apply only to the mix file: the mix holds a true-peak ceiling
//! (AAC −1.5 dBTP, PCM −0.3 dBTP) unless it goes out as a raw / unlimited
//! 32-bit float master.

use crate::export::{export_audio, master_mix, partial_path, prepared_mixer, render_range, write_wav_f32, ExportError, MasterReport};
use nle_core::{AssetId, AudioRole, BusId, Project, Time};
use nle_media::audio::StemTap;
use nle_media::{ffmpeg_cli, AudioBuffer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    Wav24,
    WavFloat,
    Flac,
    Aac { kbps: u32 },
}

impl AudioFormat {
    pub const ALL: [AudioFormat; 4] = [AudioFormat::Wav24, AudioFormat::WavFloat, AudioFormat::Flac, AudioFormat::Aac { kbps: 320 }];

    pub fn label(self) -> &'static str {
        match self {
            AudioFormat::Wav24 => "WAV 24-bit",
            AudioFormat::WavFloat => "WAV 32-bit float",
            AudioFormat::Flac => "FLAC 24-bit",
            AudioFormat::Aac { .. } => "AAC (.m4a)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Wav24 | AudioFormat::WavFloat => "wav",
            AudioFormat::Flac => "flac",
            AudioFormat::Aac { .. } => "m4a",
        }
    }

    /// FFmpeg output codec arguments; `None` = written directly (no FFmpeg).
    fn codec_args(self) -> Option<Vec<String>> {
        let v = |a: &[&str]| Some(a.iter().map(|s| s.to_string()).collect());
        match self {
            AudioFormat::WavFloat => None,
            AudioFormat::Wav24 => v(&["-c:a", "pcm_s24le"]),
            AudioFormat::Flac => v(&["-c:a", "flac", "-sample_fmt", "s32", "-bits_per_raw_sample", "24"]),
            AudioFormat::Aac { kbps } => Some(vec!["-c:a".into(), "aac".into(), "-b:a".into(), format!("{kbps}k")]),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AudioExportSettings {
    /// The mix file. Stems go next to it as `<name> - <bus>.<ext>`.
    pub path: PathBuf,
    pub format: AudioFormat,
    pub start: Time,
    pub end: Time,
    /// Write the full mix.
    pub mix: bool,
    pub stems: Vec<StemTap>,
    /// Normalize the mix (not the stems) to this integrated loudness.
    pub loudness_target: Option<f32>,
    /// Raw / unlimited master: no master-bus or true-peak limiting on the
    /// mix, so overs survive. Only for [`AudioFormat::WavFloat`], the one
    /// format that can carry samples above full scale.
    pub raw_master: bool,
}

/// What [`run_audio_export`] wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioExportOutput {
    pub files: Vec<PathBuf>,
    /// Mastering of the mix; `None` when only stems were written.
    pub master: Option<MasterReport>,
}

impl AudioExportSettings {
    pub fn new(path: impl Into<PathBuf>, project: &Project) -> Self {
        AudioExportSettings {
            path: path.into(),
            format: AudioFormat::Wav24,
            start: Time::ZERO,
            end: project.timeline.extent(),
            mix: true,
            stems: Vec::new(),
            loudness_target: None,
            raw_master: false,
        }
    }

    /// Whether the mix goes out unlimited (raw master on a float WAV).
    pub fn unlimited(&self) -> bool {
        self.raw_master && self.format == AudioFormat::WavFloat
    }

    /// Every file this export writes: (`None` = the mix, or the stem's tap, path).
    pub fn outputs(&self, project: &Project) -> Vec<(Option<StemTap>, PathBuf)> {
        let ext = self.format.extension();
        let mix = self.path.with_extension(ext);
        let mut out = Vec::new();
        if self.mix {
            out.push((None, mix.clone()));
        }
        for tap in &self.stems {
            out.push((Some(*tap), stem_path(&mix, &stem_label(project, *tap), ext)));
        }
        out
    }
}

/// The bus name used in a stem's file name.
pub fn stem_label(project: &Project, tap: StemTap) -> String {
    match tap {
        StemTap::Role(role) => BusId::Role(role).label().to_string(),
        StemTap::User(id) => project.timeline.mixer.user_bus(id).map_or_else(|| format!("Bus {id}"), |(_, b)| b.name.clone()),
    }
}

/// Every stem a project can export: the role busses, then its user busses.
pub fn all_stems(project: &Project) -> Vec<StemTap> {
    AudioRole::ALL.into_iter().map(StemTap::Role).chain(project.timeline.mixer.user.iter().map(|b| StemTap::User(b.id))).collect()
}

/// `mix.ext` → `mix - Label.ext` (characters Windows forbids replaced).
pub fn stem_path(mix: &Path, label: &str, ext: &str) -> PathBuf {
    let stem = mix.file_stem().unwrap_or_default().to_string_lossy();
    let clean: String = label.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    mix.with_file_name(format!("{stem} - {}.{ext}", clean.trim()))
}

/// Run an audio-only export (blocking — call from a background thread).
/// `progress` gets 0..1. Each file appears only once it is complete.
pub fn run_audio_export(
    project: &Project,
    settings: &AudioExportSettings,
    audio_sources: &HashMap<AssetId, Arc<AudioBuffer>>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f32),
) -> Result<AudioExportOutput, ExportError> {
    let outputs = settings.outputs(project);
    if outputs.is_empty() {
        return Err(ExportError::Encoder("nothing to export: choose the mix or at least one stem".into()));
    }
    let codec = settings.format.codec_args();
    if codec.is_some() && !ffmpeg_cli::available() {
        return Err(ExportError::NoFfmpeg);
    }
    if let Some(dir) = settings.path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));

    let sources = export_audio(project, audio_sources, settings.start, settings.end, cancel)?;
    let mut mixer = prepared_mixer(project, &sources);
    let n = outputs.len() as f32;
    let mut written = AudioExportOutput::default();
    mixer.set_master_limiter(!settings.unlimited());
    for (k, (tap, path)) in outputs.iter().enumerate() {
        mixer.reset_state();
        mixer.set_stem(*tap);
        let base = k as f32 / n;
        let pcm = render_range(&mut mixer, settings.start, settings.end, cancel, &mut |f| progress(base + f / n));
        let Some(mut pcm) = pcm else { return Err(ExportError::Cancelled) };
        if tap.is_none() {
            let lossy = matches!(settings.format, AudioFormat::Aac { .. });
            written.master = Some(master_mix(&mut pcm, settings.loudness_target, lossy, !settings.unlimited(), mixer.master_reduction_db()));
        }
        let part = partial_path(path, &id);
        let r = encode(&pcm, &part, codec.as_deref(), &id).and_then(|()| {
            std::fs::rename(&part, path).map_err(|e| std::io::Error::new(e.kind(), format!("cannot replace {} (open in another program?): {e}", path.display())).into())
        });
        if let Err(e) = r {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        written.files.push(path.clone());
    }
    progress(1.0);
    Ok(written)
}

fn encode(pcm: &[f32], out: &Path, codec: Option<&[String]>, id: &str) -> Result<(), ExportError> {
    let Some(codec) = codec else {
        write_wav_f32(out, 48_000, pcm)?;
        return Ok(());
    };
    let wav = std::env::temp_dir().join(format!("rcp-audio-export-{id}.wav"));
    write_wav_f32(&wav, 48_000, pcm)?;
    let mut args: Vec<String> = ["-y", "-hide_banner", "-loglevel", "error", "-i"].iter().map(|s| s.to_string()).collect();
    args.push(wav.to_string_lossy().into_owned());
    args.extend(codec.iter().cloned());
    args.push(out.to_string_lossy().into_owned());
    let res = ffmpeg_cli::output_confined(ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(&args));
    let _ = std::fs::remove_file(&wav);
    let res = res?;
    if !res.status.success() {
        let stderr = String::from_utf8_lossy(&res.stderr);
        let msg = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error").trim().to_string();
        return Err(ExportError::Encoder(msg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::{AudioStreamInfo, Clip, MediaAsset};

    fn tone(secs: f64, hz: f32, amp: f32) -> Arc<AudioBuffer> {
        let n = (48_000.0 * secs) as usize;
        Arc::new(AudioBuffer::new(
            48_000,
            2,
            (0..n)
                .flat_map(|i| {
                    let v = (std::f32::consts::TAU * hz * i as f32 / 48_000.0).sin() * amp;
                    [v, v]
                })
                .collect(),
        ))
    }

    fn read_f32_wav(p: &Path) -> Vec<f32> {
        let b = std::fs::read(p).unwrap();
        b[44..].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
    }

    #[test]
    fn mix_and_stems_are_written_and_stems_hold_only_their_bus() {
        let mut project = Project::new("stems");
        let mut voice = MediaAsset::new("voice", "/v.wav", Time::from_secs(1));
        voice.audio = Some(AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut music = MediaAsset::new("music", "/m.wav", Time::from_secs(1));
        music.audio = voice.audio.clone();
        let mut v = Clip::from_asset(&voice);
        v.audio.role = AudioRole::Dialogue;
        project.timeline.append(v).unwrap();
        let mut m = Clip::from_asset(&music);
        m.audio.role = AudioRole::Music;
        m.audio.ducking.enabled = false;
        project.timeline.connect(m, Time::ZERO, -1).unwrap();
        let bus = project.timeline.mixer.add_user_bus("Sub/1", AudioRole::Music).unwrap();
        let sources: HashMap<AssetId, Arc<AudioBuffer>> = [(voice.id, tone(1.0, 440.0, 0.3)), (music.id, tone(1.0, 2_000.0, 0.3))].into_iter().collect();
        project.assets.push(voice);
        project.assets.push(music);

        let dir = std::env::temp_dir().join(format!("rcp-stems-{}", std::process::id()));
        let mut s = AudioExportSettings::new(dir.join("show.wav"), &project);
        s.format = AudioFormat::WavFloat;
        s.stems = vec![StemTap::Role(AudioRole::Dialogue), StemTap::Role(AudioRole::Music), StemTap::User(bus)];
        let files = run_audio_export(&project, &s, &sources, &AtomicBool::new(false), |_| {}).unwrap().files;
        let names: Vec<String> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["show.wav", "show - Dialogue.wav", "show - Music.wav", "show - Sub_1.wav"]);
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt();
        let mix = read_f32_wav(&files[0]);
        let dia = read_f32_wav(&files[1]);
        let mus = read_f32_wav(&files[2]);
        let sub = read_f32_wav(&files[3]);
        assert_eq!(mix.len(), 48_000 * 2);
        assert!(rms(&dia[4_000..]) > 0.15 && rms(&mus[4_000..]) > 0.15);
        assert!(rms(&sub[4_000..]) < 1e-6, "nothing is routed to the user bus");
        // the stems add up to the mix (master at unity; the limiter idles at this level)
        let diff: Vec<f32> = (4_000..mix.len()).map(|i| mix[i] - dia[i] - mus[i]).collect();
        assert!(rms(&diff) < 1e-3, "stems sum to the mix: {}", rms(&diff));
        let _ = std::fs::remove_dir_all(&dir);

        // FLAC through FFmpeg when it is installed
        if ffmpeg_cli::available() {
            let mut s = AudioExportSettings::new(dir.join("show"), &project);
            s.format = AudioFormat::Flac;
            let files = run_audio_export(&project, &s, &sources, &AtomicBool::new(false), |_| {}).unwrap().files;
            assert!(files[0].extension().unwrap() == "flac" && std::fs::metadata(&files[0]).unwrap().len() > 10_000);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn true_peak_db(x: &[f32]) -> f32 {
        let mut m = nle_media::audio::meter::LoudnessMeter::new(48_000);
        m.push(x);
        m.true_peak_db().unwrap_or(f32::NEG_INFINITY)
    }

    #[test]
    fn a_hot_mix_holds_the_pcm_ceiling_unless_exported_raw() {
        let mut project = Project::new("hot");
        let mut a = MediaAsset::new("hot", "/h.wav", Time::from_secs(1));
        a.audio = Some(AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut c = Clip::from_asset(&a);
        c.audio.role = AudioRole::Effects;
        project.timeline.append(c).unwrap();
        // +6 dBFS: a 1 kHz tone at twice full scale
        let sources: HashMap<AssetId, Arc<AudioBuffer>> = [(a.id, tone(1.0, 1_000.0, 2.0))].into_iter().collect();
        project.assets.push(a);
        let dir = std::env::temp_dir().join(format!("rcp-hot-{}", std::process::id()));
        let mut s = AudioExportSettings::new(dir.join("hot.wav"), &project);
        s.format = AudioFormat::WavFloat;

        let out = run_audio_export(&project, &s, &sources, &AtomicBool::new(false), |_| {}).unwrap();
        let tp = true_peak_db(&read_f32_wav(&out.files[0]));
        assert!(tp <= -0.3 + 0.05, "limited mix true peak {tp:.2} dBTP");
        let m = out.master.unwrap();
        assert_eq!(m.ceiling_dbtp, Some(-0.3));
        assert!(m.is_hot(), "6 dB over is a hot mix: {m:?}");

        s.raw_master = true;
        let out = run_audio_export(&project, &s, &sources, &AtomicBool::new(false), |_| {}).unwrap();
        let raw = read_f32_wav(&out.files[0]);
        let peak = raw.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 1.9, "raw master keeps the overs: {peak}");
        assert_eq!(out.master.unwrap().ceiling_dbtp, None);

        // raw only applies to float WAV
        s.format = AudioFormat::Wav24;
        assert!(!s.unlimited());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
