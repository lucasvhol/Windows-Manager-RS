//! 1-Click Cleaner: scan junk files, dead registry entries and activity traces at
//! once, review the totals, and clean everything selected with one click. The item
//! choice and options are remembered; silent mode cleans straight after the scan, and
//! Windows can shut down when the clean is done.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    tool_settings,
    ui::{Confirm, Message, ToolButton, format_bytes, frame, section, show_message},
};
use wm_tools_cleaner::junk::{CleanReport, Rule, RuleScan};
use wm_tools_cleaner::registry_cleaner::{self, Issue};
use wm_tools_cleaner::registry_defrag;
use wm_tools_security::privacy::{self, Trace};

use crate::actions;

const ID: &str = "oneclickclean";
/// Seconds Windows waits before shutting down, so the user can still cancel.
const SHUTDOWN_DELAY: u32 = 60;
/// Seconds before the restart that finishes a registry defrag. Longer than the
/// Registry Defrag tool's, as silent mode restarts without asking first.
const DEFRAG_DELAY: u32 = 60;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub junk: bool,
    pub registry: bool,
    /// One switch per entry of `Trace::ALL`.
    pub traces: [bool; 5],
    /// Clean as soon as the scan ends, without the review step.
    pub silent: bool,
    /// Shut Windows down after a clean started from the window.
    pub shutdown: bool,
    /// Compact the registry after a clean started from the window, then restart.
    /// Takes the place of `shutdown`; scheduled cleans never do it.
    pub defrag: bool,
    /// Unix time of the last clean.
    pub last_clean: Option<i64>,
    pub last_summary: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            junk: true,
            registry: true,
            traces: [true, true, true, false, false],
            silent: false,
            shutdown: false,
            defrag: false,
            last_clean: None,
            last_summary: String::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        tool_settings::load(ID)
    }

    pub fn selected_traces(&self) -> Vec<Trace> {
        Trace::ALL
            .iter()
            .zip(self.traces)
            .filter(|(_, on)| *on)
            .map(|(t, _)| *t)
            .collect()
    }

    pub fn anything(&self) -> bool {
        self.junk || self.registry || self.traces.contains(&true)
    }

    fn mark_done(&mut self, summary: &str) {
        self.last_clean = Some(chrono::Local::now().timestamp());
        self.last_summary = summary.to_owned();
    }

    pub fn last_clean_text(&self) -> String {
        match self
            .last_clean
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        {
            Some(t) => format!(
                "Last cleaned {}",
                t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
            ),
            None => "Not cleaned with this tool yet".into(),
        }
    }
}

#[derive(Clone, Default)]
pub struct ScanResult {
    pub junk: Vec<(Rule, RuleScan)>,
    pub issues: Vec<Issue>,
    /// Item count per entry of `Trace::ALL`.
    pub traces: [usize; 5],
}

impl ScanResult {
    pub fn junk_bytes(&self) -> u64 {
        self.junk.iter().map(|(_, s)| s.bytes).sum()
    }
}

/// Scans every item, so the counts are there even for unticked ones.
pub fn scan(elevated: bool, progress: &dyn Fn(f32, &str)) -> ScanResult {
    progress(0.1, "Scanning junk files…");
    let junk = actions::scan_junk(&actions::default_junk_rules(elevated));
    progress(0.6, "Checking the registry…");
    let issues = registry_cleaner::scan();
    progress(0.9, "Counting activity traces…");
    let traces = Trace::ALL.map(privacy::count);
    ScanResult { junk, issues, traces }
}

