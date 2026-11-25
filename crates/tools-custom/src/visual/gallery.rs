//! The picture-based sections of Visual Customizer: folder colorizer, theme changer,
//! lock-screen pictures and Bing pictures.

use std::collections::HashMap;
use std::path::PathBuf;

use wm_core::{
    AppCtx,
    egui::{self, TextureHandle},
    file_icon,
    shell::{pick_icon, reveal_in_explorer, shell_open},
    task::BackgroundTask,
    ui::Message,
};

use super::folders::{self, PALETTE};
use super::pictures::{self, BingImage, Picture, Resolution};
use super::themes::{self, Theme};

type Thumbs = Vec<(String, [usize; 2], Vec<u8>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Orientation {
    All,
    Landscape,
    Portrait,
}

pub struct Gallery {
    textures: HashMap<String, Option<TextureHandle>>,
    // Folder colorizer
    folders: Vec<PathBuf>,
    color: usize,
    custom_icon: Option<String>,
    // Themes
    themes: Option<Vec<Theme>>,
    // Lock screen
    pictures: Option<Vec<Picture>>,
    orientation: Orientation,
    lock_dest: PathBuf,
    // Bing
    bing: Option<Vec<BingImage>>,
    bing_task: Option<BackgroundTask<Vec<BingImage>>>,
    resolution: Resolution,
    bing_dest: PathBuf,
    download: Option<(bool, BackgroundTask<PathBuf>)>,
    // Thumbnails decoded in the background, by key.
    thumbs: HashMap<String, TextureHandle>,
    thumb_task: Option<BackgroundTask<Thumbs>>,
}

impl Default for Gallery {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
            folders: Vec::new(),
            color: 6,
            custom_icon: None,
            themes: None,
            pictures: None,
            orientation: Orientation::Landscape,
            lock_dest: pictures::default_folder("Lock screen"),
            bing: None,
            bing_task: None,
            resolution: Resolution::Uhd,
            bing_dest: pictures::default_folder("Bing"),
            download: None,
            thumbs: HashMap::new(),
            thumb_task: None,
        }
    }
}

fn err(e: anyhow::Error) -> Result<String, String> {
    Err(format!("{e:#}"))
}

impl Gallery {
    fn icon_texture(&mut self, ctx: &egui::Context, spec: &str) -> Option<TextureHandle> {
        if let Some(t) = self.textures.get(spec) {
            return t.clone();
        }
        let tex = file_icon::extract_spec(spec, 32).map(|img| {
            let image = egui::ColorImage::from_rgba_unmultiplied([img.size, img.size], &img.rgba);
            ctx.load_texture(format!("gallery-{spec}"), image, egui::TextureOptions::LINEAR)
        });
        self.textures.insert(spec.to_owned(), tex.clone());
        tex
    }

    fn swatch_texture(&mut self, ctx: &egui::Context, i: usize) -> TextureHandle {
        let key = format!("swatch-{i}");
        if let Some(Some(t)) = self.textures.get(&key) {
            return t.clone();
        }
        let img = folders::render_folder(40, PALETTE[i].1);
        let image = egui::ColorImage::from_rgba_unmultiplied([40, 40], img.as_raw());
        let t = ctx.load_texture(key.clone(), image, egui::TextureOptions::LINEAR);
        self.textures.insert(key, Some(t.clone()));
        t
    }

    /// Starts decoding thumbnails for `(key, path)` pairs not loaded yet.
    fn load_thumbs(&mut self, ctx: &egui::Context, files: Vec<(String, PathBuf)>) {
        if self.thumb_task.is_some() {
            return;
        }
        let todo: Vec<(String, PathBuf)> = files
            .into_iter()
            .filter(|(k, _)| !self.thumbs.contains_key(k))
            .collect();
        if todo.is_empty() {
            return;
        }
        self.thumb_task = Some(BackgroundTask::spawn(ctx, move |_| {
            Ok(todo
                .into_iter()
                .filter_map(|(k, p)| {
                    // Windows' thumbnail cache is much faster than decoding a 4K photo.
                    if let Some((w, h, rgba)) = file_icon::thumbnail(&p, 240) {
                        return Some((k, [w, h], rgba));
                    }
                    let bytes = std::fs::read(&p).ok()?;
                    let (size, rgba) = pictures::thumbnail(&bytes, 240)?;
                    Some((k, size, rgba))
                })
                .collect())
        }));
    }

