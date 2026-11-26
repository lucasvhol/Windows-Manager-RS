//! Registry Cleaner window, laid out like the original: the "Scan Items" tree on the
//! left, the found entries as Main Key | Sub Key | Value Name | Value, and an Options
//! window (exclusions, backup choices, `.reg` import).

use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, registry,
    safety::restore_point,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, empty_state, frame, show_message},
};

use super::{Backup, Issue, Kind, Options, export_backup, fix, import_reg, scan_with};

pub struct RegistryCleaner {
    opts: Options,
    saved: Options,
    task: Option<BackgroundTask<Vec<Issue>>>,
    issues: Option<Vec<Issue>>,
    marked: Vec<bool>,
    focused: Option<usize>,
    filter: String,
    /// Scan on the next frame (after a cleanup).
    scan_on_open: bool,
    show_options: bool,
    new_exclude: String,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for RegistryCleaner {
    fn default() -> Self {
        let opts = Options::saved();
        Self {
            saved: opts.clone(),
            opts,
            task: None,
            issues: None,
            marked: Vec::new(),
            focused: None,
            filter: String::new(),
            scan_on_open: false,
            show_options: false,
            new_exclude: String::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl RegistryCleaner {
    fn rescan(&mut self, ctx: &egui::Context) {
        let opts = self.opts.clone();
        self.issues = None;
        self.focused = None;
        self.task = Some(BackgroundTask::spawn(ctx, move |p| {
            Ok(scan_with(&opts, &|s| p.report(0.0, s.to_owned())))
        }));
    }

    fn save_opts(&mut self) {
        if self.opts.save().is_ok() {
            self.saved = self.opts.clone();
        }
    }

    fn options_window(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let mut open = self.show_options;
        let mut import = false;
        egui::Window::new("Options")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                self.options_panel(ui, ctx);
                ui.separator();
                import = ui
                    .button(format!("{}  Import a .reg file…", icons::DOWNLOAD))
                    .on_hover_text("Undo in the Change Journal restores the keys as they were")
                    .clicked();
            });
        self.show_options = open;
        if import
            && let Some(file) = rfd::FileDialog::new()
                .add_filter("Registry file", &["reg"])
                .pick_file()
        {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                import_reg(ctx, &file)
                    .map(|n| {
                        format!("Imported {n} keys. Undo in the Change Journal restores them as they were.")
                    })
                    .map_err(|e| format!("{e:#}"))
            });
        }
    }

    fn options_panel(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.scope(|ui| {
            ui.strong("Backup before fixing");
            ui.radio_value(
                &mut self.opts.backup,
                Backup::Journal,
                "Change Journal only (every fix can be undone there)",
            );
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.opts.backup, Backup::RegFile, "Also save a .reg file to");
                let folder =
                    self.opts.backup_folder.clone().or_else(|| {
                        wm_core::shell::known_folder(wm_core::shell::KnownFolder::Documents).ok()
                    });
                ui.monospace(folder.map(|f| f.display().to_string()).unwrap_or_default());
                if ui.small_button("Change…").clicked()
                    && let Some(d) = rfd::FileDialog::new().pick_folder()
                {
                    self.opts.backup_folder = Some(d);
                }
            });
            ui.add_enabled(
                ctx.elevated,
                egui::Checkbox::new(
                    &mut self.opts.restore_point,
                    "Create a System Restore point first",
                ),
            )
            .on_disabled_hover_text("Needs administrator rights");
            ui.add_space(4.0);
            ui.strong("Never report");
            ui.weak(
                r"A key, a key\value, or a pattern such as *\Steam\* (also matched against the shown path).",
            );
            let mut remove = None;
            for (i, e) in self.opts.exclude.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✕").clicked() {
                        remove = Some(i);
                    }
                    ui.monospace(e);
                });
            }
            if let Some(i) = remove {
                self.opts.exclude.remove(i);
            }
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_exclude).hint_text("Key or pattern"));
                if ui
                    .add_enabled(!self.new_exclude.trim().is_empty(), egui::Button::new("Add"))
                    .clicked()
                {
                    self.opts.exclude.push(self.new_exclude.trim().to_owned());
                    self.new_exclude.clear();
                }
            });
            ui.horizontal(|ui| {
                let dirty = self.opts != self.saved;
                if ui.add_enabled(dirty, egui::Button::new("Save options")).clicked() {
                    self.save_opts();
                    self.message = Some(Ok("Options saved. Scan again to apply the exclusions.".into()));
                }
                if dirty && ui.button("Discard").clicked() {
                    self.opts = self.saved.clone();
                }
            });
        });
    }

    fn do_fix(&mut self, ctx: &AppCtx<'_>, chosen: Vec<Issue>) -> Result<String, String> {
        if self.opts.restore_point && ctx.elevated {
            restore_point::create("Before Win Manager RS registry cleanup")
                .map_err(|e| format!("Could not create a restore point, nothing was changed: {e:#}"))?;
        }
        let mut note = String::new();
        if self.opts.backup == Backup::RegFile {
            let folder = self
                .opts
                .backup_folder
                .clone()
                .or_else(|| wm_core::shell::known_folder(wm_core::shell::KnownFolder::Documents).ok())
                .ok_or("No backup folder")?;
            let file = folder.join(format!(
                "Registry backup {}.reg",
                chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
            ));
            export_backup(&chosen, &file)
                .map_err(|e| format!("Backup failed, nothing was changed: {e:#}"))?;
            note = format!(" Backup saved to {}.", file.display());
        }
        fix(ctx, &chosen)
            .map(|n| format!("Fixed {n} issues; all can be undone from the Change Journal.{note}"))
            .map_err(|e| format!("{e:#}"))
    }
}

