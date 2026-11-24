//! Component Store Cleaner: analyze and clean up the WinSxS component store with DISM,
//! and check or repair its health (CheckHealth, ScanHealth, RestoreHealth).

use anyhow::{Result, bail};
use wm_core::{
    AppCtx, Category, Tool,
    command::{percent_in, run_streaming},
    egui, icons,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, show_message, toolbar},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Analyze,
    Cleanup { reset_base: bool },
    CheckHealth,
    ScanHealth,
    RestoreHealth { limit_access: bool },
}

impl Op {
    pub fn args(self) -> Vec<&'static str> {
        let mut a = vec!["/Online", "/Cleanup-Image"];
        match self {
            Op::Analyze => a.push("/AnalyzeComponentStore"),
            Op::Cleanup { reset_base } => {
                a.push("/StartComponentCleanup");
                if reset_base {
                    a.push("/ResetBase");
                }
            }
            Op::CheckHealth => a.push("/CheckHealth"),
            Op::ScanHealth => a.push("/ScanHealth"),
            Op::RestoreHealth { limit_access } => {
                a.push("/RestoreHealth");
                if limit_access {
                    a.push("/LimitAccess");
                }
            }
        }
        a
    }

    fn title(self) -> &'static str {
        match self {
            Op::Analyze => "Analyzing the component store",
            Op::Cleanup { .. } => "Cleaning up superseded components",
            Op::CheckHealth => "Checking for recorded corruption",
            Op::ScanHealth => "Scanning the component store",
            Op::RestoreHealth { .. } => "Repairing the component store",
        }
    }

    /// Operations that change the system (journaled).
    fn modifies(self) -> bool {
        matches!(self, Op::Cleanup { .. } | Op::RestoreHealth { .. })
    }
}

/// `key : value` lines from DISM output (labels are localised, so they are shown as-is).
pub fn summary_lines(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|l| l.split_once(" : "))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

/// Health verdict from the exit code and the (localised) text. DISM exits 0 for both
/// "healthy" and "repairable", so the English wording is checked; other languages fall
/// back to showing the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Repairable,
    NotRepairable,
    Repaired,
    Unknown,
}

pub fn health_of(op: Op, lines: &[String]) -> Health {
    let text = lines.join("\n").to_ascii_lowercase();
    if text.contains("is not repairable") {
        Health::NotRepairable
    } else if text.contains("restore operation completed successfully") {
        Health::Repaired
    } else if text.contains("is repairable") {
        Health::Repairable
    } else if text.contains("no component store corruption detected") {
        Health::Healthy
    } else if matches!(op, Op::RestoreHealth { .. }) && text.contains("completed successfully") {
        Health::Repaired
    } else {
        Health::Unknown
    }
}

pub struct Outcome {
    op: Op,
    lines: Vec<String>,
}

fn run(op: Op, progress: &wm_core::task::Progress<Outcome>) -> Result<Outcome> {
    let (code, lines) = run_streaming("dism", &op.args(), |s| {
        let pct = percent_in(s).unwrap_or(0.0) / 100.0;
        progress.report(pct, s.to_owned());
    })?;
    if code != 0 {
        let tail: Vec<&str> = lines
            .iter()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(3)
            .map(String::as_str)
            .collect();
        bail!(
            "DISM failed (exit code {code:#x}): {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" ")
        );
    }
    Ok(Outcome { op, lines })
}

#[derive(Default)]
pub struct ComponentStoreCleaner {
    task: Option<(Op, BackgroundTask<Outcome>)>,
    last: Option<Outcome>,
    reset_base: bool,
    limit_access: bool,
    confirm: Confirm<Op>,
    message: Message,
}

impl ComponentStoreCleaner {
    fn start(&mut self, ctx: &egui::Context, op: Op) {
        self.message = None;
        self.task = Some((op, BackgroundTask::spawn(ctx, move |p| run(op, p))));
    }

    fn poll(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let Some((op, t)) = &mut self.task else { return };
        t.poll();
        let op = *op;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.strong(format!("{}…", op.title()));
            });
            ui.add(egui::ProgressBar::new(t.progress).show_percentage());
            ui.weak(if t.status.is_empty() {
                "Starting DISM…"
            } else {
                &t.status
            });
            ui.weak("DISM cannot be stopped safely once started; closing this window does not stop it.");
        });
        let Some(r) = t.take_result() else { return };
        self.task = None;
        match r {
            Ok(out) => {
                if op.modifies() {
                    let what = match op {
                        Op::Cleanup { reset_base: true } => "Component store cleanup with /ResetBase",
                        Op::Cleanup { .. } => "Component store cleanup",
                        _ => "Component store repair (RestoreHealth)",
                    };
                    let _ = ctx.journal.record("winsxs", what, UndoAction::None);
                }
                self.message = Some(Ok(match (op, health_of(op, &out.lines)) {
                    (Op::Analyze, _) => "Analysis finished.".into(),
                    (Op::Cleanup { .. }, _) => "Cleanup finished. Run Analyze to see the new size.".into(),
                    (_, Health::Healthy) => "No component store corruption was found.".into(),
                    (_, Health::Repairable) => "The component store is damaged but repairable. Use Repair (RestoreHealth).".into(),
                    (_, Health::NotRepairable) => "The component store is damaged and cannot be repaired here; an in-place upgrade of Windows is the usual fix.".into(),
                    (_, Health::Repaired) => "The component store was repaired. Run Check system files (SFC) in Repair Center next.".into(),
                    (_, Health::Unknown) => "Finished. See the DISM output below.".into(),
                }));
                self.last = Some(out);
            }
            Err(e) => self.message = Some(Err(e)),
        }
    }
}

