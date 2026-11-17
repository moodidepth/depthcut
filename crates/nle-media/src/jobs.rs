//! Background jobs with progress, cancellation and a result slot — polled by
//! the UI every frame (downloads, transcription, analysis).

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Inner<T> {
    progress: Mutex<(f32, String)>,
    result: Mutex<Option<Result<T, String>>>,
    cancel: AtomicBool,
    done: AtomicBool,
}

/// Handle to a job running on its own thread.
pub struct Job<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for Job<T> {
    fn clone(&self) -> Self {
        Job { inner: self.inner.clone() }
    }
}

/// Passed to the job body: report progress, check for cancellation.
pub struct JobCtx<T> {
    inner: Arc<Inner<T>>,
}

impl<T> JobCtx<T> {
    pub fn set_progress(&self, fraction: f32, message: impl Into<String>) {
        *self.inner.progress.lock() = (fraction.clamp(0.0, 1.0), message.into());
    }
    pub fn set_fraction(&self, fraction: f32) {
        self.inner.progress.lock().0 = fraction.clamp(0.0, 1.0);
    }
    pub fn cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Relaxed)
    }
    /// A cheap, `Send` cancellation probe for callbacks.
    pub fn cancel_flag(&self) -> impl Fn() -> bool + Send + Sync + 'static
    where
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        move || inner.cancel.load(Ordering::Relaxed)
    }
    /// A `Send` progress setter for callbacks.
    pub fn progress_fn(&self) -> impl Fn(f32) + Send + Sync + 'static
    where
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        move |f| inner.progress.lock().0 = f.clamp(0.0, 1.0)
    }
    /// A `Send` status-message setter for callbacks (keeps the fraction).
    pub fn message_fn(&self) -> impl Fn(String) + Send + Sync + 'static
    where
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        move |m| inner.progress.lock().1 = m
    }
}

impl<T: Send + 'static> Job<T> {
    pub fn spawn(name: &str, body: impl FnOnce(&JobCtx<T>) -> Result<T, String> + Send + 'static) -> Self {
        let inner = Arc::new(Inner { progress: Mutex::new((0.0, String::new())), result: Mutex::new(None), cancel: AtomicBool::new(false), done: AtomicBool::new(false) });
        let ctx = JobCtx { inner: inner.clone() };
        let thread_inner = inner.clone();
        let spawned = std::thread::Builder::new().name(format!("rcp-job-{name}")).spawn(move || {
            crate::threads::pin_current_to_background_pool();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&ctx))).unwrap_or_else(|_| Err("job panicked".into()));
            let r = if ctx.cancelled() && r.is_err() { Err("Cancelled".into()) } else { r };
            *thread_inner.result.lock() = Some(r);
            thread_inner.done.store(true, Ordering::Release);
        });
        if let Err(e) = spawned {
            *inner.result.lock() = Some(Err(format!("could not start job: {e}")));
            inner.done.store(true, Ordering::Release);
        }
        Job { inner }
    }

    /// An already finished job (for synchronous fallbacks).
    pub fn ready(result: Result<T, String>) -> Self {
        let inner = Arc::new(Inner { progress: Mutex::new((1.0, String::new())), result: Mutex::new(Some(result)), cancel: AtomicBool::new(false), done: AtomicBool::new(true) });
        Job { inner }
    }
}

impl<T> Job<T> {
    pub fn progress(&self) -> (f32, String) {
        self.inner.progress.lock().clone()
    }
    pub fn is_done(&self) -> bool {
        self.inner.done.load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        self.inner.cancel.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Relaxed)
    }
    /// Take the result once the job finished (returns `None` while running).
    pub fn take(&self) -> Option<Result<T, String>> {
        if !self.is_done() {
            return None;
        }
        self.inner.result.lock().take()
    }
    /// Block until done (tests, export).
    pub fn wait(&self) -> Option<Result<T, String>> {
        while !self.is_done() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.inner.result.lock().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_result_and_cancel() {
        let j = Job::spawn("t", |ctx| {
            ctx.set_progress(0.5, "half");
            Ok(42)
        });
        assert_eq!(j.wait(), Some(Ok(42)));
        assert!(j.take().is_none(), "result is taken once");

        let j: Job<()> = Job::spawn("c", |ctx| {
            while !ctx.cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err("stopped".into())
        });
        j.cancel();
        assert_eq!(j.wait(), Some(Err("Cancelled".into())));

        let j: Job<()> = Job::spawn("p", |_| panic!("boom"));
        assert_eq!(j.wait(), Some(Err("job panicked".into())));
    }
}
