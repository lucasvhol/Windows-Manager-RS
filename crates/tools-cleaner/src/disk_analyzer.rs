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
    AppCtx, Category, Layout, Tool,
    drives::{self, DriveInfo},
    egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2},
    explorer_verb::{self, Target, Verb},
    icons,
    shell::reveal_in_explorer,
    task::{BackgroundTask, Progress},
    ui::{Confirm, Message, ToolButton, empty_state, frame, show_message},
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

/// Slice colours of the pie chart (the original's Office chart palette).
const PIE_COLORS: [Color32; 10] = [
    Color32::from_rgb(0x4F, 0x81, 0xBD),
    Color32::from_rgb(0xC0, 0x50, 0x4D),
    Color32::from_rgb(0x9B, 0xBB, 0x59),
    Color32::from_rgb(0x80, 0x64, 0xA2),
    Color32::from_rgb(0x4B, 0xAC, 0xC6),
    Color32::from_rgb(0xF7, 0x96, 0x46),
    Color32::from_rgb(0x2C, 0x5D, 0x95),
    Color32::from_rgb(0x96, 0x3C, 0x3A),
    Color32::from_rgb(0x76, 0x92, 0x3C),
    Color32::from_rgb(0x5F, 0x49, 0x7A),
];
/// The pie shows this many folders; the rest are one "[Other folders]" slice.
const PIE_MAX: usize = 14;
/// Share bar behind the tree labels.
const SHARE_BAR: Color32 = Color32::from_rgba_premultiplied(28, 66, 44, 70);

/// One slice of the pie chart.
#[derive(Debug, Clone, PartialEq)]
pub struct Slice {
    pub label: String,
    pub size: u64,
    /// The child folder it stands for (None for the files and "other" slices).
    pub child: Option<usize>,
}

/// The slices of `node`'s pie, in drawing order: its largest folders, the rest as one
/// "[Other folders]" slice, and the files directly in it as "[Files]", like the original.
pub fn pie_slices(node: &DirNode, max: usize) -> Vec<Slice> {
    let mut out: Vec<Slice> = node
        .children
        .iter()
        .enumerate()
        .filter(|(_, c)| c.size > 0)
        .take(max)
        .map(|(i, c)| Slice {
            label: c.name.clone(),
            size: c.size,
            child: Some(i),
        })
        .collect();
    let shown: u64 = out.iter().map(|s| s.size).sum();
    let other = node.size - node.own_files_size - shown;
    if other > 0 {
        out.push(Slice {
            label: "[Other folders]".into(),
            size: other,
            child: None,
        });
    }
    if node.own_files_size > 0 {
        out.push(Slice {
            label: "[Files]".into(),
            size: node.own_files_size,
            child: None,
        });
    }
    out
}

/// `s` cut to `max` characters with an ellipsis.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max - 1).chain(['…']).collect()
    }
}

