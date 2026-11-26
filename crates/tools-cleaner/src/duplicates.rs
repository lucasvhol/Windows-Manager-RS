//! Duplicate Files Finder: groups files by content (size → partial hash → full BLAKE3
//! hash, in parallel), by name and content, by name and size, or by name alone.
//! Search chosen folders or whole drives (Windows, program, app-data and game
//! folders excluded), within a size range and file-type patterns, optionally skipping
//! hidden, system or read-only files. Marked copies go to the Recycle Bin or another
//! folder; single files can be renamed. Every change is journaled and undoable.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    icons,
    paths::{move_path, wildcard_match},
    safety::UndoAction,
    shell::reveal_in_explorer,
    task::{BackgroundTask, Progress},
    tool_settings,
    ui::{Confirm, Message, ToolButton, drive_picker, empty_state, format_bytes, frame, show_message},
};

use crate::desktop::unique_destination;

const TOOL: &str = "duplicates";
const PARTIAL: usize = 64 * 1024;
const MB: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchBy {
    Content,
    NameAndContent,
    NameAndSize,
    Name,
}

impl MatchBy {
    const ALL: [MatchBy; 4] = [
        MatchBy::Content,
        MatchBy::NameAndContent,
        MatchBy::NameAndSize,
        MatchBy::Name,
    ];

    fn label(self) -> &'static str {
        match self {
            MatchBy::Content => "Same content",
            MatchBy::NameAndContent => "Same name and content",
            MatchBy::NameAndSize => "Same name and size (fast)",
            MatchBy::Name => "Same name only",
        }
    }

    fn hashes(self) -> bool {
        matches!(self, MatchBy::Content | MatchBy::NameAndContent)
    }
}

/// File-type presets for the include patterns.
const TYPE_PRESETS: [(&str, &str); 6] = [
    ("All files", ""),
    (
        "Pictures",
        "*.jpg;*.jpeg;*.png;*.gif;*.bmp;*.webp;*.heic;*.tif;*.tiff;*.raw;*.cr2;*.nef",
    ),
    ("Music", "*.mp3;*.flac;*.wav;*.m4a;*.aac;*.ogg;*.wma;*.opus"),
    ("Videos", "*.mp4;*.mkv;*.avi;*.mov;*.wmv;*.webm;*.m4v"),
    (
        "Documents",
        "*.pdf;*.doc;*.docx;*.xls;*.xlsx;*.ppt;*.pptx;*.txt;*.rtf;*.odt;*.epub",
    ),
    ("Archives and installers", "*.zip;*.rar;*.7z;*.iso;*.exe;*.msi"),
];

/// Search settings, remembered between sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Criteria {
    pub folders: Vec<PathBuf>,
    /// Search only `folders`; otherwise also the chosen drives.
    pub only_folders: bool,
    pub drives: Vec<char>,
    pub match_by: MatchBy,
    pub min_size: u64,
    /// 0 = no upper limit.
    pub max_size: u64,
    /// `*.jpg;*.png` — empty = every file.
    pub include: String,
    pub exclude: String,
    pub skip_hidden: bool,
    pub skip_system: bool,
    pub skip_readonly: bool,
}

impl Default for Criteria {
    fn default() -> Self {
        Self {
            folders: std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .into_iter()
                .collect(),
            only_folders: true,
            drives: Vec::new(),
            match_by: MatchBy::Content,
            min_size: 1024 * 1024,
            max_size: 0,
            include: String::new(),
            exclude: String::new(),
            skip_hidden: true,
            skip_system: true,
            skip_readonly: false,
        }
    }
}

fn patterns(s: &str) -> Vec<String> {
    s.split([';', ','])
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect()
}

