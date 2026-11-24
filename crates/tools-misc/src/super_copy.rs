//! Super Copy: parallel file copy with skip-identical, overwrite policies, timestamp
//! preservation, BLAKE3 verification and a per-file report. Interrupted copies can be
//! resumed by running the same job again (completed, identical files are skipped).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Tool, egui,
    safety::UndoAction,
    task::{BackgroundTask, Progress},
    ui::{Message, format_bytes, show_message},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    Never,
    IfNewer,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub overwrite: Overwrite,
    pub verify: bool,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub copied: usize,
    pub skipped: usize,
    pub failed: Vec<String>,
    pub bytes: u64,
    /// Files that did not exist before (safe to remove on undo).
    pub created: Vec<PathBuf>,
    pub overwritten: usize,
}

/// (source file, destination file) pairs for copying `sources` into `dest_dir`.
pub fn plan(sources: &[PathBuf], dest_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut jobs = Vec::new();
    for src in sources {
        let base = src.file_name().context("invalid source")?;
        if dest_dir.starts_with(src) {
            bail!("cannot copy {} into itself", src.display());
        }
        if src.is_dir() {
            for e in WalkDir::new(src)
                .follow_links(false)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
            {
                let rel = e.path().strip_prefix(src)?;
                jobs.push((e.path().to_path_buf(), dest_dir.join(base).join(rel)));
            }
        } else {
            jobs.push((src.clone(), dest_dir.join(base)));
        }
    }
    Ok(jobs)
}

fn hash(path: &Path) -> Result<blake3::Hash> {
    let mut h = blake3::Hasher::new();
    h.update_reader(BufReader::new(File::open(path)?))?;
    Ok(h.finalize())
}

/// Same size and modification time (to the second): treat as already copied.
fn looks_identical(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    let secs = |m: &std::fs::Metadata| {
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    };
    a.len() == b.len() && secs(a) == secs(b)
}

fn copy_one(src: &Path, dst: &Path, verify: bool, copied_bytes: &AtomicU64) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_file_name(format!(
        "{}.wmcopy",
        dst.file_name().unwrap_or_default().to_string_lossy()
    ));
    let result = (|| -> Result<()> {
        let mut input = BufReader::with_capacity(1 << 20, File::open(src)?);
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            copied_bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        out.flush()?;
        drop(out);
        File::options()
            .write(true)
            .open(&tmp)?
            .set_modified(std::fs::metadata(src)?.modified()?)?;
        if verify && hash(src)? != hash(&tmp)? {
            bail!("verification failed (contents differ)");
        }
        // Replace the destination only after the new copy is complete (and verified).
        if dst.exists() {
            std::fs::remove_file(dst)?;
        }
        std::fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub fn run<T: Send>(jobs: &[(PathBuf, PathBuf)], opts: Options, progress: Option<&Progress<T>>) -> Report {
    let total: u64 = jobs
        .iter()
        .filter_map(|(s, _)| std::fs::metadata(s).ok())
        .map(|m| m.len())
        .sum();
    let bytes = AtomicU64::new(0);
    let report = Mutex::new(Report::default());
    jobs.par_iter().for_each(|(src, dst)| {
        if progress.is_some_and(|p| p.is_cancelled()) {
            return;
        }
        let outcome = (|| -> Result<Option<bool>> {
            let smd = std::fs::metadata(src)?;
            let existed = match std::fs::metadata(dst) {
                Ok(dmd) => {
                    let skip = match opts.overwrite {
                        _ if looks_identical(&smd, &dmd) => true,
                        Overwrite::Never => true,
                        Overwrite::IfNewer => smd.modified()? <= dmd.modified()?,
                        Overwrite::Always => false,
                    };
                    if skip {
                        bytes.fetch_add(smd.len(), Ordering::Relaxed);
                        return Ok(None);
                    }
                    true
                }
                Err(_) => false,
            };
            copy_one(src, dst, opts.verify, &bytes)?;
            Ok(Some(existed))
        })();
        let mut r = report.lock().unwrap();
        match outcome {
            Ok(None) => r.skipped += 1,
            Ok(Some(existed)) => {
                r.copied += 1;
                r.bytes += std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
                if existed {
                    r.overwritten += 1;
                } else {
                    r.created.push(dst.clone());
                }
            }
            Err(e) => r.failed.push(format!("{}: {e:#}", src.display())),
        }
        if let Some(p) = progress {
            let done = bytes.load(Ordering::Relaxed);
            p.report(
                done as f32 / total.max(1) as f32,
                format!("{} of {}", format_bytes(done), format_bytes(total)),
            );
        }
    });
    report.into_inner().unwrap()
}

pub struct SuperCopy {
    sources: Vec<PathBuf>,
    dest: Option<PathBuf>,
    overwrite: Overwrite,
    verify: bool,
    task: Option<BackgroundTask<Report>>,
    started: Option<std::time::Instant>,
    message: Message,
}

impl Default for SuperCopy {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            dest: None,
            overwrite: Overwrite::Never,
            verify: true,
            task: None,
            started: None,
            message: None,
        }
    }
}