/// The steps that write to the change journal, run after the junk deletion (which
/// may run on a worker thread). Returns one line per item.
pub fn finish(
    ctx: &AppCtx<'_>,
    settings: &Settings,
    res: &ScanResult,
    junk: Result<Option<CleanReport>, String>,
) -> Vec<String> {
    let mut parts = vec![];
    match junk {
        Ok(Some(rep)) => {
            let desc = format!(
                "Junk files: {} deleted, {} freed",
                rep.removed,
                format_bytes(rep.freed)
            );
            let _ = ctx.journal.record("oneclick", desc.clone(), UndoAction::None);
            parts.push(if rep.failed > 0 {
                format!("{desc} ({} in use, skipped)", rep.failed)
            } else {
                desc
            });
        }
        Ok(None) => {}
        Err(e) => parts.push(format!("Junk files: {e}")),
    }
    if settings.registry {
        let usable: Vec<Issue> = res
            .issues
            .iter()
            .filter(|i| !wm_core::registry::needs_admin(&i.key) || ctx.elevated)
            .cloned()
            .collect();
        let skipped = res.issues.len() - usable.len();
        parts.push(match registry_cleaner::fix(ctx, &usable) {
            Ok(n) if skipped > 0 => {
                format!("Registry: {n} entries fixed ({skipped} need administrator rights)")
            }
            Ok(n) => format!("Registry: {n} entries fixed"),
            Err(e) => format!("Registry: {e:#}"),
        });
    }
    let traces = settings.selected_traces();
    if !traces.is_empty() {
        let mut n = 0;
        let mut errors = vec![];
        for t in traces {
            match privacy::clear(ctx, t) {
                Ok(c) => n += c,
                Err(e) => errors.push(format!("{}: {e:#}", t.label())),
            }
        }
        let mut line = format!("Privacy: {n} traces cleared");
        if !errors.is_empty() {
            line += &format!(" ({})", errors.join("; "));
        }
        parts.push(line);
    }
    parts
}

/// Scheduled run (`--run oneclick-clean`): scans and cleans the saved selection.
/// It never shuts Windows down; that option belongs to cleans started by hand.
pub fn run_saved(ctx: &AppCtx<'_>) -> Result<String> {
    let mut settings = Settings::load();
    if !settings.anything() {
        anyhow::bail!("nothing is selected in the 1-Click Cleaner");
    }
    let res = scan(ctx.elevated, &|_, _| {});
    let junk = settings
        .junk
        .then(|| wm_tools_cleaner::junk::clean(&res.junk, false));
    let summary = finish(ctx, &settings, &res, Ok(junk)).join(". ") + ".";
    settings.mark_done(&summary);
    tool_settings::save(ID, &settings)?;
    Ok(summary)
}

fn schedule_shutdown() -> Result<()> {
    let delay = SHUTDOWN_DELAY.to_string();
    run_checked(hidden_command("shutdown").args([
        "/s",
        "/t",
        &delay,
        "/c",
        "Win Manager RS finished cleaning. Windows will shut down in one minute.",
    ]))?;
    Ok(())
}

fn cancel_shutdown() -> Result<()> {
    run_checked(hidden_command("shutdown").arg("/a"))?;
    Ok(())
}