/// Draws the pie of `node` with labelled slices and a legend; returns the child folder
/// whose slice was clicked.
fn pie_chart(ui: &mut egui::Ui, node: &DirNode, unit: Unit) -> Option<usize> {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let slices = pie_slices(node, PIE_MAX);
    let total = slices.iter().map(|s| s.size).sum::<u64>().max(1) as f32;
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let painter = ui.painter_at(rect);
    let text_color = ui.visuals().text_color();
    if slices.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "This folder is empty.",
            egui::FontId::proportional(13.0),
            ui.visuals().weak_text_color(),
        );
        return None;
    }
    // Legend texts first: the legend gets its own column on the right.
    let font = egui::FontId::proportional(12.0);
    let texts: Vec<String> = slices
        .iter()
        .map(|s| format!("{} ({})", shorten(&s.label, 40), unit.format(s.size)))
        .collect();
    let rows: Vec<_> = texts
        .iter()
        .map(|t| painter.layout_no_wrap(t.clone(), font.clone(), text_color))
        .collect();
    let legend_w = rows.iter().map(|g| g.size().x).fold(0.0, f32::max) + 22.0;
    let chart = if rect.width() - legend_w > 360.0 {
        Rect::from_min_max(rect.min, Pos2::new(rect.right() - legend_w - 8.0, rect.bottom()))
    } else {
        rect
    };
    let center = chart.center();
    // Callout labels need room on both sides; narrow windows rely on the legend.
    let callouts = chart.width() >= 760.0;
    let margin = if callouts { 170.0 } else { 16.0 };
    let radius = ((chart.width() - 2.0 * margin).min(chart.height() - 2.0 * 50.0) / 2.0).max(40.0);
    let at = |a: f32, r: f32| center + Vec2::new(a.cos(), a.sin()) * r;

    // Which slice is under the pointer (angles run clockwise from 12 o'clock).
    let hovered = response.hover_pos().and_then(|p| {
        let d = p - center;
        if d.length() > radius {
            return None;
        }
        let rel = (d.y.atan2(d.x) + FRAC_PI_2).rem_euclid(TAU) / TAU * total;
        let mut acc = 0.0;
        slices.iter().position(|s| {
            acc += s.size as f32;
            rel < acc
        })
    });

    let mut start = -FRAC_PI_2;
    let mut labels: Vec<(f32, String, f32)> = Vec::new(); // (mid angle, text, sweep)
    for (i, s) in slices.iter().enumerate() {
        let sweep = s.size as f32 / total * TAU;
        let mut color = PIE_COLORS[i % PIE_COLORS.len()];
        if hovered == Some(i) {
            color = color.lerp_to_gamma(Color32::WHITE, 0.25);
        }
        let steps = ((sweep / 0.02).ceil() as usize).max(2);
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(center, color);
        for k in 0..=steps {
            mesh.colored_vertex(at(start + sweep * k as f32 / steps as f32, radius), color);
        }
        for k in 1..=steps as u32 {
            mesh.add_triangle(0, k, k + 1);
        }
        painter.add(mesh);
        if slices.len() > 1 {
            painter.line_segment(
                [center, at(start, radius)],
                Stroke::new(1.0, ui.visuals().panel_fill),
            );
        }
        labels.push((start + sweep / 2.0, texts[i].clone(), sweep));
        start += sweep;
    }

    // Callout labels outside the pie, nudged apart on each side; tiny slices only
    // get one when there are few slices (the legend lists all of them).
    let few = slices.len() <= 6;
    for right in [true, false] {
        let mut side: Vec<(f32, &String)> = labels
            .iter()
            .filter(|(m, _, sweep)| callouts && (m.cos() >= 0.0) == right && (few || *sweep > 0.05))
            .map(|(m, t, _)| (*m, t))
            .collect();
        side.sort_by(|a, b| at(a.0, radius).y.total_cmp(&at(b.0, radius).y));
        let mut last_y = f32::MIN;
        for (m, text) in side {
            let elbow = at(m, radius + 16.0);
            let y = elbow.y.max(last_y + 20.0);
            last_y = y;
            let dx = if right { 14.0 } else { -14.0 };
            let end = Pos2::new(elbow.x + dx, y);
            let line = Stroke::new(1.0, ui.visuals().weak_text_color());
            painter.line_segment([at(m, radius), elbow], line);
            painter.line_segment([elbow, end], line);
            let galley = painter.layout_no_wrap(text.clone(), font.clone(), text_color);
            let size = galley.size() + Vec2::new(8.0, 4.0);
            let x = if right { end.x } else { end.x - size.x };
            let x = x.min(chart.right() - size.x).max(chart.left());
            let min = Pos2::new(x, y - size.y / 2.0);
            let boxed = Rect::from_min_size(min, size);
            painter.rect_filled(boxed, 0.0, ui.visuals().extreme_bg_color);
            painter.rect_stroke(
                boxed,
                0.0,
                Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                egui::StrokeKind::Inside,
            );
            painter.galley(min + Vec2::new(4.0, 2.0), galley, text_color);
        }
    }

    // Legend in the bottom-right corner.
    let mut y = rect.bottom() - rows.len() as f32 * 17.0 - 4.0;
    for (i, g) in rows.into_iter().enumerate() {
        let x = rect.right() - legend_w;
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(x, y + 3.0), Vec2::splat(10.0)),
            1.0,
            PIE_COLORS[i % PIE_COLORS.len()],
        );
        painter.galley(Pos2::new(x + 16.0, y), g, text_color);
        y += 17.0;
    }

    let hovered = hovered?;
    let s = &slices[hovered];
    let tip = format!(
        "{} — {} ({:.1}%)",
        s.child
            .map(|c| node.children[c].path.display().to_string())
            .unwrap_or_else(|| s.label.clone()),
        unit.format(s.size),
        s.size as f32 / total * 100.0
    );
    let response = response.on_hover_text_at_pointer(tip);
    let child = s
        .child
        .filter(|&c| !node.children[c].children.is_empty() || node.children[c].own_files > 0)?;
    response.clicked().then_some(child)
}

/// What the running scan will do with its result.
enum Job {
    Full,
    /// Rescan of the folder at this trail.
    Branch(Vec<usize>),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum View {
    Chart,
    LargestFiles,
}

pub struct DiskAnalyzer {
    task: Option<(Job, BackgroundTask<ScanResult>)>,
    result: Option<ScanResult>,
    /// Indices from the root to the selected folder.
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
            view: View::Chart,
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

