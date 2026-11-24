//! Disk Analyzer: scan a drive or folder, browse folder sizes as a treemap and list,
//! find the largest files, rescan one branch, and export the result (HTML, CSV or
//! an indented text tree). It can also add "Analyze with Win Manager" to Explorer's
//! context menu for folders and drives.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use egui_extras::{Column, TableBuilder};
use rayon::prelude::*;
use wm_core::{
    AppCtx, Category, Tool,
    drives::{self, DriveInfo},
    egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2},
    explorer_verb::{self, Target, Verb},
    icons,
    shell::reveal_in_explorer,
    task::{BackgroundTask, Progress},
    ui::{Confirm, Message, ToolButton, show_message, status_bar, toolbar},
};

const LARGEST: usize = 200;
/// Text and HTML exports leave out folders smaller than this share of the exported root.
const EXPORT_MIN_SHARE: f64 = 0.001;

const VERB: Verb<'static> = Verb {
    id: "WinManager.DiskAnalyzer",
    label: "Analyze with Win Manager",
    args: "--open diskanalyzer --analyze",
    targets: &[Target::Folder, Target::Drive],
};

#[derive(Debug, Clone, Default)]
pub struct DirNode {
    pub name: String,
    pub path: PathBuf,
    /// Total bytes including subfolders.
    pub size: u64,
    /// Bytes of files directly in this folder.
    pub own_files_size: u64,
    /// Number of files directly in this folder.
    pub own_files: u64,
    /// Number of files including subfolders.
    pub files: u64,
    pub children: Vec<DirNode>,
}

impl DirNode {
    /// Recomputes the totals from the children and sorts them largest first.
    fn resum(&mut self) {
        self.children.sort_by_key(|c| Reverse(c.size));
        self.size = self.own_files_size + self.children.iter().map(|c| c.size).sum::<u64>();
        self.files = self.own_files + self.children.iter().map(|c| c.files).sum::<u64>();
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub root: DirNode,
    pub largest: Vec<(u64, PathBuf)>,
    pub errors: u64,
    pub total_files: u64,
}

impl ScanResult {
    /// Swaps in a fresh scan of the folder at `trail` and updates every total above it.
    /// Returns the trail to the same folder after re-sorting.
    pub fn replace_branch(&mut self, trail: &[usize], fresh: ScanResult) -> Vec<usize> {
        fn walk(node: &mut DirNode, trail: &[usize], fresh: &mut Option<DirNode>) -> Vec<usize> {
            match trail.split_first() {
                None => {
                    let mut new = fresh.take().unwrap_or_default();
                    new.name = std::mem::take(&mut node.name);
                    *node = new;
                    Vec::new()
                }
                Some((&i, rest)) => {
                    let Some(child) = node.children.get_mut(i) else {
                        return Vec::new();
                    };
                    let path = child.path.clone();
                    let below = walk(child, rest, fresh);
                    node.resum();
                    let at = node.children.iter().position(|c| c.path == path).unwrap_or(i);
                    std::iter::once(at).chain(below).collect()
                }
            }
        }
        let old = DisplayNode::at(&self.root, trail);
        let (old_path, old_files) = (old.path.clone(), old.files);
        let new_files = fresh.root.files;
        self.largest.retain(|(_, p)| !p.starts_with(&old_path));
        self.largest.extend(fresh.largest);
        self.largest.sort_by_key(|(s, _)| Reverse(*s));
        self.largest.truncate(LARGEST);
        self.errors += fresh.errors;
        self.total_files = self.total_files - old_files + new_files;
        walk(&mut self.root, trail, &mut Some(fresh.root))
    }
}

struct DisplayNode;

impl DisplayNode {
    fn at<'a>(root: &'a DirNode, trail: &[usize]) -> &'a DirNode {
        trail.iter().fold(root, |n, &i| n.children.get(i).unwrap_or(n))
    }
}

struct Scanner<'a> {
    progress: &'a Progress<ScanResult>,
    files: AtomicU64,
    errors: AtomicU64,
    heap: Mutex<BinaryHeap<Reverse<(u64, PathBuf)>>>,
    /// Smallest size currently in a full heap (fast pre-check without locking).
    threshold: AtomicU64,
}

