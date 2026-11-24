//! Registry Cleaner window: category tree (scan on/off, counts, expandable items),
//! exclusion list, backup choices and `.reg` import.

use std::collections::HashSet;

use wm_core::{
    AppCtx, Category, Tool, egui, icons, registry,
    safety::restore_point,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, show_message, status_bar, toolbar},
};

use super::{Backup, Issue, Kind, Options, export_backup, fix, import_reg, scan_with};

pub struct RegistryCleaner {
    opts: Options,
    saved: Options,
    task: Option<BackgroundTask<Vec<Issue>>>,
    issues: Option<Vec<Issue>>,
    marked: Vec<bool>,
    open: HashSet<Kind>,
    focused: Option<usize>,
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
            open: HashSet::new(),
            focused: None,
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

    fn options_panel(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
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

impl Tool for RegistryCleaner {
    fn id(&self) -> &'static str {
        "regclean"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.task.is_none() && self.issues.is_none() {
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
        }

        let busy = self.task.is_some();
        let fix_label = format!("Fix {}", chosen.len());
        let focused = self.focused.and_then(|i| issues.get(i)).cloned();
        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Scan").enabled(!busy)),
                Some(ToolButton::new(icons::WRENCH, &fix_label).enabled(!busy && !chosen.is_empty())),
                None,
                Some(ToolButton::new(icons::EYE_SLASH, "Ignore").enabled(focused.is_some())),
                Some(ToolButton::new(icons::TREE_STRUCTURE, "Registry").enabled(focused.is_some())),
                None,
                Some(ToolButton::new(icons::DOWNLOAD, "Import .reg")),
                Some(ToolButton::new(icons::SLIDERS, "Options")),
            ],
        ) {
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
            Some(3) => {
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
            Some(4) => {
                if let Some(i) = &focused {
                    let _ = registry::open_in_regedit(&i.key);
                }
            }
            Some(6) => {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("Registry file", &["reg"])
                    .pick_file()
                {
                    self.message = Some(if ctx.config.dry_run {
                        Ok(ctx.i18n.tr("common-dry-run-skip"))
                    } else {
                        import_reg(ctx, &file)
                            .map(|n| format!("Imported {n} keys. Undo in the Change Journal restores them as they were."))
                            .map_err(|e| format!("{e:#}"))
                    });
                }
            }
            Some(7) => self.show_options = !self.show_options,
            _ => {}
        }
        if self.show_options {
            self.options_panel(ui, ctx);
        }
        if let Some(t) = &self.task {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(&t.status);
            });
        }
        show_message(ui, &self.message);

        let status = format!(
            "{} issues found, {} selected{}",
            issues.len(),
            chosen.len(),
            if ctx.elevated {
                String::new()
            } else {
                "; machine-wide entries need administrator rights to fix".into()
            }
        );
        egui::Panel::bottom("regclean-status")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| status_bar(ui, &status, |_| {}));

        let mut kinds_changed = false;
        let mut groups: Vec<&str> = Vec::new();
        for k in Kind::ALL {
            if !groups.contains(&k.group()) {
                groups.push(k.group());
            }
        }
        let mut active = self.opts.kinds();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            for group in groups {
                ui.strong(group);
                for kind in Kind::ALL.into_iter().filter(|k| k.group() == group) {
                    let idx: Vec<usize> = (0..issues.len()).filter(|&i| issues[i].kind == kind).collect();
                    let mut on = active.contains(&kind);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        if ui
                            .checkbox(&mut on, kind.label())
                            .on_hover_text("Scan for this")
                            .changed()
                        {
                            active.retain(|k| *k != kind);
                            if on {
                                active.push(kind);
                            }
                            kinds_changed = true;
                        }
                        if !kind.note().is_empty() {
                            ui.weak(icons::INFO).on_hover_text(kind.note());
                        }
                        if !on {
                            ui.weak("not scanned");
                        } else if busy && self.issues.is_none() {
                            ui.weak("…");
                        } else if idx.is_empty() {
                            ui.weak("none");
                        } else {
                            let open = self.open.contains(&kind);
                            let arrow = if open { "▾" } else { "▸" };
                            if ui.link(format!("{arrow} {} found", idx.len())).clicked() {
                                if open {
                                    self.open.remove(&kind);
                                } else {
                                    self.open.insert(kind);
                                }
                            }
                        }
                    });
                    if self.open.contains(&kind) {
                        for i in idx {
                            let issue = &issues[i];
                            ui.horizontal(|ui| {
                                ui.add_space(32.0);
                                let ok = usable(issue);
                                ui.add_enabled(ok, egui::Checkbox::without_text(&mut self.marked[i]))
                                    .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                                let r = ui
                                    .selectable_label(self.focused == Some(i), &issue.detail)
                                    .on_hover_text(issue.location());
                                if r.clicked() {
                                    self.focused = Some(i);
                                }
                            });
                        }
                    }
                }
                ui.add_space(8.0);
            }
        });
        if kinds_changed {
            self.opts.kinds = Some(active);
            self.save_opts();
            if !busy {
                self.rescan(ui.ctx());
            }
        }
    }
}