pub struct OneClickCleaner {
    settings: Settings,
    scan: Option<BackgroundTask<ScanResult>>,
    /// The running scan was started in silent mode and cleans when it ends.
    clean_after_scan: bool,
    result: Option<ScanResult>,
    clean: Option<BackgroundTask<Option<CleanReport>>>,
    defrag: Option<BackgroundTask<registry_defrag::Scheduled>>,
    shutdown_pending: bool,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for OneClickCleaner {
    fn default() -> Self {
        Self {
            settings: Settings::load(),
            scan: None,
            clean_after_scan: false,
            result: None,
            clean: None,
            defrag: None,
            shutdown_pending: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl OneClickCleaner {
    fn save(&mut self) {
        if let Err(e) = tool_settings::save(ID, &self.settings) {
            self.message = Some(Err(format!("{e:#}")));
        }
    }

    fn start_clean(&mut self, ui: &egui::Ui, dry_run: bool) {
        if dry_run {
            self.message = Some(Ok("Dry run: nothing was cleaned.".into()));
            return;
        }
        let Some(res) = &self.result else { return };
        let junk = if self.settings.junk {
            res.junk.clone()
        } else {
            Vec::new()
        };
        // Only the deletion runs in the background; the journal entries are written on
        // the UI thread through the app's single journal instance.
        self.clean = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
            Ok((!junk.is_empty()).then(|| wm_tools_cleaner::junk::clean(&junk, false)))
        }));
    }

    fn poll(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.scan {
            t.poll();
            if let Some(r) = t.take_result() {
                self.scan = None;
                match r {
                    Ok(r) => {
                        self.result = Some(r);
                        if std::mem::take(&mut self.clean_after_scan) {
                            self.start_clean(ui, ctx.config.dry_run);
                        }
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if let Some(t) = &mut self.clean {
            t.poll();
            if let Some(r) = t.take_result() {
                self.clean = None;
                let res = self.result.take().unwrap_or_default();
                let parts = finish(ctx, &self.settings, &res, r);
                let summary = parts.join(". ") + ".";
                self.settings.mark_done(&summary);
                self.save();
                self.message = Some(Ok(summary));
                if self.settings.defrag && ctx.elevated {
                    self.defrag = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                        registry_defrag::compact(&registry_defrag::hives())
                    }));
                } else if self.settings.shutdown {
                    match schedule_shutdown() {
                        Ok(()) => self.shutdown_pending = true,
                        Err(e) => self.message = Some(Err(format!("Shutting down: {e:#}"))),
                    }
                }
            }
        }
        if let Some(t) = &mut self.defrag {
            t.poll();
            if let Some(r) = t.take_result() {
                self.defrag = None;
                let cleaned = match &self.message {
                    Some(Ok(s)) => s.clone(),
                    _ => String::new(),
                };
                self.message = Some(match r {
                    Ok(s) => registry_defrag::finish_compact(ctx, &s, DEFRAG_DELAY)
                        .map(|m| format!("{cleaned} {m}")),
                    Err(e) => Err(format!("{cleaned} Registry defrag: {e}")),
                });
            }
        }
    }
}

fn count_label(ui: &mut egui::Ui, text: Option<String>) {
    match text {
        Some(t) => ui.strong(t),
        None => ui.weak("—"),
    };
}

impl Tool for OneClickCleaner {
    fn id(&self) -> &'static str {
        ID
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.poll(ui, ctx);
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            self.start_clean(ui, ctx.config.dry_run);
        }

        let busy = self.scan.is_some() || self.clean.is_some() || self.defrag.is_some();
        let scan_label = if self.settings.silent {
            "Scan and clean"
        } else {
            "Scan"
        };
        let can_clean = self.result.is_some() && !self.settings.silent;
        match frame::toolbar(
            ui,
            &[
                Some(
                    ToolButton::new(icons::MAGNIFYING_GLASS, scan_label)
                        .enabled(!busy && self.settings.anything()),
                ),
                Some(
                    ToolButton::new(icons::BROOM, "Clean selected")
                        .enabled(can_clean && !busy && self.settings.anything()),
                ),
            ],
            |_| {},
        ) {
            Some(0) => {
                let elevated = ctx.elevated;
                self.message = None;
                self.result = None;
                self.clean_after_scan = self.settings.silent;
                self.scan = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    Ok(scan(elevated, &|f, s| p.report(f, s)))
                }));
            }
            Some(1) => {
                let question = if self.settings.defrag && ctx.elevated {
                    "Clean the selected items now? The registry is then compacted and Windows restarts."
                } else if self.settings.shutdown {
                    "Clean the selected items now? Windows shuts down when the clean is done."
                } else {
                    "Clean the selected items now?"
                };
                self.confirm.ask(question, ());
            }
            _ => {}
        }

        let status = if let Some(t) = &self.scan {
            t.status.clone()
        } else if self.clean.is_some() {
            "Cleaning…".to_owned()
        } else if self.defrag.is_some() {
            "Compacting the registry…".to_owned()
        } else if self.settings.last_summary.is_empty() {
            self.settings.last_clean_text()
        } else {
            format!(
                "{} · {}",
                self.settings.last_clean_text(),
                self.settings.last_summary
            )
        };
        let progress = self.scan.as_ref().map(|t| t.progress);
        let mut cancel_shutdown_now = false;
        let shutdown_pending = self.shutdown_pending;
        frame::status(ui, &status, |ui| {
            if shutdown_pending {
                cancel_shutdown_now = ui.button("Cancel shutdown").clicked();
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Windows shuts down in {SHUTDOWN_DELAY} seconds."),
                );
            }
            if let Some(p) = progress {
                ui.add(egui::ProgressBar::new(p).desired_width(200.0).show_percentage());
            } else if busy {
                ui.spinner();
            }
        });
        if cancel_shutdown_now {
            self.shutdown_pending = false;
            self.message = Some(
                cancel_shutdown()
                    .map(|()| "Shutdown cancelled.".into())
                    .map_err(|e| format!("Cancelling the shutdown: {e:#}")),
            );
        }

        let before = self.settings.clone();
        let res = self.result.clone();
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            section(
                ui,
                "Items to clean",
                |_| {},
                |ui| {
                    egui::Grid::new("oneclick_items")
                        .num_columns(2)
                        .spacing([32.0, 6.0])
                        .show(ui, |ui| {
                            ui.checkbox(
                                &mut self.settings.junk,
                                "Junk files (temporary files, caches, error reports)",
                            );
                            count_label(ui, res.as_ref().map(|r| format_bytes(r.junk_bytes())));
                            ui.end_row();
                            ui.checkbox(&mut self.settings.registry, "Dead registry entries");
                            count_label(ui, res.as_ref().map(|r| r.issues.len().to_string()));
                            ui.end_row();

                            let mut all = self.settings.traces.iter().all(|t| *t);
                            let partial = !all && self.settings.traces.contains(&true);
                            if ui
                                .add(egui::Checkbox::new(&mut all, "Privacy traces").indeterminate(partial))
                                .changed()
                            {
                                self.settings.traces = [all; 5];
                            }
                            count_label(
                                ui,
                                res.as_ref().map(|r| {
                                    let n: usize = r
                                        .traces
                                        .iter()
                                        .zip(self.settings.traces)
                                        .filter(|(_, on)| *on)
                                        .map(|(n, _)| n)
                                        .sum();
                                    format!("{n} selected")
                                }),
                            );
                            ui.end_row();
                            for (i, t) in Trace::ALL.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.add_space(24.0);
                                    ui.checkbox(&mut self.settings.traces[i], t.label());
                                });
                                count_label(ui, res.as_ref().map(|r| r.traces[i].to_string()));
                                ui.end_row();
                            }
                        });
                    ui.add_space(4.0);
                    ui.weak(
                    "Junk files are deleted permanently. Registry entries and activity traces (except the clipboard) can be restored from the Change Journal.",
                );
                },
            );
            ui.add_space(8.0);
            section(
                ui,
                "Options",
                |_| {},
                |ui| {
                    ui.checkbox(&mut self.settings.silent, "Silent mode")
                        .on_hover_text("Clean as soon as the scan ends, without showing the results first.");
                    ui.checkbox(&mut self.settings.shutdown, "Shut down Windows when done")
                    .on_hover_text(format!(
                        "Windows shuts down {SHUTDOWN_DELAY} seconds after the clean, and you can still cancel. Scheduled cleans never shut down."
                    ));
                    ui.add_enabled(
                    ctx.elevated,
                    egui::Checkbox::new(&mut self.settings.defrag, "Compact the registry, then restart"),
                )
                .on_hover_text(format!(
                    "After the clean, the registry files are rebuilt without their empty space and Windows restarts {DEFRAG_DELAY} seconds later to finish (instead of shutting down). Don't change settings before the restart. Scheduled cleans never do this."
                ))
                .on_disabled_hover_text("Needs the app to run as administrator.");
                },
            );
        });
        if self.settings != before {
            self.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_tolerate_old_files() {
        let mut s = Settings {
            silent: true,
            traces: [false, true, false, true, false],
            ..Default::default()
        };
        s.mark_done("Registry: 2 entries fixed.");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.selected_traces(), vec![Trace::TypedPaths, Trace::JumpLists]);
        assert!(back.last_clean_text().starts_with("Last cleaned "));

        // A file from an older version (or an empty one) keeps the defaults.
        let old: Settings = serde_json::from_str(r#"{"junk":false}"#).unwrap();
        assert!(!old.junk && old.registry && !old.shutdown);
        assert_eq!(old.last_clean_text(), "Not cleaned with this tool yet");
    }

    #[test]
    fn nothing_selected_means_nothing_to_do() {
        let s = Settings {
            junk: false,
            registry: false,
            traces: [false; 5],
            ..Default::default()
        };
        assert!(!s.anything());
        assert!(Settings::default().anything());
    }

    #[test]
    fn finish_with_nothing_selected_touches_nothing() {
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
        let s = Settings {
            junk: false,
            registry: false,
            traces: [false; 5],
            ..Default::default()
        };
        let parts = finish(&ctx, &s, &ScanResult::default(), Ok(None));
        assert!(parts.is_empty());
        assert!(journal.entries().is_empty());
    }
}
