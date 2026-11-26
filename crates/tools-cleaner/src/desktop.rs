//! Desktop Cleaner: shows each desktop item's size, owner, last use and launch count,
//! selects items unused for a number of days, and moves them to an "unused" folder on
//! the desktop or organizes them into a dated folder sorted by type. Broken shortcuts
//! go to the Recycle Bin. The unused-item move can run monthly as a scheduled task
//! (`--run desktop-unused`). Moves are journaled and can be moved back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    shell::{KnownFolder, is_broken_shortcut, known_folder, reveal_in_explorer},
    task::BackgroundTask,
    tool_settings,
    ui::{Confirm, Message, ToolButton, empty_state, format_bytes, frame, show_message},
    user_assist::{self, Usage},
};

const TOOL: &str = "desktop";
const TASK: &str = r"\Win Manager RS\Desktop cleanup";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Shortcut,
    Folder,
    Document,
    Image,
    Media,
    Archive,
    Installer,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Shortcut => "Shortcuts",
            Kind::Folder => "Folders",
            Kind::Document => "Documents",
            Kind::Image => "Images",
            Kind::Media => "Video and audio",
            Kind::Archive => "Archives",
            Kind::Installer => "Installers",
            Kind::Other => "Other",
        }
    }

    pub fn of(path: &Path) -> Kind {
        if path.is_dir() {
            return Kind::Folder;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "lnk" | "url" => Kind::Shortcut,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt" | "ods"
            | "md" | "csv" | "epub" => Kind::Document,
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "heic" | "svg" | "tif" | "tiff" | "ico" => {
                Kind::Image
            }
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "mp3" | "wav" | "flac" | "m4a" | "ogg" => {
                Kind::Media
            }
            "zip" | "rar" | "7z" | "tar" | "gz" | "xz" | "bz2" | "iso" => Kind::Archive,
            "exe" | "msi" | "msix" | "msixbundle" | "appx" => Kind::Installer,
            _ => Kind::Other,
        }
    }
}

/// Options remembered between sessions (and used by the scheduled cleanup).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Items not used for this many days count as unused.
    pub unused_days: u32,
    /// Folder on the desktop that unused items are moved into.
    pub unused_folder: String,
    /// Where Organize moves items; `None` = `Documents\Desktop cleanup <date>`.
    pub organize_target: Option<PathBuf>,
    pub by_type: bool,
    /// Also handle the shared (all users) desktop; needs administrator rights.
    pub include_public: bool,
    /// Leave folders where they are when moving unused items.
    pub keep_folders: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            unused_days: 30,
            unused_folder: "Unused desktop items".into(),
            organize_target: None,
            by_type: true,
            include_public: false,
            keep_folders: false,
        }
    }
}

impl Options {
    fn organize_dir(&self) -> PathBuf {
        self.organize_target.clone().unwrap_or_else(|| {
            let docs = known_folder(KnownFolder::Documents).unwrap_or_else(|_| PathBuf::from("."));
            docs.join(format!("Desktop cleanup {}", Local::now().format("%Y-%m-%d")))
        })
    }

    fn unused_dir(&self) -> Result<PathBuf> {
        let name = self.unused_folder.trim();
        if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            bail!("\"{name}\" is not a valid folder name");
        }
        Ok(known_folder(KnownFolder::Desktop)?.join(name))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub kind: Kind,
    pub broken: bool,
    pub public: bool,
    pub size: u64,
    pub created: Option<DateTime<Local>>,
    pub accessed: Option<DateTime<Local>>,
    pub modified: Option<DateTime<Local>>,
    /// Launches Explorer recorded for the item (or anything inside a folder).
    pub usage: Usage,
}

impl Item {
    /// The best evidence of the last use. Shortcuts are read by this tool (to check
    /// their target), so their access time is not used.
    pub fn last_used(&self) -> Option<DateTime<Local>> {
        match self.kind {
            Kind::Shortcut | Kind::Installer => self.usage.last,
            Kind::Folder => self.usage.last.max(self.modified),
            _ => self.usage.last.max(self.accessed).max(self.modified),
        }
    }

