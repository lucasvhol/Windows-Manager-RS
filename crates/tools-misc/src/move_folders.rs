//! Move System Folders: your personal folders (Documents, Downloads, Pictures…) with
//! their location and size; move one elsewhere, with its files or by changing only where
//! Windows points. Done the way Windows' own Location tab does it (SHSetKnownFolderPath),
//! journaled: undo moves the files back and restores the folder entries.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Contacts, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Favorites,
    FOLDERID_Links, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_SavedGames, FOLDERID_SavedSearches,
    FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath, SHSetKnownFolderPath,
};
use windows::core::{GUID, HSTRING};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, registry,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, format_bytes, frame, section, show_message},
};

const USF: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders";
const SF: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders";

pub const FOLDERS: [(&str, GUID); 11] = [
    ("Desktop", FOLDERID_Desktop),
    ("Documents", FOLDERID_Documents),
    ("Downloads", FOLDERID_Downloads),
    ("Music", FOLDERID_Music),
    ("Pictures", FOLDERID_Pictures),
    ("Videos", FOLDERID_Videos),
    ("Favorites", FOLDERID_Favorites),
    ("Contacts", FOLDERID_Contacts),
    ("Links", FOLDERID_Links),
    ("Saved Games", FOLDERID_SavedGames),
    ("Searches", FOLDERID_SavedSearches),
];

pub fn location(id: &GUID) -> Result<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(PathBuf::from(s?))
    }
}

fn set_location(id: &GUID, path: &Path) -> Result<()> {
    unsafe { SHSetKnownFolderPath(id, 0, None, &HSTRING::from(path.as_os_str())) }
        .with_context(|| format!("telling Windows about {}", path.display()))
}

pub fn folder_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn same_drive(a: &Path, b: &Path) -> bool {
    let d = |p: &Path| p.to_string_lossy().chars().next().map(|c| c.to_ascii_uppercase());
    d(a).is_some() && d(a) == d(b)
}

/// Checks a new location: not inside the current one (or the other way round), not a
/// drive root, and empty if it exists.
pub fn check_target(current: &Path, target: &Path) -> Result<()> {
    if target.parent().is_none() {
        bail!("Choose a folder, not the root of a drive.");
    }
    let lower = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let (c, t) = (lower(current), lower(target));
    if c == t {
        bail!("That's where the folder already is.");
    }
    if t.starts_with(&format!("{c}\\")) || c.starts_with(&format!("{t}\\")) {
        bail!("The new location can't be inside the current one, or the other way round.");
    }
    if target.exists() && std::fs::read_dir(target).is_ok_and(|mut d| d.next().is_some()) {
        bail!("{} isn't empty.", target.display());
    }
    Ok(())
}

