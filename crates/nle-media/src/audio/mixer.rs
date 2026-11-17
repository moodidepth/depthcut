//! Timeline audio mixer: renders all audible clips (volume keyframes,
//! fades, pan, 3-band EQ, retiming, dialogue auto-ducking) to stereo.

use super::ducking::{DuckCurve, DuckEnvelope, Ducker, RmsAccumulator};
use super::eq::{BandKind, Biquad, ThreeBandEq};
use super::vocal::VocalClarity;
use crate::frame::AudioBuffer;
use nle_core::eval::AudioSpan;
use nle_core::{AssetId, AudioRole, ClipId, Time};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

const BLOCK: usize = 64;
/// WSOLA grain length (≈ 30 ms of output); hop is half.
const GRAIN_SECS: f64 = 0.030;
/// WSOLA search tolerance either side of the ideal grain position.
const WSOLA_TOL_SECS: f64 = 0.008;
/// Speed-path crossfade time constant (resample ⇄ pitch-preserved, and the
/// extreme slow-mo mute) so speed keyframes never click.
const PATH_XFADE_SECS: f32 = 0.015;

/// Master-bus soft-knee peak limiter: −1.0 dBFS ceiling, 4 dB knee
/// (gain reduction starts at −3 dBFS), instant attack so no sample ever
/// exceeds the ceiling, 80 ms release. No lookahead → zero latency, so the
/// exported audio stays sample-aligned with video.
pub struct MasterLimiter {
    env: f32,
    release: f32,
}

impl MasterLimiter {
    pub const CEILING_DB: f32 = -1.0;
    pub const KNEE_DB: f32 = 4.0;

    pub fn new(sample_rate: u32) -> Self {
        MasterLimiter { env: 1.0, release: (-1.0 / (0.080 * sample_rate as f32)).exp() }
    }

    pub fn ceiling() -> f32 {
        10f32.powf(Self::CEILING_DB / 20.0)
    }

    /// Static soft-knee curve (ratio ∞): linear gain for a sample peak.
    pub fn curve_gain(peak: f32) -> f32 {
        if peak <= 1e-9 {
            return 1.0;
        }
        let (t, w) = (Self::CEILING_DB, Self::KNEE_DB);
        let l = 20.0 * peak.log10();
        let y = if l < t - 0.5 * w {
            l
        } else if l < t + 0.5 * w {
            l - (l - t + 0.5 * w).powi(2) / (2.0 * w)
        } else {
            t
        };
        10f32.powf((y - l) / 20.0)
    }

    /// Limit one stereo frame in place (channel-linked).
    pub fn process(&mut self, frame: &mut [f32]) {
        let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let g = Self::curve_gain(peak);
        self.env = if g < self.env { g } else { g + (self.env - g) * self.release };
        let ceil = Self::ceiling();
        for s in frame {
            // env ≤ g always, so this clamp only guards float rounding
            *s = (*s * self.env).clamp(-ceil, ceil);
        }
    }

    pub fn reset(&mut self) {
        self.env = 1.0;
    }
}

/// Per-clip retime state: smoothed path weights + WSOLA grain history.
struct RetimeState {
    /// 0 = plain resample (pitch follows speed), 1 = pitch-preserved stretch.
    stretch: f32,
    /// 1 = audible, 0 = extreme slow-mo muted.
    live: f32,
    /// Recently placed grains: (grain index, chosen source position).
    grains: VecDeque<(i64, f64)>,
}

impl RetimeState {
    /// Source position of grain `g`: the ideal position nudged within ±`tol`
    /// to the offset whose waveform best continues grain `g-1` (WSOLA), so
    /// overlapping grains add in phase instead of comb-filtering.
    fn grain_pos(&mut self, g: i64, ideal: impl FnOnce() -> f64, src: &AudioBuffer, hop_src: f64, tol: i64, ovl: usize) -> f64 {
        if let Some(&(_, p)) = self.grains.iter().find(|(i, _)| *i == g) {
            return p;
        }
        let ideal = ideal();
        let p = match self.grains.iter().find(|(i, _)| *i == g - 1) {
            Some(&(_, prev)) => best_match(src, prev + hop_src, ideal, tol, ovl),
            None => ideal,
        };
        self.grains.push_back((g, p));
        while self.grains.len() > 4 {
            self.grains.pop_front();
        }
        p
    }
}

