//! The hub's Update button and window: checks GitHub Releases when pressed, offers the
//! new version, downloads and verifies the installer, and runs it.

use std::path::PathBuf;

use wm_core::{
    egui, icons,
    task::BackgroundTask,
    update::{self, Release},
};

#[derive(Default)]
pub struct Updater {
    open: bool,
    check: Option<BackgroundTask<Option<Release>>>,
    found: Option<Result<Option<Release>, String>>,
    download: Option<BackgroundTask<PathBuf>>,
    error: Option<String>,
}

impl Updater {
    pub fn button(&mut self, ui: &mut egui::Ui) {
        let label = format!("{} Update", icons::ARROWS_CLOCKWISE);
        if ui
            .button(label)
            .on_hover_text("Check for a newer version")
            .clicked()
        {
            self.open = true;
            self.found = None;
            self.error = None;
            if update::repo().is_some() && self.check.is_none() {
                self.check = Some(BackgroundTask::spawn(ui.ctx(), |_| update::latest()));
            }
        }
    }

    /// Draws the window; returns true when the installer was started and the app should close.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        if let Some(t) = &mut self.check {
            t.poll();
            if let Some(r) = t.take_result() {
                self.check = None;
                self.found = Some(r);
            }
        }
        let mut quit = false;
        if let Some(t) = &mut self.download {
            t.poll();
            if let Some(r) = t.take_result() {
                self.download = None;
                match r.and_then(|p| update::run_installer(&p).map_err(|e| format!("{e:#}"))) {
                    Ok(()) => quit = true,
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if !self.open {
            return quit;
        }
        let modal = egui::Modal::new(egui::Id::unique("updater")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(egui::RichText::new("Updates").size(20.0).strong());
            ui.weak(format!("You have version {}.", update::current_version()));
            ui.add_space(8.0);
            if update::repo().is_none() {
                ui.label("This copy was built on this PC, so it has no release page to check. Copies installed from a release check for updates here.");
            } else if self.check.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Checking…");
                });
            } else {
                match &self.found {
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("Couldn't check: {e}"));
                    }
                    Some(Ok(Some(r))) if update::is_newer(&r.version, update::current_version()) => {
                        ui.label(egui::RichText::new(format!("Version {} is available.", r.version.trim_start_matches('v'))).strong());
                        if !r.notes.trim().is_empty() {
                            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                                ui.label(r.notes.trim());
                            });
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let busy = self.download.is_some();
                            if ui
                                .add_enabled(!busy && r.installer.is_some(), egui::Button::new("Download and install"))
                                .clicked()
                            {
                                let release = r.clone();
                                self.error = None;
                                self.download = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                                    let dir = std::env::temp_dir().join("win-manager-rs-update");
                                    update::download(&release, &dir)
                                }));
                            }
                            if !r.page.is_empty() && ui.button("Release page").clicked() {
                                let _ = wm_core::shell::shell_open(&r.page, "");
                            }
                            if busy {
                                ui.spinner();
                                ui.weak("Downloading and checking…");
                            }
                        });
                    }
                    Some(Ok(_)) => {
                        ui.label("You have the latest version. 👍");
                    }
                    None => {}
                }
            }
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);
            if ui.button("Close").clicked() {
                self.open = false;
            }
        });
        if modal.should_close() && self.download.is_none() {
            self.open = false;
        }
        quit
    }
}
