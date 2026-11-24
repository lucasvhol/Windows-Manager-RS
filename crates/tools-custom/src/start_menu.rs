//! Start Menu Manager: the "All apps" shortcuts (yours and all users') as a folder
//! tree with owner, availability and target; add, "pin any" (files, folders and web
//! addresses as apps), rename, launch, remove; back up and restore the taskbar pins
//! and the Start layout; and Start menu options.
//!
//! Pinning to the taskbar or Start cannot be done for another program on current
//! Windows, so "Pin any" creates an All apps entry that can be pinned from Start.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Tool,
    egui::{self, Vec2},
    icons, paths,
    process::{hidden_command, restart_explorer, run_checked},
    safety::UndoAction,
    shell::{
        ShortcutSpec, create_shortcut, is_broken_shortcut, read_shortcut, reveal_in_explorer, shell_open,
    },
    tweaks::TweakPanel,
    ui::{Confirm, Message, ToolButton, show_message, status_bar, toolbar},
};

const TOOL: &str = "startmenu";
const TASKBAND: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Taskband";

#[derive(Debug, Clone, PartialEq)]
pub struct StartItem {
    pub path: PathBuf,
    /// Folder inside Programs ("" for top level).
    pub folder: String,
    pub name: String,
    pub all_users: bool,
    pub broken: bool,
    pub target: String,
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

fn taskbar_pins_dir() -> PathBuf {
    PathBuf::from(paths::expand_env(
        r"%APPDATA%\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar",
    ))
}

fn start_layout_dir() -> PathBuf {
    PathBuf::from(paths::expand_env(
        r"%LOCALAPPDATA%\Packages\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\LocalState",
    ))
}

const LAYOUT_FILES: [&str; 2] = ["start2.bin", "start.bin"];

fn url_of(file: &Path) -> String {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("URL=").map(str::to_owned)))
        .unwrap_or_default()
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
            let target = if is_lnk {
                read_shortcut(&path)
                    .map(|s| format!("{} {}", s.target.display(), s.args).trim().to_owned())
                    .unwrap_or_default()
            } else {
                url_of(&path)
            };
            StartItem {
                name: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                broken: is_lnk && is_broken_shortcut(&path),
                folder,
                all_users,
                target,
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

fn valid_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        bail!("\"{n}\" cannot be used as a name");
    }
    Ok(n.to_owned())
}

