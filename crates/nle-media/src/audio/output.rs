//! Low-latency playback engine.
//!
//! A dedicated mixer thread renders the timeline ~60 ms ahead into a
//! lock-free SPSC ring (`rtrb`); the cpal real-time callback only pops
//! samples — no locks, no allocation. The number of frames the device has
//! consumed is the master clock the UI follows for A/V sync.

use super::mixer::TimelineMixer;
use crate::frame::AudioBuffer;
use nle_core::eval::AudioSpan;
use nle_core::{AssetId, Time};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const RENDER_BLOCK: usize = 256;
const LOOKAHEAD_SECS: f64 = 0.06;
/// No callback since the last (re)start.
const NO_CALLBACK: u64 = u64::MAX;
/// Upper bound for the reported device latency (bogus driver timestamps).
const MAX_LATENCY_NANOS: u64 = 250_000_000;

struct Transport {
    playing: bool,
    speed: f64,
    /// Timeline position of the first sample after the last (re)start.
    base: Time,
    /// Timeline position of the next sample the mixer will render.
    render_pos: Time,
    started_at: Instant,
}

struct Shared {
    mixer: Mutex<TimelineMixer>,
    transport: Mutex<Transport>,
    consumed: AtomicU64,
    /// Seqlock-published callback snapshot: frames consumed before the last
    /// callback, its wall time (ns since `epoch`) and the device's output
    /// latency. Lets `position()` interpolate between callbacks and report the
    /// sample at the speakers, not the one handed to the driver.
    cb_seq: AtomicU64,
    cb_start_frames: AtomicU64,
    cb_nanos: AtomicU64,
    latency_nanos: AtomicU64,
    /// Last position returned while playing (flicks), keeps the clock monotonic.
    last_pos: AtomicI64,
    epoch: Instant,
    flush_gen: AtomicU64,
    quit: AtomicBool,
    has_device: AtomicBool,
    sample_rate: u32,
}

pub struct AudioEngine {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    #[cfg(feature = "audio-out")]
    _stream: Option<cpal::Stream>,
    pub device_name: String,
}

impl AudioEngine {
    /// Open the default output device (falls back to a silent clock).
    pub fn new() -> Self {
        #[cfg(feature = "audio-out")]
        {
            match open_device() {
                Ok((stream, producer, shared, name)) => {
                    let mut engine = AudioEngine { shared, thread: None, _stream: Some(stream), device_name: name };
                    engine.spawn_mixer(Some(producer));
                    return engine;
                }
                Err(e) => log::warn!("audio output unavailable ({e}); using silent clock"),
            }
        }
        let mut engine = AudioEngine {
            shared: make_shared(48_000, false),
            thread: None,
            #[cfg(feature = "audio-out")]
            _stream: None,
            device_name: "None".into(),
        };
        engine.spawn_mixer(None);
        engine
    }

    fn spawn_mixer(&mut self, producer: Option<rtrb::Producer<f32>>) {
        let shared = self.shared.clone();
        // Without a mixer thread playback is silent but the transport clock
        // still runs, so the editor stays usable.
        self.thread = std::thread::Builder::new()
            .name("rcp-audio-mixer".into())
            .spawn(move || mixer_loop(shared, producer))
            .map_err(|e| log::error!("audio mixer thread unavailable: {e}"))
            .ok();
    }

