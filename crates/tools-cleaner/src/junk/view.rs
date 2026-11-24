//! Junk File Cleaner window: rule list with sizes, drive picker for the drive-wide
//! scans, a review list per rule (skip single items), and options (skip list, system
//! files, Recycle Bin).

use std::path::PathBuf;

use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Tool, egui, icons,
    safety::UndoAction,
    shell::reveal_in_explorer,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, drive_picker, format_bytes, show_message, toolbar},
};

use super::{CleanReport, Kind, Options, RULES, Rule, RuleScan, clean, parse_rules, scan_rule_with};

pub struct JunkCleaner {
    rules: Vec<Rule>,
    selected: Vec<bool>,
    opts: Options,
    saved: Options,
    scans: Option<Vec<RuleScan>>,
    scan_task: Option<BackgroundTask<Vec<RuleScan>>>,
    clean_task: Option<BackgroundTask<CleanReport>>,
    /// Rule whose items are listed for review.
    review: Option<usize>,
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
            review: None,
            show_options: false,
            new_skip: String::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
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
        self.review = None;
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

    fn drives_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Drives for empty folders and zero-byte files:");
            drive_picker(ui, &mut self.opts.drives);
        });
    }

    fn options_panel(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.checkbox(&mut self.opts.skip_system, "Leave system files alone")
                .on_hover_text("Files and folders Windows marks as system files are never cleaned.");
            ui.checkbox(
                &mut self.opts.use_recycle_bin,
                "Move to the Recycle Bin instead of deleting",
            )
            .on_hover_text("Lets you restore files, but frees no space until the Recycle Bin is emptied.");
            ui.add_space(4.0);
            ui.strong("Never clean");
            ui.weak(r"A folder (everything inside it is kept) or a pattern such as *.log or *\Projects\*.");
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
    }

    fn review_panel(&mut self, ui: &mut egui::Ui) {
        let Some(i) = self.review else { return };
        let Some(scan) = self.scans.as_ref().map(|s| s[i].clone()) else {
            return;
        };
        let rows: Vec<(PathBuf, Option<u64>)> = scan
            .dirs
            .iter()
            .map(|d| (d.clone(), None))
            .chain(scan.files.iter().map(|(p, s)| (p.clone(), Some(*s))))
            .collect();
        let mut close = false;
        let mut skip = None;
        egui::Panel::bottom("junk-review")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(&self.rules[i].title);
                    ui.weak(format!("{} items", rows.len()));
                    if ui.small_button("✕").clicked() {
                        close = true;
                    }
                });
                ui.weak("\"Keep\" leaves an item out of this cleanup and adds it to the Never clean list.");
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                TableBuilder::new(ui)
                    .striped(true)
                    .column(Column::remainder().clip(true))
                    .column(Column::exact(80.0))
                    .column(Column::exact(110.0))
                    .body(|body| {
                        body.rows(20.0, rows.len(), |mut row| {
                            let (p, size) = &rows[row.index()];
                            row.col(|ui| {
                                ui.label(p.display().to_string());
                            });
                            row.col(|ui| {
                                ui.weak(size.map_or("folder".into(), format_bytes));
                            });
                            row.col(|ui| {
                                if ui.small_button("Show").clicked() {
                                    reveal_in_explorer(p);
                                }
                                if ui.small_button("Keep").clicked() {
                                    skip = Some(p.clone());
                                }
                            });
                        });
                    });
            });
        if let Some(p) = skip {
            self.opts.skip.push(p.display().to_string());
            let _ = self.opts.save().map(|_| self.saved.clone_from(&self.opts));
            if let Some(scans) = &mut self.scans {
                for s in scans.iter_mut() {
                    s.remove(&p);
                }
            }
        }
        if close {
            self.review = None;
        }
    }
}

