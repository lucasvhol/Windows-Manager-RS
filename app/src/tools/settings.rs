//! Settings: interface, safety, program password, tweak backup and program data.

use std::path::Path;

use wm_core::{
    AppCtx, Category, Layout, Tool,
    config::Theme,
    egui,
    i18n::LANGUAGES,
    icons, password, paths,
    safety::{UndoAction, restore_point},
    task::BackgroundTask,
    tweaks::{self, SavedTweaks, Tweak},
    ui::{Message, frame, section, section_nav, show_message},
};

/// Every tweak definition in the app (for saving and restoring them all).
pub fn all_tweaks() -> Vec<Tweak> {
    [
        wm_tools_optimizer::speed::DEFINITIONS,
        wm_tools_security::PRIVACY_TWEAKS,
        wm_tools_security::LOCKDOWN_TWEAKS,
        wm_tools_security::SYSTEM_SECURITY_TWEAKS,
        wm_tools_security::OTHER_SECURITY_TWEAKS,
        wm_tools_security::DEVICE_SECURITY_TWEAKS,
        wm_tools_custom::VISUAL_TWEAKS,
        wm_tools_custom::EXPLORER_TWEAKS,
        wm_tools_custom::START_TWEAKS,
        wm_tools_custom::THISPC_TWEAKS,
        wm_tools_custom::NAVPANE_TWEAKS,
        wm_tools_custom::CUSTOM_TWEAKS,
        wm_tools_custom::APP_SETTINGS_TWEAKS,
        wm_tools_network::NETWORK_TWEAKS,
    ]
    .iter()
    .flat_map(|t| tweaks::parse(t).unwrap_or_default())
    .collect()
}

/// Copies every file in `from` to `to` (non-recursive); returns how many were copied.
fn copy_dir_files(from: &Path, to: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(to)?;
    let mut n = 0;
    for e in std::fs::read_dir(from)?.flatten().filter(|e| e.path().is_file()) {
        std::fs::copy(e.path(), to.join(e.file_name()))?;
        n += 1;
    }
    Ok(n)
}

#[derive(Default)]
pub struct Settings {
    section: usize,
    restore_task: Option<BackgroundTask<()>>,
    new_password: String,
    repeat_password: String,
    message: Message,
}

const SECTIONS: [(&str, &str); 5] = [
    (icons::SLIDERS, "General"),
    (icons::SHIELD_CHECK, "Safety"),
    (icons::LOCK, "Program password"),
    (icons::FLOPPY_DISK, "Tweak backup"),
    (icons::FOLDER_OPEN, "Program data"),
];

