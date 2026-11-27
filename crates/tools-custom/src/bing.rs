//! Bing Images Downloader: save Bing's pictures of the day (any of the last eight) to a
//! folder, in a chosen market and size, optionally with the title written on them, set
//! one as the wallpaper, and download today's picture every day through Task Scheduler.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    scheduler::{self, Schedule, valid_time},
    shell::{KnownFolder, known_folder, shell_open},
    task::BackgroundTask,
    tool_settings,
    ui::{Message, ToolButton, empty_state, frame, section, show_message},
};

use crate::visual::pictures::{self, BingImage, MARKETS, Resolution};

const ID: &str = "bing";
const TASK: &str = "Bing picture of the day";
/// The scheduled action (`--run bing-daily`).
pub const DAILY_ACTION: &str = "bing-daily";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub folder: Option<PathBuf>,
    pub market: String,
    pub resolution: Resolution,
    /// Write the picture's title and copyright on it.
    pub caption: bool,
    /// The daily download also sets the picture as the wallpaper.
    pub wallpaper: bool,
    pub daily: bool,
    pub time: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            folder: None,
            market: String::new(),
            resolution: Resolution::Uhd,
            caption: false,
            wallpaper: false,
            daily: false,
            time: "09:00".into(),
        }
    }
}

impl Settings {
    pub fn folder(&self) -> PathBuf {
        self.folder.clone().unwrap_or_else(|| {
            known_folder(KnownFolder::Pictures)
                .unwrap_or_else(|_| PathBuf::from(wm_core::paths::expand_env(r"%USERPROFILE%\Pictures")))
                .join("Bing")
        })
    }
}

/// Downloads one picture with the settings (caption added only to a new file).
pub fn save(img: &BingImage, s: &Settings) -> Result<PathBuf> {
    let dir = s.folder();
    let existed = dir.join(img.file_name_for(s.resolution)).exists();
    let path = img.save(s.resolution, &dir)?;
    if s.caption && !existed {
        let text = if img.title.is_empty() {
            img.copyright.clone()
        } else {
            format!("{} · {}", img.title, img.copyright)
        };
        pictures::caption(&path, &text)?;
    }
    Ok(path)
}

/// The scheduled action: today's picture, saved (and set as wallpaper if chosen).
pub fn run_daily(ctx: &AppCtx<'_>) -> Result<String> {
    let s: Settings = tool_settings::load(ID);
    let list = pictures::bing_list_for(&s.market)?;
    let today = list.first().context("Bing returned no pictures")?;
    let path = save(today, &s)?;
    if s.wallpaper {
        pictures::set_wallpaper(ctx, &path)?;
    }
    Ok(format!(
        "Saved {}{}.",
        path.display(),
        if s.wallpaper {
            " and set it as the wallpaper"
        } else {
            ""
        }
    ))
}

pub struct BingDownloader {
    settings: Settings,
    list: Option<Result<Vec<BingImage>, String>>,
    task: Option<BackgroundTask<Vec<BingImage>>>,
    saving: Option<BackgroundTask<Vec<PathBuf>>>,
    message: Message,
}

impl Default for BingDownloader {
    fn default() -> Self {
        Self {
            settings: tool_settings::load(ID),
            list: None,
            task: None,
            saving: None,
            message: None,
        }
    }
}

impl BingDownloader {
    fn persist(&mut self) {
        if let Err(e) = tool_settings::save(ID, &self.settings) {
            self.message = Some(Err(format!("{e:#}")));
        }
    }

    fn save_in_background(&mut self, ui: &egui::Ui, images: Vec<BingImage>) {
        let s = self.settings.clone();
        self.saving = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
            let mut out = Vec::new();
            let n = images.len().max(1) as f32;
            for (i, img) in images.iter().enumerate() {
                p.report(i as f32 / n, format!("Saving {}…", img.date));
                out.push(save(img, &s)?);
            }
            Ok(out)
        }));
    }
}