/// Offset in `ideal ± tol` maximising normalised cross-correlation with the
/// natural continuation at `natural` (mono, 4× decimated for speed).
fn best_match(src: &AudioBuffer, natural: f64, ideal: f64, tol: i64, ovl: usize) -> f64 {
    const DEC: usize = 4;
    let mono = |p: f64| {
        let (l, r) = src.stereo_at(p);
        l + r
    };
    let reference: Vec<f32> = (0..ovl / DEC).map(|j| mono(natural + (j * DEC) as f64)).collect();
    let ref_e: f32 = reference.iter().map(|v| v * v).sum();
    if ref_e < 1e-9 {
        return ideal;
    }
    let base = ideal.round();
    let (mut best, mut best_d) = (f32::MIN, 0i64);
    let mut d = -tol;
    while d <= tol {
        let (mut xy, mut yy) = (0.0f32, 0.0f32);
        for (j, x) in reference.iter().enumerate() {
            let y = mono(base + d as f64 + (j * DEC) as f64);
            xy += x * y;
            yy += y * y;
        }
        // normalised correlation, gently biased toward the ideal position
        let score = xy / (yy * ref_e).sqrt().max(1e-9) - 0.05 * (d.abs() as f32 / tol.max(1) as f32);
        if score > best {
            best = score;
            best_d = d;
        }
        d += 2;
    }
    base + best_d as f64
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub struct TimelineMixer {
    pub sample_rate: u32,
    spans: Vec<AudioSpan>,
    sources: HashMap<AssetId, Arc<AudioBuffer>>,
    eqs: HashMap<ClipId, ThreeBandEq>,
    lowpasses: HashMap<ClipId, Biquad>,
    sub_basses: HashMap<ClipId, Biquad>,
    duckers: HashMap<ClipId, Ducker>,
    /// Cached look-ahead dialogue envelope (shared with export); when absent
    /// the causal `duckers` are the fallback.
    duck_env: Option<Arc<DuckEnvelope>>,
    duck_curves: HashMap<ClipId, (nle_core::DuckingSettings, Arc<DuckCurve>)>,
    vocals: HashMap<ClipId, VocalClarity>,
    retime: HashMap<ClipId, RetimeState>,
    limiter: MasterLimiter,
    tmp: Vec<f32>,
    dialogue: Vec<f32>,
    ducked: Vec<(ClipId, Vec<f32>)>,
    pub master_gain: f32,
    /// Peak level per channel of the last rendered block (for meters).
    pub peaks: [f32; 2],
}

impl TimelineMixer {
    pub fn new(sample_rate: u32) -> Self {
        TimelineMixer {
            sample_rate,
            spans: Vec::new(),
            sources: HashMap::new(),
            eqs: HashMap::new(),
            lowpasses: HashMap::new(),
            sub_basses: HashMap::new(),
            duckers: HashMap::new(),
            duck_env: None,
            duck_curves: HashMap::new(),
            vocals: HashMap::new(),
            retime: HashMap::new(),
            limiter: MasterLimiter::new(sample_rate),
            tmp: Vec::new(),
            dialogue: Vec::new(),
            ducked: Vec::new(),
            master_gain: 1.0,
            peaks: [0.0; 2],
        }
    }

    pub fn set_spans(&mut self, spans: Vec<AudioSpan>) {
        if spans != self.spans {
            // edited retime/trim → cached grain positions are stale
            for st in self.retime.values_mut() {
                st.grains.clear();
            }
        }
        self.spans = spans;
    }

    pub fn spans(&self) -> &[AudioSpan] {
        &self.spans
    }

    /// Install (or clear) the cached dialogue envelope that drives ducking.
    pub fn set_duck_envelope(&mut self, env: Option<Arc<DuckEnvelope>>) {
        let same = match (&env, &self.duck_env) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.duck_env = env;
            self.duck_curves.clear();
        }
    }

    pub fn duck_envelope(&self) -> Option<&Arc<DuckEnvelope>> {
        self.duck_env.as_ref()
    }

    fn duck_curve(&mut self, clip: ClipId, settings: nle_core::DuckingSettings) -> Option<Arc<DuckCurve>> {
        let env = self.duck_env.as_ref()?;
        match self.duck_curves.get(&clip) {
            Some((s, c)) if *s == settings => Some(c.clone()),
            _ => {
                let c = Arc::new(env.gain_curve(&settings));
                self.duck_curves.insert(clip, (settings, c.clone()));
                Some(c)
            }
        }
    }

    pub fn set_source(&mut self, asset: AssetId, buf: Arc<AudioBuffer>) {
        self.sources.insert(asset, buf);
    }

    pub fn has_source(&self, asset: AssetId) -> bool {
        self.sources.contains_key(&asset)
    }

    /// Clear filter/ducker state (call after a seek).
    pub fn reset_state(&mut self) {
        self.eqs.clear();
        self.lowpasses.clear();
        self.sub_basses.clear();
        self.duckers.clear();
        self.vocals.clear();
        self.retime.clear();
        self.limiter.reset();
    }

    /// Render `out.len() / 2` stereo frames starting at timeline time
    /// `start`, with the timeline advancing `speed` seconds per second.
    pub fn render(&mut self, start: Time, speed: f64, out: &mut [f32]) {
        out.fill(0.0);
        let frames = out.len() / 2;
        if frames == 0 || speed == 0.0 {
            self.peaks = [0.0; 2];
            return;
        }
        let sr = self.sample_rate as f64;
        let t0 = start.as_secs_f64();
        let t1 = t0 + speed * frames as f64 / sr;
        let (lo, hi) = (t0.min(t1), t0.max(t1));

        self.dialogue.clear();
        self.dialogue.resize(out.len(), 0.0);
        let mut ducked = std::mem::take(&mut self.ducked);
        ducked.clear();
        let mut tmp = std::mem::take(&mut self.tmp);

        for span in &self.spans {
            if span.end().as_secs_f64() <= lo || span.start.as_secs_f64() >= hi {
                continue;
            }
            let Some(src) = self.sources.get(&span.asset) else { continue };
            tmp.clear();
            tmp.resize(out.len(), 0.0);
            let (s_start, s_end) = (span.start.as_secs_f64(), span.end().as_secs_f64());
            let src_rate = src.sample_rate as f64;
            let mut b = 0;
            while b < frames {
                let n = BLOCK.min(frames - b);
                let ta = t0 + speed * b as f64 / sr;
                let tb = t0 + speed * (b + n) as f64 / sr;
                let pa = span.source_time(Time::from_secs_f64(ta)).as_secs_f64() * src_rate;
                let pb = span.source_time(Time::from_secs_f64(tb)).as_secs_f64() * src_rate;
                let ga = span.gain_at(Time::from_secs_f64(ta));
                let gb = span.gain_at(Time::from_secs_f64(tb));
                let pan = span.pan_at(Time::from_secs_f64(ta));
                // Instantaneous clip speed for pitch-preservation & extreme slow-mo auto-mute.
                let local_t = Time::from_secs_f64((ta - span.local_origin.as_secs_f64()).max(0.0));
                let clip_speed = span.retime.as_ref().map(|r| r.speed_at(local_t).abs()).unwrap_or(1.0);
                let live_target = if span.params.mute_extreme_slowmo && clip_speed < 0.30 { 0.0 } else { 1.0 };
                // Pitch-preserved stretch fades in smoothly as speed leaves 1.0×
                // (no hard per-block switch → no clicks at speed keyframes).
                let stretch_target =
                    if span.params.preserve_pitch && span.retime.is_some() && clip_speed > 0.05 { smoothstep(0.02, 0.08, (clip_speed - 1.0).abs() as f32) } else { 0.0 };
                let xfade = 1.0 - (-1.0 / (PATH_XFADE_SECS * sr as f32)).exp();
                let st = self.retime.entry(span.clip).or_insert_with(|| RetimeState { stretch: stretch_target, live: live_target, grains: VecDeque::new() });
                let grain_len = (GRAIN_SECS * sr).round().max(64.0);
                let grain_step = 0.5 * grain_len;
                let pitch_step = src_rate / sr;
                let tol = (WSOLA_TOL_SECS * src_rate).round() as i64;
                let ovl = (grain_step * pitch_step).round() as usize;
                let origin = span.local_origin.as_secs_f64();
                // stereo balance: attenuate the opposite side
                let (pl, pr) = (if pan > 0.0 { 1.0 - pan } else { 1.0 }, if pan < 0.0 { 1.0 + pan } else { 1.0 });
                let mut lp = if span.params.lowpass_enabled {
                    let hz = span.params.lowpass_hz.at_f32(local_t).clamp(80.0, 22_000.0);
                    let q = span.params.lowpass_q.clamp(0.4, 8.0);
                    let filt = self.lowpasses.entry(span.clip).or_insert_with(|| Biquad::new(BandKind::LowPass, sr as f32, hz, 0.0, q));
                    filt.set(BandKind::LowPass, sr as f32, hz, 0.0, q);
                    Some(filt)
                } else {
                    None
                };
                for k in 0..n {
                    let t = ta + speed * k as f64 / sr;
                    if t < s_start || t >= s_end {
                        continue;
                    }
                    st.stretch += (stretch_target - st.stretch) * xfade;
                    st.live += (live_target - st.live) * xfade;
                    if st.stretch < 1e-3 {
                        st.stretch = 0.0;
                    } else if st.stretch > 0.999 {
                        st.stretch = 1.0;
                    }
                    if st.live < 1e-4 {
                        st.live = 0.0;
                    }
                    let (mut l, mut r) = (0.0f32, 0.0f32);
                    if st.live > 0.0 && st.stretch < 1.0 {
                        let f = k as f64 / n as f64;
                        let (a, b) = src.stereo_at(pa + (pb - pa) * f);
                        let w = 1.0 - st.stretch;
                        l += a * w;
                        r += b * w;
                    }
                    if st.live > 0.0 && st.stretch > 0.0 {
                        // WSOLA: two 50 %-overlapped Hann grains at native
                        // pitch (sin² windows sum to exactly 1).
                        let ts = t * sr;
                        let g0 = (ts / grain_step).floor() as i64;
                        let ideal = |g: i64| {
                            let tg = (g as f64 * grain_step / sr).max(origin);
                            span.source_time(Time::from_secs_f64(tg)).as_secs_f64() * src_rate
                        };
                        let hop = grain_step * pitch_step;
                        let p1 = st.grain_pos(g0 - 1, || ideal(g0 - 1), src, hop, tol, ovl);
                        let p0 = st.grain_pos(g0, || ideal(g0), src, hop, tol, ovl);
                        let off0 = ts - g0 as f64 * grain_step;
                        let w0 = (std::f64::consts::PI * off0 / grain_len).sin().powi(2) as f32;
                        let (l0, r0) = src.stereo_at(p0 + off0 * pitch_step);
                        let (l1, r1) = src.stereo_at(p1 + (off0 + grain_step) * pitch_step);
                        let w = st.stretch;
                        l += (l0 * w0 + l1 * (1.0 - w0)) * w;
                        r += (r0 * w0 + r1 * (1.0 - w0)) * w;
                    }
                    let (l, r) = (l * st.live, r * st.live);
                    let f = k as f64 / n as f64;
                    let g = ga + (gb - ga) * f as f32;
                    let mut sl = l * g * pl;
                    let mut sr_ch = r * g * pr;
                    if let Some(filt) = lp.as_deref_mut() {
                        sl = filt.process(0, sl);
                        sr_ch = filt.process(1, sr_ch);
                    }
                    let i = 2 * (b + k);
                    tmp[i] = sl;
                    tmp[i + 1] = sr_ch;
                }
                b += n;
            }
            let eq = self.eqs.entry(span.clip).or_insert_with(|| ThreeBandEq::new(self.sample_rate as f32, span.params.eq));
            eq.update(&span.params.eq);
            eq.process(&mut tmp);
            if span.params.sub_bass_punch && span.params.sub_bass_db.abs() > 0.05 {
                let gain_db = span.params.sub_bass_db.clamp(-12.0, 15.0);
                let sb = self.sub_basses.entry(span.clip).or_insert_with(|| Biquad::new(BandKind::LowShelf, self.sample_rate as f32, 80.0, gain_db, 0.707));
                sb.set(BandKind::LowShelf, self.sample_rate as f32, 80.0, gain_db, 0.707);
                for frame in tmp.as_chunks_mut::<2>().0 {
                    frame[0] = sb.process(0, frame[0]);
                    frame[1] = sb.process(1, frame[1]);
                }
            }
            if span.params.vocal_clarity {
                let sr = self.sample_rate as f32;
                self.vocals.entry(span.clip).or_insert_with(|| VocalClarity::new(sr)).process(&mut tmp);
            }

            if span.params.role == AudioRole::Dialogue {
                for (d, s) in self.dialogue.iter_mut().zip(&tmp) {
                    *d += s;
                }
            } else if span.params.ducking.enabled {
                ducked.push((span.clip, tmp.clone()));
            } else {
                for (o, s) in out.iter_mut().zip(&tmp) {
                    *o += s;
                }
            }
        }

        // dialogue bus + sidechain ducking
        for (o, d) in out.iter_mut().zip(&self.dialogue) {
            *o += d;
        }
        for (clip, buf) in &ducked {
            let settings = self.spans.iter().find(|s| s.clip == *clip).map(|s| s.params.ducking).unwrap_or_default();
            if let Some(curve) = self.duck_curve(*clip, settings) {
                // look-ahead curve on the timeline clock: identical in preview and export
                for (i, frame) in buf.as_chunks::<2>().0.iter().enumerate() {
                    let g = curve.gain_at(t0 + speed * i as f64 / sr);
                    out[2 * i] += frame[0] * g;
                    out[2 * i + 1] += frame[1] * g;
                }
                continue;
            }
            let ducker = self.duckers.entry(*clip).or_insert_with(|| Ducker::new(self.sample_rate as f32, settings));
            ducker.update(&settings);
            for (i, frame) in buf.as_chunks::<2>().0.iter().enumerate() {
                let side = 0.5 * (self.dialogue[2 * i] + self.dialogue[2 * i + 1]);
                let g = ducker.process(side);
                out[2 * i] += frame[0] * g;
                out[2 * i + 1] += frame[1] * g;
            }
        }

        // Master bus: always-on −1 dBFS soft-knee limiter (playback + export).
        let mut peaks = [0.0f32; 2];
        for frame in out.as_chunks_mut::<2>().0 {
            for s in frame.iter_mut() {
                *s = if s.is_finite() { *s * self.master_gain } else { 0.0 };
            }
            self.limiter.process(frame);
            peaks[0] = peaks[0].max(frame[0].abs());
            peaks[1] = peaks[1].max(frame[1].abs());
        }
        self.peaks = peaks;
        self.tmp = tmp;
        self.ducked = ducked;
    }
}

