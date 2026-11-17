//! Streaming audio preview for remote URLs (Openverse, Internet Archive, local files).
//!
//! Spawns `ffmpeg` to decode a stream into 32-bit float interleaved stereo PCM on
//! `stdout`, buffers samples through a lock-free `rtrb` ring, and plays them on
//! the default `cpal` output device without downloading the full asset first.

use crate::ffmpeg_cli;
use parking_lot::Mutex;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

#[cfg(feature = "audio-out")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Convert raw little-endian `f32` PCM byte chunks into `f32` samples, carrying
/// any trailing partial (<4 byte) remainder across read boundaries.
pub fn decode_pcm_f32le_chunks(bytes: &[u8], carry: &mut Vec<u8>, out: &mut Vec<f32>) {
    out.clear();
    if carry.is_empty() {
        let whole = (bytes.len() / 4) * 4;
        out.reserve(whole / 4);
        for chunk in bytes[..whole].as_chunks::<4>().0 {
            out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        if whole < bytes.len() {
            carry.extend_from_slice(&bytes[whole..]);
        }
        return;
    }

    carry.extend_from_slice(bytes);
    let whole = (carry.len() / 4) * 4;
    out.reserve(whole / 4);
    for chunk in carry[..whole].as_chunks::<4>().0 {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    carry.drain(..whole);
}

/// Build FFmpeg arguments for streaming an audio URL to 48 kHz (or device rate)
/// interleaved `f32le` stereo PCM on stdout.
pub fn ffmpeg_stream_args(url: &str, sample_rate: u32) -> Vec<String> {
    let mut args = vec!["-hide_banner".to_string(), "-loglevel".to_string(), "error".to_string()];
    if url.starts_with("http://") || url.starts_with("https://") {
        args.extend(["-reconnect".to_string(), "1".to_string(), "-reconnect_streamed".to_string(), "1".to_string(), "-reconnect_delay_max".to_string(), "2".to_string()]);
    }
    args.extend([
        "-i".to_string(),
        url.to_string(),
        "-vn".to_string(),
        "-f".to_string(),
        "f32le".to_string(),
        "-ac".to_string(),
        "2".to_string(),
        "-ar".to_string(),
        sample_rate.to_string(),
        "pipe:1".to_string(),
    ]);
    args
}

/// Non-blocking single-track audio preview player for remote URLs / local audio.
pub struct AudioPreview {
    current_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    playing: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
}

impl Default for AudioPreview {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPreview {
    pub fn new() -> Self {
        Self {
            current_id: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            playing: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            child: Arc::new(Mutex::new(None)),
            worker: None,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn current_id(&self) -> Option<&str> {
        self.current_id.as_deref()
    }

    pub fn is_playing_id(&self, id: &str) -> bool {
        self.is_playing() && self.current_id.as_deref() == Some(id)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }

    /// Reap finished background workers so `current_id` clears after natural EOF.
    pub fn poll(&mut self) {
        if !self.is_playing() {
            if let Some(handle) = self.worker.take() {
                let _ = handle.join();
                self.current_id = None;
            }
        }
    }

    /// Stop any active preview immediately.
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(ref mut c) = *self.child.lock() {
            let _ = c.kill();
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        self.playing.store(false, Ordering::Relaxed);
        self.current_id = None;
    }

    /// Start streaming and playing `url`, tagged with `id`. Stops any prior stream.
    pub fn play(&mut self, id: impl Into<String>, url: impl Into<String>) {
        self.stop();
        let id = id.into();
        let url = url.into();
        if url.trim().is_empty() {
            *self.error.lock() = Some("No audio preview URL available".to_string());
            return;
        }
        if !ffmpeg_cli::available() {
            *self.error.lock() = Some("FFmpeg not found (required for streaming preview)".to_string());
            return;
        }

        self.current_id = Some(id);
        self.stop_flag = Arc::new(AtomicBool::new(false));
        self.playing = Arc::new(AtomicBool::new(true));
        *self.error.lock() = None;

        let stop = self.stop_flag.clone();
        let playing = self.playing.clone();
        let error = self.error.clone();
        let child_slot = self.child.clone();

        match std::thread::Builder::new().name("audio-preview".into()).spawn(move || {
            run_preview_worker(&url, &stop, &error, &child_slot);
            playing.store(false, Ordering::Relaxed);
        }) {
            Ok(h) => self.worker = Some(h),
            Err(e) => {
                self.playing.store(false, Ordering::Relaxed);
                self.current_id = None;
                *self.error.lock() = Some(format!("Failed to start preview thread: {e}"));
            }
        }
    }
}

impl Drop for AudioPreview {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(not(feature = "audio-out"))]
fn run_preview_worker(_url: &str, _stop: &AtomicBool, error: &Mutex<Option<String>>, _child_slot: &Mutex<Option<Child>>) {
    *error.lock() = Some("Audio output feature disabled".to_string());
}

#[cfg(feature = "audio-out")]
fn run_preview_worker(url: &str, stop: &AtomicBool, error: &Mutex<Option<String>>, child_slot: &Mutex<Option<Child>>) {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        *error.lock() = Some("No default audio output device".to_string());
        return;
    };
    let default_cfg = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            *error.lock() = Some(format!("Audio output error: {e}"));
            return;
        }
    };
    let sample_rate = default_cfg.sample_rate().0.max(8_000);
    let out_channels = default_cfg.channels().max(1) as usize;
    let sample_format = default_cfg.sample_format();
    let stream_cfg: cpal::StreamConfig = default_cfg.into();

    // 4 seconds of stereo f32 buffer between FFmpeg stdout and cpal callback
    let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(sample_rate as usize * 2 * 4);

    let err_cb = |e: cpal::StreamError| log::warn!("audio preview stream error: {e}");
    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(&stream_cfg, move |data: &mut [f32], _| write_cpal_frames(data, out_channels, &mut cons, |s| s), err_cb, None),
        cpal::SampleFormat::I16 => device.build_output_stream(
            &stream_cfg,
            move |data: &mut [i16], _| write_cpal_frames(data, out_channels, &mut cons, |s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
            err_cb,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            &stream_cfg,
            move |data: &mut [u16], _| write_cpal_frames(data, out_channels, &mut cons, |s| ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16),
            err_cb,
            None,
        ),
        other => {
            *error.lock() = Some(format!("Unsupported audio output format: {other:?}"));
            return;
        }
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            *error.lock() = Some(format!("Failed to build audio stream: {e}"));
            return;
        }
    };
    if let Err(e) = stream.play() {
        *error.lock() = Some(format!("Failed to start audio stream: {e}"));
        return;
    }

    let args = ffmpeg_stream_args(url, sample_rate);
    let mut proc = match ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            *error.lock() = Some(format!("Failed to spawn ffmpeg: {e}"));
            return;
        }
    };
    let Some(mut stdout) = proc.stdout.take() else {
        let _ = proc.kill();
        let _ = proc.wait();
        *error.lock() = Some("Failed to capture ffmpeg stdout".to_string());
        return;
    };
    *child_slot.lock() = Some(proc);

    let mut buf = [0u8; 4096];
    let mut carry = Vec::with_capacity(4);
    let mut samples = Vec::with_capacity(1024);

    while !stop.load(Ordering::Relaxed) {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decode_pcm_f32le_chunks(&buf[..n], &mut carry, &mut samples);
                let mut pushed = 0usize;
                while pushed < samples.len() && !stop.load(Ordering::Relaxed) {
                    let avail = prod.slots();
                    if avail == 0 {
                        std::thread::sleep(Duration::from_millis(4));
                        continue;
                    }
                    let take = avail.min(samples.len() - pushed);
                    if let Ok(mut chunk) = prod.write_chunk(take) {
                        let (a, b) = chunk.as_mut_slices();
                        let na = a.len();
                        a.copy_from_slice(&samples[pushed..pushed + na]);
                        b.copy_from_slice(&samples[pushed + na..pushed + take]);
                        chunk.commit_all();
                        pushed += take;
                    }
                }
            }
            Err(e) => {
                if !stop.load(Ordering::Relaxed) {
                    *error.lock() = Some(format!("Stream read error: {e}"));
                }
                break;
            }
        }
    }

    let maybe_child = child_slot.lock().take();
    if let Some(mut c) = maybe_child {
        if stop.load(Ordering::Relaxed) {
            let _ = c.kill();
        }
        let status = c.wait_with_output();
        if !stop.load(Ordering::Relaxed) {
            if let Ok(out) = status {
                if !out.status.success() {
                    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if !msg.is_empty() && error.lock().is_none() {
                        *error.lock() = Some(msg);
                    }
                }
            }
        }
    }

    // Wait briefly for buffered samples to finish playing on natural EOF
    if !stop.load(Ordering::Relaxed) {
        let cap = sample_rate as usize * 2 * 4;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            if prod.slots() >= cap {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    drop(stream);
}

#[cfg(feature = "audio-out")]
fn write_cpal_frames<T: Copy>(out: &mut [T], channels: usize, cons: &mut rtrb::Consumer<f32>, convert: impl Fn(f32) -> T) {
    let ch = channels.max(1);
    let zero = convert(0.0);
    for frame in out.chunks_mut(ch) {
        let l = cons.pop().unwrap_or(0.0);
        let r = cons.pop().unwrap_or(l);
        if ch == 1 {
            frame[0] = convert(0.5 * (l + r));
        } else {
            frame[0] = convert(l);
            frame[1] = convert(r);
            for extra in &mut frame[2..] {
                *extra = zero;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_chunked_f32le_with_remainder() {
        let src = [0.25f32, -0.5f32, 0.75f32];
        let mut bytes = Vec::new();
        for s in src {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let mut carry = Vec::new();
        let mut out = Vec::new();
        // Split across unaligned byte boundaries
        decode_pcm_f32le_chunks(&bytes[..5], &mut carry, &mut out);
        assert_eq!(out, vec![0.25]);
        assert_eq!(carry.len(), 1);
        decode_pcm_f32le_chunks(&bytes[5..8], &mut carry, &mut out);
        assert_eq!(out, vec![-0.5]);
        assert!(carry.is_empty());
        decode_pcm_f32le_chunks(&bytes[8..], &mut carry, &mut out);
        assert_eq!(out, vec![0.75]);
        assert!(carry.is_empty());
    }

    #[test]
    fn builds_stream_args_and_preview_lifecycle() {
        let args = ffmpeg_stream_args("https://example.org/sound.mp3", 48_000);
        assert!(args.windows(2).any(|w| w == ["-reconnect", "1"]));
        assert!(args.ends_with(&["-f".to_string(), "f32le".to_string(), "-ac".to_string(), "2".to_string(), "-ar".to_string(), "48000".to_string(), "pipe:1".to_string(),]));

        let local_args = ffmpeg_stream_args("clip.mp3", 44_100);
        assert!(!local_args.contains(&"-reconnect".to_string()));

        let mut prev = AudioPreview::new();
        assert!(!prev.is_playing());
        assert_eq!(prev.current_id(), None);
        prev.play("track-1", "   ");
        assert!(!prev.is_playing());
        assert!(prev.error().is_some());
        prev.stop();
        assert!(!prev.is_playing());
    }
}