    pub fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }

    pub fn has_device(&self) -> bool {
        self.shared.has_device.load(Ordering::Relaxed)
    }

    pub fn set_spans(&self, spans: Vec<AudioSpan>) {
        self.shared.mixer.lock().set_spans(spans);
    }

    pub fn set_source(&self, asset: AssetId, buf: Arc<AudioBuffer>) {
        self.shared.mixer.lock().set_source(asset, buf);
    }

    pub fn has_source(&self, asset: AssetId) -> bool {
        self.shared.mixer.lock().has_source(asset)
    }

    /// Install the cached dialogue envelope (look-ahead ducking), or `None`.
    pub fn set_duck_envelope(&self, env: Option<Arc<super::ducking::DuckEnvelope>>) {
        self.shared.mixer.lock().set_duck_envelope(env);
    }

    pub fn set_master_gain(&self, g: f32) {
        self.shared.mixer.lock().master_gain = g;
    }

    pub fn peaks(&self) -> [f32; 2] {
        self.shared.mixer.lock().peaks
    }

    pub fn is_playing(&self) -> bool {
        self.shared.transport.lock().playing
    }

    /// Start (or restart) playback at `at` with shuttle `speed` (J/K/L).
    pub fn play(&self, at: Time, speed: f64) {
        let mut t = self.shared.transport.lock();
        t.playing = speed != 0.0;
        t.speed = speed;
        t.base = at;
        t.render_pos = at;
        t.started_at = Instant::now();
        self.shared.consumed.store(0, Ordering::SeqCst);
        self.shared.cb_nanos.store(NO_CALLBACK, Ordering::SeqCst);
        self.shared.last_pos.store(i64::MIN, Ordering::SeqCst);
        self.shared.flush_gen.fetch_add(1, Ordering::SeqCst);
        drop(t);
        self.shared.mixer.lock().reset_state();
    }

    pub fn pause(&self) -> Time {
        let pos = self.position();
        let mut t = self.shared.transport.lock();
        t.playing = false;
        t.base = pos;
        t.render_pos = pos;
        self.shared.flush_gen.fetch_add(1, Ordering::SeqCst);
        pos
    }

    /// The audible position — the master clock for video during playback.
    pub fn position(&self) -> Time {
        let t = self.shared.transport.lock();
        if !t.playing {
            return t.base;
        }
        let elapsed = if self.has_device() { self.shared.audible_secs() } else { t.started_at.elapsed().as_secs_f64() };
        let pos = t.base + Time::from_secs_f64(elapsed * t.speed);
        // never step backwards while playing (forward or reverse shuttle)
        let last = self.shared.last_pos.load(Ordering::Relaxed);
        let pos = if last == i64::MIN {
            pos
        } else if t.speed >= 0.0 {
            Time(pos.0.max(last))
        } else {
            Time(pos.0.min(last))
        };
        self.shared.last_pos.store(pos.0, Ordering::Relaxed);
        pos
    }
}

impl Shared {
    /// Seconds of audio that have actually reached the speakers since the
    /// last (re)start: the frames handed over before the last callback, plus
    /// the wall time since that callback minus the output latency, capped at
    /// everything handed over so far.
    fn audible_secs(&self) -> f64 {
        let rate = self.sample_rate as f64;
        let (start, nanos) = loop {
            let s0 = self.cb_seq.load(Ordering::Acquire);
            let start = self.cb_start_frames.load(Ordering::Acquire);
            let nanos = self.cb_nanos.load(Ordering::Acquire);
            if s0.is_multiple_of(2) && self.cb_seq.load(Ordering::Acquire) == s0 {
                break (start, nanos);
            }
            std::hint::spin_loop();
        };
        let consumed = self.consumed.load(Ordering::Acquire);
        if nanos == NO_CALLBACK {
            return 0.0;
        }
        let now = self.epoch.elapsed().as_nanos() as f64;
        let latency = self.latency_nanos.load(Ordering::Relaxed) as f64;
        audible_position(start as f64 / rate, consumed as f64 / rate, (now - nanos as f64 - latency) * 1e-9)
    }

    /// Real-time callback bookkeeping (atomics only).
    fn publish_callback(&self, popped: u64, latency: Option<Duration>) {
        let nanos = self.epoch.elapsed().as_nanos() as u64;
        self.cb_seq.fetch_add(1, Ordering::AcqRel);
        let before = self.consumed.fetch_add(popped, Ordering::AcqRel);
        self.cb_start_frames.store(before, Ordering::Release);
        self.cb_nanos.store(nanos, Ordering::Release);
        self.cb_seq.fetch_add(1, Ordering::AcqRel);
        if let Some(l) = latency {
            self.latency_nanos.store((l.as_nanos() as u64).min(MAX_LATENCY_NANOS), Ordering::Relaxed);
        }
    }
}