impl Tool for SuperCopy {
    fn id(&self) -> &'static str {
        "supercopy"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                let secs = self
                    .started
                    .take()
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(1.0)
                    .max(0.001);
                self.message = Some(r.and_then(|rep| {
                    let desc = format!(
                        "Copied {} files ({}), skipped {}{}",
                        rep.copied,
                        format_bytes(rep.bytes),
                        rep.skipped,
                        if rep.overwritten > 0 {
                            format!(", replaced {}", rep.overwritten)
                        } else {
                            String::new()
                        }
                    );
                    let _ = ctx.journal.record(
                        "supercopy",
                        desc.clone(),
                        UndoAction::DeleteCreatedFiles {
                            paths: rep.created.clone(),
                        },
                    );
                    let speed = format!(" at {}/s", format_bytes((rep.bytes as f64 / secs) as u64));
                    if rep.failed.is_empty() {
                        Ok(format!("{desc}{speed}."))
                    } else {
                        Err(format!(
                            "{desc}. {} failed: {}",
                            rep.failed.len(),
                            rep.failed.join("; ")
                        ))
                    }
                }));
            }
        }

        ui.horizontal(|ui| {
            if ui.button("Add files…").clicked()
                && let Some(f) = rfd::FileDialog::new().pick_files()
            {
                self.sources.extend(f);
            }
            if ui.button("Add folder…").clicked()
                && let Some(d) = rfd::FileDialog::new().pick_folder()
            {
                self.sources.push(d);
            }
            if ui
                .add_enabled(!self.sources.is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                self.sources.clear();
            }
        });
        for s in &self.sources {
            ui.weak(s.display().to_string());
        }
        ui.horizontal(|ui| {
            ui.label("Copy to:");
            ui.label(
                self.dest
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(choose a folder)".into()),
            );
            if ui.button("Choose…").clicked()
                && let Some(d) = rfd::FileDialog::new().pick_folder()
            {
                self.dest = Some(d);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Existing files:");
            ui.selectable_value(&mut self.overwrite, Overwrite::Never, "Keep");
            ui.selectable_value(
                &mut self.overwrite,
                Overwrite::IfNewer,
                "Replace if source is newer",
            );
            ui.selectable_value(&mut self.overwrite, Overwrite::Always, "Replace");
        });
        ui.checkbox(&mut self.verify, "Verify every copied file (BLAKE3 checksum)");
        if self.overwrite != Overwrite::Never {
            ui.weak("Replaced files are not kept; undo only removes files that did not exist before.");
        }
        ui.weak("Identical files (same size and date) are always skipped, so re-running a job resumes it.");

        let ready = !self.sources.is_empty() && self.dest.is_some() && self.task.is_none();
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new("Start copy")).clicked() {
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                } else if let Some(dest) = self.dest.clone() {
                    match plan(&self.sources, &dest) {
                        Err(e) => self.message = Some(Err(format!("{e:#}"))),
                        Ok(jobs) => {
                            let opts = Options {
                                overwrite: self.overwrite,
                                verify: self.verify,
                            };
                            self.message = None;
                            self.started = Some(std::time::Instant::now());
                            self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                                Ok(run(&jobs, opts, Some(p)))
                            }));
                        }
                    }
                }
            }
            if let Some(t) = &self.task {
                ui.add(
                    egui::ProgressBar::new(t.progress)
                        .desired_width(260.0)
                        .text(t.status.clone()),
                );
                if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                    t.cancel();
                }
            }
        });
        show_message(ui, &self.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::write(root.join("a.txt"), "alpha").unwrap();
        std::fs::write(root.join("sub/b.bin"), vec![7u8; 300_000]).unwrap();
        std::fs::write(root.join("sub/deep/c.txt"), "gamma").unwrap();
    }

    #[test]
    fn copies_tree_skips_identical_and_respects_policies() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        tree(&src);
        std::fs::create_dir_all(&dst).unwrap();
        let opts = Options {
            overwrite: Overwrite::Never,
            verify: true,
        };

        let jobs = plan(std::slice::from_ref(&src), &dst).unwrap();
        assert_eq!(jobs.len(), 3);
        let r = run::<()>(&jobs, opts, None);
        assert_eq!(
            (r.copied, r.skipped, r.failed.len(), r.created.len()),
            (3, 0, 0, 3)
        );
        assert_eq!(
            std::fs::read(dst.join("src/sub/b.bin")).unwrap(),
            vec![7u8; 300_000]
        );
        assert_eq!(
            std::fs::metadata(dst.join("src/a.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            std::fs::metadata(src.join("a.txt")).unwrap().modified().unwrap(),
            "timestamps preserved"
        );

        // Second run: everything identical → skipped (resume behaviour).
        let r = run::<()>(&jobs, opts, None);
        assert_eq!((r.copied, r.skipped), (0, 3));

        // A changed source is kept with Never, replaced with Always.
        std::fs::write(src.join("a.txt"), "ALPHA, changed").unwrap();
        assert_eq!(run::<()>(&jobs, opts, None).copied, 0);
        let r = run::<()>(
            &jobs,
            Options {
                overwrite: Overwrite::Always,
                verify: true,
            },
            None,
        );
        assert_eq!((r.copied, r.overwritten, r.created.len()), (1, 1, 0));
        assert_eq!(
            std::fs::read_to_string(dst.join("src/a.txt")).unwrap(),
            "ALPHA, changed"
        );
        assert!(!dst.join("src/a.txt.wmcopy").exists(), "no temp files left");
    }

    #[test]
    fn refuses_copy_into_itself() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plan(&[dir.path().to_owned()], &dir.path().join("inner")).is_err());
    }
}
