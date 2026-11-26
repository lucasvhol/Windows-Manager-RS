use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui,
    i18n::FluentArgs,
    icons,
    safety::UndoAction,
    ui::{ToolButton, empty_state, frame},
};

#[derive(Default)]
pub struct Journal {
    message: Option<Result<String, String>>,
    selected: Option<u64>,
    filter: String,
}

impl Tool for Journal {
    fn id(&self) -> &'static str {
        "journal"
    }

    fn category(&self) -> Category {
        Category::Home
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let i18n = ctx.i18n;
        let entries = ctx.journal.entries();
        let selected = self
            .selected
            .and_then(|id| entries.iter().find(|e| e.id == id))
            .filter(|e| !e.undone && e.undo != UndoAction::None)
            .map(|e| e.id);
        let undo_label = i18n.tr("journal-undo");
        let mut undo_id = None;
        if frame::toolbar(
            ui,
            &[Some(
                ToolButton::new(icons::ARROW_COUNTER_CLOCKWISE, &undo_label).enabled(selected.is_some()),
            )],
            |_| {},
        ) == Some(0)
        {
            undo_id = selected;
        }
        let filter_hint = format!("{}  {}", icons::MAGNIFYING_GLASS, i18n.tr("common-filter"));
        frame::options(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(filter_hint)
                    .desired_width(220.0),
            );
            ui.weak(i18n.tr("journal-intro"));
        });
        let mut args = FluentArgs::new();
        args.set("count", entries.len());
        frame::status(ui, &i18n.tr_args("journal-count", Some(&args)), |_| {});

        let f = self.filter.to_lowercase();
        // `entries()` lists the newest change first.
        let visible: Vec<_> = entries
            .iter()
            .filter(|e| {
                f.is_empty()
                    || e.tool.to_lowercase().contains(&f)
                    || e.description.to_lowercase().contains(&f)
            })
            .collect();
        frame::central_fixed(ui, |ui| {
            match &self.message {
                Some(Ok(m)) => {
                    ui.label(m);
                }
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                None => {}
            }
            if visible.is_empty() {
                empty_state(ui, icons::CLOCK_COUNTER_CLOCKWISE, &i18n.tr("journal-empty"), "");
                return;
            }
            let mut clicked = None;
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            TableBuilder::new(ui)
                .striped(true)
                .sense(egui::Sense::click())
                .column(Column::initial(150.0))
                .column(Column::initial(120.0).clip(true))
                .column(Column::remainder().clip(true))
                .column(Column::exact(90.0))
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
                .body(|body| {
                    body.rows(24.0, visible.len(), |mut row| {
                        let e = visible[row.index()];
                        row.set_selected(self.selected == Some(e.id));
                        row.col(|ui| {
                            ui.weak(e.time.format("%Y-%m-%d %H:%M:%S").to_string());
                        });
                        row.col(|ui| {
                            ui.label(&e.tool);
                        });
                        row.col(|ui| {
                            ui.label(&e.description).on_hover_text(&e.description);
                        });
                        row.col(|ui| {
                            if e.undone {
                                ui.weak(i18n.tr("journal-undone"));
                            } else if e.undo != UndoAction::None
                                && ui.small_button(i18n.tr("journal-undo")).clicked()
                            {
                                undo_id = Some(e.id);
                            }
                        });
                        if row.response().clicked() {
                            clicked = Some(e.id);
                        }
                    });
                });
            if clicked.is_some() {
                self.selected = clicked;
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
