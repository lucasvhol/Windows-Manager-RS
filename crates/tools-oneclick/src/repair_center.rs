//! Repair Center: Windows' own integrity and repair tools with live output.

use wm_core::{
    AppCtx, Category, Tool, egui,
    safety::UndoAction,
    shell::shell_open,
    task::BackgroundTask,
    ui::{Message, show_message},
};

use wm_core::command::run_streaming;

/// (title, description, program, arguments)
pub const REPAIRS: [(&str, &str, &str, &[&str]); 4] = [
    (
        "Check system files (SFC)",
        "Verifies protected Windows files and replaces damaged ones. 10–20 minutes.",
        "sfc",
        &["/scannow"],
    ),
    (
        "Check the component store (DISM ScanHealth)",
        "Checks the store SFC repairs from. 5–15 minutes.",
        "dism",
        &["/Online", "/Cleanup-Image", "/ScanHealth"],
    ),
    (
        "Repair the component store (DISM RestoreHealth)",
        "Downloads clean copies of damaged components from Windows Update. Run SFC again afterwards.",
        "dism",
        &["/Online", "/Cleanup-Image", "/RestoreHealth"],
    ),
    (
        "Scan drive C: for errors (CHKDSK)",
        "Online scan; problems found are scheduled for repair at the next restart.",
        "chkdsk",
        &["C:", "/scan"],
    ),
];

/// (exit code, output lines) of a finished repair tool.
type RunResult = (i32, Vec<String>);

#[derive(Default)]
pub struct RepairCenter {
    running: Option<(usize, BackgroundTask<RunResult>)>,
    output: Vec<String>,
    message: Message,
}

impl Tool for RepairCenter {
    fn id(&self) -> &'static str {
        "repair"
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some((i, t)) = &mut self.running {
            t.poll();
            if let Some(r) = t.take_result() {
                let title = REPAIRS[*i].0;
                self.running = None;
                match r {
                    Ok((code, lines)) => {
                        let _ = ctx.journal.record(
                            "repair",
                            format!("Ran {title} (exit code {code})"),
                            UndoAction::None,
                        );
                        self.output = lines;
                        self.message = Some(if code == 0 {
                            Ok(format!("{title} finished."))
                        } else {
                            Err(format!("{title} exited with code {code}; see the output."))
                        });
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if !ctx.elevated {
            ui.label("These tools need administrator rights.");
        }
        let busy = self.running.is_some();
        for (i, (title, desc, program, args)) in REPAIRS.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        ctx.elevated && !busy,
                        egui::Button::new(*title).min_size(egui::vec2(330.0, 26.0)),
                    )
                    .clicked()
                {
                    if ctx.config.dry_run {
                        self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    } else {
                        self.output.clear();
                        self.message = None;
                        let (program, args) = (*program, *args);
                        self.running = Some((
                            i,
                            BackgroundTask::spawn(ui.ctx(), move |p| {
                                run_streaming(program, args, |s| p.report(0.0, s.to_owned()))
                            }),
                        ));
                    }
                }
                ui.weak(*desc);
            });
        }
        ui.horizontal(|ui| {
            if ui.button("Windows Memory Diagnostic").clicked() {
                self.message = shell_open("mdsched.exe", "").err().map(|e| Err(format!("{e:#}")));
            }
            if ui.button("System Restore").clicked() {
                self.message = shell_open("rstrui.exe", "").err().map(|e| Err(format!("{e:#}")));
            }
        });
        if let Some((i, t)) = &self.running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("{}: {}", REPAIRS[*i].0, t.status));
            });
        }
        show_message(ui, &self.message);
        if !self.output.is_empty() {
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(self.output.join("\n")).monospace().small())
                        .selectable(true),
                );
            });
        }
    }
}