impl Tool for BingDownloader {
    fn id(&self) -> &'static str {
        ID
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.list = Some(r);
                self.task = None;
            }
        }
        if let Some(t) = &mut self.saving {
            t.poll();
            if let Some(r) = t.take_result() {
                self.saving = None;
                self.message = Some(r.map(|paths| match paths.as_slice() {
                    [one] => format!("Saved {}.", one.display()),
                    many => format!(
                        "Saved {} pictures in {}.",
                        many.len(),
                        self.settings.folder().display()
                    ),
                }));
            }
        }
        let before = self.settings.clone();
        let busy = self.task.is_some();
        let list_len = match &self.list {
            Some(Ok(l)) => l.len(),
            _ => 0,
        };
        let save_all = format!("Save all {list_len}");
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::DOWNLOAD, "Get pictures").enabled(!busy)),
                Some(
                    ToolButton::new(icons::FLOPPY_DISK, &save_all)
                        .enabled(list_len > 0 && self.saving.is_none()),
                ),
                None,
                Some(ToolButton::new(icons::FOLDER_OPEN, "Open folder")),
                Some(ToolButton::new(icons::FOLDERS, "Change folder")),
            ],
            |ui| {
                if busy {
                    ui.spinner();
                }
            },
        ) {
            Some(0) => {
                let market = self.settings.market.clone();
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                    pictures::bing_list_for(&market)
                }));
            }
            Some(1) => {
                if let Some(Ok(list)) = &self.list {
                    let l = list.clone();
                    self.save_in_background(ui, l);
                }
            }
            Some(3) => {
                let dir = self.settings.folder();
                let _ = std::fs::create_dir_all(&dir);
                let _ = shell_open(&dir.display().to_string(), "");
            }
            Some(4) => {
                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                    self.settings.folder = Some(d);
                }
            }
            _ => {}
        }
        frame::options(ui, |ui| {
            ui.label("Market");
            egui::ComboBox::from_id_salt("bing-market")
                .selected_text(
                    MARKETS
                        .iter()
                        .find(|m| m.0 == self.settings.market)
                        .map_or("?", |m| m.1),
                )
                .show_ui(ui, |ui| {
                    for (code, name) in MARKETS {
                        ui.selectable_value(&mut self.settings.market, code.to_owned(), name);
                    }
                });
            ui.label("Size");
            egui::ComboBox::from_id_salt("bing-res")
                .selected_text(self.settings.resolution.label())
                .show_ui(ui, |ui| {
                    for r in Resolution::WITH_PORTRAIT {
                        ui.selectable_value(&mut self.settings.resolution, r, r.label());
                    }
                });
            ui.checkbox(
                &mut self.settings.caption,
                "Write the title and copyright on the picture",
            );
        });
        if self.settings.market != before.market {
            self.list = None;
        }
        let progress = self.saving.as_ref().map(|t| (t.progress, t.status.clone()));
        frame::status(
            ui,
            &format!("Saves to {}", self.settings.folder().display()),
            |ui| {
                if let Some((p, text)) = progress {
                    ui.add(egui::ProgressBar::new(p).desired_width(200.0).text(text));
                }
            },
        );

        let mut save_one = None;
        let mut wallpaper = None;
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            section(
                ui,
                "Every day",
                |_| {},
                |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.settings.daily, "Download today's picture every day at");
                        ui.add(egui::TextEdit::singleline(&mut self.settings.time).desired_width(64.0));
                        ui.checkbox(&mut self.settings.wallpaper, "and set it as the wallpaper");
                    });
                    ui.weak(
                    "Runs through Task Scheduler, even when this app is closed. It asks bing.com once a day.",
                );
                },
            );
            ui.add_space(8.0);
            match &self.list {
                None => empty_state(
                    ui,
                    icons::IMAGE,
                    "Bing's pictures of the day",
                    "Press Get pictures to ask bing.com for the last eight.",
                ),
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                Some(Ok(list)) => {
                    section(
                        ui,
                        &format!("Latest pictures ({})", list.len()),
                        |_| {},
                        |ui| {
                            egui::Grid::new("bing-list")
                                .striped(true)
                                .num_columns(3)
                                .spacing([16.0, 6.0])
                                .show(ui, |ui| {
                                    for img in list {
                                        let date = format!(
                                            "{}-{}-{}",
                                            img.date.get(..4).unwrap_or(""),
                                            img.date.get(4..6).unwrap_or(""),
                                            img.date.get(6..8).unwrap_or("")
                                        );
                                        ui.weak(date);
                                        ui.vertical(|ui| {
                                            ui.strong(&img.title);
                                            ui.weak(&img.copyright);
                                        });
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_enabled(
                                                    self.saving.is_none(),
                                                    egui::Button::new("Save").small(),
                                                )
                                                .clicked()
                                            {
                                                save_one = Some(img.clone());
                                            }
                                            if ui.small_button("Set as wallpaper").clicked() {
                                                wallpaper = Some(img.clone());
                                            }
                                        });
                                        ui.end_row();
                                    }
                                });
                        },
                    );
                }
            }
        });
        let schedule_changed = self.settings.daily != before.daily
            || (self.settings.daily && self.settings.time != before.time && valid_time(&self.settings.time));
        if schedule_changed {
            self.message = Some(if self.settings.daily && !valid_time(&self.settings.time) {
                self.settings.daily = false;
                Err("Enter the time as HH:MM (24-hour).".into())
            } else if self.settings.daily {
                scheduler::schedule(
                    ctx,
                    TASK,
                    DAILY_ACTION,
                    Schedule::Daily,
                    &self.settings.time,
                    false,
                )
                .map_err(|e| format!("{e:#}"))
            } else {
                scheduler::unschedule(ctx, TASK)
                    .map(|_| "Daily download turned off.".into())
                    .map_err(|e| format!("{e:#}"))
            });
        }
        if self.settings != before {
            self.persist();
        }
        if let Some(img) = save_one {
            self.save_in_background(ui, vec![img]);
        }
        if let Some(img) = wallpaper {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                save(&img, &self.settings)
                    .and_then(|p| pictures::set_wallpaper(ctx, &p).map(|()| p))
                    .map(|p| {
                        format!(
                            "Wallpaper set to {}. Undo it from the Change Journal.",
                            p.display()
                        )
                    })
                    .map_err(|e| format!("{e:#}"))
            });
        }
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_and_round_trip() {
        let s = Settings::default();
        assert!(s.folder().ends_with("Bing"));
        let json = serde_json::to_string(&Settings {
            resolution: Resolution::Portrait,
            ..s.clone()
        })
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Settings>(&json).unwrap().resolution,
            Resolution::Portrait
        );
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), s);
    }

    #[test]
    fn caption_draws_on_a_picture() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.jpg");
        let img = image::RgbImage::from_pixel(400, 240, image::Rgb([200, 200, 200]));
        img.save(&p).unwrap();
        pictures::caption(&p, "Test title · (c) someone").unwrap();
        let out = image::open(&p).unwrap().to_rgb8();
        assert_eq!(out.dimensions(), (400, 240));
        // The stripe at the bottom is darker than the untouched top.
        assert!(out.get_pixel(5, 238).0[0] < out.get_pixel(5, 5).0[0]);
        // Some text pixels are bright inside the stripe.
        let bright = (0..400)
            .flat_map(|x| (200..240).map(move |y| (x, y)))
            .any(|(x, y)| out.get_pixel(x, y).0[0] > 200);
        assert!(bright, "text was drawn");
    }
}