impl Criteria {
    fn roots(&self) -> Vec<(PathBuf, bool)> {
        let mut roots: Vec<(PathBuf, bool)> = self.folders.iter().map(|f| (f.clone(), false)).collect();
        if !self.only_folders {
            let letters: Vec<char> = if self.drives.is_empty() {
                wm_core::paths::expand_env("%SystemDrive%")
                    .chars()
                    .take(1)
                    .collect()
            } else {
                self.drives.clone()
            };
            roots.extend(
                letters
                    .into_iter()
                    .map(|c| (PathBuf::from(format!("{}:\\", c.to_ascii_uppercase())), true)),
            );
        }
        roots
    }

    /// Whether a file passes the size, type and attribute filters.
    pub fn accepts(
        &self,
        path: &Path,
        md: &std::fs::Metadata,
        include: &[String],
        exclude: &[String],
    ) -> bool {
        use std::os::windows::fs::MetadataExt;
        let len = md.len();
        if len < self.min_size.max(1) || (self.max_size > 0 && len > self.max_size) {
            return false;
        }
        let attrs = md.file_attributes();
        if (self.skip_readonly && attrs & 0x1 != 0)
            || (self.skip_hidden && attrs & 0x2 != 0)
            || (self.skip_system && attrs & 0x4 != 0)
        {
            return false;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        (include.is_empty() || include.iter().any(|p| wildcard_match(p, &name)))
            && !exclude.iter().any(|p| wildcard_match(p, &name))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub files: Vec<FileInfo>,
}

impl Group {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).max().unwrap_or(0)
    }

    /// Space recovered by keeping only the largest copy.
    pub fn wasted(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum::<u64>() - self.size()
    }