/// Moves everything in `from` into `to`, returning (from, to) pairs for undo. On the
/// same drive whole items are renamed; otherwise files are moved one by one and the
/// source folders are kept (empty) so the move can be undone. Rolls back on failure.
pub fn move_contents(from: &Path, to: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    std::fs::create_dir_all(to)?;
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| -> Result<()> {
        if same_drive(from, to) {
            for e in std::fs::read_dir(from)?.flatten() {
                let dst = to.join(e.file_name());
                std::fs::rename(e.path(), &dst).with_context(|| format!("moving {}", e.path().display()))?;
                moves.push((e.path(), dst));
            }
        } else {
            for e in walkdir::WalkDir::new(from).min_depth(1).into_iter().flatten() {
                let rel = e.path().strip_prefix(from)?;
                let dst = to.join(rel);
                if e.file_type().is_dir() {
                    std::fs::create_dir_all(&dst)?;
                } else {
                    wm_core::paths::move_path(e.path(), &dst)
                        .with_context(|| format!("moving {} (is it open?)", e.path().display()))?;
                    moves.push((e.path().to_path_buf(), dst));
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        for (src, dst) in moves.iter().rev() {
            let _ = wm_core::paths::move_path(dst, src);
        }
        return Err(e.context("nothing was moved"));
    }
    Ok(moves)
}

fn value_snapshots() -> Vec<registry::ValueSnapshot> {
    let mut out = Vec::new();
    for key in [USF, SF] {
        if let Ok(k) = registry::open_read(key) {
            for (name, _) in k.enum_values().flatten() {
                out.push(registry::snapshot(key, &name));
            }
        }
    }
    out
}

/// Moves a known folder to `target`, optionally with its files; journaled.
pub fn relocate(ctx: &AppCtx<'_>, name: &str, id: &GUID, target: &Path, with_files: bool) -> Result<String> {
    let current = location(id)?;
    check_target(&current, target)?;
    let before = value_snapshots();
    let moves = if with_files && current.exists() {
        move_contents(&current, target)?
    } else {
        std::fs::create_dir_all(target)?;
        Vec::new()
    };
    if let Err(e) = set_location(id, target) {
        for (src, dst) in moves.iter().rev() {
            let _ = wm_core::paths::move_path(dst, src);
        }
        return Err(e);
    }
    // Entries Windows added for the move (a folder at its default place may have none)
    // are deleted again on undo.
    let mut values = before;
    for s in value_snapshots() {
        if !values.iter().any(|b| b.key == s.key && b.name == s.name) {
            values.push(registry::ValueSnapshot { previous: None, ..s });
        }
    }
    let mut steps = Vec::new();
    if !moves.is_empty() {
        steps.push(UndoAction::MoveBack { moves: moves.clone() });
    }
    steps.push(UndoAction::RestoreRegValues { values });
    ctx.journal.record(
        "movefolders",
        format!(
            "Moved {name} from {} to {}{} (undo applies fully after signing out)",
            current.display(),
            target.display(),
            if with_files {
                format!(" with {} item(s)", moves.len())
            } else {
                " (files left in place)".into()
            }
        ),
        UndoAction::Sequence { steps },
    )?;
    Ok(format!(
        "{name} is now at {}. Programs that are open may need restarting to notice.",
        target.display()
    ))
}

// ---------------------------------------------------------------------------------
// Prevent changes (policy)

const POLICY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
const LOCK_VALUE: &str = "DisablePersonalDirChange";

/// Whether the "prohibit redirecting profile folders" policy is on for this account.
pub fn changes_locked() -> bool {
    registry::read_raw(POLICY, LOCK_VALUE)
        .as_ref()
        .and_then(registry::decode_dword)
        .is_some_and(|v| v != 0)
}

/// Turns the policy on or off (hides the folders' Location tab in Explorer); journaled.
pub fn set_changes_locked(ctx: &AppCtx<'_>, on: bool) -> Result<()> {
    let snap = registry::snapshot(POLICY, LOCK_VALUE);
    if on {
        registry::write_raw(POLICY, LOCK_VALUE, &registry::dword(1))?;
    } else {
        registry::delete_value(POLICY, LOCK_VALUE)?;
    }
    ctx.journal.record(
        "movefolders",
        if on {
            "Stopped people moving the personal folders"
        } else {
            "Allowed people to move the personal folders again"
        },
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// UI

struct Row {
    name: &'static str,
    id: GUID,
    path: Option<PathBuf>,
    size: Option<u64>,
}

pub struct MoveFolders {
    rows: Vec<Row>,
    sizes: Option<BackgroundTask<Vec<u64>>>,
    selected: Option<usize>,
    target_parent: Option<PathBuf>,
    with_files: bool,
    confirm: Confirm<(usize, PathBuf, bool)>,
    message: Message,
}

impl Default for MoveFolders {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            sizes: None,
            selected: None,
            target_parent: None,
            with_files: true,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl MoveFolders {
    fn load(&mut self, ui: &egui::Ui) {
        self.rows = FOLDERS
            .iter()
            .map(|(name, id)| Row {
                name,
                id: *id,
                path: location(id).ok(),
                size: None,
            })
            .filter(|r| r.path.is_some())
            .collect();
        let paths: Vec<Option<PathBuf>> = self.rows.iter().map(|r| r.path.clone()).collect();
        self.sizes = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
            Ok(paths
                .iter()
                .map(|p| p.as_deref().map_or(0, folder_size))
                .collect())
        }));
    }
}

impl MoveFolders {
    fn lock_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let mut on = changes_locked();
        let can = registry::writable(POLICY);
        let resp = ui
            .add_enabled(
                can,
                egui::Checkbox::new(
                    &mut on,
                    "Stop people from moving these folders (hides the Location tab in their Properties)",
                ),
            )
            .on_disabled_hover_text("Needs administrator rights: restart the app as administrator.");
        if resp.changed() {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                set_changes_locked(ctx, on)
                    .map(|()| "Saved. Explorer windows opened from now on follow it.".to_owned())
                    .map_err(|e| format!("{e:#}"))
            });
        }
        if !can {
            ui.weak("Changing this needs administrator rights (restart the app as administrator).");
        }
    }
}

