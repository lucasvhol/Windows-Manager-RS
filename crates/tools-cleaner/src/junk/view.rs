//! Junk File Cleaner window, laid out like the original: the drive pane on the left
//! (a free/used pie of the selected drives, the drive list, and our junk types), and
//! the found files in one ticked list (Files, Type, Size). Options (skip list, system
//! files, Recycle Bin) open in a window.

use std::collections::HashSet;
use std::path::PathBuf;

use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Layout, Tool,
    drives::{self, DriveInfo, DriveKind},
    egui::{self, Color32, Pos2, Rect, Stroke, Vec2},
    icons,
    safety::UndoAction,
    shell::{reveal_in_explorer, show_properties},
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, empty_state, format_bytes, frame, show_message},
};

use super::{CleanReport, Kind, Options, RULES, Rule, RuleScan, clean, parse_rules, scan_rule_with};

/// Pie colours: free (gold) and used (blue), like the original.
const FREE: Color32 = Color32::from_rgb(0xD9, 0xA4, 0x29);
const USED: Color32 = Color32::from_rgb(0x64, 0x95, 0xED);

/// One row of the file list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// None for a rule that has no item list (the Recycle Bin).
    pub path: Option<PathBuf>,
    pub label: String,
    /// None for a folder.
    pub size: Option<u64>,
    pub rule: usize,
}

/// The rows for the ticked rules: folders and files of each scan, or one row for
/// a rule that only reports a total (the Recycle Bin).
pub fn rows(rules: &[Rule], scans: &[RuleScan], picked: &[bool]) -> Vec<Row> {
    let mut out = Vec::new();
    for (i, (rule, scan)) in rules.iter().zip(scans).enumerate() {
        if !picked[i] {
            continue;
        }
        if scan.count() == 0 {
            if scan.bytes > 0 {
                out.push(Row {
                    path: None,
                    label: rule.title.clone(),
                    size: Some(scan.bytes),
                    rule: i,
                });
            }
            continue;
        }
        for d in &scan.dirs {
            out.push(Row {
                path: Some(d.clone()),
                label: d.display().to_string(),
                size: None,
                rule: i,
            });
        }
        for (p, s) in &scan.files {
            out.push(Row {
                path: Some(p.clone()),
                label: p.display().to_string(),
                size: Some(*s),
                rule: i,
            });
        }
    }
    out
}

/// The work for a cleanup: each ticked rule's scan without the unticked items.
pub fn work(
    rules: &[Rule],
    scans: &[RuleScan],
    picked: &[bool],
    unticked: &HashSet<PathBuf>,
) -> Vec<(Rule, RuleScan)> {
    rules
        .iter()
        .zip(scans)
        .enumerate()
        .filter(|(i, _)| picked[*i])
        .map(|(_, (r, s))| {
            let mut s = s.clone();
            if s.count() > 0 {
                for p in unticked {
                    s.files.retain(|(f, _)| f != p);
                    s.dirs.retain(|d| d != p);
                }
                s.bytes = s.files.iter().map(|(_, b)| b).sum();
            }
            (r.clone(), s)
        })
        .filter(|(_, s)| s.count() > 0 || s.bytes > 0)
        .collect()
}

