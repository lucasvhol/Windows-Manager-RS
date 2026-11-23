//! Win+X Menu Editor: hide or restore entries of the Win+X (Start button right-click)
//! menu. Entries are shortcuts in `%LOCALAPPDATA%\Microsoft\Windows\WinX\Group1..3`;
//! hiding moves a shortcut to our data folder (journaled, undoable).
//!
//! Adding *new* entries is not supported: Windows only shows WinX shortcuts that carry
//! a special signature hash, so arbitrary shortcuts would silently not appear.

use std::path::{Path, PathBuf};

use anyhow::Result;
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    process::restart_explorer,
    safety::UndoAction,
    shell::read_shortcut,
    ui::{Message, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct WinXItem {
    pub group: String,
    pub path: PathBuf,
    pub label: String,
    pub target: String,
    pub hidden: bool,
}

fn winx_dir() -> PathBuf {
    PathBuf::from(paths::expand_env(r"%LOCALAPPDATA%\Microsoft\Windows\WinX"))
}

fn hidden_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("winx-hidden"))
}

/// "3 - Windows Explorer.lnk" → "Windows Explorer".
pub fn label_of(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.split_once(" - ") {
        // Ordering prefixes look like "3", "01a" or "04-1".
        Some((n, rest)) if n.starts_with(|c: char| c.is_ascii_digit()) && !n.contains(' ') => rest.to_owned(),
        _ => stem,
    }
}

fn list(root: &Path, hidden: bool) -> Vec<WinXItem> {
    let mut items = Vec::new();
    let Ok(groups) = std::fs::read_dir(root) else {
        return items;
    };
    for g in groups.flatten().filter(|g| g.path().is_dir()) {
        let group = g.file_name().to_string_lossy().into_owned();
        let Ok(files) = std::fs::read_dir(g.path()) else {
            continue;
        };
        for f in files
            .flatten()
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")))
        {
            let target = read_shortcut(&f)
                .map(|s| format!("{} {}", s.target.display(), s.args).trim().to_owned())
                .unwrap_or_default();
            items.push(WinXItem {
                group: group.clone(),
                label: label_of(&f),
                path: f,
                target,
                hidden,
            });
        }
    }
    items
}

pub fn enumerate() -> Vec<WinXItem> {
    let mut items = list(&winx_dir(), false);
    if let Ok(h) = hidden_dir() {
        items.extend(list(&h, true));
    }
    // Windows shows the highest group at the top of the menu.
    items.sort_by(|a, b| {
        b.group
            .cmp(&a.group)
            .then(b.path.file_name().cmp(&a.path.file_name()))
    });
    items
}

/// Moves an item between the WinX folder and the hidden folder; returns (from, to).
fn toggle(item: &WinXItem) -> Result<(PathBuf, PathBuf)> {
    let base = if item.hidden { winx_dir() } else { hidden_dir()? };
    let dest_dir = base.join(&item.group);
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(item.path.file_name().unwrap_or_default());
    std::fs::rename(&item.path, &dest)?;
    Ok((item.path.clone(), dest))
}

#[derive(Default)]
pub struct WinXEditor {
    items: Option<Vec<WinXItem>>,
    restart_pending: bool,
    message: Message,
}

impl Tool for WinXEditor {
    fn id(&self) -> &'static str {
        "winx"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Untick entries to remove them from the Win+X menu (right-click on Start).");
        ui.weak("New entries cannot be added: Windows only shows shortcuts it has signed.");
        let items = self.items.get_or_insert_with(enumerate).clone();
        ui.horizontal(|ui| {
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.items = None;
                self.message = None;
            }
            if self.restart_pending && ui.button("Restart Explorer to apply").clicked() {
                self.message = Some(
                    restart_explorer()
                        .map(|()| "Explorer restarted.".into())
                        .map_err(|e| format!("{e:#}")),
                );
                self.restart_pending = false;
            }
        });
        show_message(ui, &self.message);
        ui.separator();

        let mut action = None;
        let mut current = None;
        for item in &items {
            if current.as_ref() != Some(&item.group) {
                current = Some(item.group.clone());
                ui.add_space(6.0);
                ui.strong(&item.group);
            }
            ui.horizontal(|ui| {
                let mut shown = !item.hidden;
                if ui.checkbox(&mut shown, &item.label).changed() {
                    action = Some(item.clone());
                }
                ui.weak(&item.target);
            });
        }
        if let Some(item) = action {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                toggle(&item)
                    .and_then(|(from, to)| {
                        let verb = if item.hidden { "Restored" } else { "Hid" };
                        ctx.journal.record(
                            "winx",
                            format!("{verb} Win+X entry \"{}\"", item.label),
                            UndoAction::MoveBack {
                                moves: vec![(from, to)],
                            },
                        )?;
                        self.restart_pending = true;
                        Ok(format!("{verb} \"{}\".", item.label))
                    })
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(
            label_of(Path::new("3 - Windows Explorer.lnk")),
            "Windows Explorer"
        );
        assert_eq!(
            label_of(Path::new("01a - Windows PowerShell.lnk")),
            "Windows PowerShell"
        );
        assert_eq!(label_of(Path::new("04-1 - NetworkStatus.lnk")), "NetworkStatus");
        assert_eq!(label_of(Path::new("Tools - Misc.lnk")), "Tools - Misc");
        assert_eq!(label_of(Path::new("Custom.lnk")), "Custom");
    }

    #[test]
    fn real_winx_folder_listed() {
        let items = list(&winx_dir(), false);
        assert!(!items.is_empty(), "the Win+X folder should contain shortcuts");
        assert!(items.iter().all(|i| i.group.starts_with("Group")));
    }
}
