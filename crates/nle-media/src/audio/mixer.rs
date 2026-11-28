//! Timeline audio mixer: renders all audible clips (volume keyframes,
//! fades, pan, retiming, parametric EQ, dynamics / creative inserts,
//! dialogue leveling, auto-ducking), routes them to the role busses
//! (Dialogue, Music, SFX, General — each with EQ, inserts, fader, pan,
//! mute / solo and loudness meters) and sums those into the master bus.
//!
//! Per clip: source (or its cleaned [`Restored`] copy) → retime → gain /
//! pan → low-pass → EQ → sub-bass → vocal clarity → leveler → inserts →
//! ducking → user bus (optional) → role bus. Master: EQ → inserts → fader →
//! preview volume → −1 dBFS limiter. Bus faders and balance follow their
//! automation on the timeline clock.

use super::ducking::{DuckCurve, DuckEnvelope, Ducker, RmsAccumulator};
use super::eq::{BandKind, Biquad, ParametricEqProc};
use super::fx::{FxChain, GainReduction, Leveler};
use super::meter::{LoudnessMeter, MeterReading};
use super::plugins::PluginInstance;
use super::restore::{restore_key, RestoreKey, Restored};
use super::vocal::VocalClarity;
use crate::frame::AudioBuffer;
use nle_core::eval::AudioSpan;
use nle_core::{AssetId, AudioRole, BusId, BusSettings, ClipId, MixerSettings, Time, BUS_SLOTS, MAX_USER_BUSSES};
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
    eqs: HashMap<ClipId, ParametricEqProc>,
    clip_fx: HashMap<ClipId, FxChain>,
    levelers: HashMap<ClipId, Leveler>,
    /// Cleaned (noise-reduced / voice-isolated) source ranges per clip.
    restored: HashMap<ClipId, Restored>,
    /// Restored buffers serving the current spans: (buffer, first source frame).
    active_restored: HashMap<ClipId, (Arc<AudioBuffer>, f64)>,
    mix: MixerSettings,
    /// Honour bus solo (playback monitoring only; export ignores solo).
    pub honor_solo: bool,
    bus_bufs: [Vec<f32>; 4],
    /// Inputs of the user busses (slot − 5).
    user_bufs: Vec<Vec<f32>>,
    /// Per mixer slot ([`BUS_SLOTS`]: [`BusId::ALL`], then user busses).
    bus_eq: Vec<ParametricEqProc>,
    bus_fx: Vec<FxChain>,
    /// CLAP inserts per mixer slot, matching that bus's `plugins` slot for
    /// slot; `None` = failed to load (passed through).
    bus_plugins: [Vec<PluginSlot>; BUS_SLOTS],
    /// Smoothed per-bus (L, R) gain so fader moves never zipper.
    bus_gain: [[f32; 2]; BUS_SLOTS],
    /// Meters per mixer slot when metering is on.
    meters: Option<Vec<LoudnessMeter>>,
    gr: [GainReduction; BUS_SLOTS],
    /// Render only this bus, post-fader and without the master chain (stem export).
    stem: Option<StemTap>,
    stem_buf: Vec<f32>,
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
            clip_fx: HashMap::new(),
            levelers: HashMap::new(),
            restored: HashMap::new(),
            active_restored: HashMap::new(),
            mix: MixerSettings::default(),
            honor_solo: false,
            bus_bufs: Default::default(),
            user_bufs: vec![Vec::new(); MAX_USER_BUSSES],
            bus_eq: (0..BUS_SLOTS).map(|_| ParametricEqProc::new(sample_rate as f32, Default::default())).collect(),
            bus_fx: (0..BUS_SLOTS).map(|_| FxChain::new(sample_rate as f32)).collect(),
            bus_plugins: Default::default(),
            bus_gain: [[1.0; 2]; BUS_SLOTS],
            meters: None,
            gr: Default::default(),
            stem: None,
            stem_buf: Vec::new(),
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
        self.refresh_restored();
    }

    /// Bus settings (faders, mute / solo, bus EQ and inserts).
    ///
    /// CLAP inserts are created / destroyed here, on the caller's thread.
    pub fn set_mixer(&mut self, mix: &MixerSettings) {
        if self.mix == *mix {
            return;
        }
        self.mix = mix.clone();
        for b in 0..BUS_SLOTS {
            let empty = Vec::new();
            let wanted = self.mix.slot(b).map_or(&empty, |s| &s.plugins);
            let slots = &mut self.bus_plugins[b];
            slots.truncate(wanted.len());
            for (k, ins) in wanted.iter().enumerate() {
                let key = (ins.path.clone(), ins.id.clone());
                if slots.get(k).is_none_or(|s| s.key != key) {
                    let inst =
                        PluginInstance::load(std::path::Path::new(&ins.path), &ins.id, self.sample_rate).map_err(|e| log::warn!("plug-in insert on mixer slot {b}: {e}")).ok();
                    let slot = PluginSlot { key, inst };
                    if k < slots.len() {
                        slots[k] = slot;
                    } else {
                        slots.push(slot);
                    }
                }
                if let Some(inst) = slots[k].inst.as_mut() {
                    inst.set_params(&ins.params);
                }
            }
        }
    }

    pub fn mixer(&self) -> &MixerSettings {
        &self.mix
    }

    /// Turn loudness metering on (playback) or off (export / analysis).
    pub fn set_metering(&mut self, on: bool) {
        match (on, self.meters.is_some()) {
            (true, false) => self.meters = Some((0..BUS_SLOTS).map(|_| LoudnessMeter::new(self.sample_rate)).collect()),
            (false, true) => self.meters = None,
            _ => {}
        }
    }

    /// Current meter readings per mixer slot ([`BUS_SLOTS`]); resets the
    /// per-reading peaks and gain-reduction maxima.
    pub fn meter_readings(&mut self) -> Option<[MeterReading; BUS_SLOTS]> {
        let meters = self.meters.as_mut()?;
        let mut out = [MeterReading::default(); BUS_SLOTS];
        for (i, m) in meters.iter_mut().enumerate() {
            out[i] = m.read();
            out[i].gr = std::mem::take(&mut self.gr[i]);
        }
        Some(out)
    }

    /// Restart integrated loudness / true-peak hold on every bus.
    pub fn reset_loudness(&mut self) {
        for m in self.meters.iter_mut().flatten() {
            m.reset_loudness();
        }
    }

    /// Install a cleaned source range for `clip` (see [`super::restore`]).
    pub fn set_restored(&mut self, clip: ClipId, r: Restored) {
        self.restored.insert(clip, r);
        self.refresh_restored();
    }

    /// Restorations the current spans need that are not installed yet.
    pub fn restore_requests(&self) -> Vec<(ClipId, RestoreKey)> {
        restore_requests(&self.spans, &self.sources).into_iter().filter(|(c, k)| !self.restored.get(c).is_some_and(|r| r.key.serves(k))).collect()
    }

    fn refresh_restored(&mut self) {
        let mut active: HashMap<ClipId, (Arc<AudioBuffer>, f64)> = HashMap::new();
        let mut stale = Vec::new();
        for span in &self.spans {
            let Some(src) = self.sources.get(&span.asset) else { continue };
            let Some(want) = restore_key(span, src) else { continue };
            match self.restored.get(&span.clip) {
                Some(r) if r.key.serves(&want) => {
                    active.entry(span.clip).or_insert((r.buf.clone(), r.key.start as f64));
                }
                _ => stale.push(span.clip),
            }
        }
        for c in stale {
            active.remove(&c);
        }
        // a different buffer means different sample positions for cached grains
        for (c, st) in self.retime.iter_mut() {
            let before = self.active_restored.get(c).map(|(b, _)| Arc::as_ptr(b));
            let after = active.get(c).map(|(b, _)| Arc::as_ptr(b));
            if before != after {
                st.grains.clear();
            }
        }
        // drop restorations no span uses any more (keeps memory bounded)
        let used: std::collections::HashSet<ClipId> = self.spans.iter().map(|s| s.clip).collect();
        self.restored.retain(|c, _| used.contains(c));
        self.active_restored = active;
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
        self.refresh_restored();
    }

    /// Drop one asset's audio and the restorations rendered from it (another
    /// audio track of the file was picked); its clips are silent until the new
    /// decode arrives through [`Self::set_source`].
    pub fn remove_source(&mut self, asset: AssetId) {
        self.sources.remove(&asset);
        self.restored.retain(|_, r| r.key.asset != asset);
        self.refresh_restored();
    }

    /// Drop all audio of the project being left: its ids are reused by the next.
    pub fn clear_sources(&mut self) {
        self.sources.clear();
        self.restored.clear();
        self.duck_curves.clear();
        self.refresh_restored();
    }

    pub fn has_source(&self, asset: AssetId) -> bool {
        self.sources.contains_key(&asset)
    }

    /// Clear filter/ducker state (call after a seek).
    pub fn reset_state(&mut self) {
        self.eqs.clear();
        self.clip_fx.clear();
        self.levelers.clear();
        for f in &mut self.bus_fx {
            f.reset();
        }
        for e in &mut self.bus_eq {
            e.reset();
        }
        for p in self.bus_plugins.iter_mut().flatten().filter_map(|s| s.inst.as_mut()) {
            p.reset();
        }
        for m in self.meters.iter_mut().flatten() {
            m.reset_windows();
        }
        self.lowpasses.clear();
        self.sub_basses.clear();
        self.duckers.clear();
        self.vocals.clear();
        self.retime.clear();
        self.limiter.reset();
    }

    /// Render one bus only (post-fader, no master chain) or the full mix (`None`).
    pub fn set_stem(&mut self, stem: Option<StemTap>) {
        self.stem = stem;
    }

    /// A block of scrub audio. It stays out of the loudness measurement.
    pub(crate) fn render_scrub(&mut self, at: Time, out: &mut [f32]) {
        let meters = self.meters.take();
        self.render(at, 1.0, out);
        self.meters = meters;
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
            let (src, src_off) = match self.active_restored.get(&span.clip) {
                Some((b, off)) => (b, *off),
                None => match self.sources.get(&span.asset) {
                    Some(s) => (s, 0.0),
                    None => continue,
                },
            };
            tmp.clear();
            tmp.resize(out.len(), 0.0);
            let (s_start, s_end) = (span.start.as_secs_f64(), span.end().as_secs_f64());
            let src_rate = src.sample_rate as f64;
            let mut b = 0;
            while b < frames {
                let n = BLOCK.min(frames - b);
                let ta = t0 + speed * b as f64 / sr;
                let tb = t0 + speed * (b + n) as f64 / sr;
                let pa = span.source_time(Time::from_secs_f64(ta)).as_secs_f64() * src_rate - src_off;
                let pb = span.source_time(Time::from_secs_f64(tb)).as_secs_f64() * src_rate - src_off;
                let ga = span.gain_at(Time::from_secs_f64(ta));
                let gb = span.gain_at(Time::from_secs_f64(tb));
                let (pl, pr) = span.balance_at(Time::from_secs_f64(ta));
                // Instantaneous clip speed for pitch-preservation & extreme slow-mo auto-mute.
                let local_t = span.local_time(Time::from_secs_f64(ta)).max(Time::ZERO);
                let clip_speed = span.speed_at(Time::from_secs_f64(ta)).abs();
                let live_target = if span.params.mute_extreme_slowmo && clip_speed < 0.30 { 0.0 } else { 1.0 };
                // Pitch-preserved stretch fades in smoothly as speed leaves 1.0×
                // (no hard per-block switch → no clicks at speed keyframes).
                let stretch_target =
                    if span.params.preserve_pitch && span.is_retimed() && clip_speed > 0.05 { smoothstep(0.02, 0.08, (clip_speed - 1.0).abs() as f32) } else { 0.0 };
                let xfade = 1.0 - (-1.0 / (PATH_XFADE_SECS * sr as f32)).exp();
                let st = self.retime.entry(span.clip).or_insert_with(|| RetimeState { stretch: stretch_target, live: live_target, grains: VecDeque::new() });
                let grain_len = (GRAIN_SECS * sr).round().max(64.0);
                let grain_step = 0.5 * grain_len;
                let pitch_step = src_rate / sr;
                let tol = (WSOLA_TOL_SECS * src_rate).round() as i64;
                let ovl = (grain_step * pitch_step).round() as usize;
                let origin = span.stretch_origin().as_secs_f64();
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
                            span.source_time(Time::from_secs_f64(tg)).as_secs_f64() * src_rate - src_off
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
            let eq = self.eqs.entry(span.clip).or_insert_with(|| ParametricEqProc::new(self.sample_rate as f32, span.params.eq));
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
            if span.params.leveler.enabled {
                let lv = self.levelers.entry(span.clip).or_insert_with(|| Leveler::new(self.sample_rate as f32, span.params.leveler));
                lv.update(&span.params.leveler);
                lv.process(&mut tmp);
            }
            if span.params.fx.is_active() {
                self.clip_fx.entry(span.clip).or_insert_with(|| FxChain::new(self.sample_rate as f32)).process(&span.params.fx, &mut tmp);
            }

            // every dialogue clip feeds the (pre-bus) ducking sidechain
            if span.params.role == AudioRole::Dialogue {
                for (d, s) in self.dialogue.iter_mut().zip(&tmp) {
                    *d += s;
                }
            }
            if span.params.role != AudioRole::Dialogue && span.params.ducking.enabled {
                ducked.push((span.clip, tmp.clone()));
            } else {
                let bus = route(&mut self.bus_bufs, &mut self.user_bufs, &self.mix, span.params.role, span.params.bus);
                bus.resize(out.len(), 0.0);
                for (o, s) in bus.iter_mut().zip(&tmp) {
                    *o += s;
                }
            }
        }

        for (clip, buf) in &ducked {
            let (settings, role, user) = self.spans.iter().find(|s| s.clip == *clip).map(|s| (s.params.ducking, s.params.role, s.params.bus)).unwrap_or_default();
            let curve = self.duck_curve(*clip, settings);
            let bus = route(&mut self.bus_bufs, &mut self.user_bufs, &self.mix, role, user);
            bus.resize(out.len(), 0.0);
            if let Some(curve) = curve {
                // look-ahead curve on the timeline clock: identical in preview and export
                for (i, frame) in buf.as_chunks::<2>().0.iter().enumerate() {
                    let g = curve.gain_at(t0 + speed * i as f64 / sr);
                    bus[2 * i] += frame[0] * g;
                    bus[2 * i + 1] += frame[1] * g;
                }
                continue;
            }
            let ducker = self.duckers.entry(*clip).or_insert_with(|| Ducker::new(self.sample_rate as f32, settings));
            ducker.update(&settings);
            for (i, frame) in buf.as_chunks::<2>().0.iter().enumerate() {
                let side = 0.5 * (self.dialogue[2 * i] + self.dialogue[2 * i + 1]);
                let g = ducker.process(side);
                bus[2 * i] += frame[0] * g;
                bus[2 * i + 1] += frame[1] * g;
            }
        }

        // taken for the loops so the bus settings need no per-block clone
        let mix = std::mem::take(&mut self.mix);
        let clock = (t0, speed);
        let stem = self.stem;
        if stem.is_some() {
            self.stem_buf.clear();
            self.stem_buf.resize(out.len(), 0.0);
        }

        // user busses → the role bus they feed
        for (u, ub) in mix.user.iter().take(MAX_USER_BUSSES).enumerate() {
            let slot = 5 + u;
            let mut bus = std::mem::take(&mut self.user_bufs[u]);
            bus.resize(out.len(), 0.0);
            let audible = mix.user_audible(ub, self.honor_solo);
            self.process_bus(slot, &ub.settings, audible, &mut bus, clock);
            if stem == Some(StemTap::User(ub.id)) {
                self.stem_buf.copy_from_slice(&bus);
            }
            let dest = &mut self.bus_bufs[BusId::Role(ub.feeds).index()];
            dest.resize(out.len(), 0.0);
            for (o, s) in dest.iter_mut().zip(&bus) {
                *o += s;
            }
            bus.fill(0.0);
            self.user_bufs[u] = bus;
        }

        // role busses → master (solo is a monitoring aid: playback only)
        for (i, role) in AudioRole::ALL.into_iter().enumerate() {
            let settings = mix.bus(BusId::Role(role));
            let mut bus = std::mem::take(&mut self.bus_bufs[i]);
            bus.resize(out.len(), 0.0);
            let audible = mix.audible(role, self.honor_solo);
            self.process_bus(i, settings, audible, &mut bus, clock);
            if stem == Some(StemTap::Role(role)) {
                self.stem_buf.copy_from_slice(&bus);
            }
            for (o, s) in out.iter_mut().zip(&bus) {
                *o += s;
            }
            bus.fill(0.0);
            self.bus_bufs[i] = bus;
        }

        // Master bus: EQ, inserts, fader, preview volume, then the
        // always-on −1 dBFS soft-knee limiter (playback + export).
        let master = &mix.master;
        for s in out.iter_mut() {
            if !s.is_finite() {
                *s = 0.0;
            }
        }
        self.bus_eq[4].update(&master.eq);
        self.bus_eq[4].process(out);
        if master.fx.is_active() {
            self.bus_fx[4].process(&master.fx, out);
            let g = self.bus_fx[4].take_gr();
            merge_gr(&mut self.gr[4], g);
        }
        self.process_plugins(4, master, out);
        let a = gain_smoothing(self.sample_rate);
        let mut peaks = [0.0f32; 2];
        let mut target = bus_gains_at(master, !master.mute, Time::from_secs_f64(t0));
        for (k, frame) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            if master.is_automated() && k % BLOCK == 0 {
                target = bus_gains_at(master, !master.mute, Time::from_secs_f64(t0 + speed * k as f64 / sr));
            }
            for (c, s) in frame.iter_mut().enumerate() {
                let g = &mut self.bus_gain[4][c];
                *g += (target[c] - *g) * a;
                *s *= *g * self.master_gain;
            }
            self.limiter.process(frame);
            peaks[0] = peaks[0].max(frame[0].abs());
            peaks[1] = peaks[1].max(frame[1].abs());
        }
        if let Some(m) = self.meters.as_mut() {
            m[4].push(out);
        }
        if matches!(stem, Some(StemTap::Role(_) | StemTap::User(_))) {
            out.copy_from_slice(&self.stem_buf);
        }
        self.peaks = peaks;
        self.tmp = tmp;
        self.ducked = ducked;
        self.mix = mix;
    }

    /// CLAP inserts of bus `i`, in order (bypassed / failed ones skipped).
    fn process_plugins(&mut self, i: usize, s: &BusSettings, buf: &mut [f32]) {
        for (slot, ins) in self.bus_plugins[i].iter_mut().zip(&s.plugins) {
            if let (false, Some(inst)) = (ins.bypass, slot.inst.as_mut()) {
                inst.process(buf);
            }
        }
    }

    /// Bus EQ → inserts → fader / pan (smoothed, automated) → meter. A silent
    /// bus still feeds its meter so it falls to −∞ instead of freezing.
    /// `clock` = (timeline seconds at the first frame, speed).
    fn process_bus(&mut self, i: usize, s: &BusSettings, audible: bool, buf: &mut [f32], clock: (f64, f64)) {
        self.bus_eq[i].update(&s.eq);
        self.bus_eq[i].process(buf);
        if s.fx.is_active() {
            self.bus_fx[i].process(&s.fx, buf);
            let g = self.bus_fx[i].take_gr();
            merge_gr(&mut self.gr[i], g);
        }
        self.process_plugins(i, s, buf);
        let a = gain_smoothing(self.sample_rate);
        let g = &mut self.bus_gain[i];
        if s.is_automated() && audible {
            let sr = self.sample_rate as f64;
            let mut target = [0.0; 2];
            for (k, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                if k % BLOCK == 0 {
                    target = bus_gains_at(s, true, Time::from_secs_f64(clock.0 + clock.1 * k as f64 / sr));
                }
                for c in 0..2 {
                    g[c] += (target[c] - g[c]) * a;
                    f[c] *= g[c];
                }
            }
            if let Some(m) = self.meters.as_mut() {
                m[i].push(buf);
            }
            return;
        }
        let target = bus_gains(s, audible);
        if (g[0] - target[0]).abs() < 1e-6 && (g[1] - target[1]).abs() < 1e-6 {
            *g = target;
            if target != [1.0, 1.0] {
                for f in buf.as_chunks_mut::<2>().0 {
                    f[0] *= target[0];
                    f[1] *= target[1];
                }
            }
        } else {
            for f in buf.as_chunks_mut::<2>().0 {
                for c in 0..2 {
                    g[c] += (target[c] - g[c]) * a;
                    f[c] *= g[c];
                }
            }
        }
        if let Some(m) = self.meters.as_mut() {
            m[i].push(buf);
        }
    }
}

