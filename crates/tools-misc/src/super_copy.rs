//! Super Copy: parallel file copy with skip-identical, overwrite policies, timestamp
//! preservation, BLAKE3 verification and a per-file log. Copy, mirror (also removes
//! destination files that are gone from the source) and move modes; thread count, folder
//! depth, empty folders, folder timestamps, retries and include / exclude filters; saved
//! schemes; shut down when done.
//!
//! Interrupted copies resume by running the same job again (identical files are skipped).
//! Nothing is deleted for good: mirror's extra files and move's sources go to the Recycle
//! Bin through the Change Journal.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::{BackgroundTask, Progress},
    tool_settings,
    ui::{Confirm, Message, format_bytes, show_message},
};

const TOOL: &str = "supercopy";
/// Seconds before Windows shuts down after a job, so it can still be cancelled.
const SHUTDOWN_DELAY: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Overwrite {
    #[default]
    Never,
    IfNewer,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Mode {
    #[default]
    Copy,
    /// Copy, then remove destination files that no longer exist in the source.
    Mirror,
    /// Copy, then remove the source files.
    Move,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Copy => "Copy",
            Mode::Mirror => "Mirror",
            Mode::Move => "Move",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub mode: Mode,
    pub overwrite: Overwrite,
    pub verify: bool,
    /// 0 = one per CPU core.
    pub threads: usize,
    /// Folder levels below each source folder (0 = no limit).
    pub levels: usize,
    pub empty_dirs: bool,
    pub dir_times: bool,
    pub retries: u32,
    pub retry_wait_secs: u64,
    /// File name patterns separated by `;` (empty = all files).
    pub include: String,
    pub exclude: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Copy,
            overwrite: Overwrite::Never,
            verify: true,
            threads: 0,
            levels: 0,
            empty_dirs: true,
            dir_times: true,
            retries: 2,
            retry_wait_secs: 2,
            include: String::new(),
            exclude: String::new(),
        }
    }
}

fn patterns(s: &str) -> Vec<String> {
    s.split(';')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Whether a file name passes the include and exclude filters.
pub fn wanted(name: &str, include: &[String], exclude: &[String]) -> bool {
    let n = name.to_lowercase();
    (include.is_empty() || include.iter().any(|p| paths::wildcard_match(p, &n)))
        && !exclude.iter().any(|p| paths::wildcard_match(p, &n))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Plan {
    /// (source file, destination file)
    pub files: Vec<(PathBuf, PathBuf)>,
    /// (source folder, destination folder), parents before children.
    pub dirs: Vec<(PathBuf, PathBuf)>,
    /// Mirror: destination files with no source any more.
    pub extras: Vec<PathBuf>,
    /// The sources as given (files and folders).
    pub sources: Vec<PathBuf>,
    /// Filters or a level limit left some files out, so folders aren't moved whole.
    pub partial: bool,
}

/// What to copy from `sources` into `dest_dir`.
pub fn plan(sources: &[PathBuf], dest_dir: &Path, opts: &Options) -> Result<Plan> {
    let (inc, exc) = (patterns(&opts.include), patterns(&opts.exclude));
    let mut p = Plan {
        sources: sources.to_vec(),
        partial: !inc.is_empty() || !exc.is_empty() || opts.levels > 0,
        ..Plan::default()
    };
    for src in sources {
        let base = src.file_name().context("invalid source")?;
        if dest_dir.starts_with(src) {
            bail!("cannot copy {} into itself", src.display());
        }
        if !src.is_dir() {
            if src.is_file() {
                p.files.push((src.clone(), dest_dir.join(base)));
            }
            continue;
        }
        let root = dest_dir.join(base);
        let mut walk = WalkDir::new(src).follow_links(false);
        if opts.levels > 0 {
            // Files in the folder itself are depth 1; each level adds one.
            walk = walk.max_depth(opts.levels + 1);
        }
        let mut expected = std::collections::HashSet::new();
        for e in walk.into_iter().flatten() {
            let rel = e.path().strip_prefix(src)?;
            let dst = root.join(rel);
            if e.file_type().is_dir() {
                // A folder at depth N is level N; deeper ones are past the limit (their
                // files were already cut off by max_depth).
                if opts.levels == 0 || e.depth() <= opts.levels {
                    p.dirs.push((e.path().to_path_buf(), dst));
                }
            } else if e.file_type().is_file() && wanted(&e.file_name().to_string_lossy(), &inc, &exc) {
                expected.insert(dst.clone());
                p.files.push((e.path().to_path_buf(), dst));
            }
        }
        if opts.mode == Mode::Mirror && root.is_dir() {
            let mut walk = WalkDir::new(&root).follow_links(false);
            if opts.levels > 0 {
                walk = walk.max_depth(opts.levels + 1);
            }
            for e in walk.into_iter().flatten().filter(|e| e.file_type().is_file()) {
                let name = e.file_name().to_string_lossy();
                // Files outside the filters are left alone, as are our temporary files.
                if !expected.contains(e.path()) && wanted(&name, &inc, &exc) && !name.ends_with(".wmcopy") {
                    p.extras.push(e.path().to_path_buf());
                }
            }
        }
    }
    Ok(p)
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
    let mut counted = 0u64;
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
            counted += n as u64;
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
        // A retry counts its bytes again.
        copied_bytes.fetch_sub(counted, Ordering::Relaxed);
    }
    result
}

/// Sets a folder's modified time (folders need backup semantics to be opened).
fn set_dir_time(dir: &Path, time: std::time::SystemTime) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    File::options()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .set_modified(time)?;
    Ok(())
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
    pub dirs_created: usize,
    /// For the UI to send to the Recycle Bin: mirror's extras or move's sources.
    pub to_recycle: Vec<PathBuf>,
    pub log: Vec<String>,
    pub cancelled: bool,
}