/// Splits a key path into the original's Main Key (the hive) and Sub Key columns.
pub fn split_key(key: &str) -> (&str, &str) {
    key.split_once('\\').unwrap_or((key, ""))
}

impl Tool for RegistryCleaner {
    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn id(&self) -> &'static str {
        "regclean"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.task.is_none() && self.issues.is_none() && self.scan_on_open {
            self.scan_on_open = false;
            self.rescan(ui.ctx());
        }
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                let issues = r.unwrap_or_default();
                self.marked = vec![true; issues.len()];
                self.issues = Some(issues);
            }
        }
        let issues = self.issues.clone().unwrap_or_default();
        let elevated = ctx.elevated;
        let usable = |i: &Issue| !registry::needs_admin(&i.key) || elevated;
        let chosen: Vec<Issue> = issues
            .iter()
            .zip(&self.marked)
            .filter(|(i, m)| **m && usable(i))
            .map(|(i, _)| i.clone())
            .collect();

        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                self.do_fix(ctx, chosen.clone())
            });
            self.issues = None;
            self.scan_on_open = true;
        }

        let busy = self.task.is_some();
        let focused = self.focused.and_then(|i| issues.get(i)).cloned();
        let all_marked = !issues.is_empty() && self.marked.iter().all(|m| *m);
        let clicked = frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Scan Registry").enabled(!busy)),
                Some(ToolButton::new(icons::TRASH, "Clean Registry").enabled(!busy && !chosen.is_empty())),
                Some(
                    ToolButton::new(
                        icons::LIST_CHECKS,
                        if all_marked { "Select None" } else { "Select All" },
                    )
                    .enabled(!issues.is_empty()),
                ),
                None,
                Some(ToolButton::new(icons::ARROW_SQUARE_OUT, "Open in Registry").enabled(focused.is_some())),
                Some(ToolButton::new(icons::PLUS, "Add to Exclusions").enabled(focused.is_some())),
                None,
                Some(ToolButton::new(icons::TOOLBOX, "Registry Tools")),
                Some(ToolButton::new(icons::GEAR, "Options")),
                None,
                Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
            ],
            |_| {},
        );
        match clicked {
            Some(0) => {
                self.message = None;
                self.rescan(ui.ctx());
            }
            Some(1) => self.confirm.ask(
                format!(
                    "Remove {} dead registry entries? {}",
                    chosen.len(),
                    if self.opts.backup == Backup::RegFile {
                        "They are backed up to the Change Journal and a .reg file first."
                    } else {
                        "They are backed up to the Change Journal first."
                    }
                ),
                (),
            ),
            Some(2) => {
                let on = !all_marked;
                self.marked.iter_mut().for_each(|m| *m = on);
            }
            Some(4) => {
                if let Some(i) = &focused {
                    let _ = registry::open_in_regedit(&i.key);
                }
            }
            Some(5) => {
                if let Some(i) = &focused {
                    self.opts.exclude.push(i.location());
                    self.save_opts();
                    let loc = i.location();
                    if let Some(list) = &mut self.issues {
                        let keep: Vec<bool> = list.iter().map(|x| x.location() != loc).collect();
                        let mut k = keep.iter();
                        self.marked.retain(|_| *k.next().unwrap());
                        list.retain(|x| x.location() != loc);
                    }
                    self.focused = None;
                    self.message = Some(Ok(format!("{loc} will no longer be reported.")));
                }
            }
            Some(7) => wm_core::ui::request_open_tool(ui.ctx(), "regtools"),
            Some(8) => self.show_options = !self.show_options,
            Some(10) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
        self.options_window(ui, ctx);

        let status = match &self.task {
            Some(t) if !t.status.is_empty() => t.status.clone(),
            Some(_) => "Scanning…".into(),
            None if self.issues.is_none() => "Welcome to Registry Cleaner".into(),
            None => format!(
                "{} issues found, {} selected{}",
                issues.len(),
                chosen.len(),
                if ctx.elevated {
                    String::new()
                } else {
                    " · machine-wide entries need administrator rights to fix".into()
                }
            ),
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

        let kinds_changed = frame::left(ui, 270.0, |ui| self.scan_items(ui, &issues, busy));
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            self.issue_table(ui, &issues, elevated, ctx);
        });
        if kinds_changed && !busy && self.issues.is_some() {
            self.rescan(ui.ctx());
        }
    }
}