/// The shortcut for "pin any": programs directly; files, folders and web addresses
/// through Explorer, so Start treats them as an app that can be pinned.
pub fn pin_any_spec(target: &str) -> ShortcutSpec {
    let t = target.trim().trim_matches('"');
    let is_program = Path::new(t).extension().is_some_and(|e| {
        ["exe", "bat", "cmd", "com", "msc"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    });
    if is_program && !t.contains("://") {
        ShortcutSpec {
            target: PathBuf::from(t),
            ..Default::default()
        }
    } else {
        ShortcutSpec {
            target: PathBuf::from(paths::expand_env(r"%SystemRoot%\explorer.exe")),
            args: format!("\"{t}\""),
            description: t.to_owned(),
            ..Default::default()
        }
    }
}

/// Adds an All apps shortcut (for you) and journals it.
pub fn add_item(ctx: &AppCtx<'_>, name: &str, target: &str) -> Result<PathBuf> {
    let name = valid_name(name)?;
    let lnk = programs_dirs()[0].0.join(format!("{name}.lnk"));
    if lnk.exists() {
        bail!("A Start menu shortcut named \"{name}\" already exists");
    }
    create_shortcut(&lnk, &pin_any_spec(target))?;
    ctx.journal.record(
        TOOL,
        format!("Added Start menu shortcut {name}"),
        UndoAction::DeleteCreatedFiles {
            paths: vec![lnk.clone()],
        },
    )?;
    Ok(lnk)
}

pub fn rename(ctx: &AppCtx<'_>, item: &StartItem, new_name: &str) -> Result<()> {
    let name = valid_name(new_name)?;
    let ext = item
        .path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dest = item.path.with_file_name(format!("{name}.{ext}"));
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    std::fs::rename(&item.path, &dest)?;
    ctx.journal.record(
        TOOL,
        format!("Renamed Start menu shortcut \"{}\" to \"{name}\"", item.name),
        UndoAction::MoveBack {
            moves: vec![(item.path.clone(), dest)],
        },
    )?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let dest = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

/// Saves the taskbar pins (registry and shortcuts) and the Start layout into a new
/// folder inside `parent`. Returns the folder.
pub fn backup_layout(parent: &Path) -> Result<PathBuf> {
    let dir = parent.join(format!(
        "Start and taskbar backup {}",
        chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
    ));
    std::fs::create_dir_all(&dir)?;
    run_checked(
        hidden_command("reg")
            .args(["export", TASKBAND])
            .arg(dir.join("taskband.reg"))
            .arg("/y"),
    )
    .context("saving the taskbar pins")?;
    if taskbar_pins_dir().is_dir() {
        copy_dir(&taskbar_pins_dir(), &dir.join("TaskBar"))?;
    }
    for f in LAYOUT_FILES {
        let src = start_layout_dir().join(f);
        if src.is_file() {
            std::fs::copy(&src, dir.join(f))?;
        }
    }
    Ok(dir)
}

/// What a backup folder contains: (taskbar pins, Start layout).
pub fn backup_contents(dir: &Path) -> (bool, bool) {
    (
        dir.join("taskband.reg").is_file() && dir.join("TaskBar").is_dir(),
        LAYOUT_FILES.iter().any(|f| dir.join(f).is_file()),
    )
}

/// Restores a backup made by [`backup_layout`]. The current state is kept so the
/// whole restore can be undone from the Change Journal.
pub fn restore_layout(ctx: &AppCtx<'_>, dir: &Path, taskbar: bool, start: bool) -> Result<()> {
    let (has_taskbar, has_start) = backup_contents(dir);
    if !(taskbar && has_taskbar) && !(start && has_start) {
        bail!("{} is not a Start and taskbar backup", dir.display());
    }
    let mut steps = Vec::new();
    if taskbar && has_taskbar {
        steps.push(ctx.journal.backup_reg_key(TASKBAND)?);
        let pins = taskbar_pins_dir();
        let aside = paths::backups_dir()?.join(format!(
            "taskbar-pins-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S%3f")
        ));
        if pins.exists() {
            std::fs::rename(&pins, &aside).context("setting the current pins aside")?;
        }
        copy_dir(&dir.join("TaskBar"), &pins)?;
        steps.push(UndoAction::DeleteCreatedFiles {
            paths: vec![pins.clone()],
        });
        if aside.exists() {
            steps.push(UndoAction::MoveBack {
                moves: vec![(pins, aside)],
            });
        }
        run_checked(hidden_command("reg").arg("import").arg(dir.join("taskband.reg")))
            .context("restoring the taskbar pins")?;
    }
    if start && has_start {
        // Start keeps its layout file open; it restarts by itself.
        let _ = hidden_command("taskkill")
            .args(["/f", "/im", "StartMenuExperienceHost.exe"])
            .output();
        for f in LAYOUT_FILES {
            let src = dir.join(f);
            if !src.is_file() {
                continue;
            }
            let dest = start_layout_dir().join(f);
            if dest.is_file() {
                steps.push(ctx.journal.backup_file(&dest)?);
            } else {
                steps.push(UndoAction::DeleteCreatedFiles {
                    paths: vec![dest.clone()],
                });
            }
            std::fs::copy(&src, &dest).with_context(|| format!("restoring {f}"))?;
        }
    }
    ctx.journal.record(
        TOOL,
        format!("Restored the Start and taskbar layout from {}", dir.display()),
        UndoAction::Sequence { steps },
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Shortcuts,
    Layout,
    Options,
}

enum Ask {
    Remove(Vec<PathBuf>),
    Restore {
        dir: PathBuf,
        taskbar: bool,
        start: bool,
    },
}

pub struct StartMenuManager {
    tab: Tab,
    items: Option<Vec<StartItem>>,
    selected: Option<PathBuf>,
    filter: String,
    broken_only: bool,
    pin_any: Option<(String, String)>,
    renaming: Option<(StartItem, String)>,
    restore_dir: Option<PathBuf>,
    restore_taskbar: bool,
    restore_start: bool,
    panel: TweakPanel,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for StartMenuManager {
    fn default() -> Self {
        Self {
            tab: Tab::Shortcuts,
            items: None,
            selected: None,
            filter: String::new(),
            broken_only: false,
            pin_any: None,
            renaming: None,
            restore_dir: None,
            restore_taskbar: true,
            restore_start: true,
            panel: TweakPanel::new("startmenu", include_str!("../tweaks/start.toml")),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl StartMenuManager {
    fn done(&mut self, r: Result<String>) {
        self.message = Some(r.map_err(|e| format!("{e:#}")));
        self.items = None;
    }

    fn dry(&mut self, ctx: &AppCtx<'_>) -> bool {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
        }
        ctx.config.dry_run
    }

    fn dialogs(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some((name, target)) = &mut self.pin_any {
            let mut result = None;
            egui::Window::new("Pin any item")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label("Adds a file, folder, program or web address to All apps, where it can be pinned to Start or the taskbar.");
                    egui::Grid::new("pin-any").num_columns(2).show(ui, |ui| {
                        ui.label("Opens");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(target).hint_text("Path or https://…").desired_width(240.0));
                            let mut picked = None;
                            if ui.button("File…").clicked() {
                                picked = rfd::FileDialog::new().pick_file();
                            }
                            if ui.button("Folder…").clicked() {
                                picked = rfd::FileDialog::new().pick_folder();
                            }
                            if let Some(p) = picked {
                                *target = p.display().to_string();
                                if name.is_empty() {
                                    *name = p.file_stem().or(p.file_name()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("Name");
                        ui.text_edit_singleline(name);
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!target.trim().is_empty(), egui::Button::new("Add")).clicked() {
                            result = Some(true);
                        }
                        if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                            result = Some(false);
                        }
                    });
                });
            match result {
                Some(true) => {
                    let (name, target) = self.pin_any.take().expect("open");
                    if !self.dry(ctx) {
                        let r = add_item(ctx, &name, &target).map(|_| {
                            format!(
                                "Added \"{}\" to All apps. Right-click it in Start to pin it.",
                                name.trim()
                            )
                        });
                        self.done(r);
                    }
                }
                Some(false) => self.pin_any = None,
                None => {}
            }
        }
        if let Some((_, name)) = &mut self.renaming {
            let mut result = None;
            egui::Window::new("Rename shortcut")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.text_edit_singleline(name).request_focus();
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            result = Some(true);
                        }
                        if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                            result = Some(false);
                        }
                    });
                });
            match result {
                Some(true) => {
                    let (item, name) = self.renaming.take().expect("open");
                    if !self.dry(ctx) {
                        let r = rename(ctx, &item, &name).map(|_| "Renamed.".to_owned());
                        self.done(r);
                    }
                }
                Some(false) => self.renaming = None,
                None => {}
            }
        }
    }

    fn shortcuts_tab(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.items.get_or_insert_with(enumerate).clone();
        let usable = |i: &StartItem| !i.all_users || ctx.elevated;
        let selected = self
            .selected
            .as_ref()
            .and_then(|p| items.iter().find(|i| &i.path == p))
            .cloned();
        let broken: Vec<PathBuf> = items
            .iter()
            .filter(|i| i.broken && usable(i))
            .map(|i| i.path.clone())
            .collect();
        let can_edit = selected.as_ref().is_some_and(usable);
        let broken_label = format!("Remove broken ({})", broken.len());

        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add program")),
                Some(ToolButton::new(icons::PUSH_PIN, "Pin any")),
                None,
                Some(
                    ToolButton::new(icons::PLAY, "Launch")
                        .enabled(selected.as_ref().is_some_and(|s| !s.broken)),
                ),
                Some(ToolButton::new(icons::PENCIL_SIMPLE, "Rename").enabled(can_edit)),
                Some(ToolButton::new(icons::FOLDER_OPEN, "Show").enabled(selected.is_some())),
                Some(ToolButton::new(icons::TRASH, "Remove").enabled(can_edit)),
                None,
                Some(ToolButton::new(icons::BROOM, &broken_label).enabled(!broken.is_empty())),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
        ) {
            Some(0) => {
                if let Some(t) = rfd::FileDialog::new()
                    .add_filter("Programs", &["exe", "bat", "cmd", "msc"])
                    .pick_file()
                    && !self.dry(ctx)
                {
                    let name = t
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let r = add_item(ctx, &name, &t.display().to_string())
                        .map(|_| format!("Added \"{name}\" to All apps."));
                    self.done(r);
                }
            }
            Some(1) => self.pin_any = Some((String::new(), String::new())),
            Some(3) => {
                if let Some(s) = &selected {
                    let _ = shell_open(&s.path.display().to_string(), "");
                }
            }
            Some(4) => {
                if let Some(s) = &selected {
                    self.renaming = Some((s.clone(), s.name.clone()));
                }
            }
            Some(5) => {
                if let Some(s) = &selected {
                    reveal_in_explorer(&s.path);
                }
            }
            Some(6) => {
                if let Some(s) = &selected {
                    self.confirm.ask(
                        format!("Move \"{}\" to the Recycle Bin?", s.name),
                        Ask::Remove(vec![s.path.clone()]),
                    );
                }
            }
            Some(8) => self.confirm.ask(
                format!(
                    "Move {} Start menu shortcuts whose programs no longer exist to the Recycle Bin?",
                    broken.len()
                ),
                Ask::Remove(broken.clone()),
            ),
            Some(9) => self.items = None,
            _ => {}
        }
        show_message(ui, &self.message);
        let status = format!(
            "{} shortcuts, {} broken. Pin items from Start itself; Windows does not let other programs pin.",
            items.len(),
            items.iter().filter(|i| i.broken).count()
        );
        egui::Panel::bottom("start-status")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                status_bar(ui, &status, |ui| {
                    ui.checkbox(&mut self.broken_only, "Only broken");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text(ctx.i18n.tr("common-filter"))
                            .desired_width(150.0),
                    );
                })
            });

        let f = self.filter.to_lowercase();
        let visible: Vec<&StartItem> = items
            .iter()
            .filter(|i| !self.broken_only || i.broken)
            .filter(|i| {
                f.is_empty() || i.name.to_lowercase().contains(&f) || i.folder.to_lowercase().contains(&f)
            })
            .collect();
        let mut folders: Vec<&str> = visible.iter().map(|i| i.folder.as_str()).collect();
        folders.dedup();
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            for folder in folders {
                let rows: Vec<&&StartItem> = visible.iter().filter(|i| i.folder == folder).collect();
                egui::CollapsingHeader::new(format!(
                    "{} {} ({})",
                    icons::FOLDERS,
                    if folder.is_empty() { "(top level)" } else { folder },
                    rows.len()
                ))
                .id_salt(folder)
                .default_open(true)
                .show(ui, |ui| {
                    TableBuilder::new(ui)
                        .id_salt(folder)
                        .striped(true)
                        .sense(egui::Sense::click())
                        .vscroll(false)
                        .column(Column::initial(240.0).clip(true))
                        .column(Column::initial(70.0))
                        .column(Column::initial(80.0))
                        .column(Column::remainder().clip(true))
                        .body(|mut body| {
                            for it in rows {
                                body.row(21.0, |mut row| {
                                    row.set_selected(self.selected.as_ref() == Some(&it.path));
                                    row.col(|ui| {
                                        ui.label(&it.name);
                                    });
                                    row.col(|ui| {
                                        ui.weak(if it.all_users { "All users" } else { "You" });
                                    });
                                    row.col(|ui| {
                                        if it.broken {
                                            ui.colored_label(ui.visuals().warn_fg_color, "Missing");
                                        } else {
                                            ui.weak("Available");
                                        }
                                    });
                                    row.col(|ui| {
                                        ui.weak(&it.target);
                                    });
                                    if row.response().clicked() {
                                        clicked = Some(it.path.clone());
                                    }
                                    if row.response().double_clicked() {
                                        let _ = shell_open(&it.path.display().to_string(), "");
                                    }
                                });
                            }
                        });
                });
            }
        });
        if clicked.is_some() {
            self.selected = clicked;
        }
    }

    fn layout_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(
            "Save your taskbar pins and Start layout (pinned apps and their order) to a folder, \
             and put them back later, for example after a reset or on a new account.",
        );
        ui.add_space(6.0);
        if ui.button(format!("{}  Back up…", icons::FLOPPY_DISK)).clicked()
            && let Some(parent) = rfd::FileDialog::new()
                .set_title("Where to save the backup")
                .pick_folder()
        {
            self.message = Some(
                backup_layout(&parent)
                    .map(|d| format!("Saved to {}.", d.display()))
                    .map_err(|e| format!("{e:#}")),
            );
        }
        ui.add_space(10.0);
        ui.strong("Restore");
        ui.horizontal(|ui| {
            if ui.button("Choose a backup folder…").clicked() {
                self.restore_dir = rfd::FileDialog::new().pick_folder();
            }
            if let Some(d) = &self.restore_dir {
                ui.monospace(d.display().to_string());
            }
        });
        if let Some(d) = self.restore_dir.clone() {
            let (tb, st) = backup_contents(&d);
            if !tb && !st {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "This folder is not a Start and taskbar backup.",
                );
            } else {
                ui.add_enabled(tb, egui::Checkbox::new(&mut self.restore_taskbar, "Taskbar pins"));
                ui.add_enabled(st, egui::Checkbox::new(&mut self.restore_start, "Start layout"));
                let (t, s) = (tb && self.restore_taskbar, st && self.restore_start);
                if ui.add_enabled(t || s, egui::Button::new("Restore")).clicked() {
                    self.confirm.ask(
                        "Restore from this backup? Explorer and Start restart, so open windows of File Explorer close. \
                         Your current layout is kept and comes back with Undo.",
                        Ask::Restore { dir: d, taskbar: t, start: s },
                    );
                }
            }
        }
        show_message(ui, &self.message);
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
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n)
            && !self.dry(ctx)
        {
            let r = match ask {
                Ask::Remove(paths) => {
                    let n = paths.len();
                    ctx.journal
                        .recycle(TOOL, format!("Removed {n} Start menu shortcuts"), paths)
                        .map(|_| format!("Removed {n} shortcuts."))
                }
                Ask::Restore { dir, taskbar, start } => restore_layout(ctx, &dir, taskbar, start).map(|_| {
                    if taskbar {
                        let _ = restart_explorer();
                    }
                    "Restored. Undo in the Change Journal puts the previous layout back.".to_owned()
                }),
            };
            self.done(r);
        }
        self.dialogs(ui, ctx);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Shortcuts, "All apps");
            ui.selectable_value(&mut self.tab, Tab::Layout, "Taskbar and Start backup");
            ui.selectable_value(&mut self.tab, Tab::Options, "Start options");
        });
        ui.separator();
        match self.tab {
            Tab::Shortcuts => self.shortcuts_tab(ui, ctx),
            Tab::Layout => self.layout_tab(ui),
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
        std::fs::write(
            dir.path().join("Site.url"),
            "[InternetShortcut]\r\nURL=https://example.com/\r\n",
        )
        .unwrap();
        let mut items = scan_dir(dir.path(), false);
        items.sort_by_key(|i| i.name.clone());
        assert_eq!(items.len(), 3);
        assert_eq!(
            (items[0].name.as_str(), items[0].broken, items[0].folder.as_str()),
            ("Gone", true, "")
        );
        assert_eq!(
            (items[1].name.as_str(), items[1].broken, items[1].folder.as_str()),
            ("Notepad", false, "Tools")
        );
        assert!(items[1].target.to_lowercase().ends_with("notepad.exe"));
        assert_eq!(items[2].target, "https://example.com/");
    }

    #[test]
    fn pin_any_wraps_non_programs() {
        let p = pin_any_spec(r#""C:\Tools\app.exe""#);
        assert_eq!(p.target, PathBuf::from(r"C:\Tools\app.exe"));
        let f = pin_any_spec(r"D:\Docs\Report.pdf");
        assert!(
            f.target
                .to_string_lossy()
                .to_lowercase()
                .ends_with("explorer.exe")
        );
        assert_eq!(f.args, r#""D:\Docs\Report.pdf""#);
        assert!(pin_any_spec("https://example.com").args.contains("https://"));
    }

    #[test]
    fn add_rename_and_undo_in_real_start_menu() {
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
        let name = format!("WM Test {}", std::process::id());
        let renamed = format!("{name} renamed");
        let root = programs_dirs()[0].0.clone();
        struct Cleanup(Vec<PathBuf>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for p in &self.0 {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        let _c = Cleanup(vec![
            root.join(format!("{name}.lnk")),
            root.join(format!("{renamed}.lnk")),
        ]);
        let lnk = add_item(&ctx, &name, r"C:\Windows").unwrap();
        let item = scan_dir(&root, false)
            .into_iter()
            .find(|i| i.path == lnk)
            .unwrap();
        assert!(!item.broken && item.target.to_lowercase().contains("explorer.exe"));
        rename(&ctx, &item, &renamed).unwrap();
        assert!(root.join(format!("{renamed}.lnk")).exists());
        assert!(
            add_item(&ctx, &renamed, r"C:\Windows").is_err(),
            "duplicates refused"
        );
        journal.undo(journal.entries()[0].id).unwrap();
        assert!(lnk.exists());
    }

    #[test]
    fn backup_contains_pins_and_layout() {
        let dir = tempfile::tempdir().unwrap();
        let b = backup_layout(dir.path()).unwrap();
        let (taskbar, start) = backup_contents(&b);
        assert!(taskbar, "taskbar pins saved");
        assert_eq!(
            start,
            LAYOUT_FILES.iter().any(|f| start_layout_dir().join(f).is_file())
        );
        assert_eq!(backup_contents(dir.path()), (false, false));
    }
}