pub fn run<T: Send>(plan: &Plan, opts: &Options, progress: Option<&Progress<T>>) -> Report {
    let total: u64 = plan
        .files
        .iter()
        .filter_map(|(s, _)| std::fs::metadata(s).ok())
        .map(|m| m.len())
        .sum();
    let bytes = AtomicU64::new(0);
    let report = Mutex::new(Report::default());
    let cancelled = || progress.is_some_and(|p| p.is_cancelled());
    let work = || {
        plan.files.par_iter().for_each(|(src, dst)| {
            if cancelled() {
                return;
            }
            let mut attempt = 0;
            let outcome = loop {
                let r = (|| -> Result<Option<bool>> {
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
                match r {
                    Err(_) if attempt < opts.retries && !cancelled() => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_secs(opts.retry_wait_secs));
                    }
                    r => break r,
                }
            };
            let mut r = report.lock().unwrap();
            let line = match outcome {
                Ok(None) => {
                    r.skipped += 1;
                    if opts.mode == Mode::Move {
                        r.to_recycle.push(src.clone());
                    }
                    format!("skipped   {}", dst.display())
                }
                Ok(Some(existed)) => {
                    r.copied += 1;
                    r.bytes += std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
                    if existed {
                        r.overwritten += 1;
                    } else {
                        r.created.push(dst.clone());
                    }
                    if opts.mode == Mode::Move {
                        r.to_recycle.push(src.clone());
                    }
                    format!(
                        "{} {} -> {}",
                        if existed { "replaced " } else { "copied   " },
                        src.display(),
                        dst.display()
                    )
                }
                Err(e) => {
                    let msg = format!("{}: {e:#}", src.display());
                    r.failed.push(msg.clone());
                    format!("FAILED    {msg}")
                }
            };
            r.log.push(line);
            drop(r);
            if let Some(p) = progress {
                let done = bytes.load(Ordering::Relaxed);
                p.report(
                    done as f32 / total.max(1) as f32,
                    format!("{} of {}", format_bytes(done), format_bytes(total)),
                );
            }
        });
    };
    match rayon::ThreadPoolBuilder::new().num_threads(opts.threads).build() {
        Ok(pool) => pool.install(work),
        Err(_) => work(),
    }
    let mut r = report.into_inner().unwrap();
    r.cancelled = cancelled();
    if r.cancelled {
        r.log.push("Cancelled.".into());
        // Never remove sources or extras after a partial run.
        r.to_recycle.clear();
        return r;
    }
    if opts.empty_dirs {
        for (_, dst) in &plan.dirs {
            if !dst.exists() && std::fs::create_dir_all(dst).is_ok() {
                r.dirs_created += 1;
                r.log.push(format!("folder    {}", dst.display()));
            }
        }
    }
    // Folder dates last and deepest first, since adding entries changes a folder's date.
    if opts.dir_times {
        for (src, dst) in plan.dirs.iter().rev() {
            if dst.is_dir()
                && let Ok(t) = std::fs::metadata(src).and_then(|m| m.modified())
            {
                let _ = set_dir_time(dst, t);
            }
        }
    }
    match opts.mode {
        Mode::Mirror => r.to_recycle = plan.extras.clone(),
        // Everything was copied: recycle each source as one item (a folder with its whole
        // tree), so the Recycle Bin can put it back exactly. With filters only the copied
        // files go, and their folders stay.
        Mode::Move if r.failed.is_empty() && !plan.partial => r.to_recycle = plan.sources.clone(),
        Mode::Move if r.failed.is_empty() => {}
        // Keep every source when anything failed, so nothing is lost.
        Mode::Move => r.to_recycle.clear(),
        Mode::Copy => {}
    }
    r
}

