//! Voiceover capture: an input device (cpal) streamed to a 32-bit float WAV
//! on a writer thread, so long takes never sit in memory.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// A finished recording.
#[derive(Clone, Debug)]
pub struct Take {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

/// Streams interleaved f32 samples into a float WAV, patching the sizes on
/// [`WavWriter::finish`].
pub struct WavWriter {
    file: std::io::BufWriter<std::fs::File>,
    rate: u32,
    channels: u16,
    samples: u64,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32, channels: u16) -> std::io::Result<Self> {
        let mut w = WavWriter { file: std::io::BufWriter::new(std::fs::File::create(path)?), rate, channels, samples: 0 };
        w.header()?;
        Ok(w)
    }

    fn header(&mut self) -> std::io::Result<()> {
        let data = (self.samples * 4).min(u32::MAX as u64 - 36) as u32;
        let ch = self.channels.max(1);
        let f = &mut self.file;
        f.write_all(b"RIFF")?;
        f.write_all(&(36 + data).to_le_bytes())?;
        f.write_all(b"WAVEfmt ")?;
        f.write_all(&16u32.to_le_bytes())?;
        f.write_all(&3u16.to_le_bytes())?; // IEEE float
        f.write_all(&ch.to_le_bytes())?;
        f.write_all(&self.rate.to_le_bytes())?;
        f.write_all(&(self.rate * 4 * ch as u32).to_le_bytes())?;
        f.write_all(&(4 * ch).to_le_bytes())?;
        f.write_all(&32u16.to_le_bytes())?;
        f.write_all(b"data")?;
        f.write_all(&data.to_le_bytes())
    }

    pub fn write(&mut self, samples: &[f32]) -> std::io::Result<()> {
        for s in samples {
            self.file.write_all(&s.to_le_bytes())?;
        }
        self.samples += samples.len() as u64;
        Ok(())
    }

    /// Patch the header; returns the frame count.
    pub fn finish(mut self) -> std::io::Result<u64> {
        self.file.flush()?;
        self.file.seek(SeekFrom::Start(0))?;
        self.header()?;
        self.file.flush()?;
        Ok(self.samples / self.channels.max(1) as u64)
    }
}

/// Names of the input devices (default first when it is known).
#[cfg(feature = "audio-out")]
pub fn input_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.name().ok());
    let mut names: Vec<String> = host.input_devices().map(|it| it.filter_map(|d| d.name().ok()).collect()).unwrap_or_default();
    if let Some(d) = default {
        names.retain(|n| *n != d);
        names.insert(0, d);
    }
    names
}

/// A recording in progress. Dropping it without [`Recorder::stop`] still
/// finishes the file.
#[cfg(feature = "audio-out")]
pub struct Recorder {
    stream: Option<cpal::Stream>,
    writer: Option<std::thread::JoinHandle<std::io::Result<u64>>>,
    peak: Arc<AtomicU32>,
    frames: Arc<AtomicU64>,
    path: PathBuf,
    rate: u32,
    channels: u16,
    pub device: String,
}

