//! Smart Uninstaller window: sections for programs, updates, Windows apps and
//! leftover Program Files folders.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Context as _;
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Tool,
    egui::{self, Vec2},
    file_icon::{self, IconImage},
    icons,
    process::exe_from_command,
    registry,
    safety::UndoAction,
    shell::reveal_in_explorer,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, format_bytes, section_nav, show_message, status_bar, toolbar},
};

use super::orphans::{self, Orphan, References};
use super::{
    ExportFormat, Leftover, Program, Update, details, enumerate, export, find_leftovers, list_updates,
    lookup_url, onedrive_uninstall_command, remove_entry, remove_leftovers, remove_update, rename,
    run_command, uninstall_command, uninstall_onedrive,
};

const SECTIONS: [(&str, &str); 4] = [
    (icons::APP_WINDOW, "Programs"),
    (icons::DOWNLOAD, "Updates"),
    (icons::GRID_FOUR, "Windows apps"),
    (icons::FOLDERS, "Leftover folders"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Publisher,
    Version,
    Size,
    Installed,
    Scope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    List,
    Icons,
}

enum Ask {
    Uninstall(Vec<Program>),
    RemoveEntry(Box<Program>),
    RemoveUpdate(String),
    RecycleOrphans(Vec<PathBuf>),
    OneDrive(String),
}

/// Icons extracted on a background thread, by uninstall key.
type IconBatch = Vec<(String, Option<IconImage>)>;

/// Result of one uninstaller run: (program, silent, exit code or error).
type BatchResult = Vec<(Program, bool, Result<i32, String>)>;

pub struct SmartUninstaller {
    section: usize,
    programs: Option<Vec<Program>>,
    filter: String,
    sort: (SortKey, bool),
    mode: ViewMode,
    quiet: bool,
    selected: Option<String>,
    checked: HashSet<String>,
    batch: Option<BackgroundTask<BatchResult>>,
    onedrive: Option<BackgroundTask<i32>>,
    leftovers: Option<Vec<(String, Leftover, bool)>>,
    renaming: Option<(Program, String)>,
    details: Option<(String, Vec<(String, String)>)>,
    icon_task: Option<BackgroundTask<IconBatch>>,
    icons: HashMap<String, egui::TextureHandle>,
    /// Icons already requested (failures are not retried every frame).
    icon_tried: HashSet<String>,
    updates: Option<Result<Vec<Update>, String>>,
    updates_task: Option<BackgroundTask<Vec<Update>>>,
    update_remove: Option<BackgroundTask<String>>,
    orphans: Option<Vec<Orphan>>,
    orphans_task: Option<BackgroundTask<Vec<Orphan>>>,
    orphans_checked: HashSet<PathBuf>,
    apps: crate::apps::AppsUninstaller,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for SmartUninstaller {
    fn default() -> Self {
        Self {
            section: 0,
            programs: None,
            filter: String::new(),
            sort: (SortKey::Name, true),
            mode: ViewMode::List,
            quiet: false,
            selected: None,
            checked: HashSet::new(),
            batch: None,
            onedrive: None,
            leftovers: None,
            renaming: None,
            details: None,
            icon_task: None,
            icons: HashMap::new(),
            icon_tried: HashSet::new(),
            updates: None,
            updates_task: None,
            update_remove: None,
            orphans: None,
            orphans_task: None,
            orphans_checked: HashSet::new(),
            apps: Default::default(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

fn icon_source(p: &Program) -> Option<String> {
    if let Some(i) = &p.display_icon {
        return Some(i.clone());
    }
    // The uninstaller's own icon, unless it is a generic Windows host.
    let exe = exe_from_command(p.uninstall.as_deref()?)?;
    let lower = exe.to_ascii_lowercase();
    (!lower.contains("msiexec") && !lower.contains("rundll32") && !lower.contains("cmd.exe")).then_some(exe)
}

fn sorted<'a>(programs: &'a [Program], filter: &str, sort: (SortKey, bool)) -> Vec<&'a Program> {
    let f = filter.to_lowercase();
    let mut v: Vec<&Program> = programs
        .iter()
        .filter(|p| {
            f.is_empty() || p.name.to_lowercase().contains(&f) || p.publisher.to_lowercase().contains(&f)
        })
        .collect();
    let (key, asc) = sort;
    v.sort_by(|a, b| {
        let o = match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Publisher => a.publisher.to_lowercase().cmp(&b.publisher.to_lowercase()),
            SortKey::Version => a.version.cmp(&b.version),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Installed => a.installed.cmp(&b.installed),
            SortKey::Scope => a.per_user.cmp(&b.per_user),
        };
        if asc { o } else { o.reverse() }
    });
    v
}

/// A clickable column title; clicking again reverses the order.
fn sort_header(ui: &mut egui::Ui, label: &str, key: SortKey, sort: &mut (SortKey, bool)) {
    let arrow = match *sort {
        (k, true) if k == key => " ▲",
        (k, false) if k == key => " ▼",
        _ => "",
    };
    if ui
        .add(
            egui::Label::new(egui::RichText::new(format!("{label}{arrow}")).strong())
                .sense(egui::Sense::click()),
        )
        .clicked()
    {
        *sort = if sort.0 == key {
            (key, !sort.1)
        } else {
            // Sizes and dates read best largest / newest first.
            (key, !matches!(key, SortKey::Size | SortKey::Installed))
        };
    }
}

impl SmartUninstaller {
    fn reload(&mut self) {
        self.programs = None;
        self.details = None;
    }

    fn load_icons(&mut self, ctx: &egui::Context, programs: &[Program]) {
        if self.icon_task.is_some() {
            return;
        }
        let wanted: Vec<(String, String)> = programs
            .iter()
            .filter(|p| !self.icon_tried.contains(&p.key))
            .filter_map(|p| icon_source(p).map(|s| (p.key.clone(), s)))
            .collect();
        if wanted.is_empty() {
            return;
        }
        self.icon_tried.extend(wanted.iter().map(|(k, _)| k.clone()));
        self.icon_task = Some(BackgroundTask::spawn(ctx, move |_| {
            Ok(wanted
                .into_iter()
                .map(|(k, s)| (k, file_icon::extract_spec(&s, 32)))
                .collect())
        }));
    }

    fn poll_icons(&mut self, ctx: &egui::Context) {
        let Some(t) = &mut self.icon_task else { return };
        t.poll();
        let Some(Ok(list)) = t.take_result() else { return };
        self.icon_task = None;
        for (key, img) in list {
            let Some(img) = img else { continue };
            let image = egui::ColorImage::from_rgba_unmultiplied([img.size, img.size], &img.rgba);
            let tex = ctx.load_texture(format!("uninst-{key}"), image, egui::TextureOptions::LINEAR);
            self.icons.insert(key, tex);
        }
    }

    fn handle_confirm(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) else {
            return;
        };
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        match ask {
            Ask::Uninstall(list) => {
                for p in &list {
                    let _ = ctx.journal.record(
                        "uninstaller",
                        format!("Started uninstaller for {}", p.name),
                        UndoAction::None,
                    );
                }
                self.leftovers = None;
                let quiet = self.quiet;
                self.batch = Some(BackgroundTask::spawn(ui.ctx(), move |progress| {
                    let n = list.len();
                    Ok(list
                        .into_iter()
                        .enumerate()
                        .map(|(i, p)| {
                            progress.report(i as f32 / n as f32, format!("{} of {n}: {}", i + 1, p.name));
                            match uninstall_command(&p, quiet) {
                                Some((cmd, silent)) => {
                                    let r = run_command(&cmd).map_err(|e| format!("{e:#}"));
                                    (p, silent, r)
                                }
                                None => (p, false, Err("no uninstall command".into())),
                            }
                        })
                        .collect())
                }));
            }
            Ask::RemoveEntry(p) => {
                self.message = Some(
                    remove_entry(ctx, &p)
                        .map(|_| format!("Removed {} from the list.", p.name))
                        .map_err(|e| format!("{e:#}")),
                );
                self.checked.remove(&p.key);
                self.reload();
            }
            Ask::RemoveUpdate(id) => {
                let _ = ctx.journal.record(
                    "uninstaller",
                    format!("Requested removal of update {id}"),
                    UndoAction::None,
                );
                self.update_remove = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                    remove_update(&id)?;
                    Ok(format!("Removed {id}. Restart Windows to finish."))
                }));
            }
            Ask::RecycleOrphans(paths) => {
                let n = paths.len();
                self.message = Some(
                    ctx.journal
                        .recycle(
                            "uninstaller",
                            format!("Recycled {n} unused Program Files folders"),
                            paths,
                        )
                        .map(|_| format!("Moved {n} folders to the Recycle Bin."))
                        .map_err(|e| format!("{e:#}")),
                );
                self.orphans = None;
                self.orphans_checked.clear();
            }
            Ask::OneDrive(cmd) => {
                let _ = ctx.journal.record(
                    "uninstaller",
                    "Started the OneDrive uninstaller",
                    UndoAction::None,
                );
                self.onedrive = Some(BackgroundTask::spawn(ui.ctx(), move |_| uninstall_onedrive(&cmd)));
            }
        }
    }

    fn poll_batch(&mut self, ui: &mut egui::Ui) {
        if let Some(t) = &mut self.onedrive {
            t.poll();
            if let Some(r) = t.take_result() {
                self.onedrive = None;
                self.reload();
                self.message = Some(r.map(|c| format!("The OneDrive uninstaller exited (code {c}).")));
            }
        }
        let Some(t) = &mut self.batch else { return };
        t.poll();
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!(
                "Uninstalling {}… Follow any prompts the uninstaller shows.",
                t.status
            ));
        });
        let Some(r) = t.take_result() else { return };
        self.batch = None;
        let results = match r {
            Ok(r) => r,
            Err(e) => {
                self.message = Some(Err(e));
                return;
            }
        };
        // Programs that are gone now get a leftover scan right away.
        let still: HashSet<String> = enumerate().into_iter().map(|p| p.key).collect();
        let mut found = Vec::new();
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for (p, _, r) in &results {
            match r {
                Ok(_) if !still.contains(&p.key) => {
                    removed.push(p.name.clone());
                    for l in find_leftovers(p) {
                        found.push((p.name.clone(), l, true));
                    }
                }
                Ok(code) => kept.push(format!("{} (exit code {code}, still installed)", p.name)),
                Err(e) => kept.push(format!("{}: {e}", p.name)),
            }
            self.checked.remove(&p.key);
        }
        let mut text = format!("Uninstalled {}.", removed.len());
        if !kept.is_empty() {
            text.push_str(&format!(" Not removed: {}.", kept.join("; ")));
        }
        if !found.is_empty() {
            text.push_str(" Leftovers were found; review them below.");
        }
        self.message = Some(if kept.is_empty() { Ok(text) } else { Err(text) });
        if !found.is_empty() {
            self.leftovers = Some(found);
        }
        self.reload();
    }

    fn leftovers_panel(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(found) = &mut self.leftovers else { return };
        let mut close = false;
        let mut remove = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Leftovers");
                if ui.small_button("✕").clicked() {
                    close = true;
                }
            });
            if found.is_empty() {
                ui.label("No leftovers found.");
                return;
            }
            ui.weak("Folders go to the Recycle Bin; registry keys are backed up first. Untick anything you want to keep.");
            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for (program, item, mark) in found.iter_mut() {
                    let locked = item.needs_admin() && !ctx.elevated;
                    let label = format!("{}   ({program})", item.label());
                    ui.add_enabled(!locked, egui::Checkbox::new(mark, label))
                        .on_disabled_hover_text("Needs administrator rights");
                }
            });
            let chosen: Vec<(String, Leftover)> = found
                .iter()
                .filter(|(_, i, m)| *m && !(i.needs_admin() && !ctx.elevated))
                .map(|(p, i, _)| (p.clone(), i.clone()))
                .collect();
            if ui
                .add_enabled(!chosen.is_empty(), egui::Button::new(format!("Remove {} items", chosen.len())))
                .clicked()
            {
                remove = Some(chosen);
            }
        });
        if let Some(items) = remove {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                remove_leftovers(ctx, &items)
                    .map(|n| format!("Removed {n} leftovers."))
                    .map_err(|e| format!("{e:#}"))
            });
            close = true;
            self.reload();
        }
        if close {
            self.leftovers = None;
        }
    }

    fn rename_dialog(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some((p, name)) = &mut self.renaming else {
            return;
        };
        let mut done = None;
        egui::Window::new("Rename program")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(format!("New name for {}:", p.name));
                let r = ui.add(egui::TextEdit::singleline(name).desired_width(320.0));
                r.request_focus();
                ui.weak("Changes the name in this list and in Windows' Installed apps.");
                ui.horizontal(|ui| {
                    let ok = !name.trim().is_empty() && name.trim() != p.name;
                    if ui.add_enabled(ok, egui::Button::new("Rename")).clicked()
                        || (ok && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        done = Some(true);
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        done = Some(false);
                    }
                });
            });
        match done {
            Some(true) => {
                let (p, name) = self.renaming.take().expect("open");
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    rename(ctx, &p, name.trim())
                        .map(|_| format!("Renamed {} to {}.", p.name, name.trim()))
                        .map_err(|e| format!("{e:#}"))
                });
                self.reload();
            }
            Some(false) => self.renaming = None,
            None => {}
        }
    }

    fn programs_section(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let programs = self.programs.get_or_insert_with(enumerate).clone();
        self.load_icons(ui.ctx(), &programs);
        self.poll_icons(ui.ctx());
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| programs.iter().find(|p| &p.key == k))
            .cloned();
        let marked: Vec<Program> = programs
            .iter()
            .filter(|p| self.checked.contains(&p.key))
            .cloned()
            .collect();
        let targets: Vec<Program> = if marked.is_empty() {
            selected.iter().cloned().collect()
        } else {
            marked.clone()
        };
        let busy = self.batch.is_some() || self.onedrive.is_some();
        let writable = |p: &Program| ctx.elevated || !registry::needs_admin(&p.key);
        let sel_writable = selected.as_ref().is_some_and(writable);
        let uninstall_label = if marked.len() > 1 {
            format!("Uninstall {}", marked.len())
        } else {
            "Uninstall".to_owned()
        };

        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::TRASH, &uninstall_label).enabled(!busy && !targets.is_empty())),
                Some(ToolButton::new(icons::BROOM, "Leftovers").enabled(!busy && selected.is_some())),
                None,
                Some(ToolButton::new(icons::FILE_X, "Remove entry").enabled(!busy && sel_writable)),
                Some(ToolButton::new(icons::PENCIL_SIMPLE, "Rename").enabled(!busy && sel_writable)),
                Some(ToolButton::new(icons::INFO, "Details").enabled(selected.is_some())),
                None,
                Some(ToolButton::new(icons::GLOBE, "Look up").enabled(selected.is_some())),
                Some(
                    ToolButton::new(icons::FOLDER_OPEN, "Open folder").enabled(
                        selected
                            .as_ref()
                            .is_some_and(|p| p.install_location.as_ref().is_some_and(|l| l.exists())),
                    ),
                ),
                None,
                Some(ToolButton::new(icons::UPLOAD, "Export")),
                Some(ToolButton::new(icons::CLOUD_SLASH, "OneDrive").enabled(!busy)),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(!busy)),
            ],
        ) {
            Some(0) => {
                let missing: Vec<&str> = targets
                    .iter()
                    .filter(|p| p.uninstall.is_none())
                    .map(|p| p.name.as_str())
                    .collect();
                if !missing.is_empty() {
                    self.message = Some(Err(format!(
                        "No uninstaller registered for: {}. Use Remove entry if the program is already gone.",
                        missing.join(", ")
                    )));
                } else {
                    let names: Vec<&str> = targets.iter().map(|p| p.name.as_str()).collect();
                    let how = if self.quiet {
                        "Programs that support it are removed silently; the others show their own uninstaller."
                    } else {
                        "Each program's uninstaller opens in turn; follow its prompts."
                    };
                    self.confirm.ask(
                        format!(
                            "Uninstall {}? {how} Leftovers are looked for afterwards.",
                            names.join(", ")
                        ),
                        Ask::Uninstall(targets.clone()),
                    );
                }
            }
            Some(1) => {
                if let Some(p) = &selected {
                    let found: Vec<(String, Leftover, bool)> = find_leftovers(p)
                        .into_iter()
                        .map(|l| (p.name.clone(), l, true))
                        .collect();
                    if registry::open_read(&p.key).is_ok() {
                        self.message = Some(Err(format!(
                            "{} still appears to be installed. Remove leftovers only after uninstalling.",
                            p.name
                        )));
                    }
                    self.leftovers = Some(found);
                }
            }
            Some(3) => {
                if let Some(p) = &selected {
                    self.confirm.ask(
                        format!(
                            "Remove {} from the list without uninstalling it? Use this only when the program is \
                             already gone. The entry is backed up first.",
                            p.name
                        ),
                        Ask::RemoveEntry(Box::new(p.clone())),
                    );
                }
            }
            Some(4) => {
                if let Some(p) = &selected {
                    self.renaming = Some((p.clone(), p.name.clone()));
                }
            }
            Some(5) => {
                if let Some(p) = &selected {
                    self.details = Some((p.key.clone(), details(&p.key)));
                }
            }
            Some(7) => {
                if let Some(p) = &selected {
                    let url = p
                        .help_link
                        .clone()
                        .filter(|u| u.starts_with("http"))
                        .unwrap_or_else(|| lookup_url(p));
                    let _ = wm_core::shell::shell_open(&url, "");
                }
            }
            Some(8) => {
                if let Some(l) = selected.as_ref().and_then(|p| p.install_location.clone()) {
                    let _ = std::process::Command::new("explorer").arg(l).spawn();
                }
            }
            Some(10) => {
                let visible = sorted(&programs, &self.filter, self.sort);
                if let Some(file) = rfd::FileDialog::new()
                    .set_file_name("Installed programs.html")
                    .add_filter("Web page", &["html"])
                    .add_filter("CSV", &["csv"])
                    .save_file()
                {
                    let fmt = if file.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv")) {
                        ExportFormat::Csv
                    } else {
                        ExportFormat::Html
                    };
                    self.message = Some(
                        std::fs::write(&file, export(&visible, fmt))
                            .with_context(|| format!("writing {}", file.display()))
                            .map(|_| format!("Exported {} programs to {}.", visible.len(), file.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(11) => match onedrive_uninstall_command(&programs) {
                Some(cmd) => self.confirm.ask(
                    "Uninstall OneDrive? Files already synced to this PC stay in your OneDrive folder; \
                     they stop syncing. You can reinstall OneDrive from Microsoft later.",
                    Ask::OneDrive(cmd),
                ),
                None => self.message = Some(Ok("OneDrive is not installed.".into())),
            },
            Some(12) => {
                self.reload();
                self.icons.clear();
                self.icon_tried.clear();
            }
            _ => {}
        }

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            ui.separator();
            ui.selectable_value(&mut self.mode, ViewMode::List, format!("{} List", icons::LIST));
            ui.selectable_value(&mut self.mode, ViewMode::Icons, format!("{} Icons", icons::GRID_FOUR));
            ui.separator();
            ui.checkbox(&mut self.quiet, "Quiet uninstall when possible")
                .on_hover_text("Uses the program's silent mode or MSI's /qn. Programs without one still show their uninstaller.");
            if !self.checked.is_empty() && ui.small_button(format!("Clear {} ticks", self.checked.len())).clicked() {
                self.checked.clear();
            }
        });
        self.poll_batch(ui);
        show_message(ui, &self.message);
        self.leftovers_panel(ui, ctx);
        self.rename_dialog(ui, ctx);

        let visible = sorted(&programs, &self.filter, self.sort);
        let total: u64 = visible.iter().filter_map(|p| p.size).sum();
        let status = format!(
            "{} programs, {} estimated{}",
            visible.len(),
            format_bytes(total),
            if self.checked.is_empty() {
                String::new()
            } else {
                format!(", {} ticked", self.checked.len())
            }
        );
        egui::Panel::bottom("uninst-status")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| status_bar(ui, &status, |_| {}));
        if let Some((key, values)) = &self.details {
            let mut close = false;
            egui::Panel::bottom("uninst-details")
                .resizable(true)
                .default_size(170.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Details");
                        ui.weak(key);
                        if ui.small_button("Open in Registry Editor").clicked() {
                            let _ = registry::open_in_regedit(key);
                        }
                        if ui.small_button("✕").clicked() {
                            close = true;
                        }
                    });
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new("uninst-details-grid")
                            .num_columns(2)
                            .striped(true)
                            .show(ui, |ui| {
                                for (n, v) in values {
                                    ui.weak(n);
                                    ui.add(egui::Label::new(v).selectable(true).wrap());
                                    ui.end_row();
                                }
                            });
                    });
                });
            if close {
                self.details = None;
            }
        }

        let mut clicked = None;
        match self.mode {
            ViewMode::List => self.list_view(ui, &visible, &mut clicked),
            ViewMode::Icons => self.icon_view(ui, &visible, &mut clicked),
        }
        if let Some(k) = clicked {
            if self.details.is_some() {
                self.details = Some((k.clone(), details(&k)));
            }
            self.selected = Some(k);
        }
    }

    fn list_view(&mut self, ui: &mut egui::Ui, visible: &[&Program], clicked: &mut Option<String>) {
        let mut toggles = Vec::new();
        let headers = [
            ("Name", SortKey::Name),
            ("Publisher", SortKey::Publisher),
            ("Version", SortKey::Version),
            ("Size", SortKey::Size),
            ("Installed", SortKey::Installed),
            ("For", SortKey::Scope),
        ];
        let mut sort = self.sort;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(22.0))
            .column(Column::exact(20.0))
            .column(Column::initial(240.0).clip(true))
            .column(Column::initial(160.0).clip(true))
            .column(Column::initial(95.0).clip(true))
            .column(Column::initial(75.0))
            .column(Column::initial(85.0))
            .column(Column::remainder().at_least(70.0).clip(true))
            .header(22.0, |mut h| {
                h.col(|_| {});
                h.col(|_| {});
                for (label, key) in headers {
                    h.col(|ui| sort_header(ui, label, key, &mut sort));
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let p = visible[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(p.key.as_str()));
                    row.col(|ui| {
                        let mut on = self.checked.contains(&p.key);
                        if ui.checkbox(&mut on, "").changed() {
                            toggles.push(p.key.clone());
                        }
                    });
                    row.col(|ui| {
                        if let Some(t) = self.icons.get(&p.key) {
                            ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::splat(16.0)));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&p.name);
                    });
                    row.col(|ui| {
                        ui.weak(&p.publisher);
                    });
                    row.col(|ui| {
                        ui.weak(&p.version);
                    });
                    row.col(|ui| {
                        ui.label(p.size.map(format_bytes).unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.weak(&p.installed);
                    });
                    row.col(|ui| {
                        ui.weak(if p.per_user { "You" } else { "All users" });
                    });
                    if row.response().clicked() {
                        *clicked = Some(p.key.clone());
                    }
                });
            });
        self.sort = sort;
        for k in toggles {
            if !self.checked.remove(&k) {
                self.checked.insert(k);
            }
        }
    }

    fn icon_view(&mut self, ui: &mut egui::Ui, visible: &[&Program], clicked: &mut Option<String>) {
        ui.horizontal(|ui| {
            ui.weak("Sort by:");
            for (label, key) in [
                ("Name", SortKey::Name),
                ("Publisher", SortKey::Publisher),
                ("Size", SortKey::Size),
                ("Installed", SortKey::Installed),
            ] {
                sort_header(ui, label, key, &mut self.sort);
            }
            ui.separator();
            ui.weak("Ctrl+click ticks programs for a batch uninstall.");
        });
        let tile = Vec2::new(116.0, 92.0);
        let accent = wm_core::ui::ACCENT;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(6.0);
                for p in visible {
                    let (rect, resp) = ui.allocate_exact_size(tile, egui::Sense::click());
                    let sel = self.selected.as_deref() == Some(p.key.as_str());
                    let ticked = self.checked.contains(&p.key);
                    let painter = ui.painter_at(rect);
                    if sel {
                        painter.rect_filled(rect, 4.0, accent.gamma_multiply(0.25));
                    } else if resp.hovered() {
                        painter.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
                    }
                    if ticked {
                        painter.rect_stroke(
                            rect.shrink(1.0),
                            4.0,
                            egui::Stroke::new(2.0, accent),
                            egui::StrokeKind::Inside,
                        );
                    }
                    let icon_rect = egui::Rect::from_center_size(
                        rect.center_top() + Vec2::new(0.0, 24.0),
                        Vec2::splat(32.0),
                    );
                    match self.icons.get(&p.key) {
                        Some(t) => {
                            egui::Image::new(t).paint_at(ui, icon_rect);
                        }
                        None => {
                            painter.text(
                                icon_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                icons::APP_WINDOW,
                                egui::FontId::proportional(28.0),
                                ui.visuals().weak_text_color(),
                            );
                        }
                    }
                    let galley = ui.painter().layout(
                        p.name.clone(),
                        egui::FontId::proportional(11.5),
                        ui.visuals().text_color(),
                        tile.x - 8.0,
                    );
                    let text_pos = egui::pos2(rect.center().x - galley.size().x / 2.0, rect.top() + 46.0);
                    painter.with_clip_rect(rect.shrink(2.0)).galley(
                        text_pos,
                        galley,
                        ui.visuals().text_color(),
                    );
                    let resp = resp.on_hover_text(format!(
                        "{}\n{}{}",
                        p.name,
                        p.publisher,
                        p.size
                            .map(|s| format!("\n{}", format_bytes(s)))
                            .unwrap_or_default()
                    ));
                    if resp.clicked() {
                        if ui.input(|i| i.modifiers.ctrl) {
                            if !self.checked.remove(&p.key) {
                                self.checked.insert(p.key.clone());
                            }
                        } else {
                            *clicked = Some(p.key.clone());
                        }
                    }
                }
            });
        });
    }

    fn updates_section(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.updates.is_none() && self.updates_task.is_none() {
            self.updates_task = Some(BackgroundTask::spawn(ui.ctx(), |_| list_updates()));
        }
        if let Some(t) = &mut self.updates_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.updates = Some(r);
                self.updates_task = None;
            }
        }
        if let Some(t) = &mut self.update_remove {
            t.poll();
            if let Some(r) = t.take_result() {
                self.message = Some(r);
                self.update_remove = None;
                self.updates = None;
            }
        }
        let removing = self.update_remove.is_some();
        ui.horizontal(|ui| {
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.updates = None;
            }
            if ui.button("Open Windows Update history").clicked() {
                let _ = wm_core::shell::shell_open("ms-settings:windowsupdate-history", "");
            }
            if removing {
                ui.spinner();
                ui.label("Removing the update…");
            }
        });
        ui.weak(
            "Removing an update needs administrator rights. Some updates (servicing stack, many cumulative \
             updates) cannot be removed on their own.",
        );
        show_message(ui, &self.message);
        let list = match &self.updates {
            Some(Ok(l)) => l.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => {
                ui.spinner();
                return;
            }
        };
        let mut remove = None;
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::initial(120.0))
            .column(Column::initial(200.0))
            .column(Column::initial(100.0))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in ["Update", "Type", "Installed", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, list.len(), |mut row| {
                    let u = &list[row.index()];
                    row.col(|ui| {
                        if ui.link(&u.id).on_hover_text("Open Microsoft's article").clicked() {
                            let digits: String = u.id.chars().filter(char::is_ascii_digit).collect();
                            let _ = wm_core::shell::shell_open(
                                &format!("https://support.microsoft.com/help/{digits}"),
                                "",
                            );
                        }
                    });
                    row.col(|ui| {
                        ui.weak(&u.kind);
                    });
                    row.col(|ui| {
                        ui.weak(&u.installed);
                    });
                    row.col(|ui| {
                        if ui
                            .add_enabled(ctx.elevated && !removing, egui::Button::new("Uninstall…").small())
                            .clicked()
                        {
                            remove = Some(u.id.clone());
                        }
                    });
                });
            });
        if let Some(id) = remove {
            self.confirm.ask(
                format!("Uninstall update {id}? Windows needs a restart to finish."),
                Ask::RemoveUpdate(id),
            );
        }
    }

    fn orphans_section(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.orphans_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.orphans_task = None;
                match r {
                    Ok(o) => self.orphans = Some(o),
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        ui.label(
            "Folders in Program Files that no installed program, service, startup entry or App Path refers \
             to, and whose name matches no installed program. They are often left behind by uninstallers, \
             but check each one before removing it.",
        );
        let busy = self.orphans_task.is_some();
        let chosen: Vec<PathBuf> = self
            .orphans
            .iter()
            .flatten()
            .filter(|o| self.orphans_checked.contains(&o.path))
            .map(|o| o.path.clone())
            .collect();
        ui.horizontal(|ui| {
            let label = if self.orphans.is_some() { "Scan again" } else { "Scan" };
            if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                self.orphans_checked.clear();
                self.orphans_task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                    Ok(orphans::find(&orphans::program_files_roots(), &References::collect()))
                }));
            }
            if ui
                .add_enabled(
                    ctx.elevated && !chosen.is_empty(),
                    egui::Button::new(format!("Move {} to Recycle Bin…", chosen.len())),
                )
                .on_disabled_hover_text(if ctx.elevated {
                    "Tick the folders to remove"
                } else {
                    "Needs administrator rights"
                })
                .clicked()
            {
                self.confirm.ask(
                    format!(
                        "Move {} folders to the Recycle Bin? Programs that still use them would stop working.",
                        chosen.len()
                    ),
                    Ask::RecycleOrphans(chosen.clone()),
                );
            }
            if busy {
                ui.spinner();
                ui.label("Scanning…");
            }
        });
        show_message(ui, &self.message);
        let Some(list) = self.orphans.clone() else { return };
        if list.is_empty() {
            ui.label("No unused folders found.");
            return;
        }
        let mut toggles = Vec::new();
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(22.0))
            .column(Column::initial(420.0).clip(true))
            .column(Column::initial(90.0))
            .column(Column::initial(70.0))
            .column(Column::initial(250.0))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in ["", "Folder", "Size", "Files", "Note", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, list.len(), |mut row| {
                    let o = &list[row.index()];
                    row.col(|ui| {
                        let mut on = self.orphans_checked.contains(&o.path);
                        if ui.checkbox(&mut on, "").changed() {
                            toggles.push(o.path.clone());
                        }
                    });
                    row.col(|ui| {
                        ui.label(o.path.display().to_string());
                    });
                    row.col(|ui| {
                        ui.label(format_bytes(o.size));
                    });
                    row.col(|ui| {
                        ui.label(if o.files == 0 {
                            "empty".into()
                        } else {
                            o.files.to_string()
                        });
                    });
                    row.col(|ui| {
                        if o.has_programs {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                "Contains programs: may be a portable app",
                            );
                        } else if o.files == 0 {
                            ui.weak("Empty folder");
                        }
                    });
                    row.col(|ui| {
                        if ui.small_button("Explore").clicked() {
                            reveal_in_explorer(&o.path);
                        }
                    });
                });
            });
        for p in toggles {
            if !self.orphans_checked.remove(&p) {
                self.orphans_checked.insert(p);
            }
        }
    }
}

impl Tool for SmartUninstaller {
    fn id(&self) -> &'static str {
        "uninstaller"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.handle_confirm(ui, ctx);
        egui::Panel::left("uninst-sections")
            .resizable(false)
            .exact_size(170.0)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                right: 8,
                ..Default::default()
            }))
            .show(ui, |ui| {
                if section_nav(ui, &SECTIONS, &mut self.section) {
                    self.message = None;
                }
            });
        match self.section {
            0 => self.programs_section(ui, ctx),
            1 => self.updates_section(ui, ctx),
            2 => self.apps.ui(ui, ctx),
            _ => self.orphans_section(ui, ctx),
        }
    }
}
