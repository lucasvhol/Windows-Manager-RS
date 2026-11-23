//! Duplicate Files Finder: size → partial hash → full BLAKE3 hash, in parallel.
//! Selected duplicates go to the Recycle Bin (journaled, undoable).

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Tool, egui,
    task::{BackgroundTask, Progress},
    ui::{Confirm, Message, format_bytes, show_message},
};

const PARTIAL: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub size: u64,
    pub files: Vec<FileInfo>,
}

impl Group {
    /// Space recovered by keeping one copy.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
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
fn refine(groups: Vec<Vec<PathBuf>>, limit: Option<usize>) -> Vec<Vec<PathBuf>> {
    groups
        .into_par_iter()
        .flat_map(|files| {
            let mut by_hash: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
            for (p, h) in files
                .into_par_iter()
                .filter_map(|p| hash_file(&p, limit).map(|h| (p, h)))
                .collect::<Vec<_>>()
            {
                by_hash.entry(h).or_default().push(p);
            }
            by_hash.into_values().filter(|g| g.len() > 1).collect::<Vec<_>>()
        })
        .collect()
}

pub fn find_duplicates<T>(roots: &[PathBuf], min_size: u64, progress: Option<&Progress<T>>) -> Vec<Group> {
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut count = 0u64;
    for root in roots {
        for e in WalkDir::new(root).follow_links(false).into_iter().flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            // The same file reached through overlapping roots counts once.
            if !seen.insert(e.path().to_path_buf()) {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if md.len() >= min_size.max(1) {
                by_size.entry(md.len()).or_default().push(e.into_path());
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
        }
    }
    let candidates: Vec<Vec<PathBuf>> = by_size.into_values().filter(|v| v.len() > 1).collect();
    if let Some(p) = progress {
        p.report(0.4, format!("Comparing {} candidate groups…", candidates.len()));
    }
    let partial = refine(candidates, Some(PARTIAL));
    if let Some(p) = progress {
        p.report(0.7, "Verifying full contents…");
    }
    let full = refine(partial, None);
    let mut groups: Vec<Group> = full
        .into_iter()
        .map(|mut paths| {
            paths.sort();
            let size = std::fs::metadata(&paths[0]).map(|m| m.len()).unwrap_or(0);
            let files = paths
                .into_iter()
                .map(|path| FileInfo {
                    modified: std::fs::metadata(&path).and_then(|m| m.modified()).ok(),
                    path,
                })
                .collect();
            Group { size, files }
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.wasted()));
    groups
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Newest,
    Oldest,
    ShortestPath,
}

/// Marks every file except the one to keep in each group.
pub fn auto_select(groups: &[Group], keep: Keep) -> Vec<Vec<bool>> {
    groups
        .iter()
        .map(|g| {
            let keep_idx = match keep {
                Keep::Newest => (0..g.files.len()).max_by_key(|&i| g.files[i].modified),
                Keep::Oldest => (0..g.files.len()).min_by_key(|&i| g.files[i].modified),
                Keep::ShortestPath => (0..g.files.len()).min_by_key(|&i| g.files[i].path.as_os_str().len()),
            }
            .unwrap_or(0);
            (0..g.files.len()).map(|i| i != keep_idx).collect()
        })
        .collect()
}

pub struct DuplicateFinder {
    roots: Vec<PathBuf>,
    min_size_mb: f32,
    task: Option<BackgroundTask<Vec<Group>>>,
    groups: Vec<Group>,
    marked: Vec<Vec<bool>>,
    scanned: bool,
    confirm: Confirm<Vec<PathBuf>>,
    message: Message,
}

impl Default for DuplicateFinder {
    fn default() -> Self {
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
        Self {
            roots: home.into_iter().collect(),
            min_size_mb: 1.0,
            task: None,
            groups: Vec::new(),
            marked: Vec::new(),
            scanned: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl Tool for DuplicateFinder {
    fn id(&self) -> &'static str {
        "duplicates"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(paths) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                let n = paths.len();
                ctx.journal
                    .recycle(
                        "duplicates",
                        format!("Recycled {n} duplicate files"),
                        paths.clone(),
                    )
                    .map(|_| {
                        // Drop removed files; groups left with one file are no longer duplicates.
                        for (g, m) in self.groups.iter_mut().zip(self.marked.iter_mut()) {
                            let keep: Vec<bool> = g.files.iter().map(|f| !paths.contains(&f.path)).collect();
                            let mut k = keep.iter();
                            g.files.retain(|_| *k.next().unwrap());
                            let mut k = keep.iter();
                            m.retain(|_| *k.next().unwrap());
                        }
                        let mut k = self
                            .groups
                            .iter()
                            .map(|g| g.files.len() > 1)
                            .collect::<Vec<_>>()
                            .into_iter();
                        self.marked.retain(|_| k.next().unwrap());
                        self.groups.retain(|g| g.files.len() > 1);
                        format!("Moved {n} duplicate files to the Recycle Bin.")
                    })
                    .map_err(|e| format!("{e:#}"))
            });
        }

        ui.horizontal_wrapped(|ui| {
            ui.label("Search in:");
            let mut remove = None;
            for (i, r) in self.roots.iter().enumerate() {
                ui.label(r.display().to_string());
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
            }
            if let Some(i) = remove {
                self.roots.remove(i);
            }
            if ui.button("Add folder…").clicked()
                && let Some(d) = rfd::FileDialog::new().pick_folder()
                && !self.roots.contains(&d)
            {
                self.roots.push(d);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Ignore files smaller than");
            ui.add(
                egui::DragValue::new(&mut self.min_size_mb)
                    .range(0.0..=10_000.0)
                    .speed(0.1)
                    .suffix(" MB"),
            );
            let busy = self.task.is_some();
            if ui
                .add_enabled(
                    !busy && !self.roots.is_empty(),
                    egui::Button::new("Find duplicates"),
                )
                .clicked()
            {
                let roots = self.roots.clone();
                let min = (self.min_size_mb * 1024.0 * 1024.0) as u64;
                self.message = None;
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    Ok(find_duplicates(&roots, min, Some(p)))
                }));
            }
            if let Some(t) = &mut self.task {
                t.poll();
                ui.add(
                    egui::ProgressBar::new(t.progress)
                        .desired_width(160.0)
                        .text(t.status.clone()),
                );
                if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                    t.cancel();
                }
                if let Some(r) = t.take_result() {
                    self.task = None;
                    self.scanned = true;
                    match r {
                        Ok(groups) => {
                            self.marked = auto_select(&groups, Keep::Oldest);
                            self.groups = groups;
                        }
                        Err(e) => self.message = Some(Err(e)),
                    }
                }
            }
        });
        show_message(ui, &self.message);

        if self.groups.is_empty() {
            if self.scanned {
                ui.label("No duplicate files found.");
            }
            return;
        }
        let wasted: u64 = self.groups.iter().map(Group::wasted).sum();
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
            .map(|(g, m)| g.size * m.iter().filter(|x| **x).count() as u64)
            .sum();
        ui.horizontal(|ui| {
            ui.label(format!("{} groups, {} reclaimable", self.groups.len(), format_bytes(wasted)));
            ui.separator();
            ui.label("Select all but the");
            for (k, label) in [(Keep::Oldest, "oldest"), (Keep::Newest, "newest"), (Keep::ShortestPath, "shortest path")] {
                if ui.small_button(label).clicked() {
                    self.marked = auto_select(&self.groups, k);
                }
            }
            ui.separator();
            if ui
                .add_enabled(!marked.is_empty(), egui::Button::new(format!("Recycle {} files ({})", marked.len(), format_bytes(marked_bytes))))
                .clicked()
            {
                self.confirm.ask(
                    format!(
                        "Move {} files ({}) to the Recycle Bin? At least one copy in each group is always kept.",
                        marked.len(),
                        format_bytes(marked_bytes)
                    ),
                    marked.clone(),
                );
            }
        });
        ui.separator();

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (gi, g) in self.groups.iter().enumerate() {
                ui.strong(format!("{} × {}", g.files.len(), format_bytes(g.size)));
                for (fi, f) in g.files.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let mut m = self.marked[gi][fi];
                        // Never allow marking the last unmarked copy.
                        let others_kept = self.marked[gi].iter().enumerate().any(|(j, x)| j != fi && !x);
                        if ui
                            .add_enabled(m || others_kept, egui::Checkbox::without_text(&mut m))
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
                        ui.weak(modified);
                        if ui.link(f.path.display().to_string()).clicked() {
                            let _ = std::process::Command::new("explorer")
                                .arg(format!("/select,{}", f.path.display()))
                                .spawn();
                        }
                    });
                }
                ui.add_space(6.0);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let groups = find_duplicates::<()>(&[d.to_owned(), d.join("x")], 100, None);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(
            groups[0].files.len(),
            2,
            "overlapping roots must not double-count"
        );
        assert_eq!(groups[0].wasted(), 200_000);

        let all = find_duplicates::<()>(&[d.to_owned()], 1, None);
        assert_eq!(all.len(), 2, "tiny files compared when min size allows");
    }

    #[test]
    fn auto_select_keeps_exactly_one() {
        let t = |s| Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s));
        let g = Group {
            size: 10,
            files: vec![
                FileInfo {
                    path: r"C:\long\path\a".into(),
                    modified: t(5),
                },
                FileInfo {
                    path: r"C:\b".into(),
                    modified: t(1),
                },
                FileInfo {
                    path: r"C:\cc\c".into(),
                    modified: t(9),
                },
            ],
        };
        assert_eq!(
            auto_select(std::slice::from_ref(&g), Keep::Oldest),
            [vec![true, false, true]]
        );
        assert_eq!(
            auto_select(std::slice::from_ref(&g), Keep::Newest),
            [vec![true, true, false]]
        );
        assert_eq!(auto_select(&[g], Keep::ShortestPath), [vec![true, false, true]]);
    }
}
