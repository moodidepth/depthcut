//! Background media jobs for the UI: probing imports, filmstrip thumbnails,
//! audio decode + RMS waveforms and proxy transcodes. Jobs run on the capped
//! rayon pool (12 workers, 4 threads left for UI/audio); results come back
//! over a channel and are turned into egui textures on the UI thread.

use eframe::egui;
use nle_core::{AssetId, MediaAsset, Time};
use nle_media::proxy::{ProxyManager, ProxyStatus};
use nle_media::thumbnails::{self, Filmstrip};
use nle_media::{peaks, probe, AudioBuffer, PeakBuffer, Waveform};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

/// Thumbnails per asset filmstrip.
const STRIP_FRAMES: usize = 24;
const THUMB_W: u32 = 192;
const THUMB_H: u32 = 108;
/// Waveform resolution.
const WAVE_BUCKETS_PER_SEC: u32 = 200;

// short-lived channel messages: boxing the big variant would only add allocations
#[allow(clippy::large_enum_variant)]
enum JobResult {
    Probed {
        path: PathBuf,
        result: Result<MediaAsset, String>,
    },
    /// Fresh stream info for an existing asset (relinked / EDL / OTIO media).
    Reprobed {
        asset: AssetId,
        path: PathBuf,
        result: Result<MediaAsset, String>,
    },
    Filmstrip {
        asset: AssetId,
        strip: Filmstrip,
    },
    Audio {
        asset: AssetId,
        buffer: Arc<AudioBuffer>,
        waveform: Arc<Waveform>,
        loudness_lufs: Option<f32>,
    },
    Peaks {
        asset: AssetId,
        peaks: Arc<PeakBuffer>,
        transients: Arc<Vec<Time>>,
    },
    Failed {
        asset: AssetId,
        what: &'static str,
        error: String,
    },
}

/// Filmstrip uploaded as egui textures.
pub struct StripTextures {
    pub interval: Time,
    pub frames: Vec<(Time, egui::TextureHandle)>,
    pub aspect: f32,
}

impl StripTextures {
    /// Texture nearest to `t` (source time).
    pub fn at(&self, t: Time) -> Option<&egui::TextureHandle> {
        if self.frames.is_empty() {
            return None;
        }
        let i = if self.interval.0 > 0 { (t.0 / self.interval.0).clamp(0, self.frames.len() as i64 - 1) as usize } else { 0 };
        self.frames.get(i).map(|(_, t)| t)
    }
}

/// Events the app reacts to after [`MediaLibrary::poll`].
#[allow(clippy::large_enum_variant)] // one per finished import, consumed immediately
pub enum MediaEvent {
    Imported(MediaAsset),
    /// Probe result for an asset already in the project.
    Refreshed(AssetId, MediaAsset),
    AudioReady(AssetId, Arc<AudioBuffer>, Option<f32>),
    Message(String),
}

pub struct MediaLibrary {
    tx: Sender<JobResult>,
    rx: Receiver<JobResult>,
    pub filmstrips: HashMap<AssetId, StripTextures>,
    pub waveforms: HashMap<AssetId, Arc<Waveform>>,
    /// Min/max peak pyramids for timeline waveforms (empty for video-only assets).
    pub peaks: HashMap<AssetId, Arc<PeakBuffer>>,
    /// Loud-onset source times per asset (audio-peak snapping), sorted.
    pub transients: HashMap<AssetId, Arc<Vec<Time>>>,
    pub audio: HashMap<AssetId, Arc<AudioBuffer>>,
    pub failed: HashMap<AssetId, String>,
    requested: HashSet<(AssetId, u8)>,
    pub proxies: ProxyManager,
    pub importing: usize,
    proxy_seen: HashMap<AssetId, PathBuf>,
}

impl Default for MediaLibrary {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaLibrary {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        MediaLibrary {
            tx,
            rx,
            filmstrips: HashMap::new(),
            waveforms: HashMap::new(),
            peaks: HashMap::new(),
            transients: HashMap::new(),
            audio: HashMap::new(),
            failed: HashMap::new(),
            requested: HashSet::new(),
            proxies: ProxyManager::new(),
            importing: 0,
            proxy_seen: HashMap::new(),
        }
    }