impl Settings {
    fn general(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let i18n = ctx.i18n;
        egui::Grid::new("settings")
            .num_columns(2)
            .spacing([16.0, 10.0])
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

                ui.label("Interface size");
                let mut pct = (ctx.config.ui_scale * 100.0).round() as u32;
                if ui
                    .add(egui::Slider::new(&mut pct, 75..=175).step_by(5.0).suffix(" %"))
                    .drag_stopped()
                {
                    ctx.config.ui_scale = pct as f32 / 100.0;
                }
                ui.end_row();

                ui.label("When minimised");
                ui.checkbox(&mut ctx.config.minimize_to_tray, "Hide in the notification area")
                    .on_hover_text("Click the Win Manager icon to bring the window back. The icon belongs to My Task's rule monitor, which starts if needed.");
                ui.end_row();
            });
    }

    fn safety(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let i18n = ctx.i18n;
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
        });
        if let Some(task) = &mut self.restore_task {
            task.poll();
            if let Some(result) = task.take_result() {
                self.restore_task = None;
                self.message = Some(result.map(|()| {
                    let _ = ctx
                        .journal
                        .record("settings", "Created restore point", UndoAction::None);
                    i18n.tr("settings-restore-point-created")
                }));
            }
        }
    }

    fn password(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Ask for a password when Win Manager RS starts. Scheduled maintenance still runs without it.",
        );
        ui.weak("Only a salted Argon2id hash is stored. If you forget the password, delete config.toml in the program data folder.");
        ui.add_space(8.0);
        egui::Grid::new("pw")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("New password");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_password)
                        .password(true)
                        .desired_width(220.0),
                );
                ui.end_row();
                ui.label("Repeat");
                ui.add(
                    egui::TextEdit::singleline(&mut self.repeat_password)
                        .password(true)
                        .desired_width(220.0),
                );
                ui.end_row();
            });
        ui.horizontal(|ui| {
            let ok = !self.new_password.is_empty() && self.new_password == self.repeat_password;
            if ui.add_enabled(ok, egui::Button::new("Set password")).clicked() {
                self.message = Some(
                    password::hash(&self.new_password)
                        .map(|h| {
                            ctx.config.password_hash = Some(h);
                            "Password set.".to_owned()
                        })
                        .map_err(|e| format!("{e:#}")),
                );
                self.new_password.clear();
                self.repeat_password.clear();
            }
            if ui
                .add_enabled(
                    ctx.config.password_hash.is_some(),
                    egui::Button::new("Remove password"),
                )
                .clicked()
            {
                ctx.config.password_hash = None;
                self.message = Some(Ok("Password removed.".into()));
            }
        });
    }

    fn tweak_backup(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Save every tweak you have applied in this app to a file, and apply them again later, \
             for example after reinstalling Windows.",
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .button(format!("{}  Save tweaks to file…", icons::FLOPPY_DISK))
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Tweaks", &["toml"])
                    .set_file_name("my-tweaks.toml")
                    .save_file()
            {
                let saved = tweaks::capture(&all_tweaks());
                let n = saved.on.len() + saved.numbers.len() + saved.choices.len();
                self.message = Some(
                    toml::to_string_pretty(&saved)
                        .map_err(anyhow::Error::from)
                        .and_then(|t| Ok(std::fs::write(&path, t)?))
                        .map(|()| format!("Saved {n} tweaks to {}.", path.display()))
                        .map_err(|e| format!("{e:#}")),
                );
            }
            if ui
                .button(format!("{}  Restore tweaks from file…", icons::UPLOAD))
                .clicked()
                && let Some(path) = rfd::FileDialog::new().add_filter("Tweaks", &["toml"]).pick_file()
            {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    std::fs::read_to_string(&path)
                        .map_err(anyhow::Error::from)
                        .and_then(|t| Ok(toml::from_str::<SavedTweaks>(&t)?))
                        .map(|saved| {
                            let (done, problems) =
                                tweaks::restore(&all_tweaks(), &saved, ctx.journal, "settings", ctx.elevated);
                            if problems.is_empty() {
                                format!("Applied {done} tweaks.")
                            } else {
                                format!("Applied {done} tweaks. Not applied: {}", problems.join("; "))
                            }
                        })
                        .map_err(|e| format!("{e:#}"))
                });
            }
        });
    }

    fn program_data(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Settings, saved profiles, the Change Journal and backups are kept in your profile.");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .button(format!(
                    "{}  {}",
                    icons::FOLDER_OPEN,
                    ctx.i18n.tr("settings-open-data")
                ))
                .clicked()
                && let Ok(dir) = paths::data_dir()
            {
                let _ = std::process::Command::new("explorer").arg(dir).spawn();
            }
            if ui.button("Back up settings…").clicked()
                && let Some(dest) = rfd::FileDialog::new().pick_folder()
            {
                self.message = Some(
                    paths::config_dir()
                        .and_then(|src| Ok(copy_dir_files(&src, &dest.join("win-manager-rs-settings"))?))
                        .map(|n| format!("Backed up {n} settings files."))
                        .map_err(|e| format!("{e:#}")),
                );
            }
            if ui.button("Restore settings…").clicked()
                && let Some(src) = rfd::FileDialog::new().pick_folder()
            {
                self.message = Some(
                    paths::config_dir()
                        .and_then(|dest| Ok(copy_dir_files(&src, &dest)?))
                        .map(|n| format!("Restored {n} settings files. Restart the app to load them."))
                        .map_err(|e| format!("{e:#}")),
                );
            }
        });

        ui.add_space(12.0);
        ui.strong("Options tools remember");
        ui.weak("Reset a tool to forget its remembered choices (selections, schemes, schedules' options). It starts with its defaults the next time it opens.");
        // app-window is bookkeeping for the tray icon, not an option.
        let saved: Vec<String> = wm_core::tool_settings::saved()
            .into_iter()
            .filter(|id| id != "app-window")
            .collect();
        if saved.is_empty() {
            ui.weak("No tool has remembered anything yet.");
        }
        let mut reset = None;
        egui::Grid::new("tool-settings")
            .striped(true)
            .num_columns(2)
            .show(ui, |ui| {
                for id in &saved {
                    ui.label(tool_label(ctx, id));
                    if ui.small_button("Reset").clicked() {
                        reset = Some(id.clone());
                    }
                    ui.end_row();
                }
            });
        if let Some(id) = reset {
            self.message = Some(
                wm_core::tool_settings::reset(&id)
                    .map(|()| {
                        format!(
                            "{} will start with its defaults. Restart the app if it's open.",
                            tool_label(ctx, &id)
                        )
                    })
                    .map_err(|e| format!("{e:#}")),
            );
        }
    }
}

