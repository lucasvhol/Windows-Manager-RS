//! Start Menu Manager: the "All apps" shortcuts (yours and all users') as a folder
//! tree with owner, availability and target; add, "pin any" (files, folders and web
//! addresses as apps), rename, launch, remove; back up and restore the taskbar pins
//! and the Start layout; and Start menu options.
//!
//! Pinning to the taskbar or Start cannot be done for another program on current
//! Windows, so "Pin any" creates an All apps entry that can be pinned from Start.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    icons, paths,
    process::{hidden_command, restart_explorer, run_checked},
    safety::UndoAction,
    shell::{
        ShortcutSpec, create_shortcut, is_broken_shortcut, read_shortcut, reveal_in_explorer, shell_open,
    },
    tweaks::TweakPanel,
    ui::{Confirm, Message, frame, show_message},
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
    /// Ticked shortcuts (Remove takes these; unavailable ones start ticked).
    checked: HashSet<PathBuf>,
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
            checked: HashSet::new(),
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
        let fresh = self.items.is_none();
        let items = self.items.get_or_insert_with(enumerate).clone();
        let usable = |i: &StartItem| !i.all_users || ctx.elevated;
        if fresh {
            // Like the original: shortcuts whose program is gone come ticked.
            self.checked = items
                .iter()
                .filter(|i| i.broken && usable(i))
                .map(|i| i.path.clone())
                .collect();
        }
        let selected = self
            .selected
            .as_ref()
            .and_then(|p| items.iter().find(|i| &i.path == p))
            .cloned();
        let ticked: Vec<PathBuf> = items
            .iter()
            .filter(|i| self.checked.contains(&i.path) && usable(i))
            .map(|i| i.path.clone())
            .collect();
        let targets: Vec<PathBuf> = if ticked.is_empty() {
            selected
                .iter()
                .filter(|i| usable(i))
                .map(|i| i.path.clone())
                .collect()
        } else {
            ticked
        };
        let can_edit = selected.as_ref().is_some_and(usable);
        let broken_n = items.iter().filter(|i| i.broken && usable(i)).count();

        // The original's action column (Pin to Taskbar is left out: Windows only lets
        // people pin from Start itself).
        let mut act = None;
        frame::right(ui, 210.0, |ui| {
            if frame::action(ui, icons::PENCIL_SIMPLE, "Rename", can_edit) {
                act = Some("rename");
            }
            let remove = if targets.len() > 1 {
                format!("Remove ({})", targets.len())
            } else {
                "Remove".to_owned()
            };
            if frame::action(ui, icons::X, &remove, !targets.is_empty()) {
                act = Some("remove");
            }
            if frame::action(ui, icons::FOLDER_OPEN, "Open Folder", selected.is_some()) {
                act = Some("folder");
            }
            if frame::action(
                ui,
                icons::PLAY,
                "Launch",
                selected.as_ref().is_some_and(|s| !s.broken),
            ) {
                act = Some("launch");
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  Search", icons::MAGNIFYING_GLASS))
                    .desired_width(ui.available_width()),
            );
            ui.checkbox(&mut self.broken_only, "Only unavailable");
            ui.separator();
            if frame::action(ui, icons::PLUS, "Add Program", true) {
                act = Some("add");
            }
            if frame::action(ui, icons::PUSH_PIN, "Add to All apps…", true) {
                act = Some("pinany");
            }
            if frame::action(
                ui,
                icons::BROOM,
                &format!("Tick unavailable ({broken_n})"),
                broken_n > 0,
            ) {
                act = Some("tickbroken");
            }
            if frame::action(ui, icons::ARROWS_CLOCKWISE, "Refresh", true) {
                act = Some("refresh");
            }
            ui.separator();
            if frame::action(ui, icons::FLOPPY_DISK, "Taskbar and Start Backup", true) {
                act = Some("backup");
            }
            if frame::action(ui, icons::SLIDERS, "Start Options", true) {
                act = Some("options");
            }
        });
        match act {
            Some("add") => {
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
            Some("pinany") => self.pin_any = Some((String::new(), String::new())),
            Some("launch") => {
                if let Some(s) = &selected {
                    let _ = shell_open(&s.path.display().to_string(), "");
                }
            }
            Some("rename") => {
                if let Some(s) = &selected {
                    self.renaming = Some((s.clone(), s.name.clone()));
                }
            }
            Some("folder") => {
                if let Some(s) = &selected {
                    reveal_in_explorer(&s.path);
                }
            }
            Some("remove") => self.confirm.ask(
                format!("Move {} Start menu shortcuts to the Recycle Bin?", targets.len()),
                Ask::Remove(targets.clone()),
            ),
            Some("tickbroken") => {
                self.checked = items
                    .iter()
                    .filter(|i| i.broken && usable(i))
                    .map(|i| i.path.clone())
                    .collect();
            }
            Some("refresh") => self.items = None,
            Some("backup") => self.tab = Tab::Layout,
            Some("options") => self.tab = Tab::Options,
            _ => {}
        }
        let status = format!("Total {}", items.len());
        let ticked_n = self.checked.len();
        frame::status(ui, &status, |ui| {
            if ticked_n > 0 {
                ui.weak(format!("{ticked_n} ticked"));
            }
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
        let mut toggles = Vec::new();
        let elevated = ctx.elevated;
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            let red = ui.visuals().error_fg_color;
            // Column titles, lined up with the tables below (24 + 300 + 90 px).
            let (head, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
            let strong = ui.visuals().strong_text_color();
            for (x, t) in [(40.0, "Name"), (344.0, "User"), (434.0, "Available")] {
                ui.painter().text(
                    head.left_center() + egui::vec2(x, 0.0),
                    egui::Align2::LEFT_CENTER,
                    t,
                    egui::FontId::proportional(13.5),
                    strong,
                );
            }
            ui.separator();
            // Shortcuts at the top level first, then one foldable entry per folder.
            for folder in folders {
                let rows: Vec<&&StartItem> = visible.iter().filter(|i| i.folder == folder).collect();
                let table = |ui: &mut egui::Ui| {
                    TableBuilder::new(ui)
                        .id_salt(folder)
                        .striped(true)
                        .sense(egui::Sense::click())
                        .vscroll(false)
                        .column(Column::exact(24.0))
                        .column(Column::initial(300.0).clip(true))
                        .column(Column::initial(90.0))
                        .column(Column::remainder())
                        .body(|mut body| {
                            for it in rows {
                                body.row(21.0, |mut row| {
                                    row.set_selected(self.selected.as_ref() == Some(&it.path));
                                    row.col(|ui| {
                                        let mut on = self.checked.contains(&it.path);
                                        if ui
                                            .add_enabled(
                                                !it.all_users || elevated,
                                                egui::Checkbox::without_text(&mut on),
                                            )
                                            .on_disabled_hover_text("Needs administrator rights")
                                            .changed()
                                        {
                                            toggles.push(it.path.clone());
                                        }
                                    });
                                    row.col(|ui| {
                                        let text = format!("{}  {}", icons::APP_WINDOW, it.name);
                                        let r = if it.broken {
                                            ui.colored_label(red, text)
                                        } else {
                                            ui.label(text)
                                        };
                                        r.on_hover_text(&it.target);
                                    });
                                    row.col(|ui| {
                                        ui.label(if it.all_users { "All Users" } else { "Current User" });
                                    });
                                    row.col(|ui| {
                                        if it.broken {
                                            ui.colored_label(red, "No")
                                                .on_hover_text("The program it opens no longer exists");
                                        }
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
                };
                if folder.is_empty() {
                    table(ui);
                } else {
                    egui::CollapsingHeader::new(format!("{}  {folder}", icons::FOLDER_OPEN))
                        .id_salt(folder)
                        .default_open(false)
                        .show(ui, table);
                }
            }
        });
        for p in toggles {
            if !self.checked.remove(&p) {
                self.checked.insert(p);
            }
        }
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
        if self.tab != Tab::Shortcuts {
            // Backup and Start options open from the action column; this strip goes back.
            let mut back = false;
            egui::Panel::top("startmenu-back")
                .resizable(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        back = ui.button(format!("{}  Start menu", icons::ARROW_UP)).clicked();
                        ui.label("›");
                        ui.strong(if self.tab == Tab::Layout {
                            "Taskbar and Start backup"
                        } else {
                            "Start options"
                        });
                    });
                });
            if back {
                self.tab = Tab::Shortcuts;
                self.message = None;
                return;
            }
        }
        match self.tab {
            Tab::Shortcuts => self.shortcuts_tab(ui, ctx),
            Tab::Layout => frame::central(ui, |ui| {
                show_message(ui, &self.message);
                self.layout_tab(ui);
            }),
            Tab::Options => frame::nested(ui, |ui| self.panel.ui_framed(ui, ctx)),
        }
    }
    fn layout(&self) -> Layout {
        Layout::Fill
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
