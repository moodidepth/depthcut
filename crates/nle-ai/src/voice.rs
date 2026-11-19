//! Neural voice isolation: DeepFilterNet 3 (MIT / Apache-2.0, model weights
//! embedded in the binary), run on the CPU by `tract` — no GPU, no runtime
//! to install. Registered as the `nle-media` voice-isolation backend by
//! [`register`]; the restoration pass then uses it instead of the DSP
//! fallback.
//!
//! The model works at 48 kHz in 10 ms hops with 30 ms of latency, which is
//! removed here so the cleaned voice lines up with the picture.

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::Array2;
use nle_media::audio::resample::resample_stereo;

pub const NAME: &str = "DeepFilterNet 3";
const MODEL_RATE: u32 = 48_000;

/// Make the restoration pass use the neural model.
pub fn register() {
    nle_media::audio::restore::set_voice_isolation_backend(NAME, isolate);
}

/// Interleaved stereo at `sample_rate` in → the voice, same length, out.
pub fn isolate(samples: &[f32], sample_rate: u32, cancel: &dyn Fn() -> bool) -> Option<Vec<f32>> {
    let x = resample_stereo(samples, sample_rate, MODEL_RATE);
    let y = enhance_48k(&x, cancel)?;
    let mut out = resample_stereo(&y, MODEL_RATE, sample_rate);
    out.resize(samples.len(), 0.0);
    Some(out)
}

/// Run the model over interleaved 48 kHz stereo, latency removed.
fn enhance_48k(x: &[f32], cancel: &dyn Fn() -> bool) -> Option<Vec<f32>> {
    let frames = x.len() / 2;
    let mut df = DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(2)).map_err(|e| log::warn!("{NAME}: model init failed: {e}")).ok()?;
    let hop = df.hop_size;
    // STFT window overlap + model look-ahead
    let delay = df.fft_size - hop + df.lookahead * hop;
    let total = frames + delay;
    let mut out = Vec::with_capacity(total.div_ceil(hop) * hop * 2);
    let mut noisy = Array2::<f32>::zeros((2, hop));
    let mut enh = Array2::<f32>::zeros((2, hop));
    let mut pos = 0;
    while pos < total {
        if (pos / hop) % 50 == 0 && cancel() {
            return None;
        }
        for k in 0..hop {
            let i = pos + k;
            let (l, r) = if i < frames { (x[2 * i], x[2 * i + 1]) } else { (0.0, 0.0) };
            noisy[[0, k]] = l;
            noisy[[1, k]] = r;
        }
        df.process(noisy.view(), enh.view_mut()).map_err(|e| log::warn!("{NAME}: {e}")).ok()?;
        for k in 0..hop {
            out.push(enh[[0, k]]);
            out.push(enh[[1, k]]);
        }
        pos += hop;
    }
    Some(out[delay * 2..(delay + frames) * 2].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.2
            })
            .collect()
    }

    fn power(x: &[f32]) -> f64 {
        x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64
    }

    #[test]
    fn removes_noise_keeps_length() {
        let x = noise(48_000 * 2 * 2, 7);
        let y = isolate(&x, 48_000, &|| false).unwrap();
        assert_eq!(y.len(), x.len());
        let drop_db = 10.0 * (power(&x) / power(&y).max(1e-20)).log10();
        assert!(drop_db > 20.0, "noise only {drop_db:.1} dB lower");
        // other rates go through the resampler and come back the same length
        let x = noise(44_100 * 2, 9);
        assert_eq!(isolate(&x, 44_100, &|| false).unwrap().len(), x.len());
    }

    #[test]
    fn cancel_stops_it() {
        assert!(isolate(&noise(48_000 * 2, 3), 48_000, &|| true).is_none());
    }

    /// Real speech: `RCP_SPEECH_DIR` holding `clean.wav` and `noisy.wav`
    /// (mono or stereo f32 / 16-bit, 48 kHz). The cleaned voice must be
    /// closer to the clean take and line up with it sample for sample.
    #[test]
    fn speech_gets_cleaner_and_stays_in_sync() {
        let Some(dir) = std::env::var_os("RCP_SPEECH_DIR") else { return };
        let dir = std::path::PathBuf::from(dir);
        let read = |name: &str| -> Vec<f32> {
            let mut r = hound::WavReader::open(dir.join(name)).unwrap();
            let spec = r.spec();
            let mono: Vec<f32> = match spec.sample_format {
                hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
                hound::SampleFormat::Int => r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect(),
            };
            let mono: Vec<f32> = mono.chunks(spec.channels as usize).map(|c| c[0]).collect();
            mono.iter().flat_map(|v| [*v, *v]).collect()
        };
        let clean = read("clean.wav");
        let noisy = read("noisy.wav");
        let out = isolate(&noisy, 48_000, &|| false).unwrap();
        let left = |v: &[f32]| v.iter().step_by(2).copied().collect::<Vec<f32>>();
        let (c, n, o) = (left(&clean), left(&noisy), left(&out));
        let si_sdr = |e: &[f32], lag: isize| {
            let pairs = || (0..c.len()).filter_map(|i| e.get((i as isize + lag) as usize).map(|v| (*v as f64, c[i] as f64)));
            let a = pairs().map(|(x, y)| x * y).sum::<f64>() / pairs().map(|(_, y)| y * y).sum::<f64>();
            let (s, d) = pairs().fold((0.0, 0.0), |(s, d), (x, y)| (s + (a * y).powi(2), d + (x - a * y).powi(2)));
            10.0 * (s / d).log10()
        };
        let (before, after) = (si_sdr(&n, 0), si_sdr(&o, 0));
        eprintln!("SI-SDR {before:.1} dB -> {after:.1} dB");
        assert!(after > before + 3.0, "SI-SDR {before:.1} -> {after:.1}");
        for lag in [-480, -96, 96, 480] {
            assert!(si_sdr(&o, lag) < after, "better aligned at {lag} samples");
        }
    }
}
