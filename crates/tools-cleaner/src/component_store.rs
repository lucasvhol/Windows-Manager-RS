//! Component Store Cleaner: analyze and clean up the WinSxS component store with DISM.

use anyhow::Result;
use wm_core::{
    AppCtx, Category, Tool, egui,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Analyze,
    Cleanup { reset_base: bool },
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
        }
        a
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

fn run(op: Op) -> Result<String> {
    let out = run_checked(hidden_command("dism").args(op.args()))?;
    // DISM redraws its progress bar with '\r'; keep only final lines.
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.rsplit('\r').next().unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n"))
}

#[derive(Default)]
pub struct ComponentStoreCleaner {
    task: Option<(Op, BackgroundTask<String>)>,
    output: Option<String>,
    reset_base: bool,
    confirm: Confirm<Op>,
    message: Message,
}

impl ComponentStoreCleaner {
    fn start(&mut self, ctx: &egui::Context, op: Op) {
        self.message = None;
        self.task = Some((op, BackgroundTask::spawn(ctx, move |_| run(op))));
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
        ui.label(
            "The component store (WinSxS) keeps superseded Windows components so updates can be \
             uninstalled. Cleanup removes components replaced by newer updates.",
        );
        if !ctx.elevated {
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
        if let Some((op, t)) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                let op = *op;
                self.task = None;
                match r {
                    Ok(out) => {
                        if let Op::Cleanup { reset_base } = op {
                            let _ = ctx.journal.record(
                                "winsxs",
                                format!(
                                    "Component store cleanup{}",
                                    if reset_base { " with /ResetBase" } else { "" }
                                ),
                                UndoAction::None,
                            );
                            self.message = Some(Ok("Cleanup finished.".into()));
                        }
                        self.output = Some(out);
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }

        ui.add_space(8.0);
        let busy = self.task.is_some();
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Analyze")).clicked() {
                self.start(ui.ctx(), Op::Analyze);
            }
            if ui.add_enabled(!busy, egui::Button::new("Clean up")).clicked() {
                let op = Op::Cleanup {
                    reset_base: self.reset_base,
                };
                let extra = if self.reset_base {
                    " With /ResetBase, currently installed updates can no longer be uninstalled."
                } else {
                    ""
                };
                self.confirm.ask(
                    format!("Start component store cleanup? This can take 10 minutes or more.{extra}"),
                    op,
                );
            }
            ui.checkbox(&mut self.reset_base, "Also reset base (/ResetBase)")
                .on_hover_text("Frees more space, but installed updates can no longer be uninstalled.");
            if busy {
                ui.spinner();
                ui.label("DISM is running; this can take several minutes…");
            }
        });
        show_message(ui, &self.message);

        if let Some(out) = &self.output {
            ui.add_space(8.0);
            let lines = summary_lines(out);
            if !lines.is_empty() {
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
            egui::CollapsingHeader::new("Full DISM output").show(ui, |ui| {
                ui.add(egui::Label::new(egui::RichText::new(out).monospace().small()).selectable(true));
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
        let out = "Component Store (WinSxS) information:\n\nWindows Explorer Reported Size of Component Store : 7.21 GB\n\
                   Component Store Cleanup Recommended : Yes\nThe operation completed successfully.";
        let s = summary_lines(out);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1], ("Component Store Cleanup Recommended".into(), "Yes".into()));
    }
}