/// Audible clock: `start` = seconds handed over before the last callback,
/// `handed` = seconds handed over in total, `since` = wall seconds since that
/// callback minus the output latency. The first frame of the last callback
/// reaches the speakers `latency` after the callback, so the audible point is
/// `start + since`, clamped to what exists.
pub(crate) fn audible_position(start: f64, handed: f64, since: f64) -> f64 {
    (start + since).clamp(0.0, handed.max(0.0))
}

#[cfg(test)]
mod clock_tests {
    use super::audible_position;

    #[test]
    fn audible_clock_compensates_latency_and_interpolates() {
        // 10 ms buffers, 30 ms latency: right after a callback at 1.00 s
        // handed-over, the speakers still play 0.97 s
        assert!((audible_position(1.0, 1.01, -0.03) - 0.97).abs() < 1e-9);
        // 5 ms into the buffer's audible window: smooth, not buffer-stepped
        assert!((audible_position(1.0, 1.01, 0.005) - 1.005).abs() < 1e-9);
        // late callback / underrun: never past what was handed over
        assert_eq!(audible_position(1.0, 1.01, 0.5), 1.01);
        // start of playback: nothing audible before the latency elapsed
        assert_eq!(audible_position(0.0, 0.01, -0.02), 0.0);
    }
}