impl Tool for MoveFolders {
    fn id(&self) -> &'static str {
        "movefolders"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.rows.is_empty() {
            self.load(ui);
        }
        if let Some(t) = &mut self.sizes {
            t.poll();
            if let Some(Ok(sizes)) = t.take_result() {
                for (r, s) in self.rows.iter_mut().zip(sizes) {
                    r.size = Some(s);
                }
                self.sizes = None;
            }
        }
        if let Some((i, target, with_files)) = self.confirm.show(ui.ctx(), ctx.i18n) {
            let (name, id) = (self.rows[i].name, self.rows[i].id);
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                relocate(ctx, name, &id, &target, with_files).map_err(|e| format!("{e:#}"))
            });
            self.rows.clear();
        }
        let selected = self.selected.filter(|&i| i < self.rows.len());
        let row_name = selected.map_or("", |i| self.rows[i].name);
        let current = selected.and_then(|i| self.rows[i].path.clone());
        let target = selected
            .and(self.target_parent.as_ref())
            .map(|p| p.join(row_name));
        let check = match (&target, &current) {
            (Some(t), Some(cur)) => check_target(cur, t),
            _ => Err(anyhow::anyhow!("")),
        };
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::FOLDER_OPEN, "Choose target").enabled(selected.is_some())),
                Some(ToolButton::new(icons::ARROW_SQUARE_OUT, "Move").enabled(check.is_ok())),
                None,
                Some(ToolButton::new(icons::FOLDERS, "Open folder").enabled(current.is_some())),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
            |_| {},
        ) {
            Some(0) => {
                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                    self.target_parent = Some(d);
                }
            }
            Some(1) => {
                if let (Some(i), Some(t)) = (selected, target.clone()) {
                    self.confirm.ask(
                        format!(
                            "Move {row_name} to {}{}? Close programs using files in it first. You can undo this from the Change Journal.",
                            t.display(),
                            if self.with_files { " with its files" } else { "" }
                        ),
                        (i, t, self.with_files),
                    );
                }
            }
            Some(3) => {
                if let Some(p) = &current {
                    let _ = std::process::Command::new("explorer").arg(p).spawn();
                }
            }
            Some(4) => {
                self.rows.clear();
                self.message = None;
            }
            _ => {}
        }
        frame::options(ui, |ui| {
            ui.radio_value(&mut self.with_files, true, "Move my files there too");
            ui.radio_value(&mut self.with_files, false, "Only change the location");
            ui.separator();
            self.lock_ui(ui, ctx);
        });
        let total: u64 = self.rows.iter().filter_map(|r| r.size).sum();
        frame::status(
            ui,
            &format!("{} folders, {} in all", self.rows.len(), format_bytes(total)),
            |ui| {
                ui.weak("Another drive gives Windows room, and your files survive a reinstall.");
            },
        );
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            let mut pick = None;
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            TableBuilder::new(ui)
                .striped(true)
                .sense(egui::Sense::click())
                .vscroll(false)
                .column(Column::initial(150.0))
                .column(Column::initial(420.0).clip(true))
                .column(Column::remainder())
                .header(22.0, |mut h| {
                    for t in ["Folder", "Location", "Size"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, self.rows.len(), |mut row| {
                        let i = row.index();
                        let r = &self.rows[i];
                        row.set_selected(selected == Some(i));
                        row.col(|ui| {
                            ui.label(format!("{}  {}", icons::FOLDERS, r.name));
                        });
                        row.col(|ui| {
                            ui.label(
                                r.path
                                    .as_ref()
                                    .map_or("(not available)".into(), |p| p.display().to_string()),
                            );
                        });
                        row.col(|ui| {
                            ui.weak(r.size.map_or("…".into(), format_bytes));
                        });
                        if row.response().clicked() {
                            pick = Some(i);
                        }
                    });
                });
            if let Some(i) = pick {
                self.selected = Some(i);
            }
            ui.add_space(10.0);
            if selected.is_none() {
                ui.weak("Pick a folder above, then choose where to move it.");
                return;
            }
            section(
                ui,
                &format!("Move {row_name}"),
                |_| {},
                |ui| {
                    ui.horizontal(|ui| {
                        ui.weak("Into");
                        ui.label(
                            self.target_parent
                                .as_ref()
                                .map_or("(press Choose target)".into(), |p| p.display().to_string()),
                        );
                    });
                    if let Some(t) = &target {
                        ui.horizontal(|ui| {
                            ui.weak("New location");
                            ui.label(t.display().to_string());
                        });
                        if let Err(e) = &check {
                            ui.colored_label(ui.visuals().warn_fg_color, format!("{e:#}"));
                        }
                    }
                },
            );
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy test and the relocation test must not overlap: while the policy is on,
    /// Windows refuses to relocate folders.
    static POLICY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn known_folders_resolve() {
        let docs = location(&FOLDERID_Documents).unwrap();
        assert!(docs.is_absolute());
        assert!(location(&FOLDERID_Downloads).unwrap().is_absolute());
    }

    #[test]
    fn targets_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let cur = dir.path().join("Docs");
        std::fs::create_dir_all(&cur).unwrap();
        assert!(check_target(&cur, &cur).is_err());
        assert!(check_target(&cur, &cur.join("inner")).is_err());
        assert!(
            check_target(&cur, dir.path()).is_err(),
            "parent of the current folder"
        );
        assert!(check_target(&cur, Path::new(r"D:\")).is_err());
        let busy = dir.path().join("Busy");
        std::fs::create_dir_all(&busy).unwrap();
        std::fs::write(busy.join("x"), b"x").unwrap();
        assert!(check_target(&cur, &busy).is_err(), "not empty");
        assert!(check_target(&cur, &dir.path().join("New")).is_ok());
    }

    #[test]
    fn contents_move_and_move_back() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::write(from.join("a.txt"), b"a").unwrap();
        std::fs::write(from.join("sub").join("b.txt"), b"b").unwrap();
        let to = dir.path().join("to");
        let moves = move_contents(&from, &to).unwrap();
        assert!(to.join("sub").join("b.txt").exists() && !from.join("a.txt").exists());
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let e = journal
            .record("t", "move", UndoAction::MoveBack { moves })
            .unwrap();
        journal.undo(e.id).unwrap();
        assert_eq!(std::fs::read(from.join("sub").join("b.txt")).unwrap(), b"b");
    }

    #[test]
    fn prevent_changes_policy_and_undo() {
        let _guard = POLICY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !registry::writable(POLICY) {
            return; // per-user policy keys are read-only for standard accounts here
        }
        let before = registry::snapshot(POLICY, LOCK_VALUE);
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: true,
        };
        let target = !changes_locked();
        let r = set_changes_locked(&ctx, target);
        let after = changes_locked();
        let undo = journal.entries().first().map(|e| journal.undo(e.id));
        let undone = registry::snapshot(POLICY, LOCK_VALUE);
        let _ = registry::restore(&before);
        r.unwrap();
        assert_eq!(after, target);
        undo.unwrap().unwrap();
        assert_eq!(undone, before, "undo restores the policy value exactly");
    }

    #[test]
    fn relocating_a_known_folder_and_undoing_it() {
        let _guard = POLICY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Uses Saved Searches, the least-used known folder, and puts it back.
        let before = location(&FOLDERID_SavedSearches).unwrap();
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
        let target = dir.path().join("Searches");
        let usf_before = registry::snapshot(USF, "{7D1D3A04-DEBB-4115-95CF-2F29DA2920DA}");
        let sf_before = registry::snapshot(SF, "{7D1D3A04-DEBB-4115-95CF-2F29DA2920DA}");
        let r = relocate(&ctx, "Searches", &FOLDERID_SavedSearches, &target, false);
        let moved = location(&FOLDERID_SavedSearches);
        // Undo through the journal, which restores the entries exactly.
        let undo = journal
            .entries()
            .into_iter()
            .max_by_key(|e| e.id)
            .map(|e| journal.undo(e.id));
        let usf_after = registry::snapshot(USF, "{7D1D3A04-DEBB-4115-95CF-2F29DA2920DA}");
        let sf_after = registry::snapshot(SF, "{7D1D3A04-DEBB-4115-95CF-2F29DA2920DA}");
        // Whatever happened, leave exactly the entries that were there before. (This
        // process keeps its own cached location, so the registry is what's checked.)
        let _ = registry::restore(&usf_before);
        let _ = registry::restore(&sf_before);
        r.unwrap();
        assert_eq!(moved.unwrap(), target);
        undo.unwrap().unwrap();
        assert_eq!(
            usf_after, usf_before,
            "undo restores the User Shell Folders entry"
        );
        assert_eq!(sf_after, sf_before, "undo restores the Shell Folders entry");
        let _ = before;
    }
}
