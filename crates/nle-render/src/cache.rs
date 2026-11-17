//! Byte-budgeted LRU cache, used for GPU-resident frames.
//!
//! The Intel Arc A750 has 8 GB of VRAM. The decoded-frame cache is capped at
//! **2,560 MiB** so working textures, egui and the driver always fit and the
//! GPU never spills over PCIe into system RAM while scrubbing multi-track
//! 4K. Frames are cached as raw NV12/P010 storage buffers (≈12 MB per 4K
//! 8-bit frame, ≈200 frames) and converted on demand by the YUV shader.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

/// VRAM budget for cached frames on the Arc A750.
pub const VRAM_FRAME_CACHE_BUDGET: u64 = 2_560 * 1024 * 1024;

pub trait Weighted {
    /// Size in bytes charged against the budget.
    fn weight(&self) -> u64;
}

#[derive(Debug)]
pub struct LruCache<K, V> {
    map: HashMap<K, (V, u64)>,
    order: BTreeMap<u64, K>,
    tick: u64,
    used: u64,
    budget: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl<K: Hash + Eq + Clone, V: Weighted> LruCache<K, V> {
    pub fn new(budget: u64) -> Self {
        LruCache { map: HashMap::new(), order: BTreeMap::new(), tick: 0, used: 0, budget, hits: 0, misses: 0, evictions: 0 }
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }
    pub fn used(&self) -> u64 {
        self.used
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.hits, self.misses, self.evictions)
    }

    fn bump(&mut self, k: &K) {
        if let Some((_, t)) = self.map.get_mut(k) {
            self.order.remove(t);
            self.tick += 1;
            *t = self.tick;
            self.order.insert(self.tick, k.clone());
        }
    }

    pub fn contains(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    /// Look up and mark as most recently used.
    pub fn get(&mut self, k: &K) -> Option<&V> {
        if self.map.contains_key(k) {
            self.hits += 1;
            self.bump(k);
            self.map.get(k).map(|(v, _)| v)
        } else {
            self.misses += 1;
            None
        }
    }

    /// Insert, evicting least-recently-used entries until within budget.
    /// Items larger than the whole budget are not cached. Returns evicted values.
    pub fn insert(&mut self, k: K, v: V) -> Vec<V> {
        let w = v.weight();
        let mut evicted = Vec::new();
        if let Some(old) = self.remove(&k) {
            evicted.push(old);
        }
        if w > self.budget {
            evicted.push(v);
            return evicted;
        }
        while self.used + w > self.budget {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            if let Some((ov, _)) = self.map.remove(&oldest) {
                self.used -= ov.weight();
                self.evictions += 1;
                evicted.push(ov);
            }
        }
        self.tick += 1;
        self.order.insert(self.tick, k.clone());
        self.map.insert(k, (v, self.tick));
        self.used += w;
        evicted
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let (v, t) = self.map.remove(k)?;
        self.order.remove(&t);
        self.used -= v.weight();
        Some(v)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        let doomed: Vec<K> = self.map.keys().filter(|k| !keep(k)).cloned().collect();
        for k in doomed {
            self.remove(&k);
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.used = 0;
    }

    pub fn set_budget(&mut self, budget: u64) {
        self.budget = budget;
        while self.used > self.budget {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            if let Some((ov, _)) = self.map.remove(&oldest) {
                self.used -= ov.weight();
                self.evictions += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Blob(u64);
    impl Weighted for Blob {
        fn weight(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn evicts_least_recently_used_within_budget() {
        let mut c = LruCache::new(100);
        c.insert("a", Blob(40));
        c.insert("b", Blob(40));
        assert!(c.get(&"a").is_some()); // a is now most recent
        let ev = c.insert("c", Blob(40)); // must evict b
        assert_eq!(ev.len(), 1);
        assert!(c.contains(&"a") && c.contains(&"c") && !c.contains(&"b"));
        assert_eq!(c.used(), 80);
        // too large to ever fit
        c.insert("huge", Blob(1000));
        assert!(!c.contains(&"huge"));
        assert!(c.used() <= c.budget());
    }

    #[test]
    fn arc_a750_budget_holds_4k_frames() {
        let nv12_4k = (3840 * 2160 * 3 / 2) as u64;
        let mut c = LruCache::new(VRAM_FRAME_CACHE_BUDGET);
        for i in 0..1000u32 {
            c.insert(i, Blob(nv12_4k));
            assert!(c.used() <= VRAM_FRAME_CACHE_BUDGET);
        }
        assert_eq!(c.len() as u64, VRAM_FRAME_CACHE_BUDGET / nv12_4k);
        assert!(c.len() >= 200, "≈200 4K NV12 frames fit in 2.5 GB");
        // the newest frames are the ones kept
        assert!(c.contains(&999) && !c.contains(&0));
    }
}