/// One-pole coefficient for 10 ms fader smoothing.
fn gain_smoothing(sample_rate: u32) -> f32 {
    1.0 - (-1.0 / (0.01 * sample_rate as f32)).exp()
}

/// Bus fader + balance as (L, R) gains.
/// A loaded plug-in insert (CLAP or VST3) and what it was loaded from.
struct PluginSlot {
    key: (String, String),
    inst: Option<PluginInstance>,
}

fn bus_gains(s: &BusSettings, audible: bool) -> [f32; 2] {
    if !audible {
        return [0.0; 2];
    }
    balance_gains(s.gain(), s.pan)
}

/// [`bus_gains`] with the automation at timeline time `t`.
fn bus_gains_at(s: &BusSettings, audible: bool, t: Time) -> [f32; 2] {
    if !audible {
        return [0.0; 2];
    }
    balance_gains(s.gain_at(t), s.pan_at(t))
}

fn balance_gains(g: f32, pan: f32) -> [f32; 2] {
    let pan = pan.clamp(-1.0, 1.0);
    [g * if pan > 0.0 { 1.0 - pan } else { 1.0 }, g * if pan < 0.0 { 1.0 + pan } else { 1.0 }]
}

/// Which bus a stem render taps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StemTap {
    Role(AudioRole),
    User(u64),
}