impl Scanner<'_> {
    fn offer(&self, size: u64, path: &Path) {
        if size < self.threshold.load(Ordering::Relaxed) {
            return;
        }
        let mut heap = self.heap.lock().unwrap();
        heap.push(Reverse((size, path.to_owned())));
        if heap.len() > LARGEST {
            heap.pop();
            if let Some(Reverse((min, _))) = heap.peek() {
                self.threshold.store(*min, Ordering::Relaxed);
            }
        }
    }

    fn scan_dir(&self, path: &Path, name: String) -> DirNode {
        let mut node = DirNode {
            name,
            path: path.to_owned(),
            ..Default::default()
        };
        if self.progress.is_cancelled() {
            return node;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return node;
        };
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            // Junctions and symlinks would double-count or loop.
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                subdirs.push(entry.path());
            } else if let Ok(md) = entry.metadata() {
                let size = md.len();
                node.own_files_size += size;
                node.own_files += 1;
                self.offer(size, &entry.path());
                let n = self.files.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(5_000) {
                    self.progress.report(0.0, format!("{n} files scanned…"));
                }
            }
        }
        node.children = subdirs
            .par_iter()
            .map(|p| {
                self.scan_dir(
                    p,
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        node.resum();
        node
    }
}

pub fn scan(root: &Path, progress: &Progress<ScanResult>) -> ScanResult {
    let scanner = Scanner {
        progress,
        files: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        heap: Mutex::new(BinaryHeap::new()),
        threshold: AtomicU64::new(0),
    };
    let node = scanner.scan_dir(root, root.display().to_string());
    let mut largest: Vec<(u64, PathBuf)> = scanner
        .heap
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|Reverse(x)| x)
        .collect();
    largest.sort_by_key(|(s, _)| Reverse(*s));
    ScanResult {
        total_files: scanner.files.into_inner(),
        errors: scanner.errors.into_inner(),
        root: node,
        largest,
    }
}

/// Squarified treemap layout: returns one rectangle per size (same order), filling `area`.
pub fn squarify(sizes: &[f64], area: Rect) -> Vec<Rect> {
    let total: f64 = sizes.iter().sum();
    let mut out = vec![Rect::NOTHING; sizes.len()];
    if total <= 0.0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return out;
    }
    let scale = (area.width() as f64 * area.height() as f64) / total;
    let mut order: Vec<usize> = (0..sizes.len()).filter(|&i| sizes[i] > 0.0).collect();
    order.sort_by(|&a, &b| sizes[b].total_cmp(&sizes[a]));
    let mut rect = area;
    let mut i = 0;
    while i < order.len() {
        let short = rect.width().min(rect.height()) as f64;
        // Grow the row while the worst aspect ratio improves.
        let mut row = vec![order[i]];
        let mut row_area = sizes[order[i]] * scale;
        let worst = |areas: &[f64], sum: f64| {
            let (mx, mn) = areas
                .iter()
                .fold((0f64, f64::MAX), |(a, b), &x| (a.max(x), b.min(x)));
            ((short * short * mx) / (sum * sum)).max((sum * sum) / (short * short * mn))
        };
        let mut j = i + 1;
        while j < order.len() {
            let a = sizes[order[j]] * scale;
            let current: Vec<f64> = row.iter().map(|&k| sizes[k] * scale).collect();
            let mut next = current.clone();
            next.push(a);
            if worst(&next, row_area + a) > worst(&current, row_area) {
                break;
            }
            row.push(order[j]);
            row_area += a;
            j += 1;
        }
        // Lay the row along the short side.
        let thickness = (row_area / short) as f32;
        let mut offset = 0.0f32;
        let horizontal = rect.width() >= rect.height(); // row is a column on the left
        for &k in &row {
            let len = ((sizes[k] * scale) / row_area * short) as f32;
            out[k] = if horizontal {
                Rect::from_min_size(
                    Pos2::new(rect.left(), rect.top() + offset),
                    Vec2::new(thickness, len),
                )
            } else {
                Rect::from_min_size(
                    Pos2::new(rect.left() + offset, rect.top()),
                    Vec2::new(len, thickness),
                )
            };
            offset += len;
        }
        rect = if horizontal {
            Rect::from_min_max(Pos2::new(rect.left() + thickness, rect.top()), rect.max)
        } else {
            Rect::from_min_max(Pos2::new(rect.left(), rect.top() + thickness), rect.max)
        };
        i = j;
    }
    out
}

