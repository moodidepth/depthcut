//! High-resolution RMS / peak waveform extraction (parallel over the
//! 12-thread media pool).

use crate::frame::AudioBuffer;
use rayon::prelude::*;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Waveform {
    /// Audio frames per bucket.
    pub bucket_frames: usize,
    pub sample_rate: u32,
    /// Per-bucket RMS (mono mix), 0..1.
    pub rms: Vec<f32>,
    /// Per-bucket (min, max) of the mono mix.
    pub peaks: Vec<(f32, f32)>,
}

impl Waveform {
    /// Analyse `buf` with `buckets_per_second` resolution.
    pub fn compute(buf: &AudioBuffer, buckets_per_second: u32) -> Self {
        let ch = buf.channels.max(1) as usize;
        let bucket_frames = (buf.sample_rate / buckets_per_second.max(1)).max(1) as usize;
        let chunk = bucket_frames * ch;
        let (rms, peaks): (Vec<f32>, Vec<(f32, f32)>) = buf
            .samples
            .par_chunks(chunk)
            .map(|c| {
                let mut sum = 0.0f64;
                let (mut lo, mut hi) = (0.0f32, 0.0f32);
                let frames = c.len() / ch;
                for f in c.chunks_exact(ch) {
                    let m = f.iter().sum::<f32>() / ch as f32;
                    sum += (m * m) as f64;
                    lo = lo.min(m);
                    hi = hi.max(m);
                }
                (((sum / frames.max(1) as f64).sqrt()) as f32, (lo, hi))
            })
            .unzip();
        Waveform { bucket_frames, sample_rate: buf.sample_rate, rms, peaks }
    }

    pub fn buckets_per_second(&self) -> f64 {
        self.sample_rate as f64 / self.bucket_frames.max(1) as f64
    }

    /// Max RMS over the time range `[t0, t1)` seconds (for drawing at any zoom).
    pub fn rms_range(&self, t0: f64, t1: f64) -> f32 {
        let bps = self.buckets_per_second();
        let a = ((t0 * bps).floor().max(0.0) as usize).min(self.rms.len());
        let b = ((t1 * bps).ceil().max(0.0) as usize).clamp(a, self.rms.len());
        if a == b {
            return self.rms.get(a).copied().unwrap_or(0.0);
        }
        self.rms[a..b].iter().copied().fold(0.0, f32::max)
    }

    pub fn peak_range(&self, t0: f64, t1: f64) -> f32 {
        let bps = self.buckets_per_second();
        let a = ((t0 * bps).floor().max(0.0) as usize).min(self.peaks.len());
        let b = ((t1 * bps).ceil().max(0.0) as usize).clamp(a, self.peaks.len());
        self.peaks[a..b.max(a)].iter().map(|(l, h)| l.abs().max(h.abs())).fold(0.0, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_sine_and_silence() {
        let sr = 48_000;
        let mut s: Vec<f32> = (0..sr)
            .flat_map(|i| {
                let v = (i as f32 * 0.1).sin();
                [v, v]
            })
            .collect();
        s.extend(std::iter::repeat_n(0.0, sr as usize * 2));
        let w = Waveform::compute(&AudioBuffer::new(sr, 2, s), 100);
        assert_eq!(w.rms.len(), 200);
        assert!((w.rms[50] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01);
        assert_eq!(w.rms[150], 0.0);
        assert!((w.peak_range(0.0, 1.0) - 1.0).abs() < 0.01);
        assert!(w.rms_range(1.2, 1.8) < 1e-6);
    }
}
