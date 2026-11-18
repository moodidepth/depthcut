//! Offline dialogue restoration: spectral noise reduction (learned or
//! automatic noise profile) and voice isolation.
//!
//! Both need look-ahead (STFT), so they are not run inside the real-time
//! mixer. Instead the source range a clip uses is processed once in the
//! background ([`restore`]) and the mixer reads the cleaned buffer
//! ([`Restored`]) in place of the raw source. Until it is ready playback
//! uses the raw audio; export always waits for it.
//!
//! Voice isolation uses a pluggable separation backend (an ML model, see
//! `nle-ai`) when one is registered with [`set_voice_isolation_backend`].
//! Without it a DSP fallback runs: aggressive stationary-noise suppression
//! plus speech-band limiting. The fallback removes hiss, hum, fans and room
//! tone; it cannot pull speech out of music or game audio — only the ML
//! backend can.

use crate::frame::AudioBuffer;
use nle_core::audio_fx::{NOISE_FFT, NOISE_PROFILE_BINS};
use nle_core::eval::AudioSpan;
use nle_core::{AssetId, AudioParams, NoiseReductionSettings, Time, VoiceIsolationSettings};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::{Arc, OnceLock};

const HOP: usize = NOISE_FFT / 4;
/// Extra source audio processed either side of the used range, so small
/// trims do not invalidate the cache and the STFT edges are out of sight.
const MARGIN_SECS: f64 = 0.5;

/// ML voice separation: interleaved stereo in → isolated voice (same
/// length, interleaved stereo) out. `None` = failed / cancelled.
pub type IsolationBackend = fn(samples: &[f32], sample_rate: u32, cancel: &dyn Fn() -> bool) -> Option<Vec<f32>>;

static BACKEND: OnceLock<(&'static str, IsolationBackend)> = OnceLock::new();

/// Register the ML separation backend (first registration wins).
pub fn set_voice_isolation_backend(name: &'static str, f: IsolationBackend) {
    let _ = BACKEND.set((name, f));
}

/// Name of the registered ML backend, `None` when the DSP fallback is used.
pub fn voice_isolation_backend() -> Option<&'static str> {
    BACKEND.get().map(|(n, _)| *n)
}

/// Does this clip need an offline restoration pass?
pub fn needs_restore(p: &AudioParams) -> bool {
    p.noise_reduction.enabled || p.voice_isolation.enabled
}

/// What a cleaned buffer was made from. A cached [`Restored`] is valid for
/// a clip while its key matches.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreKey {
    pub asset: AssetId,
    pub nr: NoiseReductionSettings,
    pub iso: VoiceIsolationSettings,
    /// Source frame range `[start, end)` that was processed.
    pub start: usize,
    pub end: usize,
}

impl RestoreKey {
    /// Same processing and the range still covers `[a, b)`?
    pub fn serves(&self, other: &RestoreKey) -> bool {
        self.asset == other.asset && self.nr == other.nr && self.iso == other.iso && self.start <= other.start && self.end >= other.end
    }
}

/// A cleaned source range.
#[derive(Clone, Debug)]
pub struct Restored {
    pub key: RestoreKey,
    /// Covers source frames `[key.start, key.end)`.
    pub buf: Arc<AudioBuffer>,
}

/// Source frames a span reads (retime curves are sampled), without margin.
pub fn span_source_range(span: &AudioSpan, src_rate: u32) -> (usize, usize) {
    const STEPS: i64 = 64;
    let (a, d) = (span.start.0, span.duration.0.max(0));
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for i in 0..=STEPS {
        let t = Time(a + (d as i128 * i as i128 / STEPS as i128) as i64);
        let s = span.source_time(t).as_secs_f64();
        lo = lo.min(s);
        hi = hi.max(s);
    }
    let r = src_rate as f64;
    ((lo.max(0.0) * r).floor() as usize, (hi.max(0.0) * r).ceil() as usize + 1)
}

