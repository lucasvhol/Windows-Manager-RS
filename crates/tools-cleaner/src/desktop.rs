//! Desktop Cleaner: tidy desktop items into a dated folder (optionally sorted by type)
//! and remove broken shortcuts. Moves are journaled and can be moved back.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use wm_core::{
    AppCtx, Category, Tool, egui,
    safety::UndoAction,
    shell::{KnownFolder, is_broken_shortcut, known_folder},
    ui::{Confirm, Message, show_message},
};

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

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub kind: Kind,
    pub broken: bool,
    pub public: bool,
}

pub fn scan_dir(dir: &Path, public: bool) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("desktop.ini"))
        })
        .map(|path| {
            let kind = Kind::of(&path);
            let broken = kind == Kind::Shortcut
                && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
                && is_broken_shortcut(&path);
            Item {
                path,
                kind,
                broken,
                public,
            }
        })
        .collect()
}

pub fn scan() -> Vec<Item> {
    let mut items = Vec::new();
    if let Ok(d) = known_folder(KnownFolder::Desktop) {
        items.extend(scan_dir(&d, false));
    }
    if let Ok(d) = known_folder(KnownFolder::PublicDesktop) {
        items.extend(scan_dir(&d, true));
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

pub struct DesktopCleaner {
    items: Option<Vec<Item>>,
    selected: Vec<bool>,
    target: PathBuf,
    by_type: bool,
    confirm: Confirm<Vec<PathBuf>>,
    message: Message,
}

impl Default for DesktopCleaner {
    fn default() -> Self {
        let docs = known_folder(KnownFolder::Documents).unwrap_or_else(|_| PathBuf::from("."));
        let date = chrono::Local::now().format("%Y-%m-%d");
        Self {
            items: None,
            selected: Vec::new(),
            target: docs.join(format!("Desktop cleanup {date}")),
            by_type: true,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl Tool for DesktopCleaner {
    fn id(&self) -> &'static str {
        "desktop"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.items.is_none() {
            let items = scan();
            // Pre-select everything except shortcuts and public items (which need admin).
            self.selected = items
                .iter()
                .map(|i| i.kind != Kind::Shortcut && !i.public)
                .collect();
            self.items = Some(items);
        }
        let items = self.items.clone().unwrap_or_default();

        if let Some(paths) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                ctx.journal
                    .recycle(
                        "desktop",
                        format!("Recycled {} broken shortcuts", paths.len()),
                        paths.clone(),
                    )
                    .map(|_| format!("Moved {} broken shortcuts to the Recycle Bin.", paths.len()))
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }

        let broken: Vec<PathBuf> = items
            .iter()
            .filter(|i| i.broken && (!i.public || ctx.elevated))
            .map(|i| i.path.clone())
            .collect();
        let chosen: Vec<Item> = items
            .iter()
            .zip(&self.selected)
            .filter(|(i, s)| **s && (!i.public || ctx.elevated))
            .map(|(i, _)| i.clone())
            .collect();

        ui.horizontal(|ui| {
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.items = None;
                self.message = None;
            }
            ui.weak(format!("{} items on the desktop", items.len()));
            ui.separator();
            if ui
                .add_enabled(
                    !broken.is_empty(),
                    egui::Button::new(format!("Remove broken shortcuts ({})", broken.len())),
                )
                .clicked()
            {
                self.confirm.ask(
                    format!(
                        "Move {} shortcuts whose targets no longer exist to the Recycle Bin?",
                        broken.len()
                    ),
                    broken.clone(),
                );
            }
        });
        ui.horizontal(|ui| {
            ui.label("Move selected items to:");
            ui.label(egui::RichText::new(self.target.display().to_string()).monospace());
            if ui.small_button("Change…").clicked()
                && let Some(d) = rfd::FileDialog::new().pick_folder()
            {
                self.target = d;
            }
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.by_type, "Sort into folders by type");
            if ui
                .add_enabled(
                    !chosen.is_empty(),
                    egui::Button::new(format!("Move {} items", chosen.len())),
                )
                .clicked()
            {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    move_items(&chosen, &self.target, self.by_type)
                        .and_then(|moves| {
                            let n = moves.len();
                            ctx.journal.record(
                                "desktop",
                                format!("Moved {n} desktop items to {}", self.target.display()),
                                UndoAction::MoveBack { moves },
                            )?;
                            Ok(n)
                        })
                        .map(|n| format!("Moved {n} items. Undo from the Change Journal puts them back."))
                        .map_err(|e| format!("{e:#}"))
                });
                self.items = None;
            }
        });
        show_message(ui, &self.message);
        ui.separator();
        if items.is_empty() {
            ui.label("Your desktop is already empty; there is nothing to tidy.");
            return;
        }

        let mut current = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (i, item) in items.iter().enumerate() {
                if current != Some(item.kind) {
                    current = Some(item.kind);
                    ui.add_space(6.0);
                    ui.strong(item.kind.label());
                }
                ui.horizontal(|ui| {
                    let usable = !item.public || ctx.elevated;
                    ui.add_enabled(usable, egui::Checkbox::without_text(&mut self.selected[i]));
                    ui.label(
                        item.path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    );
                    if item.broken {
                        ui.colored_label(ui.visuals().warn_fg_color, "broken shortcut");
                    }
                    if item.public {
                        ui.weak("all users");
                    }
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        std::fs::write(target.join("Documents/notes.txt"), "existing").unwrap();

        let items = scan_dir(&desk, false);
        assert_eq!(items.len(), 2);
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
}
