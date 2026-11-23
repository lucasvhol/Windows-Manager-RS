//! Disk Analyzer: scan a drive or folder, browse folder sizes as a treemap and list,
//! and find the largest files.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use egui_extras::{Column, TableBuilder};
use rayon::prelude::*;
use wm_core::{
    AppCtx, Category, Tool,
    egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2},
    task::{BackgroundTask, Progress},
    ui::{Confirm, Message, format_bytes, show_message},
};

const LARGEST: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct DirNode {
    pub name: String,
    pub path: PathBuf,
    /// Total bytes including subfolders.
    pub size: u64,
    /// Bytes of files directly in this folder.
    pub own_files_size: u64,
    pub files: u64,
    pub children: Vec<DirNode>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub root: DirNode,
    pub largest: Vec<(u64, PathBuf)>,
    pub errors: u64,
    pub total_files: u64,
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
                node.files += 1;
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
        node.children.sort_by_key(|c| Reverse(c.size));
        node.size = node.own_files_size + node.children.iter().map(|c| c.size).sum::<u64>();
        node.files += node.children.iter().map(|c| c.files).sum::<u64>();
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

fn drives() -> Vec<PathBuf> {
    ('A'..='Z')
        .map(|c| PathBuf::from(format!("{c}:\\")))
        .filter(|p| p.exists())
        .collect()
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

pub struct DiskAnalyzer {
    task: Option<BackgroundTask<ScanResult>>,
    result: Option<ScanResult>,
    /// Indices from the root to the folder being viewed.
    trail: Vec<usize>,
    view: View,
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
        self.task = Some(BackgroundTask::spawn(ctx, move |p| Ok(scan(&root, p))));
    }

    fn current<'a>(root: &'a DirNode, trail: &[usize]) -> &'a DirNode {
        trail.iter().fold(root, |n, &i| n.children.get(i).unwrap_or(n))
    }

    fn folders_view(&mut self, ui: &mut egui::Ui, result: &ScanResult) {
        let node = Self::current(&result.root, &self.trail).clone();

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
        ui.weak(format!("{} in {} files", format_bytes(node.size), node.files));

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
                    format!("{name}\n{}", format_bytes(sizes[i] as u64)),
                    egui::FontId::proportional(11.0),
                    Color32::WHITE,
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
                    format_bytes(node.children[i].size)
                )
            } else {
                format!(
                    "Files directly in this folder — {}",
                    format_bytes(node.own_files_size)
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
                        ui.label(format_bytes(c.size));
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
                        if ui.small_button("Open").clicked() {
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
                        ui.label(format_bytes(*size));
                    });
                    row.col(|ui| {
                        ui.label(path.display().to_string());
                    });
                    row.col(|ui| {
                        if ui.small_button("Show").clicked() {
                            let _ = std::process::Command::new("explorer")
                                .arg(format!("/select,{}", path.display()))
                                .spawn();
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
}

impl Tool for DiskAnalyzer {
    fn id(&self) -> &'static str {
        "diskanalyzer"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
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

        ui.horizontal_wrapped(|ui| {
            ui.label("Analyze:");
            let busy = self.task.is_some();
            for d in drives() {
                if ui
                    .add_enabled(!busy, egui::Button::new(d.display().to_string()))
                    .clicked()
                {
                    self.start(ui.ctx(), d);
                }
            }
            if ui.add_enabled(!busy, egui::Button::new("Folder…")).clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.start(ui.ctx(), dir);
            }
        });

        if let Some(task) = &mut self.task {
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
            if let Some(r) = task.take_result() {
                self.task = None;
                match r {
                    Ok(r) => self.result = Some(r),
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
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
            ui.weak(format!(
                "{} files, {}{}",
                result.total_files,
                format_bytes(result.root.size),
                if result.errors > 0 {
                    format!(", {} folders not readable", result.errors)
                } else {
                    String::new()
                }
            ));
        });
        ui.separator();
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

    #[test]
    fn scan_sizes_and_largest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("top.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("a/one.bin"), vec![0u8; 1_000]).unwrap();
        std::fs::write(dir.path().join("a/b/two.bin"), vec![0u8; 10_000]).unwrap();

        let ctx = egui::Context::default();
        let mut task = BackgroundTask::spawn(&ctx, {
            let root = dir.path().to_owned();
            move |p| Ok(scan(&root, p))
        });
        while !task.is_done() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            task.poll();
        }
        let r = task.take_result().unwrap().unwrap();
        assert_eq!(r.root.size, 11_100);
        assert_eq!(r.root.files, 3);
        assert_eq!(r.root.own_files_size, 100);
        assert_eq!(r.root.children[0].name, "a");
        assert_eq!(r.root.children[0].size, 11_000);
        assert_eq!(r.largest[0].0, 10_000);
        assert_eq!(r.total_files, 3);
    }
}