/// The restoration a span needs, or `None` when it needs none.
pub fn restore_key(span: &AudioSpan, src: &AudioBuffer) -> Option<RestoreKey> {
    if !needs_restore(&span.params) {
        return None;
    }
    let (a, b) = span_source_range(span, src.sample_rate);
    let m = (MARGIN_SECS * src.sample_rate as f64) as usize;
    let n = src.frames();
    let (start, end) = (a.saturating_sub(m).min(n), (b + m).min(n));
    let mut nr = span.params.noise_reduction.clone();
    let mut iso = span.params.voice_isolation;
    // disabled stages do not split the cache on their idle settings
    if !nr.enabled {
        nr = NoiseReductionSettings::default();
    }
    if !iso.enabled {
        iso = VoiceIsolationSettings::default();
    }
    Some(RestoreKey { asset: span.asset, nr, iso, start, end })
}

/// Clean `key`'s range of `src`. Runs for seconds on long ranges, so call it
/// from a worker; `cancel` is polled between STFT frames.
pub fn restore(src: &AudioBuffer, key: &RestoreKey, cancel: &dyn Fn() -> bool) -> Option<AudioBuffer> {
    let mut seg: Vec<f32> = (key.start..key.end.max(key.start))
        .flat_map(|i| {
            let (l, r) = src.stereo_at(i as f64);
            [l, r]
        })
        .collect();
    if key.nr.enabled {
        seg = denoise(&seg, &key.nr, cancel)?;
    }
    if key.iso.enabled {
        seg = isolate_voice(&seg, src.sample_rate, key.iso.amount, cancel)?;
    }
    Some(AudioBuffer::new(src.sample_rate, 2, seg))
}

/// Learn a noise profile (mean power spectrum) from a stretch of pure noise
/// — select a pause between words and press *Learn*.
pub fn learn_noise_profile(src: &AudioBuffer, start: usize, end: usize) -> Option<Vec<f32>> {
    let seg: Vec<f32> = (start..end.min(src.frames()))
        .flat_map(|i| {
            let (l, r) = src.stereo_at(i as f64);
            [l, r]
        })
        .collect();
    let stft = Stft::new();
    let frames = stft.power_frames(&seg, usize::MAX);
    if frames.is_empty() {
        return None;
    }
    let mut mean = vec![0.0f32; NOISE_PROFILE_BINS];
    for (_, p) in &frames {
        for (m, v) in mean.iter_mut().zip(p) {
            *m += v;
        }
    }
    let n = frames.len() as f32;
    mean.iter_mut().for_each(|m| *m /= n);
    Some(mean)
}

/// Noise estimate from the quietest 15 % of frames (pauses between words
/// are pure noise; steady noise is the same in every frame).
pub fn auto_noise_profile(seg: &[f32]) -> Option<Vec<f32>> {
    let mut frames = Stft::new().power_frames(seg, 4_000);
    if frames.is_empty() {
        return None;
    }
    frames.sort_by(|a, b| a.0.total_cmp(&b.0));
    let take = (frames.len() * 15 / 100).max(1);
    let mut mean = vec![0.0f32; NOISE_PROFILE_BINS];
    for (_, p) in &frames[..take] {
        for (m, v) in mean.iter_mut().zip(p) {
            *m += v;
        }
    }
    mean.iter_mut().for_each(|m| *m /= take as f32);
    Some(mean)
}

/// Spectral noise suppression of interleaved stereo (stereo-linked mask).
pub fn denoise(seg: &[f32], s: &NoiseReductionSettings, cancel: &dyn Fn() -> bool) -> Option<Vec<f32>> {
    let noise = match &s.profile {
        Some(p) if p.len() == NOISE_PROFILE_BINS => p.clone(),
        _ => auto_noise_profile(seg)?,
    };
    let floor = 10f32.powf(-s.reduction_db.abs() / 20.0);
    suppress(seg, &noise, s.sensitivity.max(0.1), floor, |_| 1.0, cancel)
}