pub struct JunkCleaner {
    rules: Vec<Rule>,
    selected: Vec<bool>,
    opts: Options,
    saved: Options,
    scans: Option<Vec<RuleScan>>,
    scan_task: Option<BackgroundTask<Vec<RuleScan>>>,
    clean_task: Option<BackgroundTask<CleanReport>>,
    /// Found items left out of the cleanup.
    unticked: HashSet<PathBuf>,
    /// The highlighted row (Properties, Open Folder and Add to Skip List act on it).
    current: Option<PathBuf>,
    filter: String,
    drives: Vec<DriveInfo>,
    show_options: bool,
    new_skip: String,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for JunkCleaner {
    fn default() -> Self {
        let rules = parse_rules(RULES).expect("built-in junk rules must parse");
        let opts = Options::saved();
        let selected = rules
            .iter()
            .map(|r| match &opts.selected {
                Some(ids) => ids.contains(&r.id),
                None => r.default,
            })
            .collect();
        Self {
            selected,
            rules,
            saved: opts.clone(),
            opts,
            scans: None,
            scan_task: None,
            clean_task: None,
            unticked: HashSet::new(),
            current: None,
            filter: String::new(),
            drives: Vec::new(),
            show_options: false,
            new_skip: String::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

fn system_drive() -> char {
    wm_core::paths::expand_env("%SystemDrive%")
        .chars()
        .next()
        .unwrap_or('C')
        .to_ascii_uppercase()
}

impl JunkCleaner {
    fn usable(&self, i: usize, elevated: bool) -> bool {
        !self.rules[i].admin || elevated
    }

    fn start_scan(&mut self, ctx: &egui::Context, elevated: bool) {
        let rules = self.rules.clone();
        let selected = self.selected.clone();
        let opts = self.opts.clone();
        self.scans = None;
        self.unticked.clear();
        self.current = None;
        self.scan_task = Some(BackgroundTask::spawn(ctx, move |p| {
            let n = rules.len() as f32;
            Ok(rules
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    // Slow or administrator-only scans only run when they will be used.
                    let wanted = (selected[i] || !(r.kind.uses_drives() || r.kind == Kind::InstallerOrphans))
                        && (!r.admin || elevated);
                    if !wanted {
                        return RuleScan::default();
                    }
                    p.report(i as f32 / n, format!("Scanning {}…", r.title));
                    scan_rule_with(r, &opts, &|s| p.report(i as f32 / n, s.to_owned()))
                })
                .collect())
        }));
    }

    fn persist_selection(&mut self) {
        let ids: Vec<String> = self
            .rules
            .iter()
            .zip(&self.selected)
            .filter(|(_, s)| **s)
            .map(|(r, _)| r.id.clone())
            .collect();
        self.opts.selected = Some(ids);
        if self.opts.save().is_ok() {
            self.saved = self.opts.clone();
        }
    }

    fn drive_selected(&self, letter: char) -> bool {
        if self.opts.drives.is_empty() {
            letter == system_drive()
        } else {
            self.opts.drives.contains(&letter)
        }
    }

    /// The original's left pane: selected drives, their free/used pie, the drive list
    /// with Free / Total bars, then the junk types. Returns true when a rescan is due.
    fn drive_pane(&mut self, ui: &mut egui::Ui, elevated: bool) -> bool {
        let chosen: Vec<&DriveInfo> = self
            .drives
            .iter()
            .filter(|d| self.drive_selected(d.letter))
            .collect();
        let names: Vec<String> = chosen.iter().map(|d| format!("{}:\\", d.letter)).collect();
        ui.vertical_centered(|ui| {
            ui.label(format!("Selected Drives: {}", names.join(";")));
        });
        ui.separator();
        let free: u64 = chosen.iter().map(|d| d.free).sum();
        let total: u64 = chosen.iter().map(|d| d.total).sum();
        usage_pie(ui, free, total);
        ui.separator();

        let mut rescan = false;
        let mut toggled = None;
        egui::Grid::new("junk-drives")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Drive");
                ui.strong("Free / Total");
                ui.end_row();
                for d in &self.drives {
                    let mut on = self.drive_selected(d.letter);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut on, "").changed() {
                            toggled = Some(d.letter);
                        }
                        ui.label(egui::RichText::new(icons::HARD_DRIVES).size(20.0));
                        let name = if d.label.is_empty() {
                            format!("{}:\\", d.letter)
                        } else {
                            format!("{}:\\ ({})", d.letter, d.label)
                        };
                        ui.label(format!("{name}\n{}\n{}", d.kind.label(), d.file_system));
                    });
                    let share = d.free as f32 / d.total.max(1) as f32;
                    ui.add(
                        egui::ProgressBar::new(1.0 - share)
                            .desired_width(ui.available_width().max(120.0))
                            .text(format!(
                                "{}/{} ({:.0}%)",
                                format_bytes(d.free),
                                format_bytes(d.total),
                                share * 100.0
                            )),
                    )
                    .on_hover_text("Free of total; the bar shows the used part");
                    ui.end_row();
                }
            });
        if let Some(letter) = toggled {
            if self.opts.drives.is_empty() {
                self.opts.drives.push(system_drive());
            }
            if !self.opts.drives.contains(&letter) {
                self.opts.drives.push(letter);
            } else {
                self.opts.drives.retain(|c| *c != letter);
            }
            self.opts.drives.sort_unstable();
            if self.opts.save().is_ok() {
                self.saved = self.opts.clone();
            }
            rescan = self
                .rules
                .iter()
                .zip(&self.selected)
                .any(|(r, s)| *s && r.kind.uses_drives());
        }

        ui.add_space(8.0);
        ui.strong("Junk types");
        let mut groups: Vec<String> = Vec::new();
        for r in &self.rules {
            if !groups.contains(&r.group) {
                groups.push(r.group.clone());
            }
        }
        let mut toggled_rule = None;
        for group in groups {
            ui.add_space(4.0);
            ui.weak(&group);
            for i in (0..self.rules.len()).filter(|&i| self.rules[i].group == group) {
                let usable = self.usable(i, elevated);
                let rule = &self.rules[i];
                // Locked (admin-only) rules show unticked: they are not part of the cleanup.
                let mut on = usable && self.selected[i];
                let size = match self.scans.as_ref().map(|s| &s[i]) {
                    _ if !usable => String::new(),
                    Some(s) if s.files.is_empty() && !s.dirs.is_empty() => {
                        format!("{} folders", s.dirs.len())
                    }
                    Some(s) => format_bytes(s.bytes),
                    None => String::new(),
                };
                ui.horizontal(|ui| {
                    let r = ui.add_enabled(usable, egui::Checkbox::new(&mut on, &rule.title));
                    if r.changed() {
                        toggled_rule = Some((i, on));
                        if on && (rule.kind.uses_drives() || rule.kind == Kind::InstallerOrphans) {
                            rescan = true; // these are only scanned when ticked
                        }
                    }
                    let r = r.on_hover_text(&rule.description);
                    if !usable {
                        r.on_disabled_hover_text("Needs administrator rights");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(size);
                    });
                });
            }
        }
        if let Some((i, on)) = toggled_rule {
            self.selected[i] = on;
        }
        rescan
    }

    fn options_window(&mut self, ui: &mut egui::Ui) {
        let mut open = self.show_options;
        egui::Window::new("Options")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.checkbox(&mut self.opts.skip_system, "Leave system files alone")
                    .on_hover_text("Files and folders Windows marks as system files are never cleaned.");
                ui.checkbox(
                    &mut self.opts.use_recycle_bin,
                    "Move to the Recycle Bin instead of deleting",
                )
                .on_hover_text(
                    "Lets you restore files, but frees no space until the Recycle Bin is emptied.",
                );
                ui.add_space(4.0);
                ui.strong("Skip list");
                ui.weak(
                    r"A folder (everything inside it is kept) or a pattern such as *.log or *\Projects\*.",
                );
                let mut remove = None;
                for (i, s) in self.opts.skip.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✕").clicked() {
                            remove = Some(i);
                        }
                        ui.monospace(s);
                    });
                }
                if let Some(i) = remove {
                    self.opts.skip.remove(i);
                }
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_skip).hint_text("Folder or pattern"));
                    if ui
                        .add_enabled(!self.new_skip.trim().is_empty(), egui::Button::new("Add"))
                        .clicked()
                    {
                        self.opts.skip.push(self.new_skip.trim().to_owned());
                        self.new_skip.clear();
                    }
                    if ui.button("Add folder…").clicked()
                        && let Some(d) = rfd::FileDialog::new().pick_folder()
                    {
                        self.opts.skip.push(d.display().to_string());
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    let dirty = self.opts != self.saved;
                    if ui.add_enabled(dirty, egui::Button::new("Save options")).clicked() {
                        self.message = Some(
                            self.opts
                                .save()
                                .map(|_| {
                                    self.saved = self.opts.clone();
                                    "Options saved. Scan again to apply them.".to_owned()
                                })
                                .map_err(|e| format!("{e:#}")),
                        );
                    }
                    if dirty && ui.button("Discard").clicked() {
                        self.opts = self.saved.clone();
                    }
                });
            });
        self.show_options = open;
    }

    fn file_table(&mut self, ui: &mut egui::Ui, rows: &[&Row]) {
        let mut toggles: Vec<(Option<PathBuf>, usize)> = Vec::new();
        let mut current = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(24.0))
            .column(Column::remainder().at_least(240.0).clip(true))
            .column(Column::initial(190.0).clip(true))
            .column(Column::initial(80.0))
            .header(22.0, |mut h| {
                for t in ["", "Files", "Type", "Size"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let r = rows[row.index()];
                    row.set_selected(r.path.is_some() && self.current == r.path);
                    row.col(|ui| {
                        let mut on = match &r.path {
                            Some(p) => !self.unticked.contains(p),
                            None => self.selected[r.rule],
                        };
                        if ui.checkbox(&mut on, "").changed() {
                            toggles.push((r.path.clone(), r.rule));
                        }
                    });
                    row.col(|ui| {
                        let icon = match (&r.path, r.size) {
                            (None, _) => icons::RECYCLE,
                            (_, None) => icons::FOLDER_OPEN,
                            _ => icons::FILE_TEXT,
                        };
                        ui.label(format!("{icon}  {}", r.label));
                    });
                    row.col(|ui| {
                        ui.label(&self.rules[r.rule].title);
                    });
                    row.col(|ui| {
                        ui.label(r.size.map_or("folder".into(), format_bytes));
                    });
                    if row.response().clicked() {
                        current = Some(r.path.clone());
                    }
                });
            });
        for (path, rule) in toggles {
            match path {
                Some(p) => {
                    if !self.unticked.remove(&p) {
                        self.unticked.insert(p);
                    }
                }
                None => self.selected[rule] = !self.selected[rule],
            }
        }
        if let Some(c) = current {
            self.current = c;
        }
    }
}