pub fn write_log(report: &Report, header: &str) -> Result<PathBuf> {
    let dir = paths::logs_dir()?;
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("supercopy-{stamp}.log"));
    let mut text = format!("{header}\r\n\r\n");
    for l in &report.log {
        text += l;
        text += "\r\n";
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

// ---------------------------------------------------------------------------------
// Schemes

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Scheme {
    pub name: String,
    pub sources: Vec<PathBuf>,
    pub dest: Option<PathBuf>,
    pub options: Options,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct Saved {
    schemes: Vec<Scheme>,
    log: bool,
}

// ---------------------------------------------------------------------------------
// UI

pub struct SuperCopy {
    sources: Vec<PathBuf>,
    dest: Option<PathBuf>,
    opts: Options,
    saved: Saved,
    scheme_name: String,
    shutdown: bool,
    shutdown_pending: bool,
    task: Option<BackgroundTask<Report>>,
    started: Option<std::time::Instant>,
    confirm: Confirm<()>,
    last_log: Option<PathBuf>,
    message: Message,
}

impl Default for SuperCopy {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            dest: None,
            opts: Options::default(),
            saved: tool_settings::load(TOOL),
            scheme_name: String::new(),
            shutdown: false,
            shutdown_pending: false,
            task: None,
            started: None,
            confirm: Confirm::default(),
            last_log: None,
            message: None,
        }
    }
}

impl SuperCopy {
    fn save_settings(&mut self) {
        if let Err(e) = tool_settings::save(TOOL, &self.saved) {
            self.message = Some(Err(format!("{e:#}")));
        }
    }

    fn finish(&mut self, ctx: &AppCtx<'_>, rep: Report) -> Result<String, String> {
        let secs = self
            .started
            .take()
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(1.0)
            .max(0.001);
        let mut desc = format!(
            "{}: {} files copied ({}), {} skipped",
            self.opts.mode.label(),
            rep.copied,
            format_bytes(rep.bytes),
            rep.skipped
        );
        if rep.overwritten > 0 {
            desc += &format!(", {} replaced", rep.overwritten);
        }
        if rep.dirs_created > 0 {
            desc += &format!(", {} empty folders", rep.dirs_created);
        }
        let _ = ctx.journal.record(
            TOOL,
            desc.clone(),
            UndoAction::DeleteCreatedFiles {
                paths: rep.created.clone(),
            },
        );
        let mut extra = String::new();
        if !rep.to_recycle.is_empty() {
            let n = rep.to_recycle.len();
            let what = if self.opts.mode == Mode::Move {
                format!("Move: sent the {n} source item(s) to the Recycle Bin")
            } else {
                format!("Mirror: moved {n} files no longer in the source to the Recycle Bin")
            };
            match ctx.journal.recycle(TOOL, what.clone(), rep.to_recycle.clone()) {
                Ok(_) => extra += &format!(" {what}."),
                Err(e) => extra += &format!(" Couldn't recycle: {e:#}."),
            }
        }
        if self.saved.log {
            match write_log(&rep, &desc) {
                Ok(p) => self.last_log = Some(p),
                Err(e) => extra += &format!(" Log not written: {e:#}."),
            }
        }
        if self.shutdown && !rep.cancelled {
            let r = run_checked(hidden_command("shutdown").args([
                "/s",
                "/t",
                &SHUTDOWN_DELAY.to_string(),
                "/c",
                "Win Manager RS finished copying. Windows will shut down in one minute.",
            ]));
            match r {
                Ok(_) => self.shutdown_pending = true,
                Err(e) => extra += &format!(" Shutting down failed: {e:#}."),
            }
        }
        let speed = format!(" at {}/s", format_bytes((rep.bytes as f64 / secs) as u64));
        if rep.cancelled {
            Err(format!("{desc}. Cancelled; run it again to resume.{extra}"))
        } else if rep.failed.is_empty() {
            Ok(format!("{desc}{speed}.{extra}"))
        } else {
            let more = if self.opts.mode == Mode::Move {
                " No sources were removed."
            } else {
                ""
            };
            Err(format!(
                "{desc}. {} failed: {}.{more}{extra}",
                rep.failed.len(),
                rep.failed.join("; ")
            ))
        }
    }

