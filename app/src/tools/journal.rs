use egui_extras::{Column, TableBuilder};
use wm_core::{AppCtx, Category, Tool, egui, i18n::FluentArgs, safety::UndoAction};

#[derive(Default)]
pub struct Journal {
    message: Option<Result<String, String>>,
}

impl Tool for Journal {
    fn id(&self) -> &'static str {
        "journal"
    }

    fn category(&self) -> Category {
        Category::Home
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let i18n = ctx.i18n;
        ui.label(i18n.tr("journal-intro"));
        let entries = ctx.journal.entries();
        let mut args = FluentArgs::new();
        args.set("count", entries.len());
        ui.weak(i18n.tr_args("journal-count", Some(&args)));
        match &self.message {
            Some(Ok(m)) => {
                ui.label(m);
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }
        ui.add_space(8.0);
        if entries.is_empty() {
            ui.label(i18n.tr("journal-empty"));
            return;
        }

        let mut undo_id = None;
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::auto().at_least(140.0))
            .column(Column::auto().at_least(110.0))
            .column(Column::remainder())
            .column(Column::auto().at_least(70.0))
            .header(22.0, |mut h| {
                for key in [
                    "journal-col-time",
                    "journal-col-tool",
                    "journal-col-change",
                    "journal-col-action",
                ] {
                    h.col(|ui| {
                        ui.strong(i18n.tr(key));
                    });
                }
            })
            .body(|mut body| {
                for e in &entries {
                    body.row(22.0, |mut row| {
                        row.col(|ui| {
                            ui.label(e.time.format("%Y-%m-%d %H:%M:%S").to_string());
                        });
                        row.col(|ui| {
                            ui.label(&e.tool);
                        });
                        row.col(|ui| {
                            ui.label(&e.description);
                        });
                        row.col(|ui| {
                            if e.undone {
                                ui.weak(i18n.tr("journal-undone"));
                            } else if e.undo != UndoAction::None
                                && ui.button(i18n.tr("journal-undo")).clicked()
                            {
                                undo_id = Some(e.id);
                            }
                        });
                    });
                }
            });

        if let Some(id) = undo_id {
            self.message = Some(
                ctx.journal
                    .undo(id)
                    .map(|()| format!("Change #{id} undone."))
                    .map_err(|e| format!("Undo failed: {e:#}")),
            );
        }
    }
}