/// How sizes are shown (and written to text and HTML exports).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Auto,
    Kb,
    Mb,
    Gb,
}

impl Unit {
    const ALL: [Unit; 4] = [Unit::Auto, Unit::Kb, Unit::Mb, Unit::Gb];

    fn label(self) -> &'static str {
        match self {
            Unit::Auto => "Automatic",
            Unit::Kb => "KB",
            Unit::Mb => "MB",
            Unit::Gb => "GB",
        }
    }

    pub fn format(self, bytes: u64) -> String {
        let b = bytes as f64;
        match self {
            Unit::Auto => wm_core::ui::format_bytes(bytes),
            Unit::Kb => format!("{} KB", group_thousands((b / 1024.0).ceil() as u64)),
            Unit::Mb => format!("{:.1} MB", b / (1024.0 * 1024.0)),
            Unit::Gb => format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0)),
        }
    }
}

fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Export formats, chosen by the extension picked in the save dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Html,
    Csv,
    Text,
}

impl ExportFormat {
    fn from_path(p: &Path) -> Self {
        match p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .as_deref()
        {
            Some("csv") => ExportFormat::Csv,
            Some("txt") => ExportFormat::Text,
            _ => ExportFormat::Html,
        }
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Every folder below `node` (depth first), with its depth, for exports.
fn walk_folders<'a>(node: &'a DirNode, depth: usize, out: &mut Vec<(usize, &'a DirNode)>) {
    out.push((depth, node));
    for c in &node.children {
        walk_folders(c, depth + 1, out);
    }
}

/// Builds the export of `node`. CSV lists every folder in bytes; text and HTML trees
/// leave out folders under 0.1% of `node` to stay readable.
pub fn export(node: &DirNode, format: ExportFormat, unit: Unit) -> String {
    let mut all = Vec::new();
    walk_folders(node, 0, &mut all);
    let total = node.size.max(1) as f64;
    let min = (total * EXPORT_MIN_SHARE) as u64;
    let share = |n: &DirNode| n.size as f64 / total * 100.0;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let mut out = String::new();
    match format {
        ExportFormat::Csv => {
            out.push_str("Path,Size (bytes),Files,Share of root (%)\r\n");
            for (_, n) in &all {
                let _ = write!(
                    out,
                    "{},{},{},{:.2}\r\n",
                    csv_field(&n.path.display().to_string()),
                    n.size,
                    n.files,
                    share(n)
                );
            }
        }
        ExportFormat::Text => {
            let _ = write!(
                out,
                "Disk usage of {} ({stamp})\r\nFolders under 0.1% of the total are left out.\r\n\r\n",
                node.path.display()
            );
            for (depth, n) in all.iter().filter(|(_, n)| n.size >= min) {
                let name = if *depth == 0 {
                    n.path.display().to_string()
                } else {
                    n.name.clone()
                };
                let _ = write!(
                    out,
                    "{}{name}  [{}, {:.1}%, {} files]\r\n",
                    "    ".repeat(*depth),
                    unit.format(n.size),
                    share(n),
                    n.files
                );
            }
        }
        ExportFormat::Html => {
            let title = html_escape(&node.path.display().to_string());
            let _ = write!(
                out,
                "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>Disk usage of {title}</title>\n<style>\n\
                 body{{font:14px 'Segoe UI',sans-serif;margin:24px;color:#222}}\n\
                 table{{border-collapse:collapse;width:100%}}\n\
                 th,td{{text-align:left;padding:3px 8px;border-bottom:1px solid #e4e4e4;white-space:nowrap}}\n\
                 td.n{{text-align:right;font-variant-numeric:tabular-nums}}\n\
                 .bar{{background:#e8eef7;width:160px;height:10px;border-radius:2px}}\n\
                 .bar i{{display:block;height:10px;background:#387ade;border-radius:2px}}\n\
                 @media(prefers-color-scheme:dark){{body{{background:#1e1e1e;color:#ddd}}th,td{{border-color:#333}}.bar{{background:#2c3440}}}}\n\
                 </style></head><body>\n<h1>Disk usage of {title}</h1>\n\
                 <p>{} in {} files, exported {stamp}. Folders under 0.1% of the total are left out.</p>\n\
                 <table><tr><th>Folder</th><th>Size</th><th>Share</th><th></th><th>Files</th></tr>\n",
                unit.format(node.size),
                node.files
            );
            for (depth, n) in all.iter().filter(|(_, n)| n.size >= min) {
                let name = if *depth == 0 {
                    n.path.display().to_string()
                } else {
                    n.name.clone()
                };
                let pct = share(n);
                let _ = writeln!(
                    out,
                    "<tr><td style=\"padding-left:{}px\" title=\"{}\">{}</td><td class=\"n\">{}</td>\
                     <td class=\"n\">{pct:.1}%</td><td><div class=\"bar\"><i style=\"width:{pct:.1}%\"></i></div></td>\
                     <td class=\"n\">{}</td></tr>",
                    8 + depth * 18,
                    html_escape(&n.path.display().to_string()),
                    html_escape(&name),
                    unit.format(n.size),
                    n.files
                );
            }
            out.push_str("</table></body></html>\n");
        }
    }
    out
}

fn tile_color(i: usize, dark: bool) -> Color32 {
    const HUES: [(u8, u8, u8); 6] = [
        (66, 133, 180),
        (90, 160, 110),
        (200, 140, 60),
        (150, 100, 170),
        (190, 90, 90),
        (80, 160, 160),
    ];
    let (r, g, b) = HUES[i % HUES.len()];
    if dark {
        Color32::from_rgb(r / 4 * 3, g / 4 * 3, b / 4 * 3)
    } else {
        Color32::from_rgb(r, g, b)
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum View {
    Folders,
    LargestFiles,
}

/// What the running scan will do with its result.
enum Job {
    Full,
    /// Rescan of the folder at this trail.
    Branch(Vec<usize>),
}

pub struct DiskAnalyzer {
    task: Option<(Job, BackgroundTask<ScanResult>)>,
    result: Option<ScanResult>,
    /// Indices from the root to the folder being viewed.
    trail: Vec<usize>,
    view: View,
    unit: Unit,
    drives: Vec<DriveInfo>,
    drive: usize,
    /// The `--analyze <path>` argument (from the Explorer menu) was handled.
    handled_args: bool,
    menu_installed: Option<bool>,
    confirm: Confirm<PathBuf>,
    message: Message,
}

impl Default for DiskAnalyzer {
    fn default() -> Self {
        Self {
            task: None,
            result: None,
            trail: Vec::new(),
            view: View::Folders,
            unit: Unit::Auto,
            drives: Vec::new(),
            drive: 0,
            handled_args: false,
            menu_installed: None,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl DiskAnalyzer {
    fn start(&mut self, ctx: &egui::Context, root: PathBuf) {
        self.result = None;
        self.trail.clear();
        self.message = None;
        self.task = Some((Job::Full, BackgroundTask::spawn(ctx, move |p| Ok(scan(&root, p)))));
    }

    fn rescan_branch(&mut self, ctx: &egui::Context) {
        let Some(result) = &self.result else { return };
        let path = DisplayNode::at(&result.root, &self.trail).path.clone();
        self.message = None;
        self.task = Some((
            Job::Branch(self.trail.clone()),
            BackgroundTask::spawn(ctx, move |p| Ok(scan(&path, p))),
        ));
    }

    fn refresh_drives(&mut self) {
        let previous = self.drives.get(self.drive).map(|d| d.letter);
        self.drives = drives::list();
        let system = std::env::var("SystemDrive")
            .ok()
            .and_then(|s| s.chars().next())
            .map(|c| c.to_ascii_uppercase());
        self.drive = self
            .drives
            .iter()
            .position(|d| Some(d.letter) == previous.or(system))
            .unwrap_or(0);
    }

    fn export_current(&mut self) {
        let Some(result) = &self.result else { return };
        let node = DisplayNode::at(&result.root, &self.trail);
        let stem: String = node
            .path
            .display()
            .to_string()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let Some(file) = rfd::FileDialog::new()
            .set_file_name(format!("Disk usage {}.html", stem.trim_matches('_')))
            .add_filter("Web page", &["html"])
            .add_filter("CSV (all folders, bytes)", &["csv"])
            .add_filter("Text tree", &["txt"])
            .save_file()
        else {
            return;
        };
        let text = export(node, ExportFormat::from_path(&file), self.unit);
        self.message = Some(
            std::fs::write(&file, text)
                .with_context(|| format!("writing {}", file.display()))
                .map(|_| format!("Exported to {}.", file.display()))
                .map_err(|e| format!("{e:#}")),
        );
    }

    fn toggle_menu(&mut self, ctx: &mut AppCtx<'_>) {
        let installed = VERB.is_installed();
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let r: Result<()> = if installed {
            VERB.uninstall(ctx.journal, "diskanalyzer")
        } else {
            VERB.install(ctx.journal, "diskanalyzer")
        };
        self.menu_installed = Some(VERB.is_installed());
        self.message = Some(r.map_err(|e| format!("{e:#}")).map(|_| {
            if installed {
                "Removed \"Analyze with Win Manager\" from the Explorer context menu.".into()
            } else {
                "Right-click a folder or drive in Explorer and choose \"Analyze with Win Manager\". \
                 On Windows 11 it is under \"Show more options\"."
                    .into()
            }
        }));
    }

    fn folders_view(&mut self, ui: &mut egui::Ui, result: &ScanResult) {
        let node = DisplayNode::at(&result.root, &self.trail).clone();
        let unit = self.unit;

        // Breadcrumb.
        ui.horizontal_wrapped(|ui| {
            let mut n = &result.root;
            if ui.link(&n.name).clicked() {
                self.trail.clear();
            }
            for depth in 0..self.trail.len() {
                n = &n.children[self.trail[depth]];
                ui.label("›");
                if ui.link(&n.name).clicked() {
                    self.trail.truncate(depth + 1);
                }
            }
        });
        ui.weak(format!("{} in {} files", unit.format(node.size), node.files));

        // Treemap of children (+ the folder's own files as one tile).
        let height = (ui.available_height() * 0.45).clamp(140.0, 320.0);
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
        let mut sizes: Vec<f64> = node.children.iter().map(|c| c.size as f64).collect();
        sizes.push(node.own_files_size as f64);
        let tiles = squarify(&sizes, rect.shrink(1.0));
        let painter = ui.painter_at(rect);
        let dark = ui.visuals().dark_mode;
        let hover = response.hover_pos();
        let mut hovered = None;
        for (i, tile) in tiles.iter().enumerate() {
            if !tile.is_positive() {
                continue;
            }
            let is_files = i == node.children.len();
            let fill = if is_files {
                ui.visuals().faint_bg_color
            } else {
                tile_color(i, dark)
            };
            painter.rect_filled(tile.shrink(1.0), 2.0, fill);
            painter.rect_stroke(
                tile.shrink(1.0),
                2.0,
                Stroke::new(1.0, ui.visuals().panel_fill),
                egui::StrokeKind::Inside,
            );
            let name = if is_files {
                "(files)".to_owned()
            } else {
                node.children[i].name.clone()
            };
            if tile.width() > 60.0 && tile.height() > 18.0 {
                painter.text(
                    tile.left_top() + Vec2::new(4.0, 3.0),
                    egui::Align2::LEFT_TOP,
                    format!("{name}\n{}", unit.format(sizes[i] as u64)),
                    egui::FontId::proportional(11.0),
                    if is_files {
                        ui.visuals().text_color()
                    } else {
                        Color32::WHITE
                    },
                );
            }
            if hover.is_some_and(|h| tile.contains(h)) {
                hovered = Some(i);
            }
        }
        if let Some(i) = hovered {
            let label = if i < node.children.len() {
                format!(
                    "{} — {}",
                    node.children[i].path.display(),
                    unit.format(node.children[i].size)
                )
            } else {
                format!(
                    "Files directly in this folder — {}",
                    unit.format(node.own_files_size)
                )
            };
            let response = response.on_hover_text(label);
            if response.clicked() && i < node.children.len() && !node.children[i].children.is_empty() {
                self.trail.push(i);
            }
        }

        ui.add_space(6.0);
        let mut open = None;
        let mut drill = None;
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::initial(280.0).clip(true))
            .column(Column::initial(90.0))
            .column(Column::initial(160.0))
            .column(Column::initial(80.0))
            .column(Column::remainder())
            .header(20.0, |mut h| {
                for t in ["Folder", "Size", "Share", "Files", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, node.children.len(), |mut row| {
                    let i = row.index();
                    let c = &node.children[i];
                    row.col(|ui| {
                        if c.children.is_empty() {
                            ui.label(&c.name);
                        } else if ui.link(&c.name).clicked() {
                            drill = Some(i);
                        }
                    });
                    row.col(|ui| {
                        ui.label(unit.format(c.size));
                    });
                    row.col(|ui| {
                        let share = c.size as f32 / node.size.max(1) as f32;
                        ui.add(
                            egui::ProgressBar::new(share)
                                .desired_width(150.0)
                                .text(format!("{:.1}%", share * 100.0)),
                        );
                    });
                    row.col(|ui| {
                        ui.label(c.files.to_string());
                    });
                    row.col(|ui| {
                        if ui.small_button("Explore").clicked() {
                            open = Some(c.path.clone());
                        }
                    });
                });
            });
        if let Some(i) = drill {
            self.trail.push(i);
        }
        if let Some(p) = open {
            let _ = std::process::Command::new("explorer").arg(p).spawn();
        }
    }

    fn largest_view(&mut self, ui: &mut egui::Ui, result: &ScanResult) {
        let mut recycle = None;
        let unit = self.unit;
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::initial(90.0))
            .column(Column::remainder().clip(true))
            .column(Column::exact(150.0))
            .header(20.0, |mut h| {
                for t in ["Size", "File", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, result.largest.len(), |mut row| {
                    let (size, path) = &result.largest[row.index()];
                    row.col(|ui| {
                        ui.label(unit.format(*size));
                    });
                    row.col(|ui| {
                        ui.label(path.display().to_string());
                    });
                    row.col(|ui| {
                        if ui.small_button("Show").clicked() {
                            reveal_in_explorer(path);
                        }
                        if ui.small_button("Recycle…").clicked() {
                            recycle = Some(path.clone());
                        }
                    });
                });
            });
        if let Some(p) = recycle {
            self.confirm
                .ask(format!("Move {} to the Recycle Bin?", p.display()), p);
        }
    }

    fn poll_task(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let Some((_, task)) = &mut self.task else { return };
        task.poll();
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(if task.status.is_empty() {
                "Scanning…"
            } else {
                &task.status
            });
            if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                task.cancel();
            }
        });
        let cancelled = task.is_cancelled();
        let Some(r) = task.take_result() else { return };
        let (job, _) = self.task.take().expect("task present");
        match (r, job) {
            // A cancelled branch rescan holds partial numbers; keep the old ones.
            (Ok(_), Job::Branch(_)) if cancelled => {}
            (Ok(r), Job::Full) => self.result = Some(r),
            (Ok(fresh), Job::Branch(trail)) => {
                if let Some(result) = &mut self.result {
                    let at = result.replace_branch(&trail, fresh);
                    if self.trail == trail {
                        self.trail = at;
                    } else {
                        self.trail.clear();
                    }
                }
            }
            (Err(e), _) => self.message = Some(Err(e)),
        }
        if self.result.is_some() {
            self.refresh_drives();
        }
    }
}

impl Tool for DiskAnalyzer {
    fn id(&self) -> &'static str {
        "diskanalyzer"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.drives.is_empty() {
            self.refresh_drives();
        }
        let installed = *self.menu_installed.get_or_insert_with(|| VERB.is_installed());
        if !self.handled_args {
            self.handled_args = true;
            if let Some(p) = explorer_verb::arg_after("--analyze").filter(|p| p.is_dir()) {
                self.start(ui.ctx(), p);
            }
        }

        if let Some(path) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                ctx.journal
                    .recycle(
                        "diskanalyzer",
                        format!("Recycled {}", path.display()),
                        vec![path.clone()],
                    )
                    .map(|_| {
                        if let Some(r) = &mut self.result {
                            r.largest.retain(|(_, p)| p != &path);
                        }
                        format!("Moved {} to the Recycle Bin.", path.display())
                    })
                    .map_err(|e| format!("{e:#}"))
            });
        }

        let busy = self.task.is_some();
        let has = self.result.is_some();
        ui.horizontal(|ui| {
            ui.label("Drive:");
            let selected = self
                .drives
                .get(self.drive)
                .map(|d| d.display_name())
                .unwrap_or_default();
            egui::ComboBox::from_id_salt("da-drive")
                .selected_text(selected)
                .width(200.0)
                .show_ui(ui, |ui| {
                    for (i, d) in self.drives.iter().enumerate() {
                        ui.selectable_value(&mut self.drive, i, d.display_name())
                            .on_hover_text(format!(
                                "{}, {}: {} free of {}",
                                d.kind.label(),
                                d.file_system,
                                wm_core::ui::format_bytes(d.free),
                                wm_core::ui::format_bytes(d.total)
                            ));
                    }
                });
            if let Some(d) = self.drives.get(self.drive) {
                let used = d.used() as f32 / d.total.max(1) as f32;
                ui.add(
                    egui::ProgressBar::new(used)
                        .desired_width(160.0)
                        .text(format!("{} free", wm_core::ui::format_bytes(d.free))),
                );
            }
            ui.separator();
            ui.label("Units:");
            egui::ComboBox::from_id_salt("da-unit")
                .selected_text(self.unit.label())
                .show_ui(ui, |ui| {
                    for u in Unit::ALL {
                        ui.selectable_value(&mut self.unit, u, u.label());
                    }
                });
        });

        let menu_label = if installed {
            "Remove from menu"
        } else {
            "Add to menu"
        };
        match toolbar(
            ui,
            &[
                Some(
                    ToolButton::new(icons::HARD_DRIVES, "Analyze drive")
                        .enabled(!busy && !self.drives.is_empty()),
                ),
                Some(ToolButton::new(icons::FOLDER_OPEN, "Analyze folder").enabled(!busy)),
                None,
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Rescan branch").enabled(!busy && has)),
                Some(ToolButton::new(icons::ARROW_SQUARE_OUT, "Explore").enabled(has)),
                Some(ToolButton::new(icons::UPLOAD, "Export").enabled(!busy && has)),
                None,
                Some(ToolButton::new(icons::MOUSE_SIMPLE, menu_label)),
            ],
        ) {
            Some(0) => {
                if let Some(d) = self.drives.get(self.drive) {
                    let root = d.root.clone();
                    self.start(ui.ctx(), root);
                }
            }
            Some(1) => {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.start(ui.ctx(), dir);
                }
            }
            Some(3) => self.rescan_branch(ui.ctx()),
            Some(4) => {
                if let Some(r) = &self.result {
                    let p = DisplayNode::at(&r.root, &self.trail).path.clone();
                    let _ = std::process::Command::new("explorer").arg(p).spawn();
                }
            }
            Some(5) => self.export_current(),
            Some(7) => self.toggle_menu(ctx),
            _ => {}
        }

        self.poll_task(ui, ctx);
        show_message(ui, &self.message);

        let Some(result) = self.result.clone() else {
            if self.task.is_none() {
                ui.weak("Choose a drive or folder to see what is using the space.");
            }
            return;
        };
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.view, View::Folders, "Folders");
            ui.selectable_value(
                &mut self.view,
                View::LargestFiles,
                format!("Largest files ({})", result.largest.len()),
            );
        });
        ui.separator();
        let status = format!(
            "{} files, {}{}",
            result.total_files,
            self.unit.format(result.root.size),
            if result.errors > 0 {
                format!(", {} folders not readable", result.errors)
            } else {
                String::new()
            }
        );
        egui::Panel::bottom("da-status")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| status_bar(ui, &status, |_| {}));
        match self.view {
            View::Folders => self.folders_view(ui, &result),
            View::LargestFiles => self.largest_view(ui, &result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_preserves_area_and_bounds() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0));
        let sizes = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0, 0.0];
        let tiles = squarify(&sizes, area);
        let total: f64 = sizes.iter().sum();
        for (s, t) in sizes.iter().zip(&tiles) {
            if *s == 0.0 {
                assert!(!t.is_positive());
                continue;
            }
            let expected = s / total * 240_000.0;
            let actual = (t.width() * t.height()) as f64;
            assert!(
                (expected - actual).abs() / expected < 0.01,
                "area {actual} vs {expected}"
            );
            assert!(area.expand(0.5).contains_rect(*t), "{t:?} outside");
        }
        assert!(squarify(&[], area).is_empty());
    }

    fn run_scan(root: &Path) -> ScanResult {
        let ctx = egui::Context::default();
        let mut task = BackgroundTask::spawn(&ctx, {
            let root = root.to_owned();
            move |p| Ok(scan(&root, p))
        });
        while !task.is_done() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            task.poll();
        }
        task.take_result().unwrap().unwrap()
    }

    #[test]
    fn scan_sizes_and_largest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("top.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("a/one.bin"), vec![0u8; 1_000]).unwrap();
        std::fs::write(dir.path().join("a/b/two.bin"), vec![0u8; 10_000]).unwrap();

        let r = run_scan(dir.path());
        assert_eq!(r.root.size, 11_100);
        assert_eq!(r.root.files, 3);
        assert_eq!(r.root.own_files, 1);
        assert_eq!(r.root.own_files_size, 100);
        assert_eq!(r.root.children[0].name, "a");
        assert_eq!(r.root.children[0].size, 11_000);
        assert_eq!(r.largest[0].0, 10_000);
        assert_eq!(r.total_files, 3);
    }

    #[test]
    fn branch_rescan_updates_totals_and_order() {
        let dir = tempfile::tempdir().unwrap();
        for (d, n) in [("big", 5_000), ("small", 1_000)] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
            std::fs::write(dir.path().join(d).join("f.bin"), vec![0u8; n]).unwrap();
        }
        let mut r = run_scan(dir.path());
        assert_eq!(r.root.children[1].name, "small");

        // "small" grows past "big", then only that branch is rescanned.
        std::fs::write(dir.path().join("small/g.bin"), vec![0u8; 9_000]).unwrap();
        let fresh = run_scan(&dir.path().join("small"));
        let trail = r.replace_branch(&[1], fresh);

        assert_eq!(trail, vec![0]);
        assert_eq!(r.root.children[0].name, "small");
        assert_eq!(r.root.children[0].size, 10_000);
        assert_eq!(r.root.size, 15_000);
        assert_eq!(r.root.files, 3);
        assert_eq!(r.total_files, 3);
        assert_eq!(r.largest[0].0, 9_000);
        assert_eq!(r.largest.len(), 3);
    }

    #[test]
    fn exports() {
        let mut root = DirNode {
            name: "C:\\".into(),
            path: "C:\\".into(),
            own_files_size: 10,
            own_files: 1,
            children: vec![
                DirNode {
                    name: "Big, \"quoted\"".into(),
                    path: "C:\\Big, \"quoted\"".into(),
                    own_files_size: 2_000_000,
                    own_files: 2,
                    ..Default::default()
                },
                DirNode {
                    name: "<tiny>".into(),
                    path: "C:\\<tiny>".into(),
                    own_files_size: 1,
                    own_files: 1,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        for c in &mut root.children {
            c.resum();
        }
        root.resum();

        let csv = export(&root, ExportFormat::Csv, Unit::Auto);
        assert!(csv.contains("\"C:\\Big, \"\"quoted\"\"\",2000000,2,"));
        assert!(csv.contains("C:\\<tiny>,1,1,"), "CSV keeps every folder");

        let txt = export(&root, ExportFormat::Text, Unit::Kb);
        assert!(txt.contains("    Big, \"quoted\"  [1,954 KB, 100.0%, 2 files]"));
        assert!(!txt.contains("<tiny>"), "tiny folders are left out");

        let html = export(&root, ExportFormat::Html, Unit::Mb);
        assert!(html.contains("Big, &quot;quoted&quot;") && html.contains("1.9 MB"));
        assert!(!html.contains("&lt;tiny&gt;"));

        assert_eq!(ExportFormat::from_path(Path::new("a.CSV")), ExportFormat::Csv);
        assert_eq!(ExportFormat::from_path(Path::new("a.txt")), ExportFormat::Text);
        assert_eq!(ExportFormat::from_path(Path::new("a")), ExportFormat::Html);
    }
}