/// The free/used pie of the selected drives with its legend.
fn usage_pie(ui: &mut egui::Ui, free: u64, total: u64) {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 110.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let radius = 48.0;
    let center = Pos2::new(rect.left() + 20.0 + radius, rect.center().y);
    let free_share = free as f32 / total.max(1) as f32;
    let mut start = -FRAC_PI_2;
    for (share, color) in [(free_share, FREE), (1.0 - free_share, USED)] {
        let sweep = share * TAU;
        if sweep <= 0.0 {
            continue;
        }
        let steps = ((sweep / 0.04).ceil() as usize).max(2);
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(center, color);
        for k in 0..=steps {
            let a = start + sweep * k as f32 / steps as f32;
            mesh.colored_vertex(center + Vec2::new(a.cos(), a.sin()) * radius, color);
        }
        for k in 1..=steps as u32 {
            mesh.add_triangle(0, k, k + 1);
        }
        painter.add(mesh);
        if share > 0.08 {
            let mid = start + sweep / 2.0;
            painter.text(
                center + Vec2::new(mid.cos(), mid.sin()) * radius * 0.55,
                egui::Align2::CENTER_CENTER,
                format!("{:.2}%", share * 100.0),
                egui::FontId::proportional(11.0),
                Color32::BLACK,
            );
        }
        start += sweep;
    }
    painter.circle_stroke(center, radius, Stroke::new(1.0, ui.visuals().panel_fill));
    let x = center.x + radius + 24.0;
    for (i, (label, value, color)) in [("Free:", free, FREE), ("Used:", total.saturating_sub(free), USED)]
        .into_iter()
        .enumerate()
    {
        let y = rect.center().y - 26.0 + i as f32 * 30.0;
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(x, y + 2.0), Vec2::new(18.0, 10.0)),
            1.0,
            color,
        );
        painter.text(
            Pos2::new(x + 24.0, y),
            egui::Align2::LEFT_TOP,
            format!("{label}\n{}/{}", format_bytes(value), format_bytes(total)),
            egui::FontId::proportional(11.5),
            ui.visuals().text_color(),
        );
    }
}