/// The input buffer a clip lands in: its user bus when that exists (and has
/// a slot), else its role bus.
fn route<'a>(roles: &'a mut [Vec<f32>; 4], users: &'a mut [Vec<f32>], mix: &MixerSettings, role: AudioRole, user: Option<u64>) -> &'a mut Vec<f32> {
    match user.and_then(|id| mix.user_bus(id)) {
        Some((slot, _)) => &mut users[slot - 5],
        None => &mut roles[BusId::Role(role).index()],
    }
}

fn merge_gr(acc: &mut GainReduction, g: GainReduction) {
    acc.gate = acc.gate.max(g.gate);
    acc.comp = acc.comp.max(g.comp);
    acc.de_esser = acc.de_esser.max(g.de_esser);
}

/// Restorations `spans` need: one key per clip (ranges of split spans merged).
pub fn restore_requests(spans: &[AudioSpan], sources: &HashMap<AssetId, Arc<AudioBuffer>>) -> Vec<(ClipId, RestoreKey)> {
    let mut out: Vec<(ClipId, RestoreKey)> = Vec::new();
    for span in spans {
        let Some(src) = sources.get(&span.asset) else { continue };
        let Some(k) = restore_key(span, src) else { continue };
        match out.iter_mut().find(|(c, _)| *c == span.clip) {
            Some((_, have)) if have.asset == k.asset => {
                have.start = have.start.min(k.start);
                have.end = have.end.max(k.end);
            }
            Some(_) => {}
            None => out.push((span.clip, k)),
        }
    }
    out
}