    /// Unused when neither the last use nor (if never used) the time it was put on
    /// the desktop is within `days`.
    pub fn is_unused(&self, days: u32, now: DateTime<Local>) -> bool {
        let reference = self.last_used().or(self.created);
        reference.is_some_and(|t| now - t > chrono::Duration::days(i64::from(days)))
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn local(t: std::io::Result<std::time::SystemTime>) -> Option<DateTime<Local>> {
    t.ok().map(DateTime::<Local>::from)
}

fn folder_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Usage of `path`, or of everything recorded under it for a folder.
fn usage_of(path: &Path, folder: bool, records: &HashMap<String, Usage>) -> Usage {
    let key = path.display().to_string().to_lowercase();
    let mut u = records.get(&key).copied().unwrap_or_default();
    if folder {
        let prefix = format!("{key}\\");
        for (k, v) in records {
            if k.starts_with(&prefix) {
                u = u.merge(*v);
            }
        }
    }
    u
}

pub fn scan_dir(dir: &Path, public: bool, records: &HashMap<String, Usage>, skip: &[PathBuf]) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("desktop.ini"))
                && !skip.iter().any(|s| s.as_path() == p.as_path())
        })
        .map(|path| {
            // Times are read before the shortcut is opened below.
            let md = std::fs::symlink_metadata(&path).ok();
            let kind = Kind::of(&path);
            let folder = kind == Kind::Folder;
            let broken = kind == Kind::Shortcut
                && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
                && is_broken_shortcut(&path);
            Item {
                size: if folder {
                    folder_size(&path)
                } else {
                    md.as_ref().map_or(0, |m| m.len())
                },
                created: md.as_ref().and_then(|m| local(m.created())),
                accessed: md.as_ref().and_then(|m| local(m.accessed())),
                modified: md.as_ref().and_then(|m| local(m.modified())),
                usage: usage_of(&path, folder, records),
                path,
                kind,
                broken,
                public,
            }
        })
        .collect()
}

pub fn scan(opts: &Options) -> Vec<Item> {
    let records = user_assist::read();
    let skip: Vec<PathBuf> = opts.unused_dir().into_iter().collect();
    let mut items = Vec::new();
    if let Ok(d) = known_folder(KnownFolder::Desktop) {
        items.extend(scan_dir(&d, false, &records, &skip));
    }
    if let Ok(d) = known_folder(KnownFolder::PublicDesktop) {
        items.extend(scan_dir(&d, true, &records, &skip));
    }
    items.sort_by(|a, b| (a.kind, a.path.file_name()).cmp(&(b.kind, b.path.file_name())));
    items
}

/// A free destination path: `name.ext`, then `name (2).ext`, `name (3).ext`, …
pub fn unique_destination(dir: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let p = Path::new(file_name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .expect("unbounded search")
}

/// Moves items into `target` (optionally into per-type subfolders). Returns the moves made.
pub fn move_items(items: &[Item], target: &Path, by_type: bool) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut moves = Vec::new();
    let mut errors = Vec::new();
    for item in items {
        let dir = if by_type {
            target.join(item.kind.label())
        } else {
            target.to_owned()
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            errors.push(format!("{}: {e}", dir.display()));
            continue;
        }
        let Some(name) = item.path.file_name() else {
            continue;
        };
        let dest = unique_destination(&dir, name);
        match std::fs::rename(&item.path, &dest) {
            Ok(()) => moves.push((item.path.clone(), dest)),
            Err(e) => errors.push(format!("{}: {e}", item.path.display())),
        }
    }
    if moves.is_empty() && !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    if !errors.is_empty() {
        tracing::warn!("some desktop items could not be moved: {}", errors.join("; "));
    }
    Ok(moves)
}

fn journal_moves(ctx: &AppCtx<'_>, moves: Vec<(PathBuf, PathBuf)>, target: &Path) -> Result<usize> {
    let n = moves.len();
    if n > 0 {
        ctx.journal.record(
            TOOL,
            format!("Moved {n} desktop items to {}", target.display()),
            UndoAction::MoveBack { moves },
        )?;
    }
    Ok(n)
}

/// Items the unused-item move would take with these options.
pub fn unused_items<'a>(items: &'a [Item], opts: &Options, elevated: bool) -> Vec<&'a Item> {
    let now = Local::now();
    items
        .iter()
        .filter(|i| !i.public || (opts.include_public && elevated))
        .filter(|i| !(opts.keep_folders && i.kind == Kind::Folder))
        .filter(|i| i.is_unused(opts.unused_days, now))
        .collect()
}