/// Voice isolation: the ML backend when registered, the DSP fallback
/// otherwise. `amount` 0..1 blends from untouched to fully isolated.
pub fn isolate_voice(seg: &[f32], sample_rate: u32, amount: f32, cancel: &dyn Fn() -> bool) -> Option<Vec<f32>> {
    let amount = amount.clamp(0.0, 1.0);
    if let Some((name, f)) = BACKEND.get() {
        match f(seg, sample_rate, cancel) {
            Some(voice) if voice.len() == seg.len() => {
                return Some(seg.iter().zip(&voice).map(|(x, v)| x + amount * (v - x)).collect());
            }
            Some(_) => log::warn!("voice isolation backend {name} returned the wrong length; using DSP fallback"),
            None if cancel() => return None,
            None => log::warn!("voice isolation backend {name} failed; using DSP fallback"),
        }
    }
    let noise = auto_noise_profile(seg)?;
    let bin_hz = sample_rate as f32 / NOISE_FFT as f32;
    // speech band 80 Hz – 9 kHz, gentle edges
    let band = move |k: usize| {
        let f = k as f32 * bin_hz;
        let lo = ((f - 50.0) / 40.0).clamp(0.0, 1.0);
        let hi = ((11_000.0 - f) / 2_000.0).clamp(0.0, 1.0);
        lo * hi
    };
    let floor = 10f32.powf(-30.0 * amount / 20.0);
    let wet = suppress(seg, &noise, 1.0 + 1.5 * amount, floor, band, cancel)?;
    Some(seg.iter().zip(&wet).map(|(x, w)| x + amount * (w - x)).collect())
}

/// STFT mask: gain per bin from the noise power `noise`, never below
/// `floor`, multiplied by `shape(bin)` (band limiting). Fast attack / slow
/// release smoothing of the gains suppresses "musical noise".
fn suppress(seg: &[f32], noise: &[f32], sensitivity: f32, floor: f32, shape: impl Fn(usize) -> f32, cancel: &dyn Fn() -> bool) -> Option<Vec<f32>> {
    let n = NOISE_FFT;
    let frames = seg.len() / 2;
    let stft = Stft::new();
    let mut out = vec![0.0f32; seg.len()];
    let mut spec = [vec![Complex32::default(); n], vec![Complex32::default(); n]];
    let mut smooth_p = vec![0.0f32; NOISE_PROFILE_BINS];
    let mut gains = vec![1.0f32; NOISE_PROFILE_BINS];
    let mut raw = vec![0.0f32; NOISE_PROFILE_BINS];
    let release = 0.3f32;
    let shape: Vec<f32> = (0..NOISE_PROFILE_BINS).map(shape).collect();
    let mut first = true;
    let mut pos = -(n as isize) + HOP as isize;
    while pos < frames as isize {
        if cancel() {
            return None;
        }
        stft.forward(seg, pos, &mut spec);
        for k in 0..NOISE_PROFILE_BINS {
            let p = 0.5 * (spec[0][k].norm_sqr() + spec[1][k].norm_sqr()) / stft.norm;
            // ~40 ms recursive average: tames the per-frame variance of noise
            // bins (the source of "musical noise") without smearing onsets much
            smooth_p[k] = if first { p } else { 0.7 * smooth_p[k] + 0.3 * p };
        }
        for k in 0..NOISE_PROFILE_BINS {
            let p = (smooth_p[k.saturating_sub(1)] + 2.0 * smooth_p[k] + smooth_p[(k + 1).min(NOISE_PROFILE_BINS - 1)]) * 0.25;
            // Wiener-style gain: ≈1 on strong bins, steep near the noise floor
            raw[k] = (1.0 - sensitivity * noise[k] / p.max(1e-20)).max(0.0);
        }
        first = false;
        for k in 0..NOISE_PROFILE_BINS {
            // 3-tap frequency smoothing, then floor and band shape
            let g = (raw[k.saturating_sub(1)] + 2.0 * raw[k] + raw[(k + 1).min(NOISE_PROFILE_BINS - 1)]) * 0.25;
            let g = g.max(floor) * shape[k];
            gains[k] = g.max(gains[k] * release);
            gains[k] = gains[k].min(1.0).max(g);
        }
        for ch in &mut spec {
            for k in 0..NOISE_PROFILE_BINS {
                ch[k] *= gains[k];
                if k > 0 && k < n / 2 {
                    ch[n - k] *= gains[k];
                }
            }
        }
        stft.inverse_add(&mut spec, pos, &mut out);
        pos += HOP as isize;
    }
    Some(out)
}

