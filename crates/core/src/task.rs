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
}