    fn start(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let Some(dest) = self.dest.clone() else { return };
        match plan(&self.sources, &dest, &self.opts) {
            Err(e) => self.message = Some(Err(format!("{e:#}"))),
            Ok(p) => {
                let opts = self.opts.clone();
                self.message = None;
                self.started = Some(std::time::Instant::now());
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |pr| {
                    Ok(run(&p, &opts, Some(pr)))
                }));
            }
        }
    }

    fn schemes_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Scheme");
            let mut load = None;
            egui::ComboBox::from_id_salt("sc-scheme")
                .selected_text(if self.scheme_name.is_empty() {
                    "(unsaved)"
                } else {
                    self.scheme_name.as_str()
                })
                .show_ui(ui, |ui| {
                    for (i, s) in self.saved.schemes.iter().enumerate() {
                        if ui.selectable_label(s.name == self.scheme_name, &s.name).clicked() {
                            load = Some(i);
                        }
                    }
                });
            if let Some(i) = load {
                let s = self.saved.schemes[i].clone();
                self.scheme_name = s.name;
                self.sources = s.sources;
                self.dest = s.dest;
                self.opts = s.options;
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.scheme_name)
                    .hint_text("Name")
                    .desired_width(140.0),
            );
            let name = self.scheme_name.trim().to_owned();
            if ui
                .add_enabled(
                    !name.is_empty() && !self.sources.is_empty(),
                    egui::Button::new("Save"),
                )
                .on_hover_text("Saves the sources, destination and options under this name.")
                .clicked()
            {
                let s = Scheme {
                    name: name.clone(),
                    sources: self.sources.clone(),
                    dest: self.dest.clone(),
                    options: self.opts.clone(),
                };
                match self.saved.schemes.iter_mut().find(|x| x.name == name) {
                    Some(x) => *x = s,
                    None => self.saved.schemes.push(s),
                }
                self.save_settings();
                self.message = Some(Ok(format!("Saved scheme \"{name}\".")));
            }
            let exists = self.saved.schemes.iter().any(|x| x.name == name);
            if ui.add_enabled(exists, egui::Button::new("Delete")).clicked() {
                self.saved.schemes.retain(|x| x.name != name);
                self.scheme_name.clear();
                self.save_settings();
            }
        });
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        let o = &mut self.opts;
        ui.horizontal(|ui| {
            ui.label("Mode");
            ui.selectable_value(&mut o.mode, Mode::Copy, "Copy")
                .on_hover_text("Copies the sources; nothing is removed.");
            ui.selectable_value(&mut o.mode, Mode::Mirror, "Mirror").on_hover_text(
                "Makes the destination match the source: files no longer in the source go to the Recycle Bin.",
            );
            ui.selectable_value(&mut o.mode, Mode::Move, "Move").on_hover_text(
                "Copies, then moves the sources to the Recycle Bin (only when every file copied).",
            );
        });
        ui.horizontal(|ui| {
            ui.label("Existing files");
            ui.selectable_value(&mut o.overwrite, Overwrite::Never, "Keep");
            ui.selectable_value(&mut o.overwrite, Overwrite::IfNewer, "Replace if source is newer");
            ui.selectable_value(&mut o.overwrite, Overwrite::Always, "Replace");
        });
        egui::CollapsingHeader::new("More options").show(ui, |ui| {
            egui::Grid::new("sc-opts")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Include files");
                    ui.add(
                        egui::TextEdit::singleline(&mut o.include)
                            .hint_text("*.jpg; *.png (empty = all)")
                            .desired_width(260.0),
                    );
                    ui.end_row();
                    ui.label("Exclude files");
                    ui.add(
                        egui::TextEdit::singleline(&mut o.exclude)
                            .hint_text("*.tmp; ~$*")
                            .desired_width(260.0),
                    );
                    ui.end_row();
                    ui.label("Folder levels");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut o.levels).range(0..=64));
                        ui.weak(if o.levels == 0 {
                            "no limit"
                        } else {
                            "below each source folder"
                        });
                    });
                    ui.end_row();
                    ui.label("Threads");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut o.threads).range(0..=64));
                        if o.threads == 0 {
                            ui.weak("one per processor core");
                        }
                    });
                    ui.end_row();
                    ui.label("Retries");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut o.retries).range(0..=20));
                        ui.label("waiting");
                        ui.add(
                            egui::DragValue::new(&mut o.retry_wait_secs)
                                .range(0..=600)
                                .suffix(" s"),
                        );
                    });
                    ui.end_row();
                });
            ui.checkbox(&mut o.empty_dirs, "Copy empty folders");
            ui.checkbox(&mut o.dir_times, "Keep folder dates");
            ui.checkbox(&mut o.verify, "Verify every copied file (BLAKE3 checksum)");
        });
    }
}