    /// Forget everything (new/opened project). Jobs still running for the old
    /// project report to the replaced channel and proxy list, so are ignored.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Probe files in the background; results arrive as [`MediaEvent::Imported`].
    pub fn import(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            self.importing += 1;
            let tx = self.tx.clone();
            rayon::spawn(move || {
                let result = probe::probe(&path).map_err(|e| e.to_string());
                let _ = tx.send(JobResult::Probed { path, result });
            });
        }
    }

    /// Probe an existing asset's file again; arrives as [`MediaEvent::Refreshed`].
    pub fn reprobe(&mut self, asset: AssetId, path: PathBuf) {
        self.importing += 1;
        let tx = self.tx.clone();
        rayon::spawn(move || {
            let result = probe::probe(&path).map_err(|e| e.to_string());
            let _ = tx.send(JobResult::Reprobed { asset, path, result });
        });
    }

    /// Drop cached thumbnails / audio for an asset whose file changed, so
    /// [`Self::ensure`] analyses it again.
    pub fn forget_asset(&mut self, id: AssetId) {
        self.filmstrips.remove(&id);
        self.waveforms.remove(&id);
        self.peaks.remove(&id);
        self.transients.remove(&id);
        self.audio.remove(&id);
        self.failed.remove(&id);
        self.requested.retain(|(a, _)| *a != id);
    }

    /// Queue thumbnails and audio analysis for an asset (once).
    pub fn ensure(&mut self, asset: &MediaAsset) {
        if asset.has_video() && self.requested.insert((asset.id, 0)) {
            let tx = self.tx.clone();
            let a = asset.clone();
            rayon::spawn(move || {
                let msg = match thumbnails::generate(&a, STRIP_FRAMES, THUMB_W, THUMB_H) {
                    Ok(strip) => JobResult::Filmstrip { asset: a.id, strip },
                    Err(e) => JobResult::Failed { asset: a.id, what: "thumbnails", error: e.to_string() },
                };
                let _ = tx.send(msg);
            });
        }
        if !asset.has_audio() {
            // Video-only clip: an empty peak buffer, never an error.
            self.peaks.entry(asset.id).or_insert_with(|| Arc::new(PeakBuffer::empty()));
        }
        if asset.has_audio() && self.requested.insert((asset.id, 1)) {
            let tx = self.tx.clone();
            let a = asset.clone();
            rayon::spawn(move || {
                // Cached peaks show the waveform before the audio is decoded.
                let cached = peaks::load_cached(&a.path);
                if let Some(pk) = &cached {
                    let _ = tx.send(peaks_result(a.id, pk.clone()));
                }
                let decoded = if probe::is_synthetic(&a) {
                    Ok(nle_media::audio::decode::synthetic_audio(&a, nle_media::audio::resample::INTERNAL_RATE))
                } else {
                    nle_media::audio::decode::decode_audio(&a)
                };
                let msg = match decoded {
                    Ok(buf) => {
                        if cached.is_none() {
                            let pk = PeakBuffer::from_audio(&buf);
                            if let Err(e) = peaks::store_cached(&a.path, &pk) {
                                log::warn!("waveform cache for {}: {e}", a.name);
                            }
                            let _ = tx.send(peaks_result(a.id, pk));
                        }
                        let waveform = Arc::new(Waveform::compute(&buf, WAVE_BUCKETS_PER_SEC));
                        let loudness_lufs = nle_media::analysis::integrated_lufs(&buf.samples, buf.channels as usize, buf.sample_rate);
                        JobResult::Audio { asset: a.id, buffer: Arc::new(buf), waveform, loudness_lufs }
                    }
                    Err(e) => JobResult::Failed { asset: a.id, what: "audio", error: e.to_string() },
                };
                let _ = tx.send(msg);
            });
        }
    }

    pub fn proxy_status(&self, id: AssetId) -> Option<ProxyStatus> {
        self.proxies.status(id)
    }

    /// Finished proxies not yet reported (asset id → proxy path).
    pub fn take_finished_proxies(&mut self) -> Vec<(AssetId, PathBuf)> {
        let mut out = Vec::new();
        for (id, st) in self.proxies.all() {
            if let ProxyStatus::Done(p) = st {
                if self.proxy_seen.get(&id) != Some(&p) {
                    self.proxy_seen.insert(id, p.clone());
                    out.push((id, p));
                }
            }
        }
        out
    }

    /// Collect finished jobs; upload thumbnails as textures.
    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<MediaEvent> {
        let mut events = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            match r {
                JobResult::Probed { path, result } => {
                    self.importing = self.importing.saturating_sub(1);
                    match result {
                        Ok(a) => events.push(MediaEvent::Imported(a)),
                        Err(e) => events.push(MediaEvent::Message(format!("Could not import {}: {e}", path.display()))),
                    }
                }
                JobResult::Reprobed { asset, path, result } => {
                    self.importing = self.importing.saturating_sub(1);
                    match result {
                        Ok(a) => events.push(MediaEvent::Refreshed(asset, a)),
                        Err(e) => events.push(MediaEvent::Message(format!("Could not read {}: {e}", path.display()))),
                    }
                }
                JobResult::Filmstrip { asset, strip } => {
                    let mut frames = Vec::with_capacity(strip.frames.len());
                    let mut aspect = 16.0 / 9.0;
                    for (i, th) in strip.frames.iter().enumerate() {
                        if th.width == 0 || th.height == 0 {
                            continue;
                        }
                        aspect = th.width as f32 / th.height as f32;
                        let img = egui::ColorImage::from_rgba_unmultiplied([th.width as usize, th.height as usize], &th.rgba);
                        let tex = ctx.load_texture(format!("strip-{}-{i}", asset.0), img, egui::TextureOptions::LINEAR);
                        frames.push((th.time, tex));
                    }
                    self.filmstrips.insert(asset, StripTextures { interval: strip.interval, frames, aspect });
                }
                JobResult::Audio { asset, buffer, waveform, loudness_lufs } => {
                    self.waveforms.insert(asset, waveform);
                    self.audio.insert(asset, buffer.clone());
                    events.push(MediaEvent::AudioReady(asset, buffer, loudness_lufs));
                }
                JobResult::Peaks { asset, peaks, transients } => {
                    self.peaks.insert(asset, peaks);
                    self.transients.insert(asset, transients);
                    ctx.request_repaint();
                }
                JobResult::Failed { asset, what, error } => {
                    log::warn!("{what} for asset {}: {error}", asset.0);
                    self.failed.insert(asset, format!("{what}: {error}"));
                }
            }
        }
        if !events.is_empty() {
            ctx.request_repaint();
        }
        events
    }
}

fn peaks_result(asset: AssetId, peaks: PeakBuffer) -> JobResult {
    let transients = peaks.transients().into_iter().map(Time::from_secs_f64).collect();
    JobResult::Peaks { asset, peaks: Arc::new(peaks), transients: Arc::new(transients) }
}

/// Where autosaves, proxies and settings live.
pub fn data_dir() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("RustCutPro")
}