    fn name(&self) -> String {
        self.files
            .first()
            .and_then(|f| f.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn hash_file(path: &Path, limit: Option<usize>) -> Option<[u8; 32]> {
    let mut f = File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    match limit {
        Some(n) => {
            let mut buf = vec![0u8; n];
            let mut filled = 0;
            while filled < n {
                match f.read(&mut buf[filled..]).ok()? {
                    0 => break,
                    k => filled += k,
                }
            }
            hasher.update(&buf[..filled]);
        }
        None => {
            hasher.update_reader(&mut f).ok()?;
        }
    }
    Some(*hasher.finalize().as_bytes())
}

/// Splits each candidate group by a hash, keeping only sub-groups with ≥ 2 files.
fn refine(groups: Vec<Vec<FileInfo>>, limit: Option<usize>) -> Vec<Vec<FileInfo>> {
    groups
        .into_par_iter()
        .flat_map(|files| {
            let mut by_hash: HashMap<[u8; 32], Vec<FileInfo>> = HashMap::new();
            for (f, h) in files
                .into_par_iter()
                .filter_map(|f| hash_file(&f.path, limit).map(|h| (f, h)))
                .collect::<Vec<_>>()
            {
                by_hash.entry(h).or_default().push(f);
            }
            by_hash.into_values().filter(|g| g.len() > 1).collect::<Vec<_>>()
        })
        .collect()
}

fn key(f: &FileInfo, by: MatchBy) -> String {
    let name = || {
        f.path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    match by {
        MatchBy::Content => f.size.to_string(),
        MatchBy::NameAndContent | MatchBy::NameAndSize => format!("{}|{}", name(), f.size),
        MatchBy::Name => name(),
    }
}

pub fn find_duplicates<T>(c: &Criteria, progress: Option<&Progress<T>>) -> Vec<Group> {
    let include = patterns(&c.include);
    let exclude = patterns(&c.exclude);
    let mut buckets: HashMap<String, Vec<FileInfo>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut count = 0u64;
    for (root, whole_drive) in c.roots() {
        let walker = WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // On whole drives, stay out of Windows, program, app-data and game folders.
                !(whole_drive
                    && e.depth() > 0
                    && e.file_type().is_dir()
                    && crate::junk::dynamic::NEVER_ENTER
                        .contains(&e.file_name().to_string_lossy().to_lowercase().as_str()))
            });
        for e in walker.flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            // The same file reached through overlapping roots counts once.
            if !seen.insert(e.path().to_path_buf()) {
                continue;
            }
            count += 1;
            if let Some(p) = progress {
                if p.is_cancelled() {
                    return Vec::new();
                }
                if count.is_multiple_of(2_000) {
                    p.report(0.0, format!("Listing files… {count}"));
                }
            }
            let Ok(md) = e.metadata() else { continue };
            if !c.accepts(e.path(), &md, &include, &exclude) {
                continue;
            }
            let f = FileInfo {
                size: md.len(),
                modified: md.modified().ok(),
                path: e.into_path(),
            };
            buckets.entry(key(&f, c.match_by)).or_default().push(f);
        }
    }
    let mut groups: Vec<Vec<FileInfo>> = buckets.into_values().filter(|v| v.len() > 1).collect();
    if c.match_by.hashes() {
        if let Some(p) = progress {
            p.report(0.4, format!("Comparing {} candidate groups…", groups.len()));
        }
        groups = refine(groups, Some(PARTIAL));
        if let Some(p) = progress {
            p.report(0.7, "Verifying full contents…");
        }
        groups = refine(groups, None);
    }
    let mut out: Vec<Group> = groups
        .into_iter()
        .map(|mut files| {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            Group { files }
        })
        .collect();
    out.sort_by_key(|g| (std::cmp::Reverse(g.wasted()), g.name()));
    out
}

/// Ways to mark copies; every preset except `None` leaves at least one file per group.
#[derive(Debug, Clone, PartialEq)]
pub enum Preset {
    KeepOldest,
    KeepNewest,
    KeepShortestPath,
    KeepLongestPath,
    /// Keep the copies inside this folder; groups without one are left unmarked.
    KeepInFolder(PathBuf),
    None,
    Invert,
}

pub fn apply_preset(groups: &[Group], marked: &[Vec<bool>], preset: &Preset) -> Vec<Vec<bool>> {
    groups
        .iter()
        .zip(marked)
        .map(|(g, m)| {
            let n = g.files.len();
            let keep_one = |idx: Option<usize>| {
                let k = idx.unwrap_or(0);
                (0..n).map(|i| i != k).collect::<Vec<bool>>()
            };
            let len = |i: usize| g.files[i].path.as_os_str().len();
            match preset {
                Preset::KeepNewest => keep_one((0..n).max_by_key(|&i| g.files[i].modified)),
                Preset::KeepOldest => keep_one((0..n).min_by_key(|&i| g.files[i].modified)),
                Preset::KeepShortestPath => keep_one((0..n).min_by_key(|&i| len(i))),
                Preset::KeepLongestPath => keep_one((0..n).max_by_key(|&i| len(i))),
                Preset::KeepInFolder(dir) => {
                    let inside: Vec<bool> = g.files.iter().map(|f| f.path.starts_with(dir)).collect();
                    if inside.iter().any(|x| *x) {
                        inside.iter().map(|x| !x).collect()
                    } else {
                        vec![false; n]
                    }
                }
                Preset::None => vec![false; n],
                Preset::Invert => {
                    let mut v: Vec<bool> = m.iter().map(|x| !x).collect();
                    if v.iter().all(|x| *x) {
                        v[0] = false;
                    }
                    v
                }
            }
        })
        .collect()
}

/// Moves files into `dir` (renamed on collisions); returns `(from, to)` pairs.
pub fn move_to(files: &[PathBuf], dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    std::fs::create_dir_all(dir)?;
    let mut moves = Vec::new();
    for f in files {
        let Some(name) = f.file_name() else { continue };
        let dest = unique_destination(dir, name);
        move_path(f, &dest).with_context(|| format!("moving {}", f.display()))?;
        moves.push((f.clone(), dest));
    }
    Ok(moves)
}

enum Ask {
    Recycle(Vec<PathBuf>),
    Move(Vec<PathBuf>, PathBuf),
}

pub struct DuplicateFinder {
    criteria: Criteria,
    task: Option<BackgroundTask<Vec<Group>>>,
    groups: Vec<Group>,
    marked: Vec<Vec<bool>>,
    focused: Option<(usize, usize)>,
    renaming: Option<(PathBuf, String)>,
    scanned: bool,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for DuplicateFinder {
    fn default() -> Self {
        Self {
            criteria: tool_settings::load(TOOL),
            task: None,
            groups: Vec::new(),
            marked: Vec::new(),
            focused: None,
            renaming: None,
            scanned: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl DuplicateFinder {
    /// Drops files that no longer exist at their listed path; groups left with one
    /// file are no longer duplicates.
    fn forget(&mut self, gone: &[PathBuf]) {
        for (g, m) in self.groups.iter_mut().zip(self.marked.iter_mut()) {
            let keep: Vec<bool> = g.files.iter().map(|f| !gone.contains(&f.path)).collect();
            let mut k = keep.iter();
            g.files.retain(|_| *k.next().unwrap());
            let mut k = keep.iter();
            m.retain(|_| *k.next().unwrap());
        }
        let alive: Vec<bool> = self.groups.iter().map(|g| g.files.len() > 1).collect();
        let mut k = alive.iter();
        self.marked.retain(|_| *k.next().unwrap());
        self.groups.retain(|g| g.files.len() > 1);
        self.focused = None;
    }

    fn criteria_panel(&mut self, ui: &mut egui::Ui) {
        let c = &mut self.criteria;
        ui.strong("Search in");
        let mut remove = None;
        for (i, f) in c.folders.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
                ui.label(f.display().to_string())
                    .on_hover_text(f.display().to_string());
            });
        }
        if let Some(i) = remove {
            c.folders.remove(i);
        }
        if ui.button("Add folder…").clicked()
            && let Some(d) = rfd::FileDialog::new().pick_folder()
            && !c.folders.contains(&d)
        {
            c.folders.push(d);
        }
        ui.checkbox(&mut c.only_folders, "Only these folders");
        if !c.only_folders {
            ui.weak("Also search these drives (Windows, program and app-data folders are skipped):");
            ui.horizontal_wrapped(|ui| drive_picker(ui, &mut c.drives));
        }
        ui.separator();
        ui.strong("Match by");
        for m in MatchBy::ALL {
            ui.radio_value(&mut c.match_by, m, m.label());
        }
        if c.match_by == MatchBy::Name {
            ui.weak("Files with the same name may differ in content; check before removing.");
        }
        ui.separator();
        ui.strong("Size");
        let mut min = c.min_size as f64 / MB;
        let mut max = c.max_size as f64 / MB;
        egui::Grid::new("dup-size").num_columns(2).show(ui, |ui| {
            ui.label("Min");
            if ui
                .add(
                    egui::DragValue::new(&mut min)
                        .range(0.0..=1e6)
                        .speed(0.1)
                        .max_decimals(2)
                        .suffix(" MB"),
                )
                .changed()
            {
                c.min_size = (min * MB) as u64;
            }
            ui.end_row();
            ui.label("Max");
            if ui
                .add(
                    egui::DragValue::new(&mut max)
                        .range(0.0..=1e7)
                        .speed(1.0)
                        .max_decimals(1)
                        .custom_formatter(|v, _| {
                            if v == 0.0 {
                                "no limit".into()
                            } else {
                                format!("{v} MB")
                            }
                        }),
                )
                .changed()
            {
                c.max_size = (max * MB) as u64;
            }
            ui.end_row();
        });
        ui.separator();
        ui.strong("File types");
        egui::ComboBox::from_id_salt("dup-types")
            .selected_text(
                TYPE_PRESETS
                    .iter()
                    .find(|(_, p)| *p == c.include)
                    .map_or("Custom", |(n, _)| n),
            )
            .width(220.0)
            .show_ui(ui, |ui| {
                for (name, p) in TYPE_PRESETS {
                    if ui.selectable_label(c.include == p, name).clicked() {
                        c.include = p.to_owned();
                    }
                }
            });
        ui.add(egui::TextEdit::singleline(&mut c.include).hint_text("Only, e.g. *.jpg;*.png"));
        ui.add(egui::TextEdit::singleline(&mut c.exclude).hint_text("Never, e.g. *.tmp;*.log"));
        ui.separator();
        ui.strong("Skip");
        ui.checkbox(&mut c.skip_hidden, "Hidden files");
        ui.checkbox(&mut c.skip_system, "System files");
        ui.checkbox(&mut c.skip_readonly, "Read-only files");
    }

    fn rename_dialog(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some((path, name)) = &mut self.renaming else {
            return;
        };
        let mut done = None;
        egui::Window::new("Rename file")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(path.display().to_string());
                ui.add(egui::TextEdit::singleline(name).desired_width(340.0))
                    .request_focus();
                let valid =
                    !name.trim().is_empty() && !name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']);
                ui.horizontal(|ui| {
                    if ui.add_enabled(valid, egui::Button::new("Rename")).clicked()
                        || (valid && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        done = Some(true);
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        done = Some(false);
                    }
                });
            });
        match done {
            Some(true) => {
                let (from, name) = self.renaming.take().expect("open");
                let to = from.with_file_name(name.trim());
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else if to.exists() {
                    Err(format!("{} already exists.", to.display()))
                } else {
                    std::fs::rename(&from, &to)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| {
                            ctx.journal.record(
                                TOOL,
                                format!("Renamed {} to {}", from.display(), to.display()),
                                UndoAction::MoveBack {
                                    moves: vec![(from.clone(), to.clone())],
                                },
                            )
                        })
                        .map(|_| {
                            for g in &mut self.groups {
                                for f in &mut g.files {
                                    if f.path == from {
                                        f.path = to.clone();
                                    }
                                }
                            }
                            format!("Renamed to {}.", to.display())
                        })
                        .map_err(|e| format!("{e:#}"))
                });
            }
            Some(false) => self.renaming = None,
            None => {}
        }
    }

    fn handle_confirm(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) else {
            return;
        };
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let (result, gone) = match ask {
            Ask::Recycle(paths) => {
                let n = paths.len();
                let r = ctx
                    .journal
                    .recycle(TOOL, format!("Recycled {n} duplicate files"), paths.clone())
                    .map(|_| format!("Moved {n} duplicate files to the Recycle Bin."));
                (r, paths)
            }
            Ask::Move(paths, dir) => {
                let r = move_to(&paths, &dir).and_then(|moves| {
                    let n = moves.len();
                    ctx.journal.record(
                        TOOL,
                        format!("Moved {n} duplicate files to {}", dir.display()),
                        UndoAction::MoveBack { moves },
                    )?;
                    Ok(format!("Moved {n} files to {}.", dir.display()))
                });
                (r, paths)
            }
        };
        let gone: Vec<PathBuf> = gone.into_iter().filter(|p| !p.exists()).collect();
        self.forget(&gone);
        self.message = Some(result.map_err(|e| format!("{e:#}")));
    }
}

