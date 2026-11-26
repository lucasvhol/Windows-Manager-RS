//! Repair Center: sections of described repair boxes (system files, apps, blocked
//! tools, security settings, associations, icons, network, reset and more). Boxes
//! that can tell whether they are needed show a status line. Each run is journaled
//! with its undo steps and leaves a log file.

mod catalog;

use std::collections::HashMap;
use std::path::PathBuf;

use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, paths,
    safety::UndoAction,
    shell::shell_open,
    task::BackgroundTask,
    ui::{Confirm, Message, frame, numbered_tabs, request_open_tool, section_nav, show_message},
};

use crate::fixer::{self, Outcome};
use catalog::{Health, How, Repair, Section};

/// Repair boxes per page (the original shows three full-width boxes per tab).
const PAGE: usize = 3;

pub struct RepairCenter {
    sections: Vec<Section>,
    section: usize,
    page: usize,
    health: HashMap<&'static str, Health>,
    /// Section the health map was computed for.
    health_for: Option<usize>,
    running: Option<(Repair, BackgroundTask<Outcome>)>,
    log: Vec<String>,
    logs: Vec<(PathBuf, String)>,
    confirm: Confirm<Repair>,
    message: Message,
}

impl Default for RepairCenter {
    fn default() -> Self {
        Self {
            sections: catalog::sections(),
            section: 0,
            page: 0,
            health: HashMap::new(),
            health_for: None,
            running: None,
            log: Vec::new(),
            logs: Vec::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

/// Saved repair logs, newest first: (path, "date · repair").
fn list_logs() -> Vec<(PathBuf, String)> {
    let Ok(dir) = paths::logs_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut logs: Vec<(PathBuf, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix("repair-")?.strip_suffix(".txt")?;
            // repair-YYYYMMDD-HHMMSS.mmm-<id>.txt
            let (date, time_id) = rest.split_once('-')?;
            let (time, id) = time_id.split_once('-')?;
            let label = format!(
                "{}-{}-{} {}:{} · {}",
                date.get(..4)?,
                date.get(4..6)?,
                date.get(6..8)?,
                time.get(..2)?,
                time.get(2..4)?,
                catalog::find(id).map_or(id, |r| r.title)
            );
            Some((e.path(), label))
        })
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0));
    logs
}

fn save_log(id: &str, lines: &[String]) -> anyhow::Result<PathBuf> {
    let path = paths::logs_dir()?.join(format!("repair-{}-{id}.txt", fixer::stamp()));
    std::fs::write(&path, lines.join("\r\n"))?;
    Ok(path)
}

/// One described repair box, full width like the original's: title, description and
/// status on the left, its button at the right. Returns true when the button is pressed.
fn repair_box(ui: &mut egui::Ui, r: &Repair, health: Option<&Health>, enabled: bool, hint: &str) -> bool {
    let mut clicked = false;
    wm_core::ui::section(
        ui,
        r.title,
        |_| {},
        |ui| {
            ui.add(egui::Label::new(egui::RichText::new(r.description).weak()).wrap());
            ui.add_space(2.0);
            match health {
                Some(Health::Fine(t)) => {
                    ui.colored_label(egui::Color32::from_rgb(70, 160, 90), format!("✔ {t}"));
                }
                Some(Health::Problem(t)) => {
                    ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {t}"));
                }
                Some(Health::Info(t)) => {
                    ui.weak(t);
                }
                None => {}
            }
            // Notes on the left and the button at the bottom right, as in the original.
            ui.horizontal(|ui| {
                let mut notes = Vec::new();
                if r.admin {
                    notes.push("administrator");
                }
                if r.restart {
                    notes.push("restart required");
                }
                if !notes.is_empty() {
                    ui.weak(notes.join(" · "));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = match r.how {
                        How::Run if r.id == "advstartup" => "Restart",
                        How::Run => "Fix",
                        How::Open(..) | How::Tool(_) => "Open",
                    };
                    let mut b =
                        ui.add_enabled(enabled, egui::Button::new(label).min_size(egui::vec2(96.0, 28.0)));
                    if r.admin {
                        b = b.on_disabled_hover_text(hint);
                    }
                    clicked = b.clicked();
                });
            });
        },
    );
    clicked
}

impl RepairCenter {
    fn poll(&mut self, ctx: &mut AppCtx<'_>) {
        let Some((r, t)) = &mut self.running else { return };
        t.poll();
        let Some(res) = t.take_result() else { return };
        let r = *r;
        self.running = None;
        self.health_for = None;
        match res {
            Ok(out) => {
                for (what, undo) in out.undo {
                    let _ = ctx.journal.record("repair", what, undo);
                }
                let failed = out.log.iter().any(|l| l.starts_with("  failed:"));
                self.log = out.log;
                let saved = save_log(r.id, &self.log);
                self.message = Some(match (failed, saved) {
                    (_, Err(e)) => Err(format!(
                        "{}: finished, but the log was not saved ({e:#}).",
                        r.title
                    )),
                    (true, _) => Err(format!("{}: finished with errors. See the log.", r.title)),
                    (false, _) => Ok(format!("{}: done.", r.title)),
                });
            }
            Err(e) => {
                self.log.push(format!("  failed: {e}"));
                let _ = save_log(r.id, &self.log);
                self.message = Some(Err(format!("{}: {e}", r.title)));
            }
        }
    }

