//! Smart Uninstaller window, laid out like the original: the installed programs as
//! a grouped icon grid (or a list), with Hotfix, Windows Apps and "Find unavailable
//! program folders" opening their own pages from the toolbar.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Context as _;
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    file_icon::{self, IconImage},
    icons,
    process::exe_from_command,
    registry,
    safety::UndoAction,
    shell::reveal_in_explorer,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, empty_state, format_bytes, frame, show_message},
};

use super::orphans::{self, Orphan, References};
use super::{
    ExportFormat, Leftover, Program, Update, details, enumerate, export, find_leftovers, list_updates,
    lookup_url, onedrive_uninstall_command, remove_entry, remove_leftovers, remove_update, rename,
    run_command, uninstall_command, uninstall_onedrive,
};

/// Pages other than the program list, with their title in the back strip.
const PAGES: [&str; 4] = ["", "Hotfix", "Windows Apps", "Unavailable program folders"];

/// Programs installed this many days ago or less are in the "Recently Installed" group.
const RECENT_DAYS: i64 = 30;

/// True when an install date ("YYYY-MM-DD") is within [`RECENT_DAYS`] of `today`.
fn is_recent(installed: &str, today: chrono::NaiveDate) -> bool {
    chrono::NaiveDate::parse_from_str(installed, "%Y-%m-%d")
        .is_ok_and(|d| (today - d).num_days() <= RECENT_DAYS && d <= today)
}

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
    /// Batch Uninstall is on: programs get tick boxes and Uninstall takes the ticked ones.
    batch_mode: bool,
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
            mode: ViewMode::Icons,
            batch_mode: false,
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

const SORT_KEYS: [SortKey; 6] = [
    SortKey::Name,
    SortKey::Publisher,
    SortKey::Version,
    SortKey::Size,
    SortKey::Installed,
    SortKey::Scope,
];

fn sort_label(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "Name",
        SortKey::Publisher => "Publisher",
        SortKey::Version => "Version",
        SortKey::Size => "Size",
        SortKey::Installed => "Install date",
        SortKey::Scope => "Installed for",
    }
}

/// Sizes and dates read best largest / newest first.
fn default_ascending(key: SortKey) -> bool {
    !matches!(key, SortKey::Size | SortKey::Installed)
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
            (key, default_ascending(key))
        };
    }
}

impl SmartUninstaller {
    fn open_page(&mut self, page: usize) {
        self.section = page;
        self.message = None;
    }

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
        let uninstall_label = if self.batch_mode && marked.len() > 1 {
            format!("Uninstall {}", marked.len())
        } else {
            "Uninstall".to_owned()
        };
        let batch_label = if self.batch_mode {
            "Batch Uninstall ✓"
        } else {
            "Batch Uninstall"
        };