/// The scheduled cleanup (`--run desktop-unused`): moves unused items with the saved options.
pub fn move_unused(ctx: &AppCtx<'_>) -> Result<String> {
    let opts: Options = tool_settings::load(TOOL);
    let items = scan(&opts);
    let chosen: Vec<Item> = unused_items(&items, &opts, ctx.elevated)
        .into_iter()
        .cloned()
        .collect();
    if chosen.is_empty() {
        return Ok("No unused desktop items.".into());
    }
    let target = opts.unused_dir()?;
    let n = journal_moves(ctx, move_items(&chosen, &target, false)?, &target)?;
    Ok(format!("Moved {n} unused desktop items to {}.", target.display()))
}

fn task_exists() -> bool {
    hidden_command("schtasks")
        .args(["/query", "/tn", TASK])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn schedule(ctx: &AppCtx<'_>) -> Result<()> {
    let exe = std::env::current_exe()?.display().to_string();
    run_checked(hidden_command("schtasks").args([
        "/create",
        "/tn",
        TASK,
        "/tr",
        &format!("\"{exe}\" --run desktop-unused"),
        "/sc",
        "MONTHLY",
        "/d",
        "1",
        "/st",
        "10:00",
        "/f",
    ]))
    .context("creating the task")?;
    ctx.journal.record(
        TOOL,
        "Scheduled the monthly desktop cleanup",
        UndoAction::RunCommand {
            program: "schtasks".into(),
            args: vec!["/delete".into(), "/tn".into(), TASK.into(), "/f".into()],
        },
    )?;
    Ok(())
}

fn unschedule(ctx: &AppCtx<'_>) -> Result<()> {
    run_checked(hidden_command("schtasks").args(["/delete", "/tn", TASK, "/f"]))
        .context("removing the task")?;
    ctx.journal
        .record(TOOL, "Removed the monthly desktop cleanup", UndoAction::None)?;
    Ok(())
}

fn when(t: Option<DateTime<Local>>) -> String {
    t.map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn when_time(t: Option<DateTime<Local>>) -> String {
    t.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// The Type column's name for a kind (the original shows "Folder", "Shortcut", …).
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Shortcut => "Shortcut",
        Kind::Folder => "Folder",
        Kind::Document => "Document",
        Kind::Image => "Image",
        Kind::Media => "Video or audio",
        Kind::Archive => "Archive",
        Kind::Installer => "Installer",
        Kind::Other => "File",
    }
}

enum Ask {
    RecycleBroken(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
    Move {
        items: Vec<Item>,
        target: PathBuf,
        by_type: bool,
    },
}

pub struct DesktopCleaner {
    opts: Options,
    saved: Options,
    items: Option<Vec<Item>>,
    load: Option<BackgroundTask<Vec<Item>>>,
    checked: Vec<bool>,
    /// Index of the highlighted row (Launch and Open Folder act on it).
    selected: Option<usize>,
    filter: String,
    show_options: bool,
    scheduled: Option<bool>,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for DesktopCleaner {
    fn default() -> Self {
        let opts: Options = tool_settings::load(TOOL);
        Self {
            saved: opts.clone(),
            opts,
            items: None,
            load: None,
            checked: Vec::new(),
            selected: None,
            filter: String::new(),
            show_options: false,
            scheduled: None,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl DesktopCleaner {
    fn refresh(&mut self) {
        self.items = None;
        self.load = None;
    }

    fn poll_load(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        if self.items.is_none() && self.load.is_none() {
            let opts = self.opts.clone();
            self.load = Some(BackgroundTask::spawn(ui.ctx(), move |_| Ok(scan(&opts))));
        }
        let Some(t) = &mut self.load else { return };
        t.poll();
        if let Some(r) = t.take_result() {
            self.load = None;
            let mut items = r.unwrap_or_default();
            // Least recently used first, like the original's Last Access Time order.
            items.sort_by_key(|i| i.last_used());
            self.selected = None;
            // Nothing is ticked at first; "Select unused" ticks by usage.
            self.checked = vec![false; items.len()];
            let _ = ctx;
            self.items = Some(items);
        }
    }

    fn options_panel(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.strong("Options");
        ui.add_space(4.0);
        ui.scope(|ui| {
            egui::Grid::new("desktop-opts")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Unused after");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.opts.unused_days).range(1..=3650));
                        ui.label("days without use");
                    });
                    ui.end_row();
                    ui.label("Unused items go to");
                    ui.horizontal(|ui| {
                        ui.label("Desktop \\");
                        ui.add(egui::TextEdit::singleline(&mut self.opts.unused_folder).desired_width(220.0));
                    });
                    ui.end_row();
                    ui.label("Organize into");
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(self.opts.organize_dir().display().to_string()).monospace(),
                        );
                        if ui.small_button("Change…").clicked()
                            && let Some(d) = rfd::FileDialog::new().pick_folder()
                        {
                            self.opts.organize_target = Some(d);
                        }
                        if self.opts.organize_target.is_some() && ui.small_button("Default").clicked() {
                            self.opts.organize_target = None;
                        }
                    });
                    ui.end_row();
                    ui.label("");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut self.opts.by_type, "Organize into folders by type");
                        ui.checkbox(
                            &mut self.opts.keep_folders,
                            "Leave folders on the desktop when moving unused items",
                        );
                        ui.add_enabled(
                            ctx.elevated,
                            egui::Checkbox::new(
                                &mut self.opts.include_public,
                                "Include the shared (all users) desktop",
                            ),
                        )
                        .on_disabled_hover_text("Needs administrator rights");
                    });
                    ui.end_row();
                });
            ui.horizontal(|ui| {
                let dirty = self.opts != self.saved;
                if ui.add_enabled(dirty, egui::Button::new("Save options")).clicked() {
                    self.message = Some(
                        tool_settings::save(TOOL, &self.opts)
                            .map(|_| {
                                self.saved = self.opts.clone();
                                "Options saved; the scheduled cleanup uses them too.".to_owned()
                            })
                            .map_err(|e| format!("{e:#}")),
                    );
                    self.refresh();
                }
                if dirty && ui.button("Discard").clicked() {
                    self.opts = self.saved.clone();
                }
            });
        });
    }

    fn handle_confirm(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) else {
            return;
        };
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        self.message = Some(match ask {
            Ask::RecycleBroken(paths) => ctx
                .journal
                .recycle(
                    TOOL,
                    format!("Recycled {} broken shortcuts", paths.len()),
                    paths.clone(),
                )
                .map(|_| format!("Moved {} broken shortcuts to the Recycle Bin.", paths.len()))
                .map_err(|e| format!("{e:#}")),
            Ask::Delete(paths) => ctx
                .journal
                .recycle(
                    TOOL,
                    format!("Recycled {} desktop items", paths.len()),
                    paths.clone(),
                )
                .map(|_| format!("Moved {} items to the Recycle Bin.", paths.len()))
                .map_err(|e| format!("{e:#}")),
            Ask::Move {
                items,
                target,
                by_type,
            } => move_items(&items, &target, by_type)
                .and_then(|m| journal_moves(ctx, m, &target))
                .map(|n| {
                    format!(
                        "Moved {n} items to {}. Undo in the Change Journal puts them back.",
                        target.display()
                    )
                })
                .map_err(|e| format!("{e:#}")),
        });
        self.refresh();
    }
}

