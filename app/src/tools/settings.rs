use wm_core::{
    AppCtx, Category, Tool,
    config::Theme,
    egui,
    i18n::LANGUAGES,
    paths,
    safety::{UndoAction, restore_point},
    task::BackgroundTask,
};

#[derive(Default)]
pub struct Settings {
    restore_task: Option<BackgroundTask<()>>,
    message: Option<Result<String, String>>,
}

impl Tool for Settings {
    fn id(&self) -> &'static str {
        "settings"
    }

    fn category(&self) -> Category {
        Category::Home
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let i18n = ctx.i18n;
        egui::Grid::new("settings")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label(i18n.tr("settings-language"));
                let current = LANGUAGES
                    .iter()
                    .find(|(t, _, _)| *t == ctx.config.language)
                    .map_or(ctx.config.language.as_str(), |(_, name, _)| name);
                egui::ComboBox::from_id_salt("language")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (tag, name, _) in LANGUAGES {
                            ui.selectable_value(&mut ctx.config.language, (*tag).to_owned(), *name);
                        }
                    });
                ui.end_row();

                ui.label(i18n.tr("settings-theme"));
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut ctx.config.theme,
                        Theme::System,
                        i18n.tr("settings-theme-system"),
                    );
                    ui.selectable_value(
                        &mut ctx.config.theme,
                        Theme::Light,
                        i18n.tr("settings-theme-light"),
                    );
                    ui.selectable_value(&mut ctx.config.theme, Theme::Dark, i18n.tr("settings-theme-dark"));
                });
                ui.end_row();
            });

        ui.add_space(8.0);
        ui.checkbox(&mut ctx.config.dry_run, i18n.tr("settings-dry-run"));
        ui.checkbox(
            &mut ctx.config.create_restore_points,
            i18n.tr("settings-restore-points"),
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let busy = self.restore_task.is_some();
            let button = ui.add_enabled(
                ctx.elevated && !busy,
                egui::Button::new(i18n.tr("settings-create-restore-point")),
            );
            if !ctx.elevated {
                button.on_disabled_hover_text(i18n.tr("shell-needs-admin"));
            } else if button.clicked() {
                self.message = None;
                self.restore_task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                    restore_point::create("Win Manager RS manual checkpoint")
                }));
            }
            if busy {
                ui.spinner();
            }
            if ui.button(i18n.tr("settings-open-data")).clicked()
                && let Ok(dir) = paths::data_dir()
            {
                let _ = std::process::Command::new("explorer").arg(dir).spawn();
            }
        });

        if let Some(task) = &mut self.restore_task {
            task.poll();
            if let Some(result) = task.take_result() {
                self.restore_task = None;
                self.message = Some(match result {
                    Ok(()) => {
                        let _ = ctx
                            .journal
                            .record("settings", "Created restore point", UndoAction::None);
                        Ok(i18n.tr("settings-restore-point-created"))
                    }
                    Err(e) => Err(e),
                });
            }
        }
        match &self.message {
            Some(Ok(m)) => {
                ui.label(m);
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }
    }
}