impl Tool for JunkCleaner {
    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn id(&self) -> &'static str {
        "junk"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.drives.is_empty() {
            self.drives = drives::list()
                .into_iter()
                .filter(|d| matches!(d.kind, DriveKind::Fixed | DriveKind::Removable))
                .collect();
        }
        if self.scans.is_none() && self.scan_task.is_none() && self.clean_task.is_none() {
            self.start_scan(ui.ctx(), ctx.elevated);
        }
        if let Some(t) = &mut self.scan_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.scan_task = None;
                match r {
                    Ok(s) => self.scans = Some(s),
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if let Some(t) = &mut self.clean_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.clean_task = None;
                self.message = Some(r.map(|rep| {
                    let how = if self.opts.use_recycle_bin {
                        "moved to the Recycle Bin"
                    } else {
                        "deleted"
                    };
                    let desc = format!(
                        "Junk cleanup: {} items {how}, {} freed",
                        rep.removed,
                        format_bytes(rep.freed)
                    );
                    let _ = ctx.journal.record("junk", desc.clone(), UndoAction::None);
                    if rep.failed > 0 {
                        format!("{desc}. {} files were in use and skipped.", rep.failed)
                    } else {
                        desc + "."
                    }
                }));
                self.scans = None; // rescan
                self.drives.clear();
            }
        }

        let busy = self.scan_task.is_some() || self.clean_task.is_some();
        let picked: Vec<bool> = (0..self.rules.len())
            .map(|i| self.selected[i] && self.usable(i, ctx.elevated))
            .collect();
        let empty = Vec::new();
        let scans = self.scans.as_ref().unwrap_or(&empty);
        let all_rows = if scans.is_empty() {
            Vec::new()
        } else {
            rows(&self.rules, scans, &picked)
        };
        let todo = if scans.is_empty() {
            Vec::new()
        } else {
            work(&self.rules, scans, &picked, &self.unticked)
        };
        let todo_bytes: u64 = todo.iter().map(|(_, s)| s.bytes).sum();
        let todo_items: usize = todo
            .iter()
            .map(|(_, s)| s.count().max(usize::from(s.bytes > 0)))
            .sum();
        let found_bytes: u64 = all_rows.iter().filter_map(|r| r.size).sum();
        let all_ticked = !all_rows.is_empty() && self.unticked.is_empty();

        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let recycle = self.opts.use_recycle_bin;
                self.persist_selection();
                let work = todo.clone();
                self.clean_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                    Ok(clean(&work, recycle))
                }));
            }
        }

        let current = self.current.clone();
        let clicked = frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Scan").enabled(!busy)),
                Some(
                    ToolButton::new(
                        icons::LIST_CHECKS,
                        if all_ticked { "Select None" } else { "Select All" },
                    )
                    .enabled(!busy && !all_rows.is_empty()),
                ),
                Some(ToolButton::new(icons::X, "Delete").enabled(!busy && todo_items > 0)),
                None,
                Some(ToolButton::new(icons::CLIPBOARD_TEXT, "Properties").enabled(current.is_some())),
                Some(ToolButton::new(icons::FOLDER_OPEN, "Open Folder").enabled(current.is_some())),
                None,
                Some(ToolButton::new(icons::GEAR, "Options")),
                Some(ToolButton::new(icons::PLUS, "Add to Skip List").enabled(!busy && current.is_some())),
                None,
                Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
            ],
            |_| {},
        );
        match clicked {
            Some(0) => {
                self.message = None;
                self.drives.clear();
                self.start_scan(ui.ctx(), ctx.elevated);
            }
            Some(1) => {
                if all_ticked {
                    self.unticked = all_rows.iter().filter_map(|r| r.path.clone()).collect();
                } else {
                    self.unticked.clear();
                }
            }
            Some(2) => {
                let how = if self.opts.use_recycle_bin {
                    "moved to the Recycle Bin"
                } else {
                    "permanently deleted (this cannot be undone)"
                };
                self.confirm.ask(
                    format!(
                        "{todo_items} items ({}) will be {how}. Continue?",
                        format_bytes(todo_bytes)
                    ),
                    (),
                );
            }
            Some(4) => {
                if let Some(p) = &current
                    && let Err(e) = show_properties(p)
                {
                    self.message = Some(Err(format!("{e:#}")));
                }
            }
            Some(5) => {
                if let Some(p) = &current {
                    reveal_in_explorer(p);
                }
            }
            Some(7) => self.show_options = !self.show_options,
            Some(8) => {
                if let Some(p) = current {
                    self.opts.skip.push(p.display().to_string());
                    let _ = self.opts.save().map(|_| self.saved.clone_from(&self.opts));
                    if let Some(scans) = &mut self.scans {
                        for s in scans.iter_mut() {
                            s.remove(&p);
                        }
                    }
                    self.message = Some(Ok(format!("{} is on the skip list now.", p.display())));
                    self.current = None;
                }
            }
            Some(10) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
        self.options_window(ui);

        let status = match &self.scan_task {
            Some(t) if !t.status.is_empty() => t.status.clone(),
            Some(_) => "Scanning…".into(),
            None if self.clean_task.is_some() => "Cleaning…".into(),
            None if self.scans.is_some() => format!(
                "Found {} items ({}); {todo_items} ticked to delete ({})",
                all_rows.len(),
                format_bytes(found_bytes),
                format_bytes(todo_bytes)
            ),
            None => "Welcome to Junk File Cleaner".into(),
        };
        frame::status(ui, &status, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  Search", icons::MAGNIFYING_GLASS))
                    .desired_width(180.0),
            );
            if busy {
                ui.spinner();
            }
        });
        let rescan = frame::left(ui, 330.0, |ui| self.drive_pane(ui, ctx.elevated));

        let f = self.filter.to_lowercase();
        let shown: Vec<&Row> = all_rows
            .iter()
            .filter(|r| f.is_empty() || r.label.to_lowercase().contains(&f))
            .collect();
        let scanned = self.scans.is_some();
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            if !scanned {
                return;
            }
            if all_rows.is_empty() {
                empty_state(
                    ui,
                    icons::SHIELD_CHECK,
                    "No junk files found",
                    "Tick more junk types on the left, or drives for empty folders and zero-byte files.",
                );
                return;
            }
            self.file_table(ui, &shown);
        });
        if rescan && !busy {
            self.start_scan(ui.ctx(), ctx.elevated);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_work_respect_ticks() {
        let rules = parse_rules(RULES).unwrap();
        let mut scans = vec![RuleScan::default(); rules.len()];
        scans[0] = RuleScan {
            files: vec![("C:\\t\\a.tmp".into(), 10), ("C:\\t\\b.tmp".into(), 5)],
            dirs: vec!["C:\\t\\empty".into()],
            roots: Vec::new(),
            bytes: 15,
        };
        scans[1] = RuleScan {
            bytes: 7,
            ..Default::default()
        };
        let mut picked = vec![false; rules.len()];
        picked[0] = true;
        picked[1] = true;

        let r = rows(&rules, &scans, &picked);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].size, None, "folders first");
        assert_eq!(r[3].path, None, "a total-only rule is one row");

        let unticked: HashSet<PathBuf> = [PathBuf::from("C:\\t\\a.tmp")].into();
        let w = work(&rules, &scans, &picked, &unticked);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].1.files, vec![(PathBuf::from("C:\\t\\b.tmp"), 5)]);
        assert_eq!(w[0].1.bytes, 5);
        assert_eq!(w[1].1.bytes, 7);

        picked[1] = false;
        assert_eq!(work(&rules, &scans, &picked, &unticked).len(), 1);
    }
}