impl Tool for JunkCleaner {
    fn id(&self) -> &'static str {
        "junk"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
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
            }
        }

        let busy = self.scan_task.is_some() || self.clean_task.is_some();
        let picked: Vec<bool> = (0..self.rules.len())
            .map(|i| self.selected[i] && self.usable(i, ctx.elevated))
            .collect();
        let (selected_bytes, selected_items) = self.scans.as_ref().map_or((0, 0), |scans| {
            scans
                .iter()
                .enumerate()
                .filter(|(i, _)| picked[*i])
                .fold((0, 0), |(b, n), (_, s)| (b + s.bytes, n + s.count()))
        });

        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else if let Some(scans) = &self.scans {
                let work: Vec<(Rule, RuleScan)> = self
                    .rules
                    .iter()
                    .zip(scans)
                    .enumerate()
                    .filter(|(i, _)| picked[*i])
                    .map(|(_, (r, s))| (r.clone(), s.clone()))
                    .collect();
                let recycle = self.opts.use_recycle_bin;
                self.persist_selection();
                self.clean_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                    Ok(clean(&work, recycle))
                }));
            }
        }

        let clean_label = format!("Clean {}", format_bytes(selected_bytes));
        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Scan").enabled(!busy)),
                Some(ToolButton::new(icons::BROOM, &clean_label).enabled(!busy && selected_items > 0)),
                None,
                Some(ToolButton::new(icons::SLIDERS, "Options")),
            ],
        ) {
            Some(0) => {
                self.message = None;
                self.start_scan(ui.ctx(), ctx.elevated);
            }
            Some(1) => {
                let how = if self.opts.use_recycle_bin {
                    "moved to the Recycle Bin"
                } else {
                    "permanently deleted (this cannot be undone)"
                };
                self.confirm.ask(
                    format!(
                        "{selected_items} items ({}) will be {how}. Continue?",
                        format_bytes(selected_bytes)
                    ),
                    (),
                );
            }
            Some(3) => self.show_options = !self.show_options,
            _ => {}
        }
        if self.show_options {
            self.options_panel(ui);
        }
        self.drives_row(ui);
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                match &self.scan_task {
                    Some(t) => ui.weak(&t.status),
                    None => ui.weak("Cleaning…"),
                };
            });
        }
        show_message(ui, &self.message);
        self.review_panel(ui);
        ui.add_space(4.0);

        let mut groups: Vec<String> = Vec::new();
        for r in &self.rules {
            if !groups.contains(&r.group) {
                groups.push(r.group.clone());
            }
        }
        let mut rescan = false;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for group in groups {
                ui.strong(&group);
                egui::Grid::new(&group)
                    .num_columns(3)
                    .striped(true)
                    .spacing([16.0, 4.0])
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        for i in (0..self.rules.len()).filter(|&i| self.rules[i].group == group) {
                            let usable = self.usable(i, ctx.elevated);
                            let rule = &self.rules[i];
                            // Locked (admin-only) rules show unticked: they are not part of the cleanup.
                            let mut locked_off = false;
                            let state = if usable {
                                &mut self.selected[i]
                            } else {
                                &mut locked_off
                            };
                            let r = ui.add_enabled(usable, egui::Checkbox::new(state, &rule.title));
                            if r.changed()
                                && self.selected[i]
                                && (rule.kind.uses_drives() || rule.kind == Kind::InstallerOrphans)
                            {
                                rescan = true; // these are only scanned when ticked
                            }
                            let r = r.on_hover_text(&rule.description);
                            if !usable {
                                r.on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                            }
                            match self.scans.as_ref().map(|s| &s[i]) {
                                _ if !usable => {
                                    ui.weak("—");
                                    ui.weak("needs administrator");
                                }
                                Some(s) if rule.kind == Kind::RecycleBin => {
                                    ui.label(format_bytes(s.bytes));
                                    ui.label("");
                                }
                                Some(s) => {
                                    let folders_only = s.files.is_empty() && !s.dirs.is_empty();
                                    if folders_only {
                                        ui.label(format!("{} folders", s.dirs.len()));
                                    } else {
                                        ui.label(format_bytes(s.bytes));
                                    }
                                    if s.count() > 0 {
                                        let what = if folders_only {
                                            "Review".to_owned()
                                        } else {
                                            format!("{} items", s.count())
                                        };
                                        if ui
                                            .link(what)
                                            .on_hover_text("List and review these items")
                                            .clicked()
                                        {
                                            self.review = Some(i);
                                        }
                                    } else {
                                        ui.weak("nothing found");
                                    }
                                }
                                None => {
                                    ui.weak("…");
                                    ui.label("");
                                }
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(10.0);
            }
        });
        if rescan && !busy {
            self.start_scan(ui.ctx(), ctx.elevated);
        }
    }
}