    fn selected_path(&self) -> Option<PathBuf> {
        let r = self.result.as_ref()?;
        Some(DisplayNode::at(&r.root, &self.trail).path.clone())
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
                        ui.horizontal(|ui| {
                            if ui.small_button("Show").clicked() {
                                reveal_in_explorer(path);
                            }
                            if ui.small_button("Recycle…").clicked() {
                                recycle = Some(path.clone());
                            }
                        });
                    });
                });
            });
        if let Some(p) = recycle {
            self.confirm
                .ask(format!("Move {} to the Recycle Bin?", p.display()), p);
        }
    }

    /// Polls the scan; returns the status-bar text while it runs.
    fn poll_task(&mut self) -> Option<String> {
        let (_, task) = self.task.as_mut()?;
        task.poll();
        let status = if task.status.is_empty() {
            "Analyzing…".to_owned()
        } else {
            format!("Analyzing… {}", task.status)
        };
        let cancelled = task.is_cancelled();
        let Some(r) = task.take_result() else {
            return Some(status);
        };
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
        None
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
        // The original's toolbar; its Analyze Drives and Unit drop-downs are the
        // pickers in the strip below.
        let clicked = frame::toolbar(
            ui,
            &[
                Some(
                    ToolButton::new(icons::HARD_DRIVES, "Analyze Drives")
                        .enabled(!busy && !self.drives.is_empty()),
                ),
                Some(ToolButton::new(icons::FILE_SEARCH, "Analyze Folder").enabled(!busy)),
                None,
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Rescan Branch").enabled(!busy && has)),
                None,
                Some(ToolButton::new(icons::FOLDER_OPEN, "Open Folder").enabled(has)),
                Some(ToolButton::new(icons::ARROW_SQUARE_OUT, "Explore Folder").enabled(has)),
                None,
                Some(ToolButton::new(icons::UPLOAD, "Export").enabled(!busy && has)),
                None,
                Some(ToolButton::new(icons::X, "Close")),
            ],
            |_| {},
        );
        frame::options(ui, |ui| {
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
            ui.label("Unit:");
            egui::ComboBox::from_id_salt("da-unit")
                .selected_text(self.unit.label())
                .show_ui(ui, |ui| {
                    for u in Unit::ALL {
                        ui.selectable_value(&mut self.unit, u, u.label());
                    }
                });
        });

        // Status bar: the original's "Analysis completed. Target: …" line, with the
        // "Add to Context Menu" box on the right.
        let running = self.poll_task();
        let status = match (&running, &self.result) {
            (Some(s), _) => s.clone(),
            (None, Some(r)) => format!(
                "Analysis completed. Target: {} (Total: {}){}",
                r.root.path.display(),
                self.unit.format(r.root.size),
                if r.errors > 0 {
                    format!(", {} folders not readable", r.errors)
                } else {
                    String::new()
                }
            ),
            (None, None) => "Ready".into(),
        };
        let mut toggle_menu = false;
        let mut cancel = false;
        frame::status(ui, &status, |ui| {
            let mut on = installed;
            toggle_menu = ui.checkbox(&mut on, "Add to Context Menu").changed();
            if running.is_some() {
                ui.spinner();
                cancel = ui.button(ctx.i18n.tr("common-cancel")).clicked();
            }
        });
        if toggle_menu {
            self.toggle_menu(ctx);
        }
        if cancel && let Some((_, t)) = &self.task {
            t.cancel();
        }

        match clicked {
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
            Some(5) => {
                if let Some(p) = self.selected_path() {
                    let _ = std::process::Command::new("explorer").arg(p).spawn();
                }
            }
            Some(6) => {
                if let Some(p) = self.selected_path() {
                    reveal_in_explorer(&p);
                }
            }
            Some(8) => self.export_current(),
            Some(10) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }

        // Left: the folder tree ("size - name" with a share bar, like the original).
        let result = self.result.clone();
        let mut pick = None;
        frame::left(ui, 290.0, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            if let Some(result) = &result {
                folder_tree(
                    ui,
                    &result.root,
                    1.0,
                    &mut Vec::new(),
                    &self.trail,
                    self.unit,
                    &mut pick,
                );
            }
        });
        if let Some(t) = pick {
            self.trail = t;
        }

        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            let Some(result) = result else {
                if self.task.is_none() {
                    empty_state(
                        ui,
                        icons::HARD_DRIVES,
                        "Analyze a drive or a folder",
                        "See which folders use the space, as a tree and a pie chart. \
                         Pick the drive above and click Analyze Drives, or click Analyze Folder.",
                    );
                }
                return;
            };
            // The chart's own strip: up one level (the original's only button there)
            // and the largest-files list.
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.trail.is_empty(), egui::Button::new(icons::ARROW_UP))
                    .on_hover_text("Up one level")
                    .clicked()
                {
                    self.trail.pop();
                }
                ui.separator();
                ui.selectable_value(&mut self.view, View::Chart, "Chart");
                ui.selectable_value(
                    &mut self.view,
                    View::LargestFiles,
                    format!("Largest files ({})", result.largest.len()),
                );
            });
            ui.separator();
            match self.view {
                View::Chart => {
                    let node = DisplayNode::at(&result.root, &self.trail);
                    if let Some(i) = pie_chart(ui, node, self.unit) {
                        self.trail.push(i);
                        expand_to(ui.ctx(), &self.trail);
                    }
                }
                View::LargestFiles => self.largest_view(ui, &result),
            }
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