#[cfg(feature = "audio-out")]
impl Recorder {
    /// Start recording `device` (by name; `None` = the default input) into `path`.
    pub fn start(device: Option<&str>, path: &Path) -> Result<Recorder, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let dev = match device {
            Some(name) => host.input_devices().map_err(|e| e.to_string())?.find(|d| d.name().is_ok_and(|n| n == name)).ok_or_else(|| format!("input “{name}” is not connected"))?,
            None => host.default_input_device().ok_or("no microphone / input device found")?,
        };
        let name = dev.name().unwrap_or_else(|_| "Input".into());
        let cfg = dev.default_input_config().map_err(|e| format!("{name}: {e}"))?;
        let (rate, channels, format) = (cfg.sample_rate().0, cfg.channels(), cfg.sample_format());
        let config: cpal::StreamConfig = cfg.into();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut wav = WavWriter::create(path, rate, channels).map_err(|e| format!("{}: {e}", path.display()))?;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
        let writer = std::thread::Builder::new()
            .name("rcp-voiceover".into())
            .spawn(move || {
                for chunk in rx {
                    wav.write(&chunk)?;
                }
                wav.finish()
            })
            .map_err(|e| e.to_string())?;
        let peak = Arc::new(AtomicU32::new(0));
        let frames = Arc::new(AtomicU64::new(0));
        let ch = channels.max(1) as u64;
        let err = |e: cpal::StreamError| log::error!("voiceover input: {e}");
        macro_rules! stream {
            ($t:ty, $conv:expr) => {{
                let (tx, peak, frames) = (tx.clone(), peak.clone(), frames.clone());
                dev.build_input_stream(
                    &config,
                    move |data: &[$t], _: &cpal::InputCallbackInfo| {
                        let buf: Vec<f32> = data.iter().map(|s| $conv(*s)).collect();
                        let p = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                        peak.fetch_max(p.to_bits(), Ordering::Relaxed);
                        frames.fetch_add(buf.len() as u64 / ch, Ordering::Relaxed);
                        let _ = tx.send(buf);
                    },
                    err,
                    None,
                )
            }};
        }
        let stream = match format {
            cpal::SampleFormat::F32 => stream!(f32, |s: f32| s),
            cpal::SampleFormat::I16 => stream!(i16, |s: i16| s as f32 / 32_768.0),
            cpal::SampleFormat::U16 => stream!(u16, |s: u16| (s as f32 - 32_768.0) / 32_768.0),
            cpal::SampleFormat::I32 => stream!(i32, |s: i32| s as f32 / 2_147_483_648.0),
            other => return Err(format!("{name}: unsupported sample format {other:?}")),
        }
        .map_err(|e| format!("{name}: {e}"))?;
        drop(tx); // the stream's sender is the only one: dropping the stream ends the writer
        stream.play().map_err(|e| format!("{name}: {e}"))?;
        log::info!("voiceover: recording {name} @ {rate} Hz, {channels} ch → {}", path.display());
        Ok(Recorder { stream: Some(stream), writer: Some(writer), peak, frames, path: path.to_path_buf(), rate, channels, device: name })
    }

    /// Peak level (linear) since the last call.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }

    /// Recorded length in seconds.
    pub fn seconds(&self) -> f64 {
        self.frames.load(Ordering::Relaxed) as f64 / self.rate.max(1) as f64
    }

    /// Stop and finish the file.
    pub fn stop(mut self) -> Result<Take, String> {
        self.finish()
    }

    fn finish(&mut self) -> Result<Take, String> {
        drop(self.stream.take());
        let frames = match self.writer.take().map(|w| w.join()) {
            Some(Ok(Ok(n))) => n,
            Some(Ok(Err(e))) => return Err(format!("{}: {e}", self.path.display())),
            Some(Err(_)) => return Err("the voiceover writer crashed".into()),
            None => return Err("already stopped".into()),
        };
        Ok(Take { path: self.path.clone(), sample_rate: self.rate, channels: self.channels, frames })
    }
}

#[cfg(feature = "audio-out")]
impl Drop for Recorder {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_wav_has_the_right_sizes() {
        let p = std::env::temp_dir().join(format!("rcp-vo-test-{}.wav", std::process::id()));
        let mut w = WavWriter::create(&p, 44_100, 1).unwrap();
        w.write(&[0.0, 0.5]).unwrap();
        w.write(&[-0.5]).unwrap();
        assert_eq!(w.finish().unwrap(), 3);
        let b = std::fs::read(&p).unwrap();
        assert_eq!(b.len(), 44 + 12);
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 36 + 12);
        assert_eq!(u16::from_le_bytes([b[22], b[23]]), 1);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 44_100);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 12);
        assert_eq!(f32::from_le_bytes(b[48..52].try_into().unwrap()), 0.5);
        let _ = std::fs::remove_file(p);
    }
}