    pub fn poll(&mut self, ctx: &egui::Context, app: &AppCtx<'_>, message: &mut Message) {
        if let Some(t) = &mut self.thumb_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.thumb_task = None;
                for (k, size, rgba) in r.unwrap_or_default() {
                    let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                    let tex = ctx.load_texture(format!("thumb-{k}"), image, egui::TextureOptions::LINEAR);
                    self.thumbs.insert(k, tex);
                }
            }
        }
        if let Some(t) = &mut self.bing_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.bing_task = None;
                match r {
                    Ok(list) => self.bing = Some(list),
                    Err(e) => *message = Some(Err(format!("Could not reach Bing: {e}"))),
                }
            }
        }
        if let Some((as_wallpaper, t)) = &mut self.download {
            t.poll();
            let as_wallpaper = *as_wallpaper;
            if let Some(r) = t.take_result() {
                self.download = None;
                *message = Some(match r {
                    Ok(path) if as_wallpaper => pictures::set_wallpaper(app, &path)
                        .map(|()| "Wallpaper set.".into())
                        .or_else(err),
                    Ok(path) => Ok(format!("Saved to {}.", path.display())),
                    Err(e) => Err(e),
                });
            }
        }
    }

    // -----------------------------------------------------------------------------

    pub fn folders_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, message: &mut Message) {
        ui.label("Give folders a colour, or any icon. Explorer stores it in the folder's own desktop.ini, so it stays with the folder.");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Add folders…").clicked()
                && let Some(dirs) = rfd::FileDialog::new().pick_folders()
            {
                for d in dirs {
                    if !self.folders.contains(&d) {
                        self.folders.push(d);
                    }
                }
            }
            if ui
                .add_enabled(!self.folders.is_empty(), egui::Button::new("Clear the list"))
                .clicked()
            {
                self.folders.clear();
            }
        });
        let mut remove = None;
        let default_folder = r"%SystemRoot%\System32\shell32.dll,3".to_owned();
        for (i, dir) in self.folders.clone().iter().enumerate() {
            ui.horizontal(|ui| {
                let spec = folders::folder_icon(dir).unwrap_or_else(|| default_folder.clone());
                match self.icon_texture(ui.ctx(), &spec) {
                    Some(t) => ui.add(egui::Image::new((t.id(), egui::vec2(24.0, 24.0)))),
                    None => ui.label(""),
                };
                ui.label(dir.display().to_string());
                if ui
                    .small_button("✕")
                    .on_hover_text("Remove from the list")
                    .clicked()
                {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.folders.remove(i);
        }
        if self.folders.is_empty() {
            ui.weak("No folders yet. Add one or more to colour them.");
        }

        ui.add_space(8.0);
        ui.strong("Colour");
        ui.horizontal_wrapped(|ui| {
            for (i, (name, _)) in PALETTE.iter().enumerate() {
                let tex = self.swatch_texture(ui.ctx(), i);
                let selected = self.custom_icon.is_none() && self.color == i;
                let r = ui
                    .add(egui::Button::image((tex.id(), egui::vec2(32.0, 32.0))).selected(selected))
                    .on_hover_text(*name);
                if r.clicked() {
                    self.color = i;
                    self.custom_icon = None;
                }
            }
            let label = match &self.custom_icon {
                Some(_) => "Other icon ✔",
                None => "Other icon…",
            };
            if ui.button(label).clicked()
                && let Some(spec) = pick_icon(self.custom_icon.as_deref().unwrap_or(""))
            {
                self.custom_icon = Some(spec);
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let any = !self.folders.is_empty();
            if ui
                .add_enabled(any, egui::Button::new("Apply to the listed folders"))
                .clicked()
            {
                *message = Some(self.apply_folders(ctx, true));
            }
            if ui
                .add_enabled(any, egui::Button::new("Restore their default icon"))
                .clicked()
            {
                *message = Some(self.apply_folders(ctx, false));
            }
        });
    }

    fn apply_folders(&mut self, ctx: &AppCtx<'_>, set: bool) -> Result<String, String> {
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        let spec = if set {
            Some(match &self.custom_icon {
                Some(s) => s.clone(),
                None => format!(
                    "{},0",
                    folders::color_icon(PALETTE[self.color].1)
                        .map_err(|e| format!("{e:#}"))?
                        .display()
                ),
            })
        } else {
            None
        };
        let mut failed = Vec::new();
        for d in &self.folders {
            if let Err(e) = folders::set_folder_icon(ctx, d, spec.as_deref()) {
                failed.push(format!("{}: {e:#}", d.display()));
            }
        }
        // Icons were read from the folders before; read them again.
        self.textures.retain(|k, _| k.starts_with("swatch-"));
        if failed.is_empty() {
            Ok(format!(
                "{} {} folder(s). Explorer may take a moment to show it.",
                if set { "Changed" } else { "Restored" },
                self.folders.len()
            ))
        } else {
            Err(failed.join("; "))
        }
    }

    // -----------------------------------------------------------------------------

    pub fn themes_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, message: &mut Message) {
        let list = self.themes.get_or_insert_with(themes::list).clone();
        let current = themes::current();
        ui.horizontal(|ui| {
            ui.label("Installed themes. Applying one changes the wallpaper, colours and sounds together; Windows shows its Personalization page while it does.");
            if ui.button("Refresh").clicked() {
                self.themes = None;
            }
        });
        self.load_thumbs(
            ui.ctx(),
            list.iter()
                .filter_map(|t| Some((t.path.display().to_string(), t.wallpaper.clone()?)))
                .collect(),
        );
        let mut apply = None;
        egui::ScrollArea::vertical()
            .id_salt("themes")
            .max_height((ui.available_height() - 30.0).max(200.0))
            .show(ui, |ui| {
                egui::Grid::new("themes-grid")
                    .num_columns(3)
                    .spacing([16.0, 16.0])
                    .show(ui, |ui| {
                        for (i, t) in list.iter().enumerate() {
                            ui.vertical(|ui| {
                                ui.set_width(200.0);
                                match self.thumbs.get(&t.path.display().to_string()) {
                                    Some(tex) => {
                                        ui.add(
                                            egui::Image::new(tex).fit_to_exact_size(egui::vec2(192.0, 108.0)),
                                        );
                                    }
                                    None => {
                                        ui.allocate_space(egui::vec2(192.0, 108.0));
                                    }
                                }
                                let is_current = current.as_ref().is_some_and(|c| {
                                    c.to_string_lossy()
                                        .eq_ignore_ascii_case(&t.path.to_string_lossy())
                                });
                                ui.horizontal(|ui| {
                                    ui.strong(&t.name);
                                    if is_current {
                                        ui.weak("· in use");
                                    }
                                });
                                if ui.add_enabled(!is_current, egui::Button::new("Apply")).clicked() {
                                    apply = Some(t.clone());
                                }
                            });
                            if i % 3 == 2 {
                                ui.end_row();
                            }
                        }
                    });
            });
        if let Some(t) = apply {
            *message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                themes::apply(ctx, &t)
                    .map(|()| format!("Applying {}…", t.name))
                    .or_else(err)
            });
        }
    }

    // -----------------------------------------------------------------------------

    fn dest_row(ui: &mut egui::Ui, dest: &mut PathBuf) {
        ui.horizontal(|ui| {
            ui.label("Save to:");
            ui.monospace(dest.display().to_string());
            if ui.button("Change…").clicked()
                && let Some(d) = rfd::FileDialog::new().set_directory(&*dest).pick_folder()
            {
                *dest = d;
            }
            if ui.button("Open").clicked() {
                if dest.is_dir() {
                    reveal_in_explorer(dest);
                } else if let Some(parent) = dest.parent() {
                    let _ = shell_open(&parent.to_string_lossy(), "");
                }
            }
        });
    }

    pub fn lock_screen_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, message: &mut Message) {
        ui.horizontal(|ui| {
            ui.label("Pictures Windows Spotlight has shown on your lock screen and desktop.");
            if ui.button("Refresh").clicked() {
                self.pictures = None;
            }
        });
        let all = self.pictures.get_or_insert_with(pictures::spotlight).clone();
        ui.horizontal(|ui| {
            ui.label("Show:");
            ui.radio_value(
                &mut self.orientation,
                Orientation::Landscape,
                "Landscape (desktop)",
            );
            ui.radio_value(&mut self.orientation, Orientation::Portrait, "Portrait (phone)");
            ui.radio_value(&mut self.orientation, Orientation::All, "All");
        });
        let shown: Vec<Picture> = all
            .into_iter()
            .filter(|p| match self.orientation {
                Orientation::All => true,
                Orientation::Landscape => p.landscape(),
                Orientation::Portrait => !p.landscape(),
            })
            .collect();
        Self::dest_row(ui, &mut self.lock_dest);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !shown.is_empty(),
                    egui::Button::new(format!("Save all {}", shown.len())),
                )
                .clicked()
            {
                *message = Some(
                    pictures::save_all(&shown, &self.lock_dest)
                        .map(|n| format!("Saved {n} new picture(s)."))
                        .or_else(err),
                );
            }
        });
        if shown.is_empty() {
            ui.weak("None found. Turn on Windows Spotlight for the lock screen (Settings > Personalization > Lock screen) and pictures collect over a few days.");
            return;
        }
        self.load_thumbs(
            ui.ctx(),
            shown
                .iter()
                .map(|p| (p.path.display().to_string(), p.path.clone()))
                .collect(),
        );
        let mut wallpaper = None;
        egui::ScrollArea::vertical()
            .id_salt("lockscreen")
            .max_height((ui.available_height() - 30.0).max(200.0))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for p in &shown {
                        ui.vertical(|ui| {
                            ui.set_width(200.0);
                            let size = if p.landscape() {
                                egui::vec2(192.0, 108.0)
                            } else {
                                egui::vec2(61.0, 108.0)
                            };
                            match self.thumbs.get(&p.path.display().to_string()) {
                                Some(t) => {
                                    ui.add(egui::Image::new(t).fit_to_exact_size(size));
                                }
                                None => {
                                    ui.allocate_space(size);
                                }
                            }
                            ui.weak(format!("{} × {}", p.width, p.height));
                            if p.landscape() && ui.small_button("Set as wallpaper").clicked() {
                                wallpaper = Some(p.clone());
                            }
                        });
                    }
                });
            });
        if let Some(p) = wallpaper {
            *message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                // Use a saved copy: Windows may delete its own copy later.
                pictures::save_all(std::slice::from_ref(&p), &self.lock_dest)
                    .and_then(|_| {
                        let stem: String = p
                            .path
                            .file_stem()
                            .map(|s| s.to_string_lossy().chars().take(24).collect())
                            .unwrap_or_default();
                        pictures::set_wallpaper(ctx, &self.lock_dest.join(format!("{stem}.jpg")))
                    })
                    .map(|()| "Wallpaper set (a copy was saved first).".into())
                    .or_else(err)
            });
        }
    }

    // -----------------------------------------------------------------------------

    pub fn bing_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, message: &mut Message) {
        ui.label("Bing's picture of the day for the last eight days. The list and pictures come from bing.com when you ask for them.");
        ui.horizontal(|ui| {
            let label = if self.bing.is_some() {
                "Check again"
            } else {
                "Get the pictures"
            };
            if ui
                .add_enabled(self.bing_task.is_none(), egui::Button::new(label))
                .clicked()
            {
                self.bing_task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                    let list = pictures::bing_list()?;
                    Ok(list)
                }));
            }
            if self.bing_task.is_some() || self.download.is_some() {
                ui.spinner();
            }
            ui.label("Size:");
            egui::ComboBox::from_id_salt("bing-res")
                .selected_text(self.resolution.label())
                .show_ui(ui, |ui| {
                    for r in Resolution::ALL {
                        ui.selectable_value(&mut self.resolution, r, r.label());
                    }
                });
        });
        Self::dest_row(ui, &mut self.bing_dest);
        let Some(list) = self.bing.clone() else { return };
        // Previews are small and come from Bing too.
        if self.thumb_task.is_none() && list.iter().any(|b| !self.thumbs.contains_key(&b.date)) {
            let todo: Vec<BingImage> = list
                .iter()
                .filter(|b| !self.thumbs.contains_key(&b.date))
                .cloned()
                .collect();
            self.thumb_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                Ok(todo
                    .into_iter()
                    .filter_map(|b| {
                        let bytes = b.preview().ok()?;
                        let (size, rgba) = pictures::thumbnail(&bytes, 240)?;
                        Some((b.date, size, rgba))
                    })
                    .collect())
            }));
        }
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("bing")
            .max_height((ui.available_height() - 30.0).max(200.0))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (i, b) in list.iter().enumerate() {
                        ui.vertical(|ui| {
                            ui.set_width(220.0);
                            match self.thumbs.get(&b.date) {
                                Some(t) => {
                                    ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(200.0, 120.0)));
                                }
                                None => {
                                    ui.allocate_space(egui::vec2(200.0, 120.0));
                                }
                            }
                            ui.strong(if b.title.is_empty() { &b.date } else { &b.title });
                            ui.add(egui::Label::new(egui::RichText::new(&b.copyright).weak().small()).wrap());
                            ui.horizontal(|ui| {
                                let free = self.download.is_none();
                                if ui.add_enabled(free, egui::Button::new("Save")).clicked() {
                                    action = Some((i, false));
                                }
                                if ui
                                    .add_enabled(free, egui::Button::new("Set as wallpaper"))
                                    .clicked()
                                {
                                    action = Some((i, true));
                                }
                            });
                        });
                    }
                });
            });
        if let Some((i, wallpaper)) = action {
            if ctx.config.dry_run {
                *message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                return;
            }
            let (b, res, dest) = (list[i].clone(), self.resolution, self.bing_dest.clone());
            self.download = Some((
                wallpaper,
                BackgroundTask::spawn(ui.ctx(), move |_| b.save(res, &dest)),
            ));
        }
    }
}