/// A readable name for a saved-settings id: the tool's title when the id is a tool,
/// otherwise the id with a hint of which tool owns it.
fn tool_label(ctx: &AppCtx<'_>, id: &str) -> String {
    let key = format!("{id}-title");
    let title = ctx.i18n.tr(&key);
    if title != key && !title.is_empty() {
        return title;
    }
    match id {
        "privacy-traces" => "Privacy Protector (trace selection)".into(),
        "themeschedule" => "Custom System (theme schedule)".into(),
        "mytask-rules" => "My Task (live rules)".into(),
        "mytask-log" => "My Task (rule monitor history)".into(),
        other => other.to_owned(),
    }
}

impl Tool for Settings {
    fn id(&self) -> &'static str {
        "settings"
    }

    fn category(&self) -> Category {
        Category::Home
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if frame::left(ui, 200.0, |ui| section_nav(ui, &SECTIONS, &mut self.section)) {
            self.message = None;
        }
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            section(
                ui,
                SECTIONS[self.section].1,
                |_| {},
                |ui| match self.section {
                    0 => self.general(ui, ctx),
                    1 => self.safety(ui, ctx),
                    2 => self.password(ui, ctx),
                    3 => self.tweak_backup(ui, ctx),
                    _ => self.program_data(ui, ctx),
                },
            );
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_tweak_file_parses_and_ids_are_unique() {
        let all = super::all_tweaks();
        assert!(all.len() > 60, "{}", all.len());
        let mut ids: Vec<&str> = all.iter().map(|t| t.id.as_str()).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate tweak ids across tools");
    }
}

#[cfg(test)]
mod sandbox_tests {
    use wm_core::{
        registry,
        safety::ChangeJournal,
        tweaks::{Kind, State},
    };

    /// Applies and reverts every switch in the app against a scratch HKCU key, checking
    /// that each one reads back as applied and that turning it off restores the off state.
    #[test]
    fn every_switch_applies_and_reverts_in_a_sandbox() {
        let sandbox = format!(
            r"HKCU\Software\win-manager-rs-test-sandbox-{}",
            std::process::id()
        );
        let dir = tempfile::tempdir().unwrap();
        let journal = ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut checked = 0;
        for mut t in super::all_tweaks().into_iter().filter(|t| t.kind == Kind::Toggle) {
            for s in &mut t.settings {
                // HKLM\X, HKCU\X, HKU\.DEFAULT\X  ->  <sandbox>\HKLM\X …
                let (root, rest) = s.key.split_once('\\').unwrap();
                s.key = format!(r"{sandbox}\{root}\{rest}");
            }
            // Start from the documented off state.
            t.revert(&journal, "test").unwrap();
            assert_eq!(t.state(), State::NotApplied, "{}: off state", t.id);
            t.apply(&journal, "test").unwrap();
            assert_eq!(t.state(), State::Applied, "{}: on state", t.id);
            t.revert(&journal, "test").unwrap();
            assert_eq!(t.state(), State::NotApplied, "{}: back off", t.id);
            checked += 1;
        }
        let (root, sub) = registry::split_path(&sandbox).unwrap();
        let _ = root.delete_subkey_all(sub);
        assert!(checked > 150, "{checked}");
    }
}
