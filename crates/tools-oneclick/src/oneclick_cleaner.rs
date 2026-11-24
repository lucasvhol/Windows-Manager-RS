//! 1-Click Cleaner: scan junk files, dead registry entries and activity traces at
//! once, review the totals, and clean everything selected with one click.

use wm_core::{
    AppCtx, Category, Tool, egui,
    task::BackgroundTask,
    ui::{Confirm, Message, format_bytes, show_message},
};
use wm_tools_cleaner::junk::{Rule, RuleScan};
use wm_tools_cleaner::registry_cleaner::{self, Issue};
use wm_tools_security::privacy::{self, Trace};

use crate::actions;

const TRACES: [Trace; 3] = [Trace::RunHistory, Trace::TypedPaths, Trace::RecentFiles];

#[derive(Clone, Default)]
pub struct ScanResult {
    pub junk: Vec<(Rule, RuleScan)>,
    pub issues: Vec<Issue>,
    pub traces: usize,
}

impl ScanResult {
    pub fn junk_bytes(&self) -> u64 {
        self.junk.iter().map(|(_, s)| s.bytes).sum()
    }
}

pub struct OneClickCleaner {
    scan: Option<BackgroundTask<ScanResult>>,
    result: Option<ScanResult>,
    clean: Option<BackgroundTask<Option<wm_tools_cleaner::junk::CleanReport>>>,
    do_junk: bool,
    do_registry: bool,
    do_traces: bool,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for OneClickCleaner {
    fn default() -> Self {
        Self {
            scan: None,
            result: None,
            clean: None,
            do_junk: true,
            do_registry: true,
            do_traces: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl Tool for OneClickCleaner {
    fn id(&self) -> &'static str {
        "oneclickclean"
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.scan {
            t.poll();
            if let Some(r) = t.take_result() {
                self.scan = None;
                match r {
                    Ok(r) => self.result = Some(r),
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if let Some(t) = &mut self.clean {
            t.poll();
            if let Some(r) = t.take_result() {
                self.clean = None;
                // Registry and traces are quick and need the journal: do them now.
                let mut parts = vec![];
                match r {
                    Ok(Some(rep)) => {
                        let desc = format!(
                            "Junk cleanup: {} files deleted, {} freed",
                            rep.removed,
                            format_bytes(rep.freed)
                        );
                        let _ =
                            ctx.journal
                                .record("oneclick", desc.clone(), wm_core::safety::UndoAction::None);
                        parts.push(if rep.failed > 0 {
                            format!("{desc} ({} in use, skipped)", rep.failed)
                        } else {
                            desc
                        });
                    }
                    Ok(None) => parts.push("Junk files skipped".into()),
                    Err(e) => parts.push(format!("Junk files: {e}")),
                }
                if let Some(res) = &self.result {
                    if self.do_registry && !res.issues.is_empty() {
                        let usable: Vec<Issue> = res
                            .issues
                            .iter()
                            .filter(|i| !wm_core::registry::needs_admin(&i.key) || ctx.elevated)
                            .cloned()
                            .collect();
                        parts.push(match registry_cleaner::fix(ctx, &usable) {
                            Ok(n) => format!("Fixed {n} registry entries"),
                            Err(e) => format!("Registry: {e:#}"),
                        });
                    }
                    if self.do_traces {
                        let n: usize = TRACES.iter().filter_map(|t| privacy::clear(ctx, *t).ok()).sum();
                        parts.push(format!("Cleared {n} activity traces"));
                    }
                }
                self.message = Some(Ok(parts.join(". ") + "."));
                self.result = None;
            }
        }
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else if let Some(res) = &self.result {
                let junk = if self.do_junk {
                    res.junk.clone()
                } else {
                    Vec::new()
                };
                // Only the deletion runs in the background; the journal entry is written on the
                // UI thread through the app's single journal instance.
                self.clean = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                    Ok((!junk.is_empty()).then(|| wm_tools_cleaner::junk::clean(&junk, false)))
                }));
            }
        }

        ui.label("Scans junk files, dead registry entries and activity history in one go.");
        ui.add_space(6.0);
        let busy = self.scan.is_some() || self.clean.is_some();
        if ui
            .add_enabled(
                !busy,
                egui::Button::new(egui::RichText::new("Scan").size(18.0)).min_size(egui::vec2(140.0, 36.0)),
            )
            .clicked()
        {
            let elevated = ctx.elevated;
            self.message = None;
            self.result = None;
            self.scan = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                p.report(0.1, "Scanning junk files…");
                let junk = actions::scan_junk(&actions::default_junk_rules(elevated));
                p.report(0.6, "Checking the registry…");
                let issues = registry_cleaner::scan();
                let traces = TRACES.iter().map(|t| privacy::count(*t)).sum();
                Ok(ScanResult { junk, issues, traces })
            }));
        }
        if let Some(t) = &self.scan {
            ui.add(
                egui::ProgressBar::new(t.progress)
                    .desired_width(300.0)
                    .text(t.status.clone()),
            );
        }
        if self.clean.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Cleaning…");
            });
        }
        show_message(ui, &self.message);

        let Some(res) = self.result.clone() else { return };
        ui.add_space(8.0);
        egui::Grid::new("oneclick_summary")
            .num_columns(2)
            .spacing([24.0, 8.0])
            .show(ui, |ui| {
                ui.checkbox(
                    &mut self.do_junk,
                    "Junk files (temporary files, caches, error reports)",
                );
                ui.strong(format_bytes(res.junk_bytes()));
                ui.end_row();
                ui.checkbox(&mut self.do_registry, "Dead registry entries");
                ui.strong(res.issues.len().to_string());
                ui.end_row();
                ui.checkbox(
                    &mut self.do_traces,
                    "Run history, address bar history and recent files",
                );
                ui.strong(res.traces.to_string());
                ui.end_row();
            });
        ui.weak("Junk files are deleted permanently. Registry entries and activity traces can be restored from the Change Journal.");
        ui.add_space(6.0);
        if ui
            .add_enabled(
                !busy && (self.do_junk || self.do_registry || self.do_traces),
                egui::Button::new("Clean selected"),
            )
            .clicked()
        {
            self.confirm.ask("Clean the selected items now?", ());
        }
    }
}
