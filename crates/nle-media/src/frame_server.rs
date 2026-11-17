//! Asynchronous frame delivery for the viewer.
//!
//! Each active asset gets a decode worker thread. The UI posts "latest
//! wins" requests (scrubbing never queues stale work); workers push decoded
//! frames into a bounded drop-oldest [`FrameQueue`] that the UI drains every
//! repaint. During playback, workers decode a few frames ahead.

use crate::decode::{self, DecodeOptions, VideoDecoder};
use crate::frame::VideoFrame;
use crate::hwaccel::HwBackend;
use crate::queue::SpscFrameRing;
use crate::MediaError;
use nle_core::{AssetId, FrameRate, MediaAsset, Time};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Frames decoded ahead of the playhead during playback.
pub const PREFETCH_FRAMES: i64 = 6;

#[derive(Clone, Debug)]
pub struct DecodedFrame {
    pub asset: AssetId,
    pub proxy: bool,
    /// Frame index at the asset's native rate.
    pub index: i64,
    pub frame: VideoFrame,
}

#[derive(Default)]
struct WorkerState {
    request: Option<Time>,
    prefetch: bool,
    quit: bool,
}

struct Worker {
    state: Arc<(Mutex<WorkerState>, Condvar)>,
    backend: Arc<Mutex<Option<HwBackend>>>,
    handle: Option<JoinHandle<()>>,
}

pub struct FrameServer {
    workers: Mutex<HashMap<(AssetId, bool), Worker>>,
    out: Arc<SpscFrameRing<DecodedFrame>>,
    max_height: Option<u32>,
    sync_decoders: Mutex<HashMap<(AssetId, bool), Box<dyn VideoDecoder>>>,
}

impl FrameServer {
    /// `max_height` downscales preview decoding (e.g. 1080 for a 4K
    /// timeline on a 1440p monitor); `None` decodes full resolution.
    pub fn new(queue_capacity: usize, max_height: Option<u32>) -> Self {
        FrameServer {
            workers: Mutex::new(HashMap::new()),
            out: Arc::new(SpscFrameRing::new(queue_capacity)),
            max_height,
            sync_decoders: Mutex::new(HashMap::new()),
        }
    }

    /// Ask for the frame of `asset` at source time `t`.
    pub fn request(&self, asset: &MediaAsset, t: Time, proxy: bool, prefetch: bool) {
        let mut workers = self.workers.lock();
        let key = (asset.id, proxy);
        if let std::collections::hash_map::Entry::Vacant(slot) = workers.entry(key) {
            match self.spawn(asset.clone(), proxy) {
                Ok(w) => {
                    slot.insert(w);
                }
                Err(e) => {
                    // Out of threads/handles: skip this frame, retry on the next request.
                    log::warn!("cannot start decode worker for {}: {e}", asset.name);
                    return;
                }
            }
        }
        let Some(w) = workers.get(&key) else { return };
        let (lock, cv) = &*w.state;
        let mut st = lock.lock();
        st.request = Some(t);
        st.prefetch = prefetch;
        cv.notify_one();
    }

    /// Frames decoded since the last call.
    pub fn drain(&self) -> Vec<DecodedFrame> {
        self.out.drain()
    }

    pub fn dropped(&self) -> u64 {
        self.out.dropped()
    }

    pub fn backend(&self, asset: AssetId) -> Option<HwBackend> {
        let w = self.workers.lock();
        w.get(&(asset, false)).or_else(|| w.get(&(asset, true))).and_then(|w| *w.backend.lock())
    }

    /// Stop the worker for an asset (e.g. removed from the project).
    pub fn forget(&self, asset: AssetId) {
        let mut w = self.workers.lock();
        for proxy in [false, true] {
            if let Some(mut worker) = w.remove(&(asset, proxy)) {
                stop(&mut worker);
            }
        }
        self.sync_decoders.lock().retain(|k, _| k.0 != asset);
    }

