//! CPU thread topology for the Ryzen 7 5800X (8 cores / 16 threads).
//!
//! Heavy background work (filmstrip thumbnails, waveform peak extraction,
//! audio resampling, proxy transcodes) runs on a global rayon pool capped at
//! **12 workers**. The remaining **4 logical threads** (CPU 0-3, i.e. the
//! first two physical cores) are left free for the wgpu UI render loop, the
//! cpal real-time audio callback, and OS / GPU-driver interrupts, so
//! scrubbing never stutters under heavy background transcoding.
//!
//! Workers are pinned to the highest-numbered logical CPUs with
//! `core_affinity`; on machines with a different core count the budget
//! scales down while always keeping the reserve.

use std::sync::OnceLock;

/// Logical threads on the target CPU (Ryzen 7 5800X).
pub const TARGET_LOGICAL_THREADS: usize = 16;
/// Upper bound of background media workers.
pub const MAX_MEDIA_WORKERS: usize = 12;
/// Logical threads kept free for UI / audio / drivers.
pub const RESERVED_THREADS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadBudget {
    pub logical: usize,
    pub workers: usize,
    pub reserved: usize,
}

impl ThreadBudget {
    pub fn detect() -> Self {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(TARGET_LOGICAL_THREADS);
        Self::for_logical(n)
    }

    pub fn for_logical(logical: usize) -> Self {
        let logical = logical.max(1);
        let reserved = RESERVED_THREADS.min(logical.saturating_sub(1));
        let workers = (logical - reserved).clamp(1, MAX_MEDIA_WORKERS);
        ThreadBudget { logical, workers, reserved }
    }

    /// Logical CPU indices the media workers are pinned to: the top
    /// `workers` CPUs, leaving CPUs `0..reserved` untouched.
    pub fn worker_cpus(&self) -> Vec<usize> {
        let first = self.logical.saturating_sub(self.workers).max(self.reserved.min(self.logical - 1));
        (first..self.logical).take(self.workers).collect()
    }
}

static POOL: OnceLock<ThreadBudget> = OnceLock::new();

/// Initialise the global rayon pool (idempotent). Call once at start-up,
/// before any `par_iter` runs.
pub fn init_media_pool() -> ThreadBudget {
    *POOL.get_or_init(|| {
        let budget = ThreadBudget::detect();
        let cpus = budget.worker_cpus();
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        let result = rayon::ThreadPoolBuilder::new()
            .num_threads(budget.workers)
            .thread_name(|i| format!("rcp-media-{i}"))
            .start_handler(move |i| {
                if let Some(cpu) = cpus.get(i % cpus.len().max(1)) {
                    if let Some(core) = core_ids.iter().find(|c| c.id == *cpu) {
                        core_affinity::set_for_current(*core);
                    }
                }
            })
            .build_global();
        match result {
            Ok(()) => log::info!(
                "media pool: {} workers pinned to CPUs {:?}, {} threads reserved for UI/audio/driver",
                budget.workers,
                budget.worker_cpus(),
                budget.reserved
            ),
            Err(e) => log::warn!("rayon global pool already initialised: {e}"),
        }
        budget
    })
}

/// Budget in effect (initialising the pool if needed).
pub fn budget() -> ThreadBudget {
    init_media_pool()
}

/// Pin the current worker thread to the background worker CPU range (`4..16` on
/// Ryzen 7 5800X) so heavy background tasks never starve the UI / audio / GPU threads.
pub fn pin_current_to_background_pool() {
    let cpus = budget().worker_cpus();
    if cpus.is_empty() {
        return;
    }
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    let tid = std::thread::current().id();
    let hash = format!("{tid:?}").bytes().fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
    if let Some(cpu) = cpus.get(hash % cpus.len()) {
        if let Some(core) = core_ids.iter().find(|c| c.id == *cpu) {
            core_affinity::set_for_current(*core);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ryzen_5800x_budget() {
        let b = ThreadBudget::for_logical(16);
        assert_eq!(b.workers, 12);
        assert_eq!(b.reserved, 4);
        assert_eq!(b.worker_cpus(), (4..16).collect::<Vec<_>>());
    }

    #[test]
    fn budget_scales_down() {
        let b = ThreadBudget::for_logical(8);
        assert_eq!((b.workers, b.reserved), (4, 4));
        let b = ThreadBudget::for_logical(2);
        assert_eq!((b.workers, b.reserved), (1, 1));
        let b = ThreadBudget::for_logical(32);
        assert_eq!(b.workers, 12, "never more than 12 workers");
        assert!(b.worker_cpus().iter().all(|c| *c >= 4));
    }
}