impl Tool for ComponentStoreCleaner {
    fn id(&self) -> &'static str {
        "winsxs"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if !ctx.elevated {
            ui.label(
                "The component store (WinSxS) keeps Windows components and the older versions \
                 that updates replaced, so updates can be uninstalled.",
            );
            ui.weak(ctx.i18n.tr("shell-needs-admin"));
            return;
        }
        if let Some(op) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                self.start(ui.ctx(), op);
            }
        }

        let busy = self.task.is_some();
        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Analyze").enabled(!busy)),
                Some(ToolButton::new(icons::BROOM, "Clean up").enabled(!busy)),
                None,
                Some(ToolButton::new(icons::SHIELD_CHECK, "Quick check").enabled(!busy)),
                Some(ToolButton::new(icons::FILE_SEARCH, "Scan health").enabled(!busy)),
                Some(ToolButton::new(icons::FIRST_AID_KIT, "Repair").enabled(!busy)),
            ],
        ) {
            Some(0) => self.start(ui.ctx(), Op::Analyze),
            Some(1) => {
                let extra = if self.reset_base {
                    " With /ResetBase, the updates installed now can no longer be uninstalled."
                } else {
                    ""
                };
                self.confirm.ask(
                    format!("Remove superseded components? This can take 10 minutes or more.{extra}"),
                    Op::Cleanup {
                        reset_base: self.reset_base,
                    },
                );
            }
            Some(3) => self.start(ui.ctx(), Op::CheckHealth),
            Some(4) => self.start(ui.ctx(), Op::ScanHealth),
            Some(5) => self.confirm.ask(
                if self.limit_access {
                    "Repair the component store from local sources only? This can take 15 minutes or more."
                } else {
                    "Repair the component store? Missing files are downloaded from Windows Update. \
                     This can take 15 minutes or more."
                },
                Op::RestoreHealth {
                    limit_access: self.limit_access,
                },
            ),
            _ => {}
        }

        ui.horizontal(|ui| {
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut self.reset_base, "Clean up: also reset the base (/ResetBase)"),
            )
            .on_hover_text("Frees more space, but the updates installed now can no longer be uninstalled.");
            ui.separator();
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(
                    &mut self.limit_access,
                    "Repair: don't use Windows Update (/LimitAccess)",
                ),
            );
        });
        ui.weak(
            "Analyze shows the store's real size and whether cleanup is recommended. Quick check reads \
             corruption Windows has already recorded (seconds); Scan health checks every component \
             (several minutes); Repair fixes what the scan found.",
        );
        ui.add_space(4.0);

        self.poll(ui, ctx);
        show_message(ui, &self.message);

        if let Some(out) = &self.last {
            ui.add_space(6.0);
            let text = out.lines.join("\n");
            let lines = summary_lines(&text);
            if out.op == Op::Analyze && !lines.is_empty() {
                egui::Grid::new("dism_summary")
                    .num_columns(2)
                    .striped(true)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for (k, v) in &lines {
                            ui.weak(k);
                            ui.strong(v);
                            ui.end_row();
                        }
                    });
            }
            egui::CollapsingHeader::new("Full DISM output")
                .default_open(out.op != Op::Analyze)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).monospace().small()).selectable(true),
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
    fn args_and_summary() {
        assert_eq!(Op::Analyze.args().last(), Some(&"/AnalyzeComponentStore"));
        assert_eq!(
            Op::Cleanup { reset_base: true }.args().last(),
            Some(&"/ResetBase")
        );
        assert_eq!(
            Op::RestoreHealth { limit_access: true }.args()[2..],
            ["/RestoreHealth", "/LimitAccess"]
        );
        assert_eq!(Op::ScanHealth.args().last(), Some(&"/ScanHealth"));
        let out = "Component Store (WinSxS) information:\n\nWindows Explorer Reported Size of Component Store : 7.21 GB\n\
                   Component Store Cleanup Recommended : Yes\nThe operation completed successfully.";
        let s = summary_lines(out);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1], ("Component Store Cleanup Recommended".into(), "Yes".into()));
    }

    #[test]
    fn health_verdicts() {
        let l = |s: &str| vec![s.to_owned(), "The operation completed successfully.".to_owned()];
        assert_eq!(
            health_of(Op::ScanHealth, &l("No component store corruption detected.")),
            Health::Healthy
        );
        assert_eq!(
            health_of(Op::CheckHealth, &l("The component store is repairable.")),
            Health::Repairable
        );
        assert_eq!(
            health_of(Op::ScanHealth, &l("The component store is not repairable.")),
            Health::NotRepairable
        );
        assert_eq!(
            health_of(
                Op::RestoreHealth { limit_access: false },
                &l("The restore operation completed successfully.")
            ),
            Health::Repaired
        );
        assert_eq!(health_of(Op::ScanHealth, &l("Unbekannt")), Health::Unknown);
    }
}