    /// Blocking decode for export (full resolution, separate decoders).
    pub fn frame_sync(&self, asset: &MediaAsset, t: Time, proxy: bool) -> Result<VideoFrame, MediaError> {
        let mut map = self.sync_decoders.lock();
        let key = (asset.id, proxy);
        let dec = match map.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(decode::open(asset, DecodeOptions { use_proxy: proxy, max_height: None })?),
        };
        let source_t = asset.video.as_ref().map(|v| v.cfr_locked_source_time(t)).unwrap_or(t);
        dec.frame_at(source_t)
    }

    pub fn clear_sync(&self) {
        self.sync_decoders.lock().clear();
    }

    fn spawn(&self, asset: MediaAsset, proxy: bool) -> std::io::Result<Worker> {
        let state = Arc::new((Mutex::new(WorkerState::default()), Condvar::new()));
        let backend = Arc::new(Mutex::new(None));
        let out = self.out.clone();
        let opts = DecodeOptions { use_proxy: proxy, max_height: self.max_height };
        let st = state.clone();
        let be = backend.clone();
        let handle = std::thread::Builder::new()
            .name(format!("rcp-decode-{}", asset.id.0))
            .spawn(move || worker_loop(asset, proxy, opts, st, be, out))?;
        Ok(Worker { state, backend, handle: Some(handle) })
    }
}

fn stop(w: &mut Worker) {
    {
        let (lock, cv) = &*w.state;
        lock.lock().quit = true;
        cv.notify_all();
    }
    if let Some(h) = w.handle.take() {
        let _ = h.join();
    }
}

impl Drop for FrameServer {
    fn drop(&mut self) {
        for (_, mut w) in self.workers.lock().drain() {
            stop(&mut w);
        }
    }
}

fn worker_loop(
    asset: MediaAsset,
    proxy: bool,
    opts: DecodeOptions,
    state: Arc<(Mutex<WorkerState>, Condvar)>,
    backend: Arc<Mutex<Option<HwBackend>>>,
    out: Arc<SpscFrameRing<DecodedFrame>>,
) {
    crate::threads::pin_current_to_background_pool();
    let mut dec: Option<Box<dyn VideoDecoder>> = None;
    let rate: FrameRate = asset.frame_rate();
    let mut last_index: Option<i64> = None;
    let mut ahead: i64 = 0;
    let max_index = asset.duration.frames_floor(rate).max(0);
    loop {
        let (req, prefetch) = {
            let (lock, cv) = &*state;
            let mut st = lock.lock();
            loop {
                if st.quit {
                    return;
                }
                if st.request.is_some() || (st.prefetch && last_index.is_some() && ahead < PREFETCH_FRAMES) {
                    break;
                }
                cv.wait_for(&mut st, Duration::from_millis(250));
            }
            (st.request.take(), st.prefetch)
        };
        if dec.is_none() {
            match decode::open(&asset, opts) {
                Ok(d) => {
                    *backend.lock() = Some(d.backend());
                    dec = Some(d);
                }
                Err(e) => {
                    log::warn!("cannot open decoder for {}: {e}", asset.name);
                    // Back off without blocking teardown: the condvar wakes at once on quit.
                    let (lock, cv) = &*state;
                    let mut st = lock.lock();
                    if !st.quit {
                        cv.wait_for(&mut st, Duration::from_millis(500));
                    }
                    continue;
                }
            }
        }
        let Some(d) = dec.as_mut() else { continue };
        let index = match req {
            Some(t) => {
                let idx = t.max(Time::ZERO).frames_floor(rate);
                if Some(idx) == last_index {
                    continue;
                }
                // already prefetched?
                if let Some(li) = last_index {
                    if idx > li && idx <= li + ahead {
                        ahead -= idx - li;
                        last_index = Some(idx);
                        continue;
                    }
                }
                ahead = 0;
                last_index = Some(idx);
                idx
            }
            None if prefetch => {
                ahead += 1;
                last_index.unwrap_or(0) + ahead
            }
            None => continue,
        };
        if index > max_index {
            ahead = PREFETCH_FRAMES;
            continue;
        }
        let raw_t = Time::from_frames(index, rate);
        let locked_t = asset.video.as_ref().map(|v| v.cfr_locked_source_time(raw_t)).unwrap_or(raw_t);
        match d.frame_at(locked_t) {
            Ok(frame) => {
                *backend.lock() = Some(d.backend());
                out.push_overwrite(DecodedFrame { asset: asset.id, proxy, index, frame });
            }
            Err(e) => {
                log::debug!("decode {} @{index}: {e}", asset.name);
                ahead = PREFETCH_FRAMES;
                // Forget the failed index so the same position can be retried,
                // and reopen the decoder after a hard error (corrupt packet,
                // dead FFmpeg pipe) instead of reusing a broken one.
                last_index = None;
                if !matches!(e, MediaError::Eof) {
                    dec = None;
                }
            }
        }
    }
}