/// Whether any span ducks under dialogue (so an envelope is worth computing).
pub fn needs_duck_envelope(spans: &[AudioSpan]) -> bool {
    spans.iter().any(|s| s.params.ducking.enabled && s.params.role != AudioRole::Dialogue)
}

/// The unmuted dialogue spans — the ducking sidechain (muted / disabled
/// clips never produce spans).
pub fn dialogue_spans(spans: &[AudioSpan]) -> Vec<AudioSpan> {
    spans.iter().filter(|s| s.params.role == AudioRole::Dialogue && !s.params.muted).cloned().collect()
}

/// 10 ms RMS envelope of the dialogue bus over `[0, end)`. Streams in
/// chunks (no full-length buffer), so it is cheap enough to rerun after edits.
pub fn dialogue_envelope(spans: &[AudioSpan], sources: &HashMap<AssetId, Arc<AudioBuffer>>, end: Time, cancel: &dyn Fn() -> bool) -> Option<DuckEnvelope> {
    let rate = super::resample::INTERNAL_RATE;
    let mut m = TimelineMixer::new(rate);
    m.set_spans(dialogue_spans(spans));
    for s in m.spans.clone() {
        if let Some(b) = sources.get(&s.asset) {
            m.set_source(s.asset, b.clone());
        }
    }
    let frames = end.max(Time::ZERO).samples_round(rate) as usize;
    let mut acc = RmsAccumulator::new(rate);
    const CHUNK: usize = 9_600;
    let mut buf = vec![0.0f32; CHUNK * 2];
    let mut i = 0;
    while i < frames {
        if cancel() {
            return None;
        }
        let n = CHUNK.min(frames - i);
        let out = &mut buf[..n * 2];
        out.fill(0.0);
        if m.spans.iter().any(|s| s.start < Time::from_samples((i + n) as i64, rate) && s.end() > Time::from_samples(i as i64, rate)) {
            m.render(Time::from_samples(i as i64, rate), 1.0, out);
        }
        acc.push_stereo(out);
        i += n;
    }
    Some(acc.finish())
}