    fn start(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>, r: Repair) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let _ = ctx
            .journal
            .record("repair", format!("Ran repair: {}", r.title), UndoAction::None);
        self.message = None;
        self.log = vec![format!("== {} ==", r.title)];
        self.running = Some((
            r,
            BackgroundTask::spawn(ui.ctx(), move |p| {
                let mut out = Outcome {
                    log: vec![format!("== {} ==", r.title)],
                    ..Default::default()
                };
                let mut status = |s: &str| p.report(0.0, s.to_owned());
                if let Err(e) = catalog::apply(r.id, &mut out, &mut status) {
                    out.log.push(format!("  failed: {e:#}"));
                }
                Ok(out)
            }),
        ));
    }

    fn press(&mut self, ui: &egui::Ui, r: Repair) {
        match r.how {
            How::Run => {
                let mut q = format!("{}?\n\n{}", r.title, r.description);
                if r.restart {
                    q += "\n\nA restart is needed for this to take full effect.";
                }
                self.confirm.ask(q, r);
            }
            How::Open(target, args) => {
                self.message = shell_open(target, args)
                    .err()
                    .map(|e| Err(format!("{}: {e:#}", r.title)));
            }
            How::Tool(id) => request_open_tool(ui.ctx(), id),
        }
    }

    fn repairs_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if self.health_for != Some(self.section) {
            self.health = self.sections[self.section]
                .repairs
                .iter()
                .filter_map(|r| catalog::health(r.id).map(|h| (r.id, h)))
                .collect();
            self.health_for = Some(self.section);
        }
        let section = &self.sections[self.section];
        let pages = section.repairs.len().div_ceil(PAGE);
        if pages > 1 {
            // Like the original: "1 System Repair", "2 System Repair", …
            numbered_tabs(ui, &vec![section.title; pages], &mut self.page);
            ui.add_space(4.0);
        }
        self.page = self.page.min(pages.saturating_sub(1));
        let hint = ctx.i18n.tr("shell-needs-admin");
        let busy = self.running.is_some();
        let mut pressed = None;
        let shown: Vec<Repair> = section
            .repairs
            .iter()
            .skip(self.page * PAGE)
            .take(PAGE)
            .copied()
            .collect();
        egui::ScrollArea::vertical()
            .id_salt("repair-boxes")
            .max_height((ui.available_height() - 150.0).max(240.0))
            .show(ui, |ui| {
                for r in &shown {
                    let enabled = (!r.admin || ctx.elevated) && !(busy && r.how == How::Run);
                    if repair_box(ui, r, self.health.get(r.id), enabled, &hint) {
                        pressed = Some(*r);
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(r) = pressed {
            self.press(ui, r);
        }
    }

    fn logs_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Each repair saves its log here.");
            if ui.button("Refresh").clicked() {
                self.logs = list_logs();
            }
            if ui.button("Open logs folder").clicked() {
                self.message = paths::logs_dir()
                    .and_then(|d| shell_open(&d.to_string_lossy(), ""))
                    .err()
                    .map(|e| Err(format!("{e:#}")));
            }
        });
        ui.add_space(4.0);
        if self.logs.is_empty() {
            ui.weak("No repairs have been run yet.");
            return;
        }
        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("repair-logs")
            .max_height((ui.available_height() - 40.0).max(200.0))
            .show(ui, |ui| {
                egui::Grid::new("repair-logs-grid")
                    .striped(true)
                    .num_columns(2)
                    .show(ui, |ui| {
                        for (path, label) in &self.logs {
                            ui.label(label);
                            if ui.small_button("Open").clicked() {
                                open = Some(path.clone());
                            }
                            ui.end_row();
                        }
                    });
            });
        if let Some(p) = open {
            self.message = shell_open(&p.to_string_lossy(), "")
                .err()
                .map(|e| Err(format!("{e:#}")));
        }
    }
}

impl Tool for RepairCenter {
    fn id(&self) -> &'static str {
        "repair"
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.poll(ctx);
        if let Some(r) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.start(ui, ctx, r);
        }
        let logs_index = self.sections.len() + 1;
        let mut nav: Vec<(&str, &str)> = self.sections.iter().map(|s| (s.icon, s.title)).collect();
        nav.push(("", ""));
        nav.push((icons::LIST, "Repair Logs"));
        if frame::left(ui, 190.0, |ui| section_nav(ui, &nav, &mut self.section)) {
            self.page = 0;
            if self.section == logs_index {
                self.logs = list_logs();
            }
        }
        // Like the original, the status bar names the section shown.
        let status = match &self.running {
            Some((r, t)) => format!("{}: {}", r.title, t.status),
            None => self
                .sections
                .get(self.section)
                .map_or("Repair Logs", |s| s.title)
                .to_owned(),
        };
        let running = self.running.is_some();
        let elevated = ctx.elevated;
        frame::status(ui, &status, |ui| {
            if running {
                ui.spinner();
            } else if !elevated {
                ui.weak("Repairs marked \"administrator\" need the app to run as administrator.");
            }
        });
        frame::central(ui, |ui| self.page_body(ui, ctx, logs_index));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl RepairCenter {
    fn page_body(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>, logs_index: usize) {
        if self.section == logs_index {
            self.logs_page(ui);
        } else {
            self.repairs_page(ui, ctx);
        }
        show_message(ui, &self.message);
        if self.log.len() > 1 && self.section != logs_index {
            egui::CollapsingHeader::new(format!("Log ({} lines)", self.log.len()))
                .default_open(self.running.is_none())
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("repair-log")
                        .max_height(160.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(self.log.join("\n")).monospace().small(),
                                )
                                .selectable(true),
                            );
                        });
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_logs_are_listed_with_their_repair() {
        let path = save_log("dns", &["== Flush DNS cache ==".into(), "  ok".into()]).unwrap();
        let logs = list_logs();
        let found = logs.iter().find(|(p, _)| *p == path).expect("listed");
        assert!(found.1.ends_with("· Flush DNS cache"), "{}", found.1);
        std::fs::remove_file(path).unwrap();
    }
}