fn tree_id(at: &[usize]) -> egui::Id {
    egui::Id::unique(("da-tree", at))
}

/// Opens the tree along `trail`, so it follows clicks in the chart.
fn expand_to(ctx: &egui::Context, trail: &[usize]) {
    for k in 0..trail.len() {
        let mut s = egui::collapsing_header::CollapsingState::load_with_default_open(
            ctx,
            tree_id(&trail[..k]),
            false,
        );
        s.set_open(true);
        s.store(ctx);
    }
}

/// One tree row: a share bar behind the label, highlighted when selected.
fn tree_row(ui: &mut egui::Ui, selected: bool, share: f32, text: String) -> egui::Response {
    let color = ui.visuals().text_color();
    let galley = ui
        .painter()
        .layout_no_wrap(text, egui::FontId::proportional(13.0), color);
    let (rect, response) = ui.allocate_exact_size(galley.size() + Vec2::new(8.0, 4.0), Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if share > 0.0 {
        let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width() * share.min(1.0), rect.height()));
        painter.rect_filled(bar, 2.0, SHARE_BAR);
    }
    painter.galley(rect.min + Vec2::new(4.0, 2.0), galley, color);
    response
}

/// Folder tree for the left pane: the root as "name (Total: size)", folders as
/// "size - name" with a bar for their share of the parent, and the files directly in
/// a folder as "size - [n Files]". `at` is the path of indices to `node`.
fn folder_tree(
    ui: &mut egui::Ui,
    node: &DirNode,
    share: f32,
    at: &mut Vec<usize>,
    trail: &[usize],
    unit: Unit,
    pick: &mut Option<Vec<usize>>,
) {
    let selected = at.as_slice() == trail;
    let text = if at.is_empty() {
        format!(
            "{}  {} (Total: {})",
            icons::FOLDER_OPEN,
            node.name,
            unit.format(node.size)
        )
    } else {
        format!(
            "{}  {} - {}",
            icons::FOLDER_OPEN,
            unit.format(node.size),
            node.name
        )
    };
    let share = if at.is_empty() { 0.0 } else { share };
    let has_body = !node.children.is_empty();
    if !has_body {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().icon_width + ui.spacing().item_spacing.x);
            if tree_row(ui, selected, share, text).clicked() {
                *pick = Some(at.clone());
            }
        });
        return;
    }
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), tree_id(at), at.is_empty())
        .show_header(ui, |ui| {
            if tree_row(ui, selected, share, text).clicked() {
                *pick = Some(at.clone());
            }
        })
        .body(|ui| {
            let total = node.size.max(1) as f32;
            for (i, c) in node.children.iter().enumerate().take(500) {
                if c.size == 0 && c.children.is_empty() {
                    continue;
                }
                at.push(i);
                folder_tree(ui, c, c.size as f32 / total, at, trail, unit, pick);
                at.pop();
            }
            if node.own_files > 0 {
                let text = format!(
                    "{}  {} - [{} Files]",
                    icons::FILE_TEXT,
                    unit.format(node.own_files_size),
                    node.own_files
                );
                ui.horizontal(|ui| {
                    ui.add_space(ui.spacing().icon_width + ui.spacing().item_spacing.x);
                    tree_row(ui, false, node.own_files_size as f32 / total, text);
                });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pie_groups_small_folders_and_files() {
        let leaf = |name: &str, size: u64| DirNode {
            name: name.into(),
            size,
            own_files_size: size,
            own_files: 1,
            ..Default::default()
        };
        let mut root = DirNode {
            own_files_size: 5,
            own_files: 2,
            children: vec![
                leaf("a", 50),
                leaf("b", 30),
                leaf("empty", 0),
                leaf("c", 10),
                leaf("d", 5),
            ],
            ..Default::default()
        };
        root.resum();
        let got: Vec<(String, u64, Option<usize>)> = pie_slices(&root, 2)
            .into_iter()
            .map(|s| (s.label, s.size, s.child))
            .collect();
        let want: Vec<(String, u64, Option<usize>)> = [
            ("a", 50, Some(0)),
            ("b", 30, Some(1)),
            ("[Other folders]", 15, None),
            ("[Files]", 5, None),
        ]
        .into_iter()
        .map(|(l, s, c)| (l.to_owned(), s, c))
        .collect();
        assert_eq!(got, want);
        assert!(pie_slices(&DirNode::default(), 5).is_empty());
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