impl Tool for DesktopCleaner {
    fn id(&self) -> &'static str {
        TOOL
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.handle_confirm(ui, ctx);
        self.poll_load(ui, ctx);
        let scheduled = *self.scheduled.get_or_insert_with(task_exists);
        let items = self.items.clone().unwrap_or_default();
        let usable = |i: &Item| !i.public || ctx.elevated;
        let broken: Vec<PathBuf> = items
            .iter()
            .filter(|i| i.broken && usable(i))
            .map(|i| i.path.clone())
            .collect();
        let chosen: Vec<Item> = items
            .iter()
            .zip(&self.checked)
            .filter(|(i, c)| **c && usable(i))
            .map(|(i, _)| i.clone())
            .collect();
        let has = !chosen.is_empty();
        let selected = self.selected.and_then(|i| items.get(i)).cloned();
        let all_ticked = !items.is_empty() && items.iter().zip(&self.checked).all(|(i, c)| *c || !usable(i));

        // The original's toolbar. Its Organize and Task drop-downs are the
        // Organize and Schedule buttons here, with the unused-item actions in the
        // strip below.
        let clicked = frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(self.load.is_none())),
                Some(
                    ToolButton::new(
                        icons::LIST_CHECKS,
                        if all_ticked { "Select None" } else { "Select All" },
                    )
                    .enabled(!items.is_empty()),
                ),
                None,
                Some(ToolButton::new(icons::PLAY, "Launch").enabled(selected.is_some())),
                Some(ToolButton::new(icons::X, "Delete").enabled(has)),
                None,
                Some(ToolButton::new(icons::FOLDER_OPEN, "Open Folder")),
                Some(ToolButton::new(icons::FOLDERS, "Organize").enabled(has)),
                Some(ToolButton::new(
                    icons::CALENDAR_CHECK,
                    if scheduled { "Unschedule" } else { "Task" },
                )),
                None,
                Some(ToolButton::new(icons::GEAR, "Options")),
                None,
                Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
            ],
            |_| {},
        );
        match clicked {
            Some(0) => {
                self.refresh();
                self.message = None;
            }
            Some(1) => {
                for (i, c) in items.iter().zip(self.checked.iter_mut()) {
                    *c = !all_ticked && usable(i);
                }
            }
            Some(3) => {
                if let Some(i) = &selected {
                    let _ = wm_core::shell::shell_open(&i.path.display().to_string(), "");
                }
            }
            Some(4) => self.confirm.ask(
                format!(
                    "Move {} desktop items to the Recycle Bin? Undo in the Change Journal restores them.",
                    chosen.len()
                ),
                Ask::Delete(chosen.iter().map(|i| i.path.clone()).collect()),
            ),
            Some(6) => match &selected {
                Some(i) => reveal_in_explorer(&i.path),
                None => {
                    if let Ok(d) = known_folder(KnownFolder::Desktop) {
                        let _ = std::process::Command::new("explorer").arg(d).spawn();
                    }
                }
            },
            Some(7) => {
                let target = self.opts.organize_dir();
                self.confirm.ask(
                    format!(
                        "Move {} items to {}{}?",
                        chosen.len(),
                        target.display(),
                        if self.opts.by_type {
                            ", sorted into folders by type"
                        } else {
                            ""
                        }
                    ),
                    Ask::Move {
                        items: chosen.clone(),
                        target,
                        by_type: self.opts.by_type,
                    },
                );
            }
            Some(8) => {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else if scheduled {
                    unschedule(ctx)
                        .map(|_| "The monthly desktop cleanup was removed.".into())
                        .map_err(|e| format!("{e:#}"))
                } else {
                    // The task uses the saved options, so unsaved edits are saved first.
                    let saved = if self.opts != self.saved {
                        tool_settings::save(TOOL, &self.opts)
                    } else {
                        Ok(())
                    };
                    if saved.is_ok() {
                        self.saved = self.opts.clone();
                    }
                    saved.and_then(|_| schedule(ctx))
                        .map(|_| {
                            format!(
                                "On the 1st of each month at 10:00, items unused for {} days move to Desktop\\{}.",
                                self.opts.unused_days, self.opts.unused_folder
                            )
                        })
                        .map_err(|e| format!("{e:#}"))
                });
                self.scheduled = None;
            }
            Some(10) => self.show_options = !self.show_options,
            Some(12) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }

        let mut action = None;
        frame::options(ui, |ui| {
            if ui
                .add_enabled(
                    !items.is_empty(),
                    egui::Button::new(format!("{}  Select unused", icons::CLOCK)),
                )
                .on_hover_text(format!(
                    "Ticks items not used in the last {} days",
                    self.opts.unused_days
                ))
                .clicked()
            {
                action = Some(0);
            }
            if ui
                .add_enabled(
                    has,
                    egui::Button::new(format!("{}  Move to unused folder", icons::ARCHIVE)),
                )
                .on_hover_text(format!(
                    "Moves the ticked items to Desktop\\{}",
                    self.opts.unused_folder
                ))
                .clicked()
            {
                action = Some(1);
            }
            if ui
                .add_enabled(
                    !broken.is_empty(),
                    egui::Button::new(format!("{}  Broken shortcuts ({})", icons::FILE_X, broken.len())),
                )
                .clicked()
            {
                action = Some(2);
            }
        });
        match action {
            Some(0) => {
                let unused: Vec<PathBuf> = unused_items(&items, &self.opts, ctx.elevated)
                    .into_iter()
                    .map(|i| i.path.clone())
                    .collect();
                for (i, c) in items.iter().zip(self.checked.iter_mut()) {
                    *c = unused.contains(&i.path);
                }
                self.message = Some(Ok(format!(
                    "Ticked {} items not used in the last {} days.",
                    unused.len(),
                    self.opts.unused_days
                )));
            }
            Some(1) => match self.opts.unused_dir() {
                Ok(target) => self.confirm.ask(
                    format!("Move {} items to {}?", chosen.len(), target.display()),
                    Ask::Move {
                        items: chosen.clone(),
                        target,
                        by_type: false,
                    },
                ),
                Err(e) => self.message = Some(Err(format!("{e:#}"))),
            },
            Some(2) => self.confirm.ask(
                format!(
                    "Move {} shortcuts whose targets no longer exist to the Recycle Bin?",
                    broken.len()
                ),
                Ask::RecycleBroken(broken.clone()),
            ),
            _ => {}
        }

        let status = if self.load.is_some() {
            "Reading the desktop…".to_owned()
        } else {
            format!("Selected: {}/{} items", chosen.len(), items.len())
        };
        let loading = self.load.is_some();
        frame::status(ui, &status, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  Search", icons::MAGNIFYING_GLASS))
                    .desired_width(180.0),
            );
            if loading {
                ui.spinner();
            }
        });
        if self.show_options {
            frame::left(ui, 340.0, |ui| self.options_panel(ui, ctx));
        }
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            if loading {
                return;
            }
            if items.is_empty() {
                empty_state(
                    ui,
                    icons::DESKTOP,
                    "Your desktop is already tidy",
                    "There is nothing on it to move or organize.",
                );
                return;
            }
            self.table(ui, &items, ctx.elevated);
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl DesktopCleaner {
    fn table(&mut self, ui: &mut egui::Ui, items: &[Item], elevated: bool) {
        let usable = |i: &Item| !i.public || elevated;
        let now = Local::now();
        let days = self.opts.unused_days;
        let f = self.filter.to_lowercase();
        let rows: Vec<usize> = (0..items.len())
            .filter(|&i| f.is_empty() || items[i].name().to_lowercase().contains(&f))
            .collect();
        let mut launch = None;
        let mut select = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(24.0))
            .column(Column::initial(250.0).clip(true))
            .column(Column::initial(100.0))
            .column(Column::initial(80.0))
            .column(Column::initial(80.0))
            .column(Column::initial(140.0))
            .column(Column::initial(50.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in [
                    "",
                    "Name",
                    "User",
                    "Size",
                    "Type",
                    "Last Access Time",
                    "Hits",
                    "Status",
                ] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(21.0, rows.len(), |mut row| {
                    let idx = rows[row.index()];
                    let item = &items[idx];
                    row.set_selected(self.selected == Some(idx));
                    row.col(|ui| {
                        ui.add_enabled(usable(item), egui::Checkbox::without_text(&mut self.checked[idx]))
                            .on_disabled_hover_text("Needs administrator rights");
                    });
                    row.col(|ui| {
                        let icon = match item.kind {
                            Kind::Folder => icons::FOLDER_OPEN,
                            Kind::Shortcut => icons::LINK,
                            _ => icons::FILE_TEXT,
                        };
                        ui.label(format!("{icon}  {}", item.name()))
                            .on_hover_text(item.path.display().to_string());
                    });
                    row.col(|ui| {
                        ui.label(if item.public { "All Users" } else { "Current User" });
                    });
                    row.col(|ui| {
                        ui.label(format_bytes(item.size));
                    });
                    row.col(|ui| {
                        ui.label(kind_name(item.kind));
                    });
                    row.col(|ui| {
                        let last = item.last_used();
                        if last.is_some() {
                            ui.label(when_time(last));
                        } else {
                            ui.weak("Not recorded")
                                .on_hover_text(format!("Added {}", when(item.created)));
                        }
                    });
                    row.col(|ui| {
                        if item.usage.runs > 0 {
                            ui.label(item.usage.runs.to_string());
                        }
                    });
                    row.col(|ui| {
                        if item.broken {
                            ui.colored_label(ui.visuals().warn_fg_color, "Broken shortcut");
                        } else if item.is_unused(days, now) {
                            ui.weak(format!("Unused for {days}+ days"));
                        }
                    });
                    let r = row.response();
                    if r.clicked() {
                        select = Some(idx);
                    }
                    if r.double_clicked() {
                        launch = Some(item.path.clone());
                    }
                });
            });
        if select.is_some() {
            self.selected = select;
        }
        if let Some(p) = launch {
            let _ = wm_core::shell::shell_open(&p.display().to_string(), "");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates the real monthly task and removes it through the journal's undo.
    #[test]
    fn schedule_and_undo() {
        if task_exists() {
            return; // the user's own schedule: leave it alone
        }
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        schedule(&ctx).unwrap();
        assert!(task_exists());
        journal.undo(journal.entries()[0].id).unwrap();
        assert!(!task_exists());
    }

    #[test]
    fn kinds() {
        assert_eq!(Kind::of(Path::new("a.PDF")), Kind::Document);
        assert_eq!(Kind::of(Path::new("setup.msi")), Kind::Installer);
        assert_eq!(Kind::of(Path::new("x.lnk")), Kind::Shortcut);
        assert_eq!(Kind::of(Path::new("noext")), Kind::Other);
    }

    #[test]
    fn move_with_collisions_and_undo() {
        let dir = tempfile::tempdir().unwrap();
        let desk = dir.path().join("desk");
        let target = dir.path().join("target");
        std::fs::create_dir_all(desk.join("Folder")).unwrap();
        std::fs::create_dir_all(target.join("Documents")).unwrap();
        std::fs::write(desk.join("notes.txt"), "a").unwrap();
        std::fs::write(desk.join("Folder/inner.bin"), [0u8; 7]).unwrap();
        std::fs::write(target.join("Documents/notes.txt"), "existing").unwrap();

        let items = scan_dir(&desk, false, &HashMap::new(), &[]);
        assert_eq!(items.len(), 2);
        let folder = items.iter().find(|i| i.kind == Kind::Folder).unwrap();
        assert_eq!(folder.size, 7);
        let moves = move_items(&items, &target, true).unwrap();
        assert_eq!(moves.len(), 2);
        assert!(
            target.join("Documents/notes (2).txt").exists(),
            "collision renamed"
        );
        assert!(target.join("Folders/Folder").is_dir());
        assert_eq!(
            std::fs::read_to_string(target.join("Documents/notes.txt")).unwrap(),
            "existing"
        );

        let j =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let e = j.record("t", "move", UndoAction::MoveBack { moves }).unwrap();
        j.undo(e.id).unwrap();
        assert!(desk.join("notes.txt").exists() && desk.join("Folder").is_dir());
    }

    #[test]
    fn usage_and_unused_rules() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Tool");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("run.exe"), b"x").unwrap();
        std::fs::write(dir.path().join("Game.lnk"), b"x").unwrap();
        std::fs::write(dir.path().join("skip.txt"), b"x").unwrap();
        let recent = Local::now() - chrono::Duration::days(3);
        let mut records = HashMap::new();
        records.insert(
            app.join("run.exe").display().to_string().to_lowercase(),
            Usage {
                runs: 4,
                last: Some(recent),
            },
        );
        let items = scan_dir(dir.path(), false, &records, &[dir.path().join("skip.txt")]);
        assert_eq!(items.len(), 2, "skip list honoured");
        let folder = items.iter().find(|i| i.kind == Kind::Folder).unwrap();
        assert_eq!(folder.usage.runs, 4, "launches inside a folder count for it");

        let now = Local::now();
        let old = now - chrono::Duration::days(90);
        let shortcut = Item {
            path: "x.lnk".into(),
            kind: Kind::Shortcut,
            broken: false,
            public: false,
            size: 0,
            created: Some(old),
            accessed: Some(now), // ignored for shortcuts: this tool reads them
            modified: Some(old),
            usage: Usage::default(),
        };
        assert!(shortcut.is_unused(30, now));
        let used = Item {
            usage: Usage {
                runs: 1,
                last: Some(recent),
            },
            ..shortcut.clone()
        };
        assert!(!used.is_unused(30, now));
        let new = Item {
            created: Some(recent),
            ..shortcut.clone()
        };
        assert!(
            !new.is_unused(30, now),
            "recently added, never used: not yet unused"
        );

        let opts = Options {
            keep_folders: true,
            ..Default::default()
        };
        let public = Item {
            public: true,
            ..shortcut.clone()
        };
        let folder_old = Item {
            kind: Kind::Folder,
            ..shortcut.clone()
        };
        let list = [shortcut, public, folder_old];
        assert_eq!(unused_items(&list, &opts, false).len(), 1);
    }

    #[test]
    fn option_validation() {
        let bad = Options {
            unused_folder: "a\\b".into(),
            ..Default::default()
        };
        assert!(bad.unused_dir().is_err());
        assert!(
            Options::default()
                .unused_dir()
                .unwrap()
                .ends_with("Unused desktop items")
        );
        assert!(
            Options::default()
                .organize_dir()
                .display()
                .to_string()
                .contains("Desktop cleanup ")
        );
    }
}