/// Run every restoration `spans` need, synchronously (export). `None` when
/// cancelled.
pub fn restore_all(spans: &[AudioSpan], sources: &HashMap<AssetId, Arc<AudioBuffer>>, cancel: &dyn Fn() -> bool) -> Option<Vec<(ClipId, Restored)>> {
    let mut out = Vec::new();
    for (clip, key) in restore_requests(spans, sources) {
        let buf = super::restore::restore(&sources[&key.asset], &key, cancel)?;
        out.push((clip, Restored { key, buf: Arc::new(buf) }));
    }
    Some(out)
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

    /// Two tones on the timeline: a 2 s Dialogue clip and a 2 s Music clip
    /// under it (no ducking), and their sources.
    fn two_roles() -> (Timeline, MediaAsset, MediaAsset) {
        let mut voice = MediaAsset::new("voice", "/v.wav", Time::from_secs(2));
        voice.audio = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut music = MediaAsset::new("music", "/m.wav", Time::from_secs(2));
        music.audio = voice.audio.clone();
        let mut tl = Timeline::default();
        let mut v = Clip::from_asset(&voice);
        v.audio.role = AudioRole::Dialogue;
        tl.append(v).unwrap();
        let mut m = Clip::from_asset(&music);
        m.audio.role = AudioRole::Music;
        m.audio.ducking.enabled = false;
        tl.connect(m, Time::ZERO, -1).unwrap();
        (tl, voice, music)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn bus_fader_automation_is_followed_on_the_timeline_clock() {
        let (mut tl, voice, music) = two_roles();
        tl.mixer.music.mute = true;
        // dialogue: 0 dB for the first second, silence from 1.0 s
        let s = Time::from_secs_f64;
        tl.mixer.dialogue.gain_auto.set(s(0.0), 0.0, Time::ZERO);
        tl.mixer.dialogue.gain_auto.set(s(0.99), 0.0, Time::ZERO);
        tl.mixer.dialogue.gain_auto.set(s(1.0), -60.0, Time::ZERO);
        let mut mixer = TimelineMixer::new(48_000);
        mixer.set_spans(nle_core::audio_spans(&tl));
        mixer.set_source(voice.id, tone(2.0, 0.3));
        mixer.set_source(music.id, tone(2.0, 0.3));
        mixer.set_mixer(&tl.mixer);
        let mut out = vec![0.0f32; 48_000 * 2 * 2];
        // rendered in small blocks like playback
        for (k, chunk) in out.chunks_mut(512).enumerate() {
            mixer.render(Time::from_samples(k as i64 * 256, 48_000), 1.0, chunk);
        }
        assert!(rms(&out[20_000 * 2..40_000 * 2]) > 0.1, "heard before the automation drops");
        assert!(rms(&out[52_000 * 2..90_000 * 2]) < 1e-4, "silent after it");
        // bypassed automation: the static fader again
        tl.mixer.dialogue.auto_off = true;
        mixer.set_mixer(&tl.mixer);
        mixer.reset_state();
        let mut out = vec![0.0f32; 48_000 * 2 * 2];
        mixer.render(Time::ZERO, 1.0, &mut out);
        assert!(rms(&out[52_000 * 2..90_000 * 2]) > 0.1);
    }

    #[test]
    fn user_bus_processes_its_clips_and_stems_tap_one_bus() {
        let (mut tl, voice, music) = two_roles();
        let id = tl.mixer.add_user_bus("Voices", AudioRole::Dialogue).unwrap();
        tl.mixer.user[0].settings.gain_db = -60.0; // closed fader
        let clip = tl.spine[0].id;
        tl.clip_mut(clip).unwrap().audio.bus = Some(id);
        let mut mixer = TimelineMixer::new(48_000);
        mixer.set_spans(nle_core::audio_spans(&tl));
        mixer.set_source(voice.id, sine(2.0, 440.0, 0.3));
        mixer.set_source(music.id, sine(2.0, 1_000.0, 0.3));
        mixer.set_mixer(&tl.mixer);

        // music only reaches master: the voice went through the closed user bus
        mixer.set_stem(Some(StemTap::Role(AudioRole::Dialogue)));
        let mut dia = vec![0.0f32; 48_000 * 2];
        mixer.render(Time::ZERO, 1.0, &mut dia);
        assert!(rms(&dia[4_000..]) < 1e-3, "dialogue stem ~-60 dB: {}", rms(&dia[4_000..]));
        mixer.set_stem(Some(StemTap::Role(AudioRole::Music)));
        mixer.reset_state();
        let mut mus = vec![0.0f32; 48_000 * 2];
        mixer.render(Time::ZERO, 1.0, &mut mus);
        assert!(rms(&mus[4_000..]) > 0.15, "music stem: {}", rms(&mus[4_000..]));

        // open the user bus: its stem and the dialogue stem carry the voice
        tl.mixer.user[0].settings.gain_db = 0.0;
        mixer.set_mixer(&tl.mixer);
        for tap in [StemTap::User(id), StemTap::Role(AudioRole::Dialogue)] {
            mixer.set_stem(Some(tap));
            mixer.reset_state();
            let mut buf = vec![0.0f32; 48_000 * 2];
            mixer.render(Time::ZERO, 1.0, &mut buf);
            assert!(rms(&buf[4_000..]) > 0.15, "{tap:?}: {}", rms(&buf[4_000..]));
        }
        // full mix again
        mixer.set_stem(None);
        mixer.reset_state();
        let mut all = vec![0.0f32; 48_000 * 2];
        mixer.render(Time::ZERO, 1.0, &mut all);
        assert!(rms(&all[4_000..]) > rms(&mus[4_000..]) * 1.2);

        // a clip sent to a bus that was deleted falls back to its role bus
        tl.mixer.user.clear();
        mixer.set_mixer(&tl.mixer);
        mixer.set_stem(Some(StemTap::Role(AudioRole::Dialogue)));
        mixer.reset_state();
        let mut back = vec![0.0f32; 48_000 * 2];
        mixer.render(Time::ZERO, 1.0, &mut back);
        assert!(rms(&back[4_000..]) > 0.15);
    }

    #[test]
    fn a_project_switch_drops_the_old_audio() {
        let mut a = MediaAsset::new("tone", "/t.wav", Time::from_secs(2));
        a.audio = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut m = TimelineMixer::new(48_000);
        m.set_source(a.id, tone(2.0, 0.3));
        assert!(m.has_source(a.id));
        m.clear_sources();
        assert!(!m.has_source(a.id));
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

    /// Dialogue on the spine (0–2 s), music connected underneath.
    fn two_role_timeline() -> (Timeline, MediaAsset, MediaAsset) {
        let info = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let mut voice = MediaAsset::new("voice", "/v.wav", Time::from_secs(2));
        voice.audio = info.clone();
        let mut music = MediaAsset::new("music", "/m.wav", Time::from_secs(2));
        music.audio = info;
        let mut tl = Timeline::default();
        let mut v = Clip::from_asset(&voice);
        v.audio.role = AudioRole::Dialogue;
        tl.append(v).unwrap();
        let mut m = Clip::from_asset(&music);
        m.audio.ducking.enabled = false; // music auto-ducks by default
        tl.connect(m, Time::ZERO, -1).unwrap();
        (tl, voice, music)
    }

    fn rms_of(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn render_with(mix: &MixerSettings, honor_solo: bool) -> Vec<f32> {
        let (tl, voice, music) = two_role_timeline();
        let mut m = TimelineMixer::new(48_000);
        m.honor_solo = honor_solo;
        m.set_spans(nle_core::audio_spans(&tl));
        m.set_source(voice.id, tone(2.0, 0.2));
        m.set_source(music.id, tone(2.0, 0.1));
        m.set_mixer(mix);
        let mut out = vec![0.0f32; 48_000 * 2];
        m.render(Time::ZERO, 1.0, &mut out);
        out
    }

    #[test]
    fn clap_inserts_on_role_bus_and_master() {
        use nle_core::PluginInsert;
        let tail = |o: &[f32]| rms_of(&o[2 * 24_000..]);
        let both = tail(&render_with(&MixerSettings::default(), false));
        let gain = |db: f64| PluginInsert {
            path: crate::audio::clap_host::test_plugin().to_string_lossy().into_owned(),
            id: "org.rustcutpro.test-gain".into(),
            name: "Test Gain".into(),
            bypass: false,
            params: vec![(0, db)],
        };
        // −6 dB on the dialogue bus: voice 0.2 → 0.1, music 0.1 → total 0.2
        let mut mix = MixerSettings::default();
        mix.bus_mut(BusId::Role(AudioRole::Dialogue)).plugins.push(gain(-6.0206));
        let o = tail(&render_with(&mix, false));
        assert!((o / (0.2 / 2f32.sqrt()) - 1.0).abs() < 0.03, "dialogue insert: {o}");
        // two in series on master: −6 + −6
        let mut mix = MixerSettings::default();
        mix.master.plugins = vec![gain(-6.0206), gain(-6.0206)];
        let o = tail(&render_with(&mix, false));
        assert!((o / (0.3 / 4.0 / 2f32.sqrt()) - 1.0).abs() < 0.03, "two master inserts: {o}");
        // a VST3 in series with a CLAP
        let vst3 = crate::audio::vst3_host::test_plugin();
        let vst3_id = crate::audio::vst3_host::describe(&vst3).unwrap()[0].id.clone();
        let mut mix = MixerSettings::default();
        mix.master.plugins = vec![gain(-6.0206), PluginInsert { path: vst3.to_string_lossy().into_owned(), id: vst3_id, ..gain(-6.0206) }];
        let o = tail(&render_with(&mix, false));
        assert!((o / (0.3 / 4.0 / 2f32.sqrt()) - 1.0).abs() < 0.03, "CLAP + VST3 on master: {o}");
        // bypassed / missing plug-ins pass audio through
        mix.master.plugins[0].bypass = true;
        mix.master.plugins[1].path = "C:/missing/none.clap".into();
        assert!((tail(&render_with(&mix, false)) - both).abs() < 1e-4);
    }

    #[test]
    fn role_busses_mute_solo_fader_and_pan() {
        let tail = |o: &[f32]| rms_of(&o[2 * 24_000..]);
        let both = tail(&render_with(&MixerSettings::default(), false));
        // identical waveforms: 0.2 + 0.1 sum coherently
        assert!((both / (0.3 / 2f32.sqrt()) - 1.0).abs() < 0.03, "default mix is the plain sum: {both}");

        let mut mix = MixerSettings::default();
        mix.bus_mut(BusId::Role(AudioRole::Music)).mute = true;
        let voice_only = tail(&render_with(&mix, false));
        assert!((voice_only / (0.2 / 2f32.sqrt()) - 1.0).abs() < 0.03, "music bus muted: {voice_only}");

        let mut mix = MixerSettings::default();
        mix.bus_mut(BusId::Role(AudioRole::Music)).solo = true;
        let solo = tail(&render_with(&mix, true));
        assert!((solo / (0.1 / 2f32.sqrt()) - 1.0).abs() < 0.03, "solo → only music: {solo}");
        let ignored = tail(&render_with(&mix, false));
        assert!((ignored - both).abs() < 1e-4, "export ignores solo");

        let mut mix = MixerSettings::default();
        mix.bus_mut(BusId::Role(AudioRole::Dialogue)).gain_db = -6.0206;
        let faded = tail(&render_with(&mix, false));
        assert!((faded / (0.2 / 2f32.sqrt()) - 1.0).abs() < 0.03, "−6 dB dialogue fader halves it: {faded}");

        let mut mix = MixerSettings::default();
        mix.master.pan = 1.0;
        let o = render_with(&mix, false);
        let (l, r): (Vec<f32>, Vec<f32>) = o[2 * 24_000..].as_chunks::<2>().0.iter().map(|f| (f[0], f[1])).unzip();
        assert!(rms_of(&l) < 1e-4 && rms_of(&r) > 0.2, "master balance hard right");

        let mut mix = MixerSettings::default();
        mix.master.mute = true;
        assert!(tail(&render_with(&mix, false)) < 1e-5, "master mute");
    }

    #[test]
    fn meters_read_each_bus_and_master() {
        let (tl, voice, music) = two_role_timeline();
        let mut m = TimelineMixer::new(48_000);
        m.set_metering(true);
        m.set_spans(nle_core::audio_spans(&tl));
        m.set_source(voice.id, tone(2.0, 0.2));
        m.set_source(music.id, tone(2.0, 0.1));
        let mut out = vec![0.0f32; 48_000 * 2];
        m.render(Time::ZERO, 1.0, &mut out);
        let r = m.meter_readings().unwrap();
        let (d, mu, fx, master) = (r[0].momentary.unwrap(), r[1].momentary.unwrap(), r[2], r[4].momentary.unwrap());
        assert!((d - mu - 6.02).abs() < 0.2, "dialogue 6 dB over music: {d} {mu}");
        assert!(fx.momentary.is_none(), "empty effects bus reads −∞");
        assert!(master > d, "master is the sum");
        assert!(r[4].true_peak_db.unwrap() < 0.0);
        assert!(r[0].peak[0] > 0.19 && r[0].peak[0] < 0.21);
        // no metering in export
        let mut e = TimelineMixer::new(48_000);
        assert!(e.meter_readings().is_none());
    }

    #[test]
    fn a_crossfade_plays_both_clips_handles() {
        let info = Some(nle_core::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm".into() });
        let asset = |name: &str| {
            let mut a = MediaAsset::new(name, format!("/{name}.wav"), Time::from_secs(3));
            a.audio = info.clone();
            a
        };
        let (a, b) = (asset("a"), asset("b"));
        let mut tl = Timeline::default();
        let mut first = Clip::from_asset_range(&a, Time::ZERO, Time::from_secs(2));
        first.audio.crossfade_out = Time::from_secs(1);
        tl.append(first).unwrap();
        tl.append(Clip::from_asset_range(&b, Time::from_secs(1), Time::from_secs(2))).unwrap();
        let spans = nle_core::audio_spans(&tl);
        // one side sounding at a time: each must carry past the cut at 2 s
        for (a_amp, b_amp, at) in [(0.2, 0.0, 2.1), (0.0, 0.2, 1.9)] {
            let mut m = TimelineMixer::new(48_000);
            m.set_spans(spans.clone());
            m.set_source(a.id, tone(3.0, a_amp));
            m.set_source(b.id, tone(3.0, b_amp));
            let mut out = vec![0.0f32; 3 * 48_000 * 2];
            m.render(Time::ZERO, 1.0, &mut out);
            let i = (at * 48_000.0) as usize * 2;
            assert!(rms_of(&out[i..i + 2_000]) > 0.02, "no handle at {at} s");
            let beyond = if at > 2.0 { 2.6 } else { 1.2 };
            let i = (beyond * 48_000.0) as usize * 2;
            assert!(rms_of(&out[i..i + 2_000]) < 0.002, "still sounding at {beyond} s");
        }
    }

    #[test]
    fn restored_buffer_replaces_the_source_for_its_clip() {
        let (mut tl, voice, music) = two_role_timeline();
        tl.spine[0].audio.noise_reduction = nle_core::NoiseReductionSettings { enabled: true, reduction_db: 12.0, sensitivity: 1.0, profile: None };
        let spans = nle_core::audio_spans(&tl);
        let sources: HashMap<AssetId, Arc<AudioBuffer>> = [(voice.id, tone(2.0, 0.2)), (music.id, tone(2.0, 0.1))].into_iter().collect();
        let reqs = restore_requests(&spans, &sources);
        assert_eq!(reqs.len(), 1, "only the denoised clip");
        assert_eq!(reqs[0].1.asset, voice.id);

        let mut m = TimelineMixer::new(48_000);
        m.set_spans(spans);
        m.set_source(voice.id, sources[&voice.id].clone());
        m.set_source(music.id, sources[&music.id].clone());
        m.set_mixer(&{
            let mut x = MixerSettings::default();
            x.bus_mut(BusId::Role(AudioRole::Music)).mute = true;
            x
        });
        assert_eq!(m.restore_requests().len(), 1);
        // a stand-in "restoration" at half level proves the mixer reads it
        let key = reqs[0].1.clone();
        let len = (key.end - key.start) * 2;
        let half: Vec<f32> = sources[&voice.id].samples[key.start * 2..key.start * 2 + len].iter().map(|s| s * 0.5).collect();
        m.set_restored(reqs[0].0, Restored { key, buf: Arc::new(AudioBuffer::new(48_000, 2, half)) });
        assert!(m.restore_requests().is_empty(), "installed restoration serves the span");
        let mut out = vec![0.0f32; 48_000 * 2];
        m.render(Time::ZERO, 1.0, &mut out);
        let got = rms_of(&out[2 * 24_000..]);
        assert!((got / (0.1 / 2f32.sqrt()) - 1.0).abs() < 0.03, "restored (half-level) audio plays: {got}");
    }
}
