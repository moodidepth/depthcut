//! Bounded ring-buffer frame queues.
//!
//! Decoders run ahead of the UI by a fixed number of frames. When the queue
//! is full the producer either blocks (playback/export: never lose frames)
//! or overwrites the oldest entry (scrubbing: latest wins), so memory use
//! is bounded no matter how fast the decoder is.

use crossbeam::queue::ArrayQueue;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Lock-free drop-oldest ring buffer (`crossbeam::queue::ArrayQueue`) for
/// zero-wait decoded frame handoff between hardware decode threads and the
/// GPU render thread.
pub struct SpscFrameRing<T> {
    ring: ArrayQueue<T>,
    dropped: AtomicU64,
}

impl<T> SpscFrameRing<T> {
    pub fn new(capacity: usize) -> Self {
        SpscFrameRing { ring: ArrayQueue::new(capacity.max(1)), dropped: AtomicU64::new(0) }
    }
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }
    pub fn len(&self) -> usize {
        self.ring.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    /// Lock-free push that overwrites the oldest entry if the ring is full.
    pub fn push_overwrite(&self, item: T) {
        if self.ring.force_push(item).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn try_pop(&self) -> Option<T> {
        self.ring.pop()
    }
    pub fn drain(&self) -> Vec<T> {
        let mut out = Vec::with_capacity(self.ring.len());
        while let Some(v) = self.ring.pop() {
            out.push(v);
        }
        out
    }
}

#[derive(Debug)]
pub struct FrameQueue<T> {
    inner: Mutex<Inner<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    capacity: usize,
}

#[derive(Debug)]
struct Inner<T> {
    buf: VecDeque<T>,
    closed: bool,
    dropped: u64,
}

impl<T> FrameQueue<T> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        FrameQueue { inner: Mutex::new(Inner { buf: VecDeque::with_capacity(capacity), closed: false, dropped: 0 }), not_empty: Condvar::new(), not_full: Condvar::new(), capacity }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn len(&self) -> usize {
        self.inner.lock().buf.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Number of frames discarded by [`push_overwrite`](Self::push_overwrite).
    pub fn dropped(&self) -> u64 {
        self.inner.lock().dropped
    }

    /// Push, overwriting the oldest element if full. Never blocks.
    pub fn push_overwrite(&self, item: T) {
        let mut g = self.inner.lock();
        if g.buf.len() == self.capacity {
            g.buf.pop_front();
            g.dropped += 1;
        }
        g.buf.push_back(item);
        drop(g);
        self.not_empty.notify_one();
    }

    /// Push, waiting up to `timeout` for space. Returns the item back if the
    /// queue stayed full or was closed.
    pub fn push_timeout(&self, item: T, timeout: Duration) -> Result<(), T> {
        let mut g = self.inner.lock();
        while g.buf.len() == self.capacity && !g.closed {
            if self.not_full.wait_for(&mut g, timeout).timed_out() {
                return Err(item);
            }
        }
        if g.closed {
            return Err(item);
        }
        g.buf.push_back(item);
        drop(g);
        self.not_empty.notify_one();
        Ok(())
    }

    pub fn try_pop(&self) -> Option<T> {
        let item = self.inner.lock().buf.pop_front();
        if item.is_some() {
            self.not_full.notify_one();
        }
        item
    }

    pub fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        let mut g = self.inner.lock();
        while g.buf.is_empty() {
            if g.closed || self.not_empty.wait_for(&mut g, timeout).timed_out() {
                break;
            }
        }
        let item = g.buf.pop_front();
        drop(g);
        if item.is_some() {
            self.not_full.notify_one();
        }
        item
    }

    /// Take everything currently queued.
    pub fn drain(&self) -> Vec<T> {
        let v: Vec<T> = self.inner.lock().buf.drain(..).collect();
        self.not_full.notify_all();
        v
    }

    pub fn clear(&self) {
        self.inner.lock().buf.clear();
        self.not_full.notify_all();
    }

    pub fn close(&self) {
        self.inner.lock().closed = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn overwrite_keeps_latest() {
        let q = FrameQueue::new(3);
        for i in 0..10 {
            q.push_overwrite(i);
        }
        assert_eq!(q.len(), 3);
        assert_eq!(q.dropped(), 7);
        assert_eq!(q.drain(), vec![7, 8, 9]);
    }

    #[test]
    fn blocking_push_applies_backpressure() {
        let q = Arc::new(FrameQueue::new(2));
        q.push_timeout(1, Duration::from_millis(1)).unwrap();
        q.push_timeout(2, Duration::from_millis(1)).unwrap();
        assert_eq!(q.push_timeout(3, Duration::from_millis(5)), Err(3));
        let q2 = q.clone();
        let consumer = std::thread::spawn(move || {
            let mut got = Vec::new();
            while got.len() < 4 {
                if let Some(v) = q2.pop_timeout(Duration::from_millis(500)) {
                    got.push(v);
                }
            }
            got
        });
        q.push_timeout(3, Duration::from_secs(2)).unwrap();
        q.push_timeout(4, Duration::from_secs(2)).unwrap();
        assert_eq!(consumer.join().unwrap(), vec![1, 2, 3, 4]);
    }
}