impl Tool for DuplicateFinder {
    fn id(&self) -> &'static str {
        TOOL
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.handle_confirm(ui, ctx);
        self.rename_dialog(ui, ctx);
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                self.scanned = true;
                match r {
                    Ok(groups) => {
                        let none: Vec<Vec<bool>> =
                            groups.iter().map(|g| vec![false; g.files.len()]).collect();
                        self.marked = apply_preset(&groups, &none, &Preset::KeepOldest);
                        self.groups = groups;
                        self.focused = None;
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }

        let busy = self.task.is_some();
        let marked: Vec<PathBuf> = self
            .groups
            .iter()
            .zip(&self.marked)
            .flat_map(|(g, m)| {
                g.files
                    .iter()
                    .zip(m)
                    .filter(|(_, m)| **m)
                    .map(|(f, _)| f.path.clone())
            })
            .collect();
        let marked_bytes: u64 = self
            .groups
            .iter()
            .zip(&self.marked)
            .flat_map(|(g, m)| g.files.iter().zip(m).filter(|(_, m)| **m).map(|(f, _)| f.size))
            .sum();
        let focused = self
            .focused
            .and_then(|(g, f)| self.groups.get(g).and_then(|g| g.files.get(f)))
            .map(|f| f.path.clone());
        let can_search = !self.criteria.folders.is_empty() || !self.criteria.only_folders;

        let clicked = frame::toolbar(
            ui,
            &[
                Some(if busy {
                    ToolButton::new(icons::STOP, "Stop")
                } else {
                    ToolButton::new(icons::MAGNIFYING_GLASS, "Find").enabled(can_search)
                }),
                None,
                Some(ToolButton::new(icons::RECYCLE, "Recycle").enabled(!marked.is_empty())),
                Some(ToolButton::new(icons::FOLDER_OPEN, "Move to…").enabled(!marked.is_empty())),
                None,
                Some(ToolButton::new(icons::PENCIL_SIMPLE, "Rename").enabled(focused.is_some())),
                Some(ToolButton::new(icons::ARROW_SQUARE_OUT, "Show").enabled(focused.is_some())),
            ],
            |_| {},
        );
        match clicked {
            Some(0) if busy => {
                if let Some(t) = &self.task {
                    t.cancel();
                }
            }
            Some(0) => {
                let c = self.criteria.clone();
                let _ = tool_settings::save(TOOL, &c);
                self.message = None;
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    Ok(find_duplicates(&c, Some(p)))
                }));
            }
            Some(2) => self.confirm.ask(
                format!(
                    "Move {} files ({}) to the Recycle Bin? At least one copy in each group is always kept.",
                    marked.len(),
                    format_bytes(marked_bytes)
                ),
                Ask::Recycle(marked.clone()),
            ),
            Some(3) => {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.confirm.ask(
                        format!("Move {} marked files to {}?", marked.len(), dir.display()),
                        Ask::Move(marked.clone(), dir),
                    );
                }
            }
            Some(5) => {
                if let Some(p) = &focused {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.renaming = Some((p.clone(), name));
                }
            }
            Some(6) => {
                if let Some(p) = &focused {
                    reveal_in_explorer(p);
                }
            }
            _ => {}
        }
        // Status bar: progress while searching, totals afterwards.
        let wasted: u64 = self.groups.iter().map(Group::wasted).sum();
        let status = match &self.task {
            Some(t) => t.status.clone(),
            None if self.groups.is_empty() => "Ready".into(),
            None => format!(
                "{} groups, {} reclaimable · {} marked ({})",
                self.groups.len(),
                format_bytes(wasted),
                marked.len(),
                format_bytes(marked_bytes)
            ),
        };
        let progress = self.task.as_ref().map(|t| t.progress);
        frame::status(ui, &status, |ui| {
            if let Some(p) = progress {
                ui.add(egui::ProgressBar::new(p).desired_width(200.0).show_percentage());
            }
        });
        frame::left(ui, 270.0, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            self.criteria_panel(ui);
        });
        if !self.groups.is_empty() {
            frame::options(ui, |ui| {
                ui.label("Mark all but the");
                let mut preset = None;
                for (label, p) in [
                    ("oldest", Preset::KeepOldest),
                    ("newest", Preset::KeepNewest),
                    ("shortest path", Preset::KeepShortestPath),
                    ("longest path", Preset::KeepLongestPath),
                ] {
                    if ui.small_button(label).clicked() {
                        preset = Some(p);
                    }
                }
                ui.separator();
                if ui.small_button("Keep copies in folder…").clicked()
                    && let Some(d) = rfd::FileDialog::new().pick_folder()
                {
                    preset = Some(Preset::KeepInFolder(d));
                }
                if ui.small_button("Unmark all").clicked() {
                    preset = Some(Preset::None);
                }
                if ui.small_button("Invert").clicked() {
                    preset = Some(Preset::Invert);
                }
                if let Some(p) = preset {
                    self.marked = apply_preset(&self.groups, &self.marked, &p);
                }
            });
        }
        frame::central_fixed(ui, |ui| self.results(ui, busy));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl DuplicateFinder {
    /// The results list (or a placeholder before the first search).
    fn results(&mut self, ui: &mut egui::Ui, busy: bool) {
        show_message(ui, &self.message);
        if self.groups.is_empty() {
            if self.scanned && !busy {
                empty_state(ui, icons::SHIELD_CHECK, "No duplicate files found", "");
            } else if !busy {
                empty_state(
                    ui,
                    icons::COPY,
                    "Find files stored more than once",
                    "Choose where to look and how to compare on the left, then press Find.",
                );
            }
            return;
        }

        // One flat list (group headers and files) so only visible rows are drawn.
        let rows: Vec<(usize, Option<usize>)> = self
            .groups
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| {
                std::iter::once((gi, None)).chain((0..g.files.len()).map(move |fi| (gi, Some(fi))))
            })
            .collect();
        let row_h = 22.0;
        let mut open = None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                for &(gi, fi) in &rows[range] {
                    let g = &self.groups[gi];
                    ui.horizontal(|ui| {
                        ui.set_height(row_h);
                        let Some(fi) = fi else {
                            ui.strong(format!("{} × {}", g.files.len(), format_bytes(g.size())));
                            ui.weak(g.name());
                            return;
                        };
                        let f = &g.files[fi];
                        ui.add_space(12.0);
                        let mut m = self.marked[gi][fi];
                        // Never allow marking the last unmarked copy.
                        let others_kept = self.marked[gi].iter().enumerate().any(|(j, x)| j != fi && !x);
                        if ui
                            .add_enabled(m || others_kept, egui::Checkbox::without_text(&mut m))
                            .on_disabled_hover_text("One copy in each group is always kept")
                            .changed()
                        {
                            self.marked[gi][fi] = m;
                        }
                        let modified = f
                            .modified
                            .map(|t| {
                                chrono::DateTime::<chrono::Local>::from(t)
                                    .format("%Y-%m-%d %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_default();
                        ui.add_sized(
                            [110.0, row_h],
                            egui::Label::new(egui::RichText::new(modified).weak()),
                        );
                        ui.add_sized([70.0, row_h], egui::Label::new(format_bytes(f.size)));
                        let sel = self.focused == Some((gi, fi));
                        let r = ui
                            .selectable_label(sel, f.path.display().to_string())
                            .on_hover_text(f.path.display().to_string());
                        if r.clicked() {
                            self.focused = Some((gi, fi));
                        }
                        if r.double_clicked() {
                            open = Some(f.path.clone());
                        }
                    });
                }
            });
        if let Some(p) = open {
            let _ = wm_core::shell::shell_open(&p.display().to_string(), "");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria(root: &Path, by: MatchBy, min: u64) -> Criteria {
        Criteria {
            folders: vec![root.to_owned()],
            match_by: by,
            min_size: min,
            skip_hidden: false,
            skip_system: false,
            ..Default::default()
        }
    }

    #[test]
    fn finds_true_duplicates_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("x")).unwrap();
        let big = vec![7u8; 200_000];
        let mut same_prefix = big.clone();
        *same_prefix.last_mut().unwrap() = 8; // differs only after the partial-hash window
        std::fs::write(d.join("a.bin"), &big).unwrap();
        std::fs::write(d.join("x/b.bin"), &big).unwrap();
        std::fs::write(d.join("c.bin"), &same_prefix).unwrap();
        std::fs::write(d.join("small1"), b"hi").unwrap();
        std::fs::write(d.join("small2"), b"hi").unwrap();

        let mut c = criteria(d, MatchBy::Content, 100);
        c.folders.push(d.join("x"));
        let groups = find_duplicates::<()>(&c, None);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(
            groups[0].files.len(),
            2,
            "overlapping roots must not double-count"
        );
        assert_eq!(groups[0].wasted(), 200_000);

        let all = find_duplicates::<()>(&criteria(d, MatchBy::Content, 1), None);
        assert_eq!(all.len(), 2, "tiny files compared when min size allows");
    }

    #[test]
    fn name_modes_size_range_and_types() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for sub in ["a", "b", "c"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        std::fs::write(d.join("a/photo.jpg"), [1u8; 50]).unwrap();
        std::fs::write(d.join("b/PHOTO.jpg"), [2u8; 50]).unwrap(); // same name+size, other content
        std::fs::write(d.join("c/photo.jpg"), [1u8; 80]).unwrap(); // same name, other size
        std::fs::write(d.join("a/notes.txt"), [3u8; 50]).unwrap();
        std::fs::write(d.join("b/other.txt"), [3u8; 50]).unwrap();

        let groups = |c: &Criteria| find_duplicates::<()>(c, None);
        let g = groups(&criteria(d, MatchBy::Name, 1));
        assert!(g.iter().any(|g| g.files.len() == 3), "{g:?}");
        let g = groups(&criteria(d, MatchBy::NameAndSize, 1));
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].files.len(), 2);
        assert!(groups(&criteria(d, MatchBy::NameAndContent, 1)).is_empty());
        assert_eq!(
            groups(&criteria(d, MatchBy::Content, 1)).len(),
            1,
            "notes/other match by content"
        );

        let mut c = criteria(d, MatchBy::Content, 1);
        c.include = "*.jpg".into();
        assert!(groups(&c).is_empty(), "types filter out the text files");
        c.include.clear();
        c.max_size = 40;
        assert!(groups(&c).is_empty(), "size range");
        c.max_size = 0;
        c.exclude = "*.txt".into();
        assert!(groups(&c).is_empty());
    }

    fn group(files: &[(&str, u64)]) -> Group {
        let t = |s| Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s));
        Group {
            files: files
                .iter()
                .map(|(p, s)| FileInfo {
                    path: PathBuf::from(p),
                    size: 10,
                    modified: t(*s),
                })
                .collect(),
        }
    }

    #[test]
    fn presets_keep_at_least_one() {
        let g = group(&[(r"C:\long\path\a", 5), (r"C:\b", 1), (r"C:\cc\c", 9)]);
        let gs = std::slice::from_ref(&g);
        let none = vec![vec![false; 3]];
        assert_eq!(
            apply_preset(gs, &none, &Preset::KeepOldest),
            [vec![true, false, true]]
        );
        assert_eq!(
            apply_preset(gs, &none, &Preset::KeepNewest),
            [vec![true, true, false]]
        );
        assert_eq!(
            apply_preset(gs, &none, &Preset::KeepShortestPath),
            [vec![true, false, true]]
        );
        assert_eq!(
            apply_preset(gs, &none, &Preset::KeepLongestPath),
            [vec![false, true, true]]
        );
        assert_eq!(
            apply_preset(gs, &none, &Preset::KeepInFolder(r"C:\cc".into())),
            [vec![true, true, false]]
        );
        assert_eq!(
            apply_preset(gs, &none, &Preset::KeepInFolder(r"D:\".into())),
            none
        );
        assert_eq!(
            apply_preset(gs, &none, &Preset::Invert),
            [vec![false, true, true]],
            "inverting never marks every copy"
        );
    }

    #[test]
    fn move_with_undo() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        std::fs::write(&a, b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("dest")).unwrap();
        std::fs::write(dir.path().join("dest/a.bin"), b"existing").unwrap();
        let moves = move_to(std::slice::from_ref(&a), &dir.path().join("dest")).unwrap();
        assert!(moves[0].1.ends_with("a (2).bin") && !a.exists());
        let j =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let e = j.record("t", "m", UndoAction::MoveBack { moves }).unwrap();
        j.undo(e.id).unwrap();
        assert!(a.exists());
    }
}