/// Render `[start, end)` of the timeline offline to 48 kHz stereo.
pub fn mixdown(spans: Vec<AudioSpan>, sources: &HashMap<AssetId, Arc<AudioBuffer>>, start: Time, end: Time) -> Vec<f32> {
    let mut m = TimelineMixer::new(super::resample::INTERNAL_RATE);
    m.set_spans(spans);
    for (id, b) in sources {
        m.set_source(*id, b.clone());
    }
    let total = (end - start).max(Time::ZERO).samples_round(m.sample_rate) as usize;
    let mut out = vec![0.0f32; total * 2];
    let chunk = m.sample_rate as usize; // 1 s
    let mut done = 0;
    while done < total {
        let n = chunk.min(total - done);
        let t = start + Time::from_samples(done as i64, m.sample_rate);
        m.render(t, 1.0, &mut out[done * 2..(done + n) * 2]);
        done += n;
    }
    out
}

/// 48 kHz stereo → 16 kHz mono (Whisper's input format).
pub fn to_mono_16k(stereo48: &[f32]) -> Vec<f32> {
    let down = super::resample::resample_stereo(stereo48, super::resample::INTERNAL_RATE, 16_000);
    down.as_chunks::<2>().0.iter().map(|f| 0.5 * (f[0] + f[1])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::eq::soft_limit;
    use nle_core::{Clip, MediaAsset, Timeline};

    fn tone(secs: f64, amp: f32) -> Arc<AudioBuffer> {
        let n = (48_000.0 * secs) as usize;
        Arc::new(AudioBuffer::new(
            48_000,
            2,
            (0..n)
                .flat_map(|i| {
                    let v = (i as f32 * 0.06).sin() * amp;
                    [v, v]
                })
                .collect(),
        ))
    }

    #[test]
    fn mixes_and_ducks_music_under_dialogue() {
        let mut voice = MediaAsset::new("voice", "/v.wav", Time::from_secs(2));
        voice.audio = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut music = MediaAsset::new("music", "/m.wav", Time::from_secs(4));
        music.audio = voice.audio.clone();
        let mut tl = Timeline::default();
        tl.append(Clip::gap(Time::from_secs(1))).unwrap();
        let mut v = Clip::from_asset(&voice);
        v.audio.role = AudioRole::Dialogue; // audio-only assets default to Music
        tl.append(v).unwrap();
        tl.append(Clip::gap(Time::from_secs(1))).unwrap();
        let mut m = Clip::from_asset(&music);
        m.audio.ducking = nle_core::DuckingSettings { enabled: true, amount_db: 20.0, threshold_db: -40.0, attack_ms: 20.0, release_ms: 100.0, ..Default::default() };
        tl.connect(m, Time::ZERO, -1).unwrap();

        let mut mixer = TimelineMixer::new(48_000);
        mixer.set_spans(nle_core::audio_spans(&tl));
        mixer.set_source(voice.id, tone(2.0, 0.0001)); // near-silent "voice" first
        mixer.set_source(music.id, tone(4.0, 0.5));

        let mut out = vec![0.0f32; 48_000 * 2 * 4];
        mixer.render(Time::ZERO, 1.0, &mut out);
        let rms = |a: usize, b: usize| (out[a * 2..b * 2].iter().map(|s| s * s).sum::<f32>() / ((b - a) * 2) as f32).sqrt();
        let music_alone = rms(10_000, 40_000);
        assert!(music_alone > 0.3, "music audible: {music_alone}");

        // now a loud voice: music must duck by ~20 dB while it talks
        mixer.set_source(voice.id, tone(2.0, 0.5));
        mixer.reset_state();
        let mut out2 = vec![0.0f32; 48_000 * 2 * 4];
        mixer.render(Time::ZERO, 1.0, &mut out2);
        // voice-only reference
        let mut solo = TimelineMixer::new(48_000);
        let mut tl2 = tl.clone();
        tl2.spine[0].connected.clear();
        solo.set_spans(nle_core::audio_spans(&tl2));
        solo.set_source(voice.id, tone(2.0, 0.5));
        let mut v_only = vec![0.0f32; 48_000 * 2 * 4];
        solo.render(Time::ZERO, 1.0, &mut v_only);
        let residual: f32 = (60_000..130_000).map(|i| (out2[2 * i] - v_only[2 * i]).powi(2)).sum::<f32>() / 70_000.0;
        let residual_rms = residual.sqrt();
        assert!(residual_rms < music_alone * 0.15, "music ducked: {residual_rms} vs {music_alone}");
    }

    #[test]
    fn offline_mixdown_and_16k_conversion() {
        let mut a = MediaAsset::new("tone", "/t.wav", Time::from_secs(2));
        a.audio = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut tl = Timeline::default();
        tl.append(Clip::gap(Time::from_secs(1))).unwrap();
        let mut c = Clip::from_asset(&a);
        c.audio.vocal_clarity = true;
        tl.append(c).unwrap();
        let sources: HashMap<AssetId, Arc<AudioBuffer>> = [(a.id, tone(2.0, 0.3))].into_iter().collect();
        let mix = mixdown(nle_core::audio_spans(&tl), &sources, Time::ZERO, Time::from_secs(3));
        assert_eq!(mix.len(), 48_000 * 2 * 3);
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        assert!(rms(&mix[..48_000 * 2 - 2_000]) < 1e-4, "gap is silent");
        assert!(rms(&mix[48_000 * 2 + 4_000..]) > 0.1, "clip audible");
        let m16 = to_mono_16k(&mix);
        assert_eq!(m16.len(), 16_000 * 3);
        assert!(rms(&m16[16_000 + 1_000..]) > 0.1);
    }

    #[test]
    fn underwater_lowpass_sweep_subbass_limiter_and_slowmo_mute() {
        let n = 48_000usize;
        // 5 kHz bright signal + 60 Hz sub-bass component
        let bright = Arc::new(AudioBuffer::new(
            48_000,
            2,
            (0..n)
                .flat_map(|i| {
                    let v = (std::f32::consts::TAU * 5_000.0 * i as f32 / 48_000.0).sin() * 0.4;
                    [v, v]
                })
                .collect(),
        ));
        let mut a = MediaAsset::new("fx", "/fx.wav", Time::from_secs(1));
        a.audio = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut c = Clip::from_asset(&a);
        c.audio.apply_pre_kill_underwater_sweep(Time::from_millis(800));
        c.audio.loudness_limiter = true;
        let mut tl = Timeline::default();
        tl.append(c).unwrap();
        let mut mixer = TimelineMixer::new(48_000);
        mixer.set_spans(nle_core::audio_spans(&tl));
        mixer.set_source(a.id, bright.clone());
        let mut out = vec![0.0f32; n * 2];
        mixer.render(Time::ZERO, 1.0, &mut out);
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        // Muffled window around 700-760ms should heavily attenuate the 5 kHz tone, then open at 800-950ms
        let muffled_rms = rms(&out[33_600 * 2..36_500 * 2]);
        let open_rms = rms(&out[40_000 * 2..46_000 * 2]);
        assert!(muffled_rms < open_rms * 0.2, "lowpass sweep muffled {muffled_rms} vs open {open_rms}");

        // Extreme slow-mo auto-mute (< 30% speed) + sub-bass limiter bound check
        let mut tl2 = Timeline::default();
        let mut c2 = Clip::from_asset(&a);
        c2.retime = Some(nle_core::Retime::uniform(Time::from_secs(1), 0.15));
        c2.duration = Time::from_secs(1);
        c2.audio.mute_extreme_slowmo = true;
        tl2.append(c2).unwrap();
        mixer.reset_state();
        mixer.set_spans(nle_core::audio_spans(&tl2));
        let mut muted = vec![0.0f32; n * 2];
        mixer.render(Time::ZERO, 1.0, &mut muted);
        assert!(rms(&muted) < 1e-5, "extreme slow-mo muted cleanly");

        assert!(soft_limit(5.0, 0.944) <= 1.0);
    }

    fn sine(secs: f64, hz: f32, amp: f32) -> Arc<AudioBuffer> {
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

    fn render_clip(c: Clip, a: &MediaAsset, buf: Arc<AudioBuffer>, gain: f32, secs: f64) -> Vec<f32> {
        let mut tl = Timeline::default();
        tl.append(c).unwrap();
        let mut m = TimelineMixer::new(48_000);
        m.master_gain = gain;
        m.set_spans(nle_core::audio_spans(&tl));
        m.set_source(a.id, buf);
        let mut out = vec![0.0f32; (48_000.0 * secs) as usize * 2];
        // playback-sized chunks: state must carry across render calls
        let mut done = 0;
        while done < out.len() / 2 {
            let n = 1_024.min(out.len() / 2 - done);
            m.render(Time::from_samples(done as i64, 48_000), 1.0, &mut out[done * 2..(done + n) * 2]);
            done += n;
        }
        out
    }

    fn asset(secs: i64) -> MediaAsset {
        let mut a = MediaAsset::new("sine", "/s.wav", Time::from_secs(secs));
        a.audio = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        a
    }

    #[test]
    fn master_limiter_holds_minus_1_dbfs_and_is_transparent_below_knee() {
        let a = asset(2);
        let ceil = MasterLimiter::ceiling();
        // +6 dBFS hot mix → never above −1 dBFS, still loud (not pumped to nothing)
        let hot = render_clip(Clip::from_asset(&a), &a, sine(2.0, 220.0, 0.5), 4.0, 1.0);
        let peak = hot.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= ceil + 1e-6, "peak {peak} above −1 dBFS ({ceil})");
        let rms = (hot.iter().map(|s| s * s).sum::<f32>() / hot.len() as f32).sqrt();
        assert!(rms > 0.5, "limited mix stays loud: {rms}");
        // −12 dBFS program: bit-transparent (below the −3 dBFS knee)
        let quiet = render_clip(Clip::from_asset(&a), &a, sine(2.0, 220.0, 0.25), 1.0, 1.0);
        for (i, s) in quiet.as_chunks::<2>().0.iter().enumerate().skip(10) {
            let want = (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin() * 0.25;
            assert!((s[0] - want).abs() < 1e-4, "sample {i}: {} vs {want}", s[0]);
        }
    }

    /// Speed keyframe 1.0× → 0.5× (Hold) with pitch preservation: no click
    /// at the boundary and the slowed section keeps its 440 Hz pitch.
    #[test]
    fn speed_ramp_preserves_pitch_without_clicks() {
        use nle_core::keyframe::{Curve, Keyframe};
        use nle_core::{FrameBlendMode, Interpolation, Retime};
        let a = asset(3);
        let src = sine(3.0, 440.0, 0.5);
        let ramp = |preserve: bool| {
            let mut curve = Curve::new();
            curve.insert(Keyframe::new(Time::ZERO, 1.0).with_interp(Interpolation::Hold));
            curve.insert(Keyframe::new(Time::from_millis(500), 0.5).with_interp(Interpolation::Hold));
            curve.insert(Keyframe::new(Time::from_secs(2), 0.5).with_interp(Interpolation::Hold));
            let mut c = Clip::from_asset(&a);
            c.retime = Some(Retime::from_velocity_curve(Time::from_secs(2), curve, FrameBlendMode::FloorFrame));
            c.duration = Time::from_secs(2);
            c.audio.preserve_pitch = preserve;
            render_clip(c, &a, src.clone(), 1.0, 1.5)
        };
        let hz = |x: &[f32], from: usize, to: usize| {
            let zc = (from + 1..to).filter(|&i| (x[2 * i - 2] < 0.0) != (x[2 * i] < 0.0)).count();
            zc as f32 * 0.5 / ((to - from) as f32 / 48_000.0)
        };
        let out = ramp(true);
        // largest sample-to-sample step of a clean 440 Hz, 0.5 sine is 0.029
        let max_step = (1_000..70_000).map(|i| (out[2 * i] - out[2 * i - 2]).abs()).fold(0.0f32, f32::max);
        assert!(max_step < 0.032, "click at the speed keyframe: step {max_step}");
        let slowed = hz(&out, 30_000, 70_000);
        assert!((slowed - 440.0).abs() < 15.0, "pitch preserved at 0.5×: {slowed} Hz");
        let rms = (out[60_000..140_000].iter().map(|s| s * s).sum::<f32>() / 80_000.0).sqrt();
        assert!(rms > 0.3, "no WSOLA cancellation: rms {rms}");
        // plain resample drops an octave (sanity check of the measurement)
        let plain = hz(&ramp(false), 30_000, 70_000);
        assert!((plain - 220.0).abs() < 15.0, "resample path: {plain} Hz");
    }

    /// Look-ahead ducking from the cached envelope: the music dips −14 dB
    /// under the voice, starts *before* the first word, and the preview
    /// (small chunks) and export (4800-sample chunks) renders are identical.
    #[test]
    fn envelope_ducking_matches_between_preview_and_export() {
        let voice = asset(2);
        let music = asset(4);
        let mut tl = Timeline::default();
        tl.append(Clip::gap(Time::from_secs(1))).unwrap();
        let mut v = Clip::from_asset(&voice);
        v.audio.role = AudioRole::Dialogue;
        tl.append(v).unwrap();
        tl.append(Clip::gap(Time::from_secs(1))).unwrap();
        let m = Clip::from_asset(&music);
        assert_eq!(m.audio.role, AudioRole::Music, "audio-only files default to Music");
        assert!(m.audio.ducking.enabled, "music auto-ducks by default");
        tl.connect(m, Time::ZERO, -1).unwrap();
        let sources: HashMap<AssetId, Arc<AudioBuffer>> = [(voice.id, sine(2.0, 300.0, 0.2)), (music.id, sine(4.0, 110.0, 0.2))].into_iter().collect();
        let spans = nle_core::audio_spans(&tl);
        assert!(needs_duck_envelope(&spans));
        let env = Arc::new(dialogue_envelope(&spans, &sources, Time::from_secs(3), &|| false).unwrap());
        assert_eq!(env.rms.len(), 300, "voice ends at 3 s → 300 frames");

        let render = |tl: &Timeline, env: Option<Arc<DuckEnvelope>>, chunk: usize| {
            let mut mx = TimelineMixer::new(48_000);
            mx.set_duck_envelope(env);
            mx.set_spans(nle_core::audio_spans(tl));
            for (id, b) in &sources {
                mx.set_source(*id, b.clone());
            }
            let mut out = vec![0.0f32; 48_000 * 4 * 2];
            let mut done = 0;
            while done < out.len() / 2 {
                let n = chunk.min(out.len() / 2 - done);
                mx.render(Time::from_samples(done as i64, 48_000), 1.0, &mut out[done * 2..(done + n) * 2]);
                done += n;
            }
            out
        };
        let preview = render(&tl, Some(env.clone()), 1_024);
        let export = render(&tl, Some(env.clone()), 4_800);
        let diff = preview.iter().zip(&export).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(diff < 1e-5, "preview and export ducking differ by {diff}");

        // isolate the music: full mix minus voice-only
        let mut voice_tl = tl.clone();
        voice_tl.spine[0].connected.clear();
        let v_only = render(&voice_tl, None, 4_800);
        let mut music_tl = tl.clone();
        music_tl.spine[0].connected[0].audio.ducking.enabled = false;
        let undamped: Vec<f32> = render(&music_tl, None, 4_800).iter().zip(&v_only).map(|(a, b)| a - b).collect();
        let music: Vec<f32> = preview.iter().zip(&v_only).map(|(a, b)| a - b).collect();
        let rms = |x: &[f32], a: f64, b: f64| {
            let (i, j) = ((a * 48_000.0) as usize * 2, (b * 48_000.0) as usize * 2);
            (x[i..j].iter().map(|s| s * s).sum::<f32>() / (j - i) as f32).sqrt()
        };
        let ratio = |a, b| rms(&music, a, b) / rms(&undamped, a, b);
        assert!((ratio(0.2, 0.8) - 1.0).abs() < 0.01, "untouched before lookahead");
        assert!(ratio(0.95, 1.0) < 0.6, "ducks ahead of the first word: {}", ratio(0.95, 1.0));
        let deep = ratio(1.2, 2.8);
        assert!((deep - nle_core::clip::db_to_gain(-14.0)).abs() < 0.02, "−14 dB under voice: {deep}");
        assert!((ratio(3.7, 3.95) - 1.0).abs() < 0.01, "released after hold + release");
    }
}