impl Tool for SuperCopy {
    fn id(&self) -> &'static str {
        TOOL
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                self.message = Some(r.and_then(|rep| self.finish(ctx, rep)));
            }
        }
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            self.start(ui, ctx);
        }

        self.schemes_ui(ui);
        ui.separator();
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
        self.options_ui(ui);
        let before = self.saved.log;
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.saved.log, "Write a log file");
            if let Some(p) = &self.last_log
                && ui.small_button("Open last log").clicked()
            {
                let _ = wm_core::shell::shell_open(&p.display().to_string(), "");
            }
            ui.checkbox(&mut self.shutdown, "Shut down Windows when done");
        });
        if self.saved.log != before {
            self.save_settings();
        }
        if self.opts.overwrite != Overwrite::Never {
            ui.weak("Replaced files are not kept; undo only removes files that did not exist before.");
        }
        ui.weak("Identical files (same size and date) are always skipped, so re-running a job resumes it.");

        let ready = !self.sources.is_empty() && self.dest.is_some() && self.task.is_none();
        ui.horizontal(|ui| {
            let label = format!("Start {}", self.opts.mode.label().to_lowercase());
            if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                match self.opts.mode {
                    Mode::Copy => self.start(ui, ctx),
                    Mode::Mirror => self.confirm.ask(
                        "Mirror: files in the destination that are no longer in the source go to the Recycle Bin. Continue?",
                        (),
                    ),
                    Mode::Move => self.confirm.ask(
                        "Move: after every file is copied, the sources go to the Recycle Bin. Continue?",
                        (),
                    ),
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
        if self.shutdown_pending {
            ui.horizontal(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Windows shuts down in {SHUTDOWN_DELAY} seconds."),
                );
                if ui.button("Cancel shutdown").clicked() {
                    self.shutdown_pending = false;
                    self.message = Some(
                        run_checked(hidden_command("shutdown").arg("/a"))
                            .map(|_| "Shutdown cancelled.".into())
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("a.txt"), "alpha").unwrap();
        std::fs::write(root.join("sub/b.bin"), vec![7u8; 300_000]).unwrap();
        std::fs::write(root.join("sub/deep/c.txt"), "gamma").unwrap();
    }

    fn opts() -> Options {
        Options {
            retries: 0,
            ..Options::default()
        }
    }

    #[test]
    fn copies_tree_skips_identical_and_respects_policies() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        tree(&src);
        std::fs::create_dir_all(&dst).unwrap();
        let o = opts();

        let p = plan(std::slice::from_ref(&src), &dst, &o).unwrap();
        assert_eq!(p.files.len(), 3);
        let r = run::<()>(&p, &o, None);
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
        assert!(dst.join("src/empty").is_dir(), "empty folders copied");
        assert_eq!(
            std::fs::metadata(dst.join("src/sub"))
                .unwrap()
                .modified()
                .unwrap(),
            std::fs::metadata(src.join("sub")).unwrap().modified().unwrap(),
            "folder dates kept"
        );
        assert_eq!(r.log.len(), 4, "one log line per file plus the empty folder");

        // Second run: everything identical → skipped (resume behaviour).
        let r = run::<()>(&p, &o, None);
        assert_eq!((r.copied, r.skipped), (0, 3));

        // A changed source is kept with Never, replaced with Always.
        std::fs::write(src.join("a.txt"), "ALPHA, changed").unwrap();
        assert_eq!(run::<()>(&p, &o, None).copied, 0);
        let always = Options {
            overwrite: Overwrite::Always,
            ..opts()
        };
        let r = run::<()>(&p, &always, None);
        assert_eq!((r.copied, r.overwritten, r.created.len()), (1, 1, 0));
        assert_eq!(
            std::fs::read_to_string(dst.join("src/a.txt")).unwrap(),
            "ALPHA, changed"
        );
        assert!(!dst.join("src/a.txt.wmcopy").exists(), "no temp files left");
    }

    #[test]
    fn filters_and_levels() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        tree(&src);
        let only_txt = Options {
            include: "*.TXT".into(),
            ..opts()
        };
        let p = plan(std::slice::from_ref(&src), &dir.path().join("x"), &only_txt).unwrap();
        assert_eq!(p.files.len(), 2);
        let shallow = Options {
            exclude: "a.*".into(),
            levels: 1,
            ..opts()
        };
        let p = plan(std::slice::from_ref(&src), &dir.path().join("x"), &shallow).unwrap();
        let names: Vec<_> = p
            .files
            .iter()
            .map(|(s, _)| s.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["b.bin"], "a.txt excluded, c.txt below the level limit");
        assert!(
            !p.dirs.iter().any(|(s, _)| s.ends_with("deep")),
            "levels limit the walk"
        );
        assert!(wanted("photo.JPG", &patterns("*.jpg; *.png"), &[]));
        assert!(!wanted("notes.txt", &patterns("*.jpg"), &[]));
        assert!(!wanted("~$doc.docx", &[], &patterns("~$*")));
    }

    #[test]
    fn mirror_finds_extras_and_move_lists_sources() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        tree(&src);
        let mirror = Options {
            mode: Mode::Mirror,
            ..opts()
        };
        let p = plan(std::slice::from_ref(&src), &dst, &mirror).unwrap();
        run::<()>(&p, &mirror, None);
        std::fs::write(dst.join("src/old.txt"), "gone from source").unwrap();
        let p = plan(std::slice::from_ref(&src), &dst, &mirror).unwrap();
        assert_eq!(p.extras, vec![dst.join("src/old.txt")]);
        let r = run::<()>(&p, &mirror, None);
        assert_eq!(r.to_recycle, vec![dst.join("src/old.txt")]);

        let mv = Options {
            mode: Mode::Move,
            ..opts()
        };
        let p = plan(std::slice::from_ref(&src), &dir.path().join("moved"), &mv).unwrap();
        let r = run::<()>(&p, &mv, None);
        assert_eq!(r.to_recycle, vec![src.clone()], "a whole folder goes as one item");
        assert!(src.join("a.txt").exists(), "run() itself never deletes sources");

        let filtered = Options {
            include: "*.txt".into(),
            ..mv
        };
        let p = plan(std::slice::from_ref(&src), &dir.path().join("moved2"), &filtered).unwrap();
        let r = run::<()>(&p, &filtered, None);
        assert_eq!(r.to_recycle.len(), 2, "with filters only the copied files go");
    }

    #[test]
    fn retries_then_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let p = Plan {
            files: vec![(dir.path().join("missing.txt"), dir.path().join("out/missing.txt"))],
            ..Plan::default()
        };
        let o = Options {
            retries: 2,
            retry_wait_secs: 0,
            mode: Mode::Move,
            ..Options::default()
        };
        let r = run::<()>(&p, &o, None);
        assert_eq!(r.failed.len(), 1);
        assert!(r.to_recycle.is_empty(), "a move with failures keeps its sources");
    }

    #[test]
    fn move_end_to_end_with_undo() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        tree(&src);
        let jdir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&jdir.path().join("j.json"), &jdir.path().join("b"))
                .unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        let mut tool = SuperCopy {
            opts: Options {
                mode: Mode::Move,
                ..opts()
            },
            ..SuperCopy::default()
        };
        tool.saved.log = false;
        let p = plan(std::slice::from_ref(&src), &dst, &tool.opts).unwrap();
        let rep = run::<()>(&p, &tool.opts, None);
        let msg = tool.finish(&ctx, rep).unwrap();
        assert!(msg.contains("sent the 1 source item"), "{msg}");
        assert!(!src.exists(), "the source folder is gone");
        assert_eq!(
            std::fs::read_to_string(dst.join("src/sub/deep/c.txt")).unwrap(),
            "gamma"
        );

        // Undo the recycle (the newest entry): the sources come back.
        let last = journal.entries().into_iter().max_by_key(|e| e.id).unwrap();
        match journal.undo(last.id) {
            Err(e) if wm_core::safety::no_recycle_bin_here(&e) => return,
            r => r.unwrap(),
        }
        assert_eq!(
            std::fs::read_to_string(src.join("sub/deep/c.txt")).unwrap(),
            "gamma"
        );
    }

    #[test]
    fn refuses_copy_into_itself() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plan(&[dir.path().to_owned()], &dir.path().join("inner"), &opts()).is_err());
    }
}
