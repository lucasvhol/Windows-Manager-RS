//! Run long operations off the UI thread with progress reporting.

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::egui;

enum Msg<T> {
    Progress(f32, String),
    Done(Result<T, String>),
}

/// Handle given to the worker closure for reporting progress and checking cancellation.
pub struct Progress<T> {
    tx: Sender<Msg<T>>,
    ctx: egui::Context,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<T> Progress<T> {
    /// `fraction` in `0.0..=1.0`.
    pub fn report(&self, fraction: f32, status: impl Into<String>) {
        let _ = self
            .tx
            .send(Msg::Progress(fraction.clamp(0.0, 1.0), status.into()));
        self.ctx.request_repaint();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A background job whose state is polled from the UI each frame.
pub struct BackgroundTask<T> {
    rx: Receiver<Msg<T>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub progress: f32,
    pub status: String,
    result: Option<Result<T, String>>,
}

impl<T: Send + 'static> BackgroundTask<T> {
    pub fn spawn<F>(ctx: &egui::Context, work: F) -> Self
    where
        F: FnOnce(&Progress<T>) -> anyhow::Result<T> + Send + 'static,
    {
        let (tx, rx) = unbounded();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress = Progress {
            tx,
            ctx: ctx.clone(),
            cancel: cancel.clone(),
        };
        std::thread::spawn(move || {
            let result = work(&progress).map_err(|e| format!("{e:#}"));
            let _ = progress.tx.send(Msg::Done(result));
            progress.ctx.request_repaint();
        });
        Self {
            rx,
            cancel,
            progress: 0.0,
            status: String::new(),
            result: None,
        }
    }

    /// Drains pending messages; call once per frame.
    pub fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Progress(p, s) => {
                    self.progress = p;
                    self.status = s;
                }
                Msg::Done(r) => {
                    self.progress = 1.0;
                    self.result = Some(r);
                }
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.result.is_some()
    }

    /// Takes the finished result (once).
    pub fn take_result(&mut self) -> Option<Result<T, String>> {
        self.result.take()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Periodically produces values on a background thread (e.g. live CPU usage).
///
/// Sampling pauses automatically when the UI has not polled for a few seconds
/// (the tool is not visible) and the thread exits when the `Sampler` is dropped.
pub struct Sampler<T> {
    rx: Receiver<T>,
    shared: std::sync::Arc<SamplerShared>,
}

struct SamplerShared {
    alive: std::sync::atomic::AtomicBool,
    base: std::time::Instant,
    /// Milliseconds since `base` when the UI last polled.
    last_seen_ms: std::sync::atomic::AtomicU64,
}

impl SamplerShared {
    fn touch(&self) {
        let ms = self.base.elapsed().as_millis() as u64;
        self.last_seen_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    fn recently_seen(&self) -> bool {
        let now = self.base.elapsed().as_millis() as u64;
        now.saturating_sub(self.last_seen_ms.load(std::sync::atomic::Ordering::Relaxed)) < 3_000
    }
}

impl<T: Send + 'static> Sampler<T> {
    /// `init` runs once on the worker thread; `sample` runs every `interval`.
    pub fn spawn<S, I, F>(ctx: &egui::Context, interval: std::time::Duration, init: I, mut sample: F) -> Self
    where
        I: FnOnce() -> S + Send + 'static,
        F: FnMut(&mut S) -> T + Send + 'static,
    {
        let (tx, rx) = unbounded();
        let shared = std::sync::Arc::new(SamplerShared {
            alive: std::sync::atomic::AtomicBool::new(true),
            base: std::time::Instant::now(),
            last_seen_ms: std::sync::atomic::AtomicU64::new(0),
        });
        let worker = shared.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut state = init();
            while worker.alive.load(std::sync::atomic::Ordering::Relaxed) {
                if worker.recently_seen() {
                    if tx.send(sample(&mut state)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                std::thread::sleep(interval);
            }
        });
        Self { rx, shared }
    }

    /// All values produced since the last call (oldest first). Also marks the UI as active.
    pub fn drain(&mut self) -> Vec<T> {
        self.shared.touch();
        self.rx.try_iter().collect()
    }

    /// Only the most recent value since the last call, if any.
    pub fn latest(&mut self) -> Option<T> {
        self.drain().pop()
    }
}

impl<T> Drop for Sampler<T> {
    fn drop(&mut self) {
        self.shared
            .alive
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn reports_progress_and_result() {
        let ctx = egui::Context::default();
        let mut task = BackgroundTask::spawn(&ctx, |p| {
            p.report(0.5, "half");
            Ok(42)
        });
        let start = Instant::now();
        while !task.is_done() {
            assert!(start.elapsed() < Duration::from_secs(5), "task timed out");
            std::thread::sleep(Duration::from_millis(5));
            task.poll();
        }
        assert_eq!(task.take_result(), Some(Ok(42)));
        assert_eq!(task.status, "half");
    }

    #[test]
    fn errors_are_stringified() {
        let ctx = egui::Context::default();
        let mut task: BackgroundTask<()> = BackgroundTask::spawn(&ctx, |_| Err(anyhow::anyhow!("boom")));
        while !task.is_done() {
            std::thread::sleep(Duration::from_millis(5));
            task.poll();
        }
        assert_eq!(task.take_result(), Some(Err("boom".into())));
    }

    #[test]
    fn sampler_produces_values_while_polled() {
        let ctx = egui::Context::default();
        let mut sampler = Sampler::spawn(
            &ctx,
            Duration::from_millis(5),
            || 0u32,
            |n| {
                *n += 1;
                *n
            },
        );
        let start = Instant::now();
        let mut got = Vec::new();
        while got.len() < 3 {
            assert!(start.elapsed() < Duration::from_secs(5), "sampler timed out");
            got.extend(sampler.drain());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(&got[..3], &[1, 2, 3]);
    }
}
