//! Start Menu Manager: browse the "All apps" shortcuts (yours and all users'),
//! remove broken or unwanted ones, add new shortcuts, and Start menu options.
//!
//! Windows 11 stores pinned items in an encrypted per-user file that no public API
//! edits, so pins are managed from Start itself.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    safety::UndoAction,
    shell::{ShortcutSpec, create_shortcut, is_broken_shortcut},
    tweaks::TweakPanel,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct StartItem {
    pub path: PathBuf,
    /// Folder inside Programs ("" for top level).
    pub folder: String,
    pub name: String,
    pub all_users: bool,
    pub broken: bool,
}

pub fn programs_dirs() -> [(PathBuf, bool); 2] {
    [
        (
            PathBuf::from(paths::expand_env(
                r"%APPDATA%\Microsoft\Windows\Start Menu\Programs",
            )),
            false,
        ),
        (
            PathBuf::from(paths::expand_env(
                r"%ProgramData%\Microsoft\Windows\Start Menu\Programs",
            )),
            true,
        ),
    ]
}

pub fn scan_dir(root: &Path, all_users: bool) -> Vec<StartItem> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("lnk") || x.eq_ignore_ascii_case("url"))
        })
        .map(|e| {
            let path = e.into_path();
            let folder = path
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let is_lnk = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"));
            StartItem {
                name: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                broken: is_lnk && is_broken_shortcut(&path),
                folder,
                all_users,
                path,
            }
        })
        .collect()
}

pub fn enumerate() -> Vec<StartItem> {
    let mut items: Vec<StartItem> = programs_dirs()
        .iter()
        .flat_map(|(d, all)| scan_dir(d, *all))
        .collect();
    items.sort_by_key(|i| (i.folder.to_lowercase(), i.name.to_lowercase()));
    items
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Shortcuts,
    Options,
}

pub struct StartMenuManager {
    tab: Tab,
    items: Option<Vec<StartItem>>,
    filter: String,
    broken_only: bool,
    panel: TweakPanel,
    confirm: Confirm<Vec<PathBuf>>,
    message: Message,
}

impl Default for StartMenuManager {
    fn default() -> Self {
        Self {
            tab: Tab::Shortcuts,
            items: None,
            filter: String::new(),
            broken_only: false,
            panel: TweakPanel::new("startmenu", include_str!("../tweaks/start.toml")),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl StartMenuManager {
    fn add_shortcut(&mut self, ctx: &AppCtx<'_>) {
        let Some(target) = rfd::FileDialog::new()
            .add_filter("Programs", &["exe", "bat", "cmd", "msc"])
            .pick_file()
        else {
            return;
        };
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let name = target
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lnk = programs_dirs()[0].0.join(format!("{name}.lnk"));
        if lnk.exists() {
            self.message = Some(Err(format!(
                "A Start menu shortcut named \"{name}\" already exists."
            )));
            return;
        }
        self.message = Some(
            create_shortcut(
                &lnk,
                &ShortcutSpec {
                    target,
                    ..Default::default()
                },
            )
            .and_then(|()| {
                ctx.journal.record(
                    "startmenu",
                    format!("Added Start menu shortcut {name}"),
                    UndoAction::DeleteCreatedFiles {
                        paths: vec![lnk.clone()],
                    },
                )
            })
            .map(|_| format!("Added \"{name}\" to All apps."))
            .map_err(|e| format!("{e:#}")),
        );
        self.items = None;
    }

    fn shortcuts_tab(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.items.get_or_insert_with(enumerate).clone();
        let usable = |i: &StartItem| !i.all_users || ctx.elevated;
        let broken: Vec<PathBuf> = items
            .iter()
            .filter(|i| i.broken && usable(i))
            .map(|i| i.path.clone())
            .collect();

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            ui.checkbox(&mut self.broken_only, "Only broken");
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.items = None;
            }
            ui.separator();
            if ui
                .add_enabled(
                    !broken.is_empty(),
                    egui::Button::new(format!("Remove {} broken", broken.len())),
                )
                .clicked()
            {
                self.confirm.ask(
                    format!(
                        "Move {} Start menu shortcuts whose programs no longer exist to the Recycle Bin?",
                        broken.len()
                    ),
                    broken.clone(),
                );
            }
            if ui.button("Add shortcut…").clicked() {
                self.add_shortcut(ctx);
            }
        });
        ui.weak(format!(
            "{} shortcuts, {} broken. Pinned tiles are managed from Start itself.",
            items.len(),
            items.iter().filter(|i| i.broken).count()
        ));

        let f = self.filter.to_lowercase();
        let mut remove = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut current: Option<&str> = None;
            for item in items
                .iter()
                .filter(|i| !self.broken_only || i.broken)
                .filter(|i| {
                    f.is_empty() || i.name.to_lowercase().contains(&f) || i.folder.to_lowercase().contains(&f)
                })
            {
                if current != Some(item.folder.as_str()) {
                    current = Some(item.folder.as_str());
                    ui.add_space(4.0);
                    ui.strong(if item.folder.is_empty() {
                        "(top level)"
                    } else {
                        item.folder.as_str()
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(&item.name);
                    if item.broken {
                        ui.colored_label(ui.visuals().warn_fg_color, "broken");
                    }
                    if item.all_users {
                        ui.weak("all users");
                    }
                    if ui.small_button("Show").clicked() {
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{}", item.path.display()))
                            .spawn();
                    }
                    if ui
                        .add_enabled(usable(item), egui::Button::new("Remove").small())
                        .clicked()
                    {
                        remove = Some(item.clone());
                    }
                });
            }
        });
        if let Some(item) = remove {
            self.confirm.ask(
                format!("Move \"{}\" to the Recycle Bin?", item.name),
                vec![item.path],
            );
        }
    }
}

impl Tool for StartMenuManager {
    fn id(&self) -> &'static str {
        "startmenu"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(paths) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                let n = paths.len();
                ctx.journal
                    .recycle("startmenu", format!("Removed {n} Start menu shortcuts"), paths)
                    .map(|_| format!("Removed {n} shortcuts."))
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Shortcuts, "All apps shortcuts");
            ui.selectable_value(&mut self.tab, Tab::Options, "Start options");
        });
        show_message(ui, &self.message);
        ui.separator();
        match self.tab {
            Tab::Shortcuts => self.shortcuts_tab(ui, ctx),
            Tab::Options => self.panel.ui(ui, ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_nested_and_broken() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Tools")).unwrap();
        create_shortcut(
            &dir.path().join("Tools/Notepad.lnk"),
            &ShortcutSpec {
                target: r"C:\Windows\notepad.exe".into(),
                ..Default::default()
            },
        )
        .unwrap();
        create_shortcut(
            &dir.path().join("Gone.lnk"),
            &ShortcutSpec {
                target: dir.path().join("missing.exe"),
                ..Default::default()
            },
        )
        .unwrap();
        let mut items = scan_dir(dir.path(), false);
        items.sort_by_key(|i| i.name.clone());
        assert_eq!(items.len(), 2);
        assert_eq!(
            (items[0].name.as_str(), items[0].broken, items[0].folder.as_str()),
            ("Gone", true, "")
        );
        assert_eq!(
            (items[1].name.as_str(), items[1].broken, items[1].folder.as_str()),
            ("Notepad", false, "Tools")
        );
    }

    #[test]
    fn real_start_menu_lists_items() {
        assert!(!enumerate().is_empty());
    }
}