impl AudioEngine {
    /// Output latency the device reports (0 when unknown).
    pub fn output_latency(&self) -> Duration {
        Duration::from_nanos(self.shared.latency_nanos.load(Ordering::Relaxed))
    }
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn make_shared(rate: u32, has_device: bool) -> Arc<Shared> {
    Arc::new(Shared {
        mixer: Mutex::new(TimelineMixer::new(rate)),
        transport: Mutex::new(Transport { playing: false, speed: 1.0, base: Time::ZERO, render_pos: Time::ZERO, started_at: Instant::now() }),
        consumed: AtomicU64::new(0),
        cb_seq: AtomicU64::new(0),
        cb_start_frames: AtomicU64::new(0),
        cb_nanos: AtomicU64::new(NO_CALLBACK),
        latency_nanos: AtomicU64::new(0),
        last_pos: AtomicI64::new(i64::MIN),
        epoch: Instant::now(),
        flush_gen: AtomicU64::new(0),
        quit: AtomicBool::new(false),
        has_device: AtomicBool::new(has_device),
        sample_rate: rate,
    })
}

fn mixer_loop(shared: Arc<Shared>, mut producer: Option<rtrb::Producer<f32>>) {
    let sr = shared.sample_rate as f64;
    let mut block = vec![0.0f32; RENDER_BLOCK * 2];
    let target = (sr * LOOKAHEAD_SECS) as usize * 2;
    let mut seen_gen = shared.flush_gen.load(Ordering::SeqCst);
    while !shared.quit.load(Ordering::Relaxed) {
        let gen = shared.flush_gen.load(Ordering::SeqCst);
        if gen != seen_gen {
            seen_gen = gen;
        }
        let (playing, speed, pos) = {
            let t = shared.transport.lock();
            (t.playing, t.speed, t.render_pos)
        };
        let Some(prod) = producer.as_mut() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        if !playing {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let capacity = prod.buffer().capacity();
        let queued = capacity - prod.slots();
        if queued >= target || prod.slots() < block.len() {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        shared.mixer.lock().render(pos, speed, &mut block);
        // drop the block if a seek happened while rendering
        if shared.flush_gen.load(Ordering::SeqCst) != seen_gen {
            continue;
        }
        for s in &block {
            let _ = prod.push(*s);
        }
        let mut t = shared.transport.lock();
        if t.playing && t.render_pos == pos {
            t.render_pos = pos + Time::from_secs_f64(speed * RENDER_BLOCK as f64 / sr);
        }
    }
}

#[cfg(feature = "audio-out")]
fn open_device() -> Result<(cpal::Stream, rtrb::Producer<f32>, Arc<Shared>, String), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no default output device")?;
    let name = device.name().unwrap_or_else(|_| "Output".into());
    let default = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = default.channels();
    let supports_48k = device
        .supported_output_configs()
        .map(|mut cfgs| cfgs.any(|c| c.channels() == channels && c.min_sample_rate().0 <= 48_000 && c.max_sample_rate().0 >= 48_000))
        .unwrap_or(false);
    let rate = if supports_48k { 48_000 } else { default.sample_rate().0 };
    let config = cpal::StreamConfig { channels, sample_rate: cpal::SampleRate(rate), buffer_size: cpal::BufferSize::Default };

    let shared = make_shared(rate, true);
    let (producer, mut cons) = rtrb::RingBuffer::<f32>::new(rate as usize); // 0.5 s of stereo
    let cb_shared = shared.clone();
    let mut seen_gen = u64::MAX;
    let ch = channels as usize;
    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // Real-time thread: atomics and the lock-free ring only.
                let gen = cb_shared.flush_gen.load(Ordering::Acquire);
                if gen != seen_gen {
                    seen_gen = gen;
                    while cons.pop().is_ok() {}
                }
                let mut popped = 0u64;
                for frame in data.chunks_mut(ch) {
                    if cons.slots() < 2 {
                        frame.fill(0.0);
                        continue;
                    }
                    let l = cons.pop().unwrap_or(0.0);
                    let r = cons.pop().unwrap_or(0.0);
                    popped += 1;
                    for (i, s) in frame.iter_mut().enumerate() {
                        *s = match (ch, i) {
                            (1, _) => 0.5 * (l + r),
                            (_, 0) => l,
                            (_, 1) => r,
                            _ => 0.0,
                        };
                    }
                }
                // only frames that carry timeline audio advance the clock
                let ts = info.timestamp();
                let latency = ts.playback.duration_since(&ts.callback);
                if popped > 0 {
                    cb_shared.publish_callback(popped, latency);
                }
            },
            |e| log::error!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    log::info!("audio output: {name} @ {rate} Hz, {channels} ch");
    Ok((stream, producer, shared, name))
}

/// Top of the preview volume slider, percent.
pub const PREVIEW_VOLUME_MAX: f32 = 150.0;

/// Perceptual (logarithmic) preview gain for a 0..150 % volume slider:
/// amplitude = (pct / 100)², i.e. 40·log10 dB — 100 % = 0 dB (unity),
/// 50 % ≈ −12 dB, 10 % = −40 dB, 150 % ≈ +7 dB, 0 % = silence. Preview only:
/// export renders through its own mixer at unity, and the master −1 dBFS
/// limiter after this gain keeps boosted playback from clipping.
pub fn preview_gain(percent: f32, muted: bool) -> f32 {
    if muted || !percent.is_finite() {
        return 0.0;
    }
    let x = percent.clamp(0.0, PREVIEW_VOLUME_MAX) / 100.0;
    x * x
}

/// The preview gain in dB (`None` = silent).
pub fn preview_gain_db(percent: f32, muted: bool) -> Option<f32> {
    let g = preview_gain(percent, muted);
    (g > 0.0).then(|| 20.0 * g.log10())
}

#[cfg(test)]
mod preview_gain_tests {
    use super::*;

    #[test]
    fn preview_volume_is_perceptual_and_unity_at_100() {
        assert_eq!(preview_gain(100.0, false), 1.0);
        assert_eq!(preview_gain(100.0, true), 0.0);
        assert_eq!(preview_gain(0.0, false), 0.0);
        assert!((preview_gain_db(50.0, false).unwrap() + 12.04).abs() < 0.01);
        assert!((preview_gain_db(150.0, false).unwrap() - 7.04).abs() < 0.01);
        assert_eq!(preview_gain(1e6, false), preview_gain(PREVIEW_VOLUME_MAX, false));
        assert_eq!(preview_gain(f32::NAN, false), 0.0);
        assert!((1..=150).all(|p| preview_gain(p as f32, false) > preview_gain((p - 1) as f32, false)));
    }
}