impl RegistryCleaner {
    /// The original's "Scan Items" tree: "Registry Entries" ticks everything, each
    /// item ticks one kind (with its count after a scan). Returns true when the
    /// choice changed.
    fn scan_items(&mut self, ui: &mut egui::Ui, issues: &[Issue], busy: bool) -> bool {
        ui.vertical_centered(|ui| ui.label("Scan Items"));
        ui.separator();
        let mut active = self.opts.kinds();
        let mut changed = false;
        let all = Kind::ALL.iter().all(|k| active.contains(k));
        let mut root = all;
        let id = ui.make_persistent_id("regclean-items");
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
            .show_header(ui, |ui| {
                let some = !all && Kind::ALL.iter().any(|k| active.contains(k));
                if ui
                    .add(egui::Checkbox::new(&mut root, "Registry Entries").indeterminate(some))
                    .changed()
                {
                    active = if root { Kind::ALL.to_vec() } else { Vec::new() };
                    changed = true;
                }
            })
            .body(|ui| {
                let mut groups: Vec<&str> = Vec::new();
                for k in Kind::ALL {
                    if !groups.contains(&k.group()) {
                        groups.push(k.group());
                    }
                }
                for group in groups {
                    ui.weak(group);
                    for kind in Kind::ALL.into_iter().filter(|k| k.group() == group) {
                        let n = issues.iter().filter(|i| i.kind == kind).count();
                        let mut on = active.contains(&kind);
                        ui.horizontal(|ui| {
                            let r = ui.checkbox(&mut on, kind.label());
                            let r = if kind.note().is_empty() {
                                r
                            } else {
                                r.on_hover_text(kind.note())
                            };
                            if r.changed() {
                                active.retain(|k| *k != kind);
                                if on {
                                    active.push(kind);
                                }
                                changed = true;
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if on && self.issues.is_some() && !busy && n > 0 {
                                    ui.label(n.to_string());
                                }
                            });
                        });
                    }
                }
            });
        if changed {
            self.opts.kinds = Some(active);
            self.save_opts();
        }
        changed
    }

    fn issue_table(&mut self, ui: &mut egui::Ui, issues: &[Issue], elevated: bool, ctx: &AppCtx<'_>) {
        if self.issues.is_none() {
            if self.task.is_none() {
                empty_state(
                    ui,
                    icons::DATABASE,
                    "Find dead registry entries",
                    "Tick what to look for on the left, then click Scan Registry.",
                );
            }
            return;
        }
        if issues.is_empty() {
            empty_state(ui, icons::SHIELD_CHECK, "No registry issues found", "");
            return;
        }
        let f = self.filter.to_lowercase();
        let rows: Vec<usize> = (0..issues.len())
            .filter(|&i| {
                f.is_empty()
                    || issues[i].location().to_lowercase().contains(&f)
                    || issues[i].detail.to_lowercase().contains(&f)
            })
            .collect();
        let mut focus = None;
        let mut open = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(24.0))
            .column(Column::initial(70.0))
            .column(Column::initial(320.0).clip(true))
            .column(Column::initial(150.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .header(22.0, |mut h| {
                for t in ["", "Main Key", "Sub Key", "Value Name", "Value"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let i = rows[row.index()];
                    let issue = &issues[i];
                    let (main, sub) = split_key(&issue.key);
                    row.set_selected(self.focused == Some(i));
                    row.col(|ui| {
                        let ok = !registry::needs_admin(&issue.key) || elevated;
                        ui.add_enabled(ok, egui::Checkbox::without_text(&mut self.marked[i]))
                            .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                    });
                    row.col(|ui| {
                        ui.label(main);
                    });
                    row.col(|ui| {
                        ui.label(sub).on_hover_text(issue.kind.label());
                    });
                    row.col(|ui| {
                        ui.label(issue.value.as_deref().unwrap_or(""));
                    });
                    row.col(|ui| {
                        ui.label(&issue.detail).on_hover_text(&issue.detail);
                    });
                    let r = row.response();
                    if r.clicked() {
                        focus = Some(i);
                    }
                    if r.double_clicked() {
                        open = Some(issue.key.clone());
                    }
                });
            });
        if focus.is_some() {
            self.focused = focus;
        }
        if let Some(k) = open {
            let _ = registry::open_in_regedit(&k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_columns() {
        assert_eq!(split_key(r"HKCU\Software\A"), ("HKCU", r"Software\A"));
        assert_eq!(split_key("HKLM"), ("HKLM", ""));
    }
}