struct Stft {
    fwd: Arc<dyn Fft<f32>>,
    inv: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Power normalisation so profiles are window-independent.
    norm: f32,
}

impl Stft {
    fn new() -> Self {
        let n = NOISE_FFT;
        let mut planner = FftPlanner::new();
        // periodic Hann: Hann² overlap-added at 75 % sums to 1.5
        let window: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()).collect();
        let norm = window.iter().map(|w| w * w).sum::<f32>();
        Stft { fwd: planner.plan_fft_forward(n), inv: planner.plan_fft_inverse(n), window, norm }
    }

    fn forward(&self, seg: &[f32], pos: isize, spec: &mut [Vec<Complex32>; 2]) {
        let frames = (seg.len() / 2) as isize;
        for (ch, buf) in spec.iter_mut().enumerate() {
            for (i, c) in buf.iter_mut().enumerate() {
                let j = pos + i as isize;
                let x = if j >= 0 && j < frames { seg[2 * j as usize + ch] } else { 0.0 };
                *c = Complex32::new(x * self.window[i], 0.0);
            }
            self.fwd.process(buf);
        }
    }

    fn inverse_add(&self, spec: &mut [Vec<Complex32>; 2], pos: isize, out: &mut [f32]) {
        let n = NOISE_FFT;
        let frames = (out.len() / 2) as isize;
        let scale = 1.0 / (n as f32 * 1.5);
        for (ch, buf) in spec.iter_mut().enumerate() {
            self.inv.process(buf);
            for (i, c) in buf.iter().enumerate() {
                let j = pos + i as isize;
                if j >= 0 && j < frames {
                    out[2 * j as usize + ch] += c.re * self.window[i] * scale;
                }
            }
        }
    }

    /// (total power, power spectrum) of up to `max` evenly spaced frames.
    fn power_frames(&self, seg: &[f32], max: usize) -> Vec<(f32, Vec<f32>)> {
        let frames = seg.len() / 2;
        if frames < NOISE_FFT {
            return Vec::new();
        }
        let count = (frames - NOISE_FFT) / HOP + 1;
        let stride = count.div_ceil(max.max(1)).max(1);
        let mut spec = [vec![Complex32::default(); NOISE_FFT], vec![Complex32::default(); NOISE_FFT]];
        (0..count)
            .step_by(stride)
            .map(|f| {
                self.forward(seg, (f * HOP) as isize, &mut spec);
                let p: Vec<f32> = (0..NOISE_PROFILE_BINS).map(|k| 0.5 * (spec[0][k].norm_sqr() + spec[1][k].norm_sqr()) / self.norm).collect();
                (p.iter().sum(), p)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white noise.
    fn noise(n: usize, amp: f32, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// 4 s: hiss throughout, a 440 Hz "voice" in 1–3 s.
    fn noisy_voice() -> (Vec<f32>, Vec<f32>) {
        let n = 48_000 * 4;
        let hiss = noise(n, 0.02, 7);
        let voice: Vec<f32> = (0..n).map(|i| if (48_000..144_000).contains(&i) { (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin() * 0.3 } else { 0.0 }).collect();
        let mix = (0..n).flat_map(|i| [voice[i] + hiss[i], voice[i] + hiss[i]]).collect();
        (mix, voice)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn auto_profile_denoise_cuts_hiss_and_keeps_voice() {
        let (mix, voice) = noisy_voice();
        let s = NoiseReductionSettings { enabled: true, reduction_db: 20.0, sensitivity: 1.5, profile: None };
        let out = denoise(&mix, &s, &|| false).unwrap();
        assert_eq!(out.len(), mix.len());
        let before = rms(&mix[2 * 4_800..2 * 43_000]);
        let after = rms(&out[2 * 4_800..2 * 43_000]);
        let cut = 20.0 * (after / before).log10();
        assert!(cut < -14.0, "hiss reduced in the pause: {cut} dB");
        // voice kept: residual against the clean voice is far below the hiss
        let err: Vec<f32> = (60_000..130_000).map(|i| out[2 * i] - voice[i]).collect();
        let voice_rms = rms(&voice[60_000..130_000]);
        assert!(rms(&err) < voice_rms * 0.12, "voice distortion {} vs {}", rms(&err), voice_rms);
    }

    #[test]
    fn learned_profile_matches_auto_and_reconstruction_is_transparent() {
        let (mix, _) = noisy_voice();
        let buf = AudioBuffer::new(48_000, 2, mix.clone());
        let learned = learn_noise_profile(&buf, 0, 40_000).unwrap();
        let auto = auto_noise_profile(&mix).unwrap();
        let (sl, sa): (f32, f32) = (learned[100..900].iter().sum(), auto[100..900].iter().sum());
        assert!((sl / sa - 1.0).abs() < 0.35, "profiles agree: {sl} vs {sa}");

        // zero sensitivity → unity gains → STFT round trip is bit-near exact
        let id = suppress(&mix, &learned, 0.0, 1.0, |_| 1.0, &|| false).unwrap();
        let diff = mix.iter().zip(&id).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(diff < 1e-4, "perfect reconstruction: {diff}");
    }

    #[test]
    fn restore_key_covers_the_used_range_and_serves_trims() {
        use nle_core::{Clip, MediaAsset, Timeline};
        let mut a = MediaAsset::new("v", "/v.wav", Time::from_secs(10));
        a.audio = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut c = Clip::from_asset(&a);
        c.source_in = Time::from_secs(2);
        c.duration = Time::from_secs(3);
        c.audio.noise_reduction.enabled = true;
        let mut tl = Timeline::default();
        tl.append(c).unwrap();
        let spans = nle_core::audio_spans(&tl);
        let src = AudioBuffer::new(48_000, 2, vec![0.0; 48_000 * 2 * 10]);
        let key = restore_key(&spans[0], &src).unwrap();
        assert_eq!(key.start, 48_000 * 2 - 24_000);
        assert!(key.end >= 48_000 * 5 + 24_000 && key.end <= 48_000 * 5 + 24_002);
        // a 0.2 s trim stays inside the margin: the cached buffer still serves
        let mut tl2 = tl.clone();
        tl2.spine[0].duration = Time::from_millis(3_200);
        let k2 = restore_key(&nle_core::audio_spans(&tl2)[0], &src).unwrap();
        assert!(key.serves(&RestoreKey { start: k2.start.max(key.start), end: k2.end - 24_000 + 1, ..k2.clone() }));
        // a settings change does not
        let mut k3 = key.clone();
        k3.nr.reduction_db += 1.0;
        assert!(!key.serves(&k3));
        let mut plain = spans[0].clone();
        plain.params.noise_reduction.enabled = false;
        assert!(restore_key(&plain, &src).is_none());
    }

    #[test]
    fn dsp_voice_isolation_removes_rumble_and_hiss() {
        let n = 48_000 * 3;
        let hiss = noise(n, 0.03, 11);
        let x: Vec<f32> = (0..n)
            .flat_map(|i| {
                let t = i as f32 / 48_000.0;
                let voice = if (48_000..96_000).contains(&i) { (std::f32::consts::TAU * 300.0 * t).sin() * 0.3 } else { 0.0 };
                let rumble = (std::f32::consts::TAU * 30.0 * t).sin() * 0.2;
                let v = voice + rumble + hiss[i];
                [v, v]
            })
            .collect();
        let out = isolate_voice(&x, 48_000, 1.0, &|| false).unwrap();
        let pause_before = rms(&x[2 * 4_800..2 * 40_000]);
        let pause_after = rms(&out[2 * 4_800..2 * 40_000]);
        assert!(pause_after < pause_before * 0.1, "pause cleaned: {pause_after} vs {pause_before}");
        let voice_after = rms(&out[2 * 60_000..2 * 90_000]);
        assert!((voice_after / (0.3 / 2f32.sqrt()) - 1.0).abs() < 0.15, "voice level kept: {voice_after}");
    }
}