        // The original's toolbar. Its View, Sort and Options drop-downs are in the strip
        // below, next to the original's second row (unavailable folders, Features,
        // Uninstall OneDrive, Export and the search box).
        let clicked_tool = frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(!busy)),
                Some(ToolButton::new(icons::LIST_CHECKS, batch_label).enabled(!busy)),
                None,
                Some(ToolButton::new(icons::TRASH, &uninstall_label).enabled(!busy && !targets.is_empty())),
                Some(ToolButton::new(icons::FILE_X, "Remove Entry").enabled(!busy && sel_writable)),
                Some(ToolButton::new(icons::PENCIL_SIMPLE, "Rename").enabled(!busy && sel_writable)),
                Some(ToolButton::new(icons::CLIPBOARD_TEXT, "Details").enabled(selected.is_some())),
                None,
                Some(ToolButton::new(icons::GLOBE, "Query").enabled(selected.is_some())),
                Some(
                    ToolButton::new(icons::FOLDER_OPEN, "Open Folder").enabled(
                        selected
                            .as_ref()
                            .is_some_and(|p| p.install_location.as_ref().is_some_and(|l| l.exists())),
                    ),
                ),
                Some(ToolButton::new(icons::BROOM, "Leftovers").enabled(!busy && selected.is_some())),
                None,
                Some(ToolButton::new(icons::WRENCH, "Hotfix")),
                Some(ToolButton::new(icons::GRID_FOUR, "Windows Apps")),
                None,
                Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
            ],
            |_| {},
        );
        match clicked_tool {
            Some(0) => {
                self.reload();
                self.icons.clear();
                self.icon_tried.clear();
            }
            Some(1) => {
                self.batch_mode = !self.batch_mode;
                if !self.batch_mode {
                    self.checked.clear();
                }
            }
            Some(3) => {
                let missing: Vec<&str> = targets
                    .iter()
                    .filter(|p| p.uninstall.is_none())
                    .map(|p| p.name.as_str())
                    .collect();
                if !missing.is_empty() {
                    self.message = Some(Err(format!(
                        "No uninstaller registered for: {}. Use Remove Entry if the program is already gone.",
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
            Some(4) => {
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
            Some(5) => {
                if let Some(p) = &selected {
                    self.renaming = Some((p.clone(), p.name.clone()));
                }
            }
            Some(6) => {
                if let Some(p) = &selected {
                    self.details = Some((p.key.clone(), details(&p.key)));
                }
            }
            Some(8) => {
                if let Some(p) = &selected {
                    let url = p
                        .help_link
                        .clone()
                        .filter(|u| u.starts_with("http"))
                        .unwrap_or_else(|| lookup_url(p));
                    let _ = wm_core::shell::shell_open(&url, "");
                }
            }
            Some(9) => {
                if let Some(l) = selected.as_ref().and_then(|p| p.install_location.clone()) {
                    let _ = std::process::Command::new("explorer").arg(l).spawn();
                }
            }
            Some(10) => {
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
            Some(12) => self.open_page(1),
            Some(13) => self.open_page(2),
            Some(15) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }

        let filter_hint = "Search";
        let mut second = None;
        frame::options(ui, |ui| {
            if ui
                .button(format!(
                    "{}  Find unavailable program folders",
                    icons::MAGNIFYING_GLASS
                ))
                .clicked()
            {
                second = Some(0);
            }
            if ui
                .button(format!("{}  Features", icons::SLIDERS))
                .on_hover_text("Turn Windows features on or off")
                .clicked()
            {
                second = Some(1);
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(format!("{}  Uninstall OneDrive", icons::CLOUD_SLASH)),
                )
                .clicked()
            {
                second = Some(2);
            }
            if ui.button(format!("{}  Export", icons::UPLOAD)).clicked() {
                second = Some(3);
            }
            ui.separator();
            ui.label("View:");
            egui::ComboBox::from_id_salt("uninst-view")
                .selected_text(match self.mode {
                    ViewMode::Icons => "Large Icons",
                    ViewMode::List => "Details",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.mode, ViewMode::Icons, "Large Icons");
                    ui.selectable_value(&mut self.mode, ViewMode::List, "Details");
                });
            ui.label("Sort:");
            egui::ComboBox::from_id_salt("uninst-sort")
                .selected_text(sort_label(self.sort.0))
                .show_ui(ui, |ui| {
                    for key in SORT_KEYS {
                        if ui.selectable_label(self.sort.0 == key, sort_label(key)).clicked() {
                            self.sort = (key, default_ascending(key));
                        }
                    }
                });
            if ui
                .small_button(if self.sort.1 { "▲" } else { "▼" })
                .on_hover_text("Reverse the order")
                .clicked()
            {
                self.sort.1 = !self.sort.1;
            }
            ui.separator();
            ui.checkbox(&mut self.quiet, "Quiet uninstall when possible")
                .on_hover_text("Uses the program's silent mode or MSI's /qn. Programs without one still show their uninstaller.");
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  {filter_hint}", icons::MAGNIFYING_GLASS))
                    .desired_width(200.0),
            );
        });
        match second {
            Some(0) => self.open_page(3),
            Some(1) => wm_core::ui::request_open_tool(ui.ctx(), "winfeatures"),
            Some(2) => match onedrive_uninstall_command(&programs) {
                Some(cmd) => self.confirm.ask(
                    "Uninstall OneDrive? Files already synced to this PC stay in your OneDrive folder; \
                     they stop syncing. You can reinstall OneDrive from Microsoft later.",
                    Ask::OneDrive(cmd),
                ),
                None => self.message = Some(Ok("OneDrive is not installed.".into())),
            },
            Some(3) => {
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
            _ => {}
        }
        self.rename_dialog(ui, ctx);

        let visible = sorted(&programs, &self.filter, self.sort);
        let total: u64 = visible.iter().filter_map(|p| p.size).sum();
        let status = format!(
            "Total installed programs: {}; Total size: {}{}",
            visible.len(),
            format_bytes(total),
            if self.checked.is_empty() {
                String::new()
            } else {
                format!("; {} ticked", self.checked.len())
            }
        );
        frame::status(ui, &status, |ui| {
            if busy {
                ui.spinner();
            }
        });
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
        frame::central_fixed(ui, |ui| {
            self.poll_batch(ui);
            show_message(ui, &self.message);
            self.leftovers_panel(ui, ctx);
            match self.mode {
                ViewMode::List => self.list_view(ui, &visible, &mut clicked),
                ViewMode::Icons => self.icon_view(ui, &visible, &mut clicked),
            }
        });
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
        let batch = self.batch_mode;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        let today = chrono::Local::now().date_naive();
        let recent_color = ui.visuals().error_fg_color;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(if batch { 22.0 } else { 0.0 }))
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
                        if batch && ui.checkbox(&mut on, "").changed() {
                            toggles.push(p.key.clone());
                        }
                    });
                    row.col(|ui| {
                        if let Some(t) = self.icons.get(&p.key) {
                            ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::splat(16.0)));
                        }
                    });
                    row.col(|ui| {
                        if is_recent(&p.installed, today) {
                            ui.colored_label(recent_color, &p.name)
                                .on_hover_text("Recently installed");
                        } else {
                            ui.label(&p.name);
                        }
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

    /// The original's large-icon view: "Recently Installed" first (names on red, as
    /// there), then the other programs. In batch mode a click ticks a program.
    fn icon_view(&mut self, ui: &mut egui::Ui, visible: &[&Program], clicked: &mut Option<String>) {
        let today = chrono::Local::now().date_naive();
        let (recent, rest): (Vec<&Program>, Vec<&Program>) = visible
            .iter()
            .copied()
            .partition(|p| is_recent(&p.installed, today));
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (title, group, is_new) in [
                ("Recently Installed", &recent, true),
                ("Installed Programs", &rest, false),
            ] {
                if group.is_empty() {
                    continue;
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(title).color(wm_core::ui::ACCENT));
                    let y = ui.min_rect().center().y;
                    let x = ui.cursor().left() + 6.0;
                    ui.painter().hline(
                        x..=ui.max_rect().right(),
                        y,
                        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                    );
                });
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(4.0, 6.0);
                    for p in group.iter() {
                        self.icon_tile(ui, p, is_new, clicked);
                    }
                });
                ui.add_space(6.0);
            }
        });
    }

    fn icon_tile(&mut self, ui: &mut egui::Ui, p: &Program, is_new: bool, clicked: &mut Option<String>) {
        let tile = Vec2::new(84.0, 76.0);
        let accent = wm_core::ui::ACCENT;
        let (rect, resp) = ui.allocate_exact_size(tile, egui::Sense::click());
        let sel = self.selected.as_deref() == Some(p.key.as_str());
        let ticked = self.checked.contains(&p.key);
        let painter = ui.painter_at(rect);
        if sel {
            painter.rect_filled(rect, 3.0, accent.gamma_multiply(0.25));
        } else if resp.hovered() {
            painter.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        if ticked {
            painter.rect_stroke(
                rect.shrink(1.0),
                3.0,
                egui::Stroke::new(2.0, accent),
                egui::StrokeKind::Inside,
            );
        }
        let icon_rect =
            egui::Rect::from_center_size(rect.center_top() + Vec2::new(0.0, 20.0), Vec2::splat(32.0));
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
        if self.batch_mode {
            let box_rect = egui::Rect::from_min_size(rect.min + Vec2::splat(3.0), Vec2::splat(13.0));
            painter.rect_stroke(
                box_rect,
                2.0,
                egui::Stroke::new(1.0, ui.visuals().text_color()),
                egui::StrokeKind::Inside,
            );
            if ticked {
                painter.text(
                    box_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    icons::CHECK,
                    egui::FontId::proportional(11.0),
                    accent,
                );
            }
        }
        // The name: two lines at most, on red for recent installs like the original.
        let text_color = if is_new {
            egui::Color32::WHITE
        } else {
            ui.visuals().text_color()
        };
        let mut job = egui::text::LayoutJob::simple(
            p.name.clone(),
            egui::FontId::proportional(11.5),
            text_color,
            tile.x - 4.0,
        );
        job.wrap.max_rows = 2;
        job.halign = egui::Align::Center;
        let galley = ui.painter().layout_job(job);
        let text_rect = egui::Rect::from_center_size(
            egui::pos2(rect.center().x, rect.top() + 40.0 + galley.size().y / 2.0),
            galley.size(),
        );
        if is_new {
            painter.rect_filled(
                egui::Rect::from_center_size(
                    text_rect.center(),
                    Vec2::new(tile.x - 2.0, galley.size().y + 2.0),
                ),
                0.0,
                egui::Color32::from_rgb(200, 40, 40),
            );
        }
        painter.galley(egui::pos2(rect.center().x, text_rect.top()), galley, text_color);
        let resp = resp.on_hover_text(format!(
            "{}\n{}{}{}",
            p.name,
            p.publisher,
            p.size
                .map(|s| format!("\n{}", format_bytes(s)))
                .unwrap_or_default(),
            if p.installed.is_empty() {
                String::new()
            } else {
                format!("\nInstalled {}", p.installed)
            }
        ));
        if resp.clicked() {
            if self.batch_mode || ui.input(|i| i.modifiers.ctrl) {
                if !self.checked.remove(&p.key) {
                    self.checked.insert(p.key.clone());
                }
                self.batch_mode = true;
            }
            *clicked = Some(p.key.clone());
        }
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
        let refresh = ctx.i18n.tr("common-refresh");
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, &refresh).enabled(!removing)),
                Some(ToolButton::new(icons::CLOCK_COUNTER_CLOCKWISE, "Update history")),
            ],
            |_| {},
        ) {
            Some(0) => self.updates = None,
            Some(1) => {
                let _ = wm_core::shell::shell_open("ms-settings:windowsupdate-history", "");
            }
            _ => {}
        }
        let status = match (&self.updates, removing) {
            (_, true) => "Removing the update…".to_owned(),
            (Some(Ok(l)), _) => format!("{} updates installed", l.len()),
            (None, _) => "Reading installed updates…".to_owned(),
            _ => String::new(),
        };
        frame::status(ui, &status, |ui| {
            ui.weak("Some updates (servicing stack, many cumulative ones) can't be removed on their own.");
        });
        let list = match &self.updates {
            Some(Ok(l)) => l.clone(),
            Some(Err(e)) => {
                let e = e.clone();
                frame::central(ui, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                });
                return;
            }
            None => {
                frame::central(ui, |ui| {
                    ui.spinner();
                });
                return;
            }
        };
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            self.updates_table(ui, ctx, &list, removing);
        });
    }

    fn updates_table(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, list: &[Update], removing: bool) {
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
        let busy = self.orphans_task.is_some();
        let chosen: Vec<PathBuf> = self
            .orphans
            .iter()
            .flatten()
            .filter(|o| self.orphans_checked.contains(&o.path))
            .map(|o| o.path.clone())
            .collect();
        let recycle_label = format!("Recycle {}", chosen.len());
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Scan").enabled(!busy)),
                Some(
                    ToolButton::new(icons::RECYCLE, &recycle_label)
                        .enabled(ctx.elevated && !chosen.is_empty()),
                ),
            ],
            |_| {},
        ) {
            Some(0) => {
                self.orphans_checked.clear();
                self.orphans_task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                    Ok(orphans::find(
                        &orphans::program_files_roots(),
                        &References::collect(),
                    ))
                }));
            }
            Some(1) => self.confirm.ask(
                format!(
                    "Move {} folders to the Recycle Bin? Programs that still use them would stop working.",
                    chosen.len()
                ),
                Ask::RecycleOrphans(chosen.clone()),
            ),
            _ => {}
        }
        let status = match &self.orphans {
            _ if busy => "Scanning Program Files…".to_owned(),
            Some(l) => format!("{} unused folders · {} ticked", l.len(), chosen.len()),
            None => "Ready".to_owned(),
        };
        frame::status(ui, &status, |ui| {
            if busy {
                ui.spinner();
            }
            if !ctx.elevated {
                ui.weak("Removing needs administrator rights");
            }
        });
        let list = self.orphans.clone();
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            match list {
                None if !busy => empty_state(
                    ui,
                    icons::FOLDERS,
                    "Find folders left behind in Program Files",
                    "Lists folders that no installed program, service, startup entry or App Path refers to. \
                     Check each one before removing it.",
                ),
                Some(l) if l.is_empty() => {
                    empty_state(ui, icons::SHIELD_CHECK, "No unused folders found", "");
                }
                Some(l) => self.orphans_table(ui, &l),
                None => {}
            }
        });
    }

    fn orphans_table(&mut self, ui: &mut egui::Ui, list: &[Orphan]) {
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

    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.handle_confirm(ui, ctx);
        if self.section != 0 {
            // The other pages open from the program list's toolbar; this strip goes back.
            let mut back = false;
            egui::Panel::top("uninst-back")
                .resizable(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        back = ui
                            .button(format!("{}  Installed programs", icons::ARROW_UP))
                            .clicked();
                        ui.label("›");
                        ui.strong(PAGES[self.section.min(PAGES.len() - 1)]);
                    });
                });
            if back {
                self.open_page(0);
                return;
            }
        }
        match self.section {
            0 => self.programs_section(ui, ctx),
            1 => self.updates_section(ui, ctx),
            2 => self.apps.ui(ui, ctx),
            _ => self.orphans_section(ui, ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_installs() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        assert!(is_recent("2026-11-26", today));
        assert!(is_recent("2026-10-27", today));
        assert!(!is_recent("2026-10-26", today));
        assert!(!is_recent("2027-01-01", today), "future dates are not recent");
        assert!(!is_recent("", today));
    }
}
