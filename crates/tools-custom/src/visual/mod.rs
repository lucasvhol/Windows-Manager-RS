//! Visual Customizer: appearance switches, fonts and sizes of window parts, icon
//! replacements (desktop system icons, Windows' stock icons, file-type icons), folder
//! colours, themes, and lock-screen and Bing pictures.

mod folders;
mod fonts;
mod gallery;
mod icons;
pub mod pictures;
mod themes;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, TextureHandle},
    file_icon, icons as glyph,
    shell::pick_icon,
    tool_settings,
    tweaks::TweakPanel,
    ui::{Confirm, Message, frame, section_nav, show_message},
};

use fonts::{Element, Metrics, Size};

const ID: &str = "visual";

const SECTIONS: [(&str, &str); 9] = [
    (glyph::GEAR, "System Settings"),
    (glyph::PENCIL_SIMPLE, "Font & Size"),
    (glyph::DESKTOP, "System Icon"),
    (glyph::IMAGE, "Other Icon"),
    (glyph::FILE_TEXT, "File Type Icon"),
    (glyph::FOLDERS, "Folder Colorizer"),
    (glyph::PAINT_BRUSH, "Theme Changer"),
    (glyph::LOCK, "Lock Screen Extractor"),
    (glyph::GLOBE, "Bing Images Downloader"),
];

/// Extensions whose icon this tool changed (only those get a Restore button, so an
/// app's own per-user icon is never removed by mistake).
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    changed_types: Vec<String>,
}

enum Ask {
    ApplyMetrics(Box<Metrics>),
    SystemFont(String),
    RestoreSystemFont,
}

pub struct VisualCustomizer {
    section: usize,
    panel: TweakPanel,
    metrics: Option<Result<Metrics, String>>,
    edited: Option<Metrics>,
    families: Vec<String>,
    text_scale: u32,
    system_font: String,
    textures: HashMap<String, Option<TextureHandle>>,
    extensions: Option<Vec<String>>,
    filter: String,
    saved: Saved,
    gallery: gallery::Gallery,
    confirm: Confirm<Ask>,
    message: Message,
}

impl VisualCustomizer {
    pub fn new(definitions: &str) -> Self {
        Self {
            section: 0,
            panel: TweakPanel::new(ID, definitions),
            metrics: None,
            edited: None,
            families: Vec::new(),
            text_scale: fonts::text_scale(),
            system_font: fonts::system_font_substitute().unwrap_or_else(|| "Segoe UI".into()),
            textures: HashMap::new(),
            extensions: None,
            filter: String::new(),
            saved: tool_settings::load(ID),
            gallery: gallery::Gallery::default(),
            confirm: Confirm::default(),
            message: None,
        }
    }

    fn texture(&mut self, ctx: &egui::Context, spec: &str) -> Option<TextureHandle> {
        if let Some(t) = self.textures.get(spec) {
            return t.clone();
        }
        let tex = file_icon::extract_spec(spec, 32).map(|img| {
            let image = egui::ColorImage::from_rgba_unmultiplied([img.size, img.size], &img.rgba);
            ctx.load_texture(format!("visual-{spec}"), image, egui::TextureOptions::LINEAR)
        });
        self.textures.insert(spec.to_owned(), tex.clone());
        tex
    }

    fn icon(&mut self, ui: &mut egui::Ui, spec: &str) {
        match self.texture(ui.ctx(), spec) {
            Some(t) => {
                ui.add(egui::Image::new((t.id(), egui::vec2(32.0, 32.0))));
            }
            None => {
                ui.allocate_space(egui::vec2(32.0, 32.0));
            }
        }
    }

    fn report(&mut self, r: anyhow::Result<()>, ok: impl Into<String>) {
        self.message = Some(r.map(|()| ok.into()).map_err(|e| format!("{e:#}")));
    }

    fn handle_confirm(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) else {
            return;
        };
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        match ask {
            Ask::ApplyMetrics(m) => {
                let r = fonts::apply_journaled(ctx, &m, "Changed window fonts and sizes");
                if r.is_ok() {
                    self.metrics = Some(Ok(*m));
                }
                self.report(r, "Applied. Open windows update as they redraw.");
            }
            Ask::SystemFont(f) => {
                let r = fonts::replace_system_font(ctx, &f);
                self.report(r, format!("{f} replaces Segoe UI after a restart."));
            }
            Ask::RestoreSystemFont => {
                let r = fonts::restore_system_font(ctx);
                if r.is_ok() {
                    self.system_font = "Segoe UI".into();
                }
                self.report(r, "Segoe UI is the system font again after a restart.");
            }
        }
    }

    fn fonts_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.metrics.is_none() {
            self.metrics = Some(Metrics::read().map_err(|e| format!("{e:#}")));
            self.families = fonts::families();
        }
        let current = match &self.metrics {
            Some(Ok(m)) => *m,
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        let dpi = fonts::system_dpi();
        let edited = self.edited.get_or_insert(current);
        ui.label("Fonts and sizes of classic window parts. Apps with their own design (Settings, Store apps) keep theirs.");
        ui.add_space(4.0);
        egui::Grid::new("visual-fonts")
            .num_columns(4)
            .spacing([12.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Item");
                ui.strong("Font");
                ui.strong("Size");
                ui.strong("Bold");
                ui.end_row();
                for e in Element::ALL {
                    ui.label(e.label());
                    let mut face = edited.face(e);
                    egui::ComboBox::from_id_salt(("font", e as u8))
                        .width(200.0)
                        .selected_text(&face)
                        .height(320.0)
                        .show_ui(ui, |ui| {
                            for f in &self.families {
                                ui.selectable_value(&mut face, f.clone(), f);
                            }
                        });
                    if face != edited.face(e) {
                        edited.set_face(e, &face);
                    }
                    let mut pt = edited.points(e, dpi);
                    if ui
                        .add(egui::DragValue::new(&mut pt).range(6..=24).suffix(" pt"))
                        .changed()
                    {
                        edited.set_points(e, pt, dpi);
                    }
                    let mut bold = edited.bold(e);
                    if ui.checkbox(&mut bold, "").changed() {
                        edited.set_bold(e, bold);
                    }
                    ui.end_row();
                }
                for s in Size::ALL {
                    ui.label(s.label());
                    let mut px = edited.size(s);
                    if ui
                        .add(egui::DragValue::new(&mut px).range(s.range()).suffix(" px"))
                        .changed()
                    {
                        edited.set_size(s, px);
                    }
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                }
            });
        let edited = *edited;
        ui.horizontal(|ui| {
            let dirty = edited != current;
            if ui.add_enabled(dirty, egui::Button::new("Apply")).clicked() {
                self.confirm.ask(
                    "Apply these fonts and sizes now? Windows saves them for your account.",
                    Ask::ApplyMetrics(Box::new(edited)),
                );
            }
            if ui
                .add_enabled(dirty, egui::Button::new("Discard changes"))
                .clicked()
            {
                self.edited = Some(current);
            }
            if ui.button("Windows defaults").clicked() {
                self.edited = Some(current.windows_defaults(dpi));
            }
            if ui.button("Re-read").clicked() {
                self.metrics = None;
                self.edited = None;
            }
        });

        ui.add_space(10.0);
        ui.strong("Text size");
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut self.text_scale, 100..=225).suffix(" %"));
            if ui.button("Apply").clicked() {
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                } else {
                    let r = fonts::set_text_scale(ctx, self.text_scale);
                    self.report(
                        r,
                        "Saved. Sign out and back in to see the new text size everywhere.",
                    );
                }
            }
            ui.weak("Makes text larger in apps and Windows without changing display scaling.");
        });

        ui.add_space(10.0);
        ui.strong("System font");
        ui.horizontal(|ui| {
            let current_sub = fonts::system_font_substitute();
            egui::ComboBox::from_id_salt("system-font")
                .width(220.0)
                .selected_text(&self.system_font)
                .height(320.0)
                .show_ui(ui, |ui| {
                    for f in &self.families {
                        ui.selectable_value(&mut self.system_font, f.clone(), f);
                    }
                });
            let hint = ctx.i18n.tr("shell-needs-admin");
            if ui
                .add_enabled(ctx.elevated, egui::Button::new("Use instead of Segoe UI"))
                .on_disabled_hover_text(&hint)
                .clicked()
            {
                self.confirm.ask(
                    format!(
                        "Use {} wherever Windows uses Segoe UI? Some text may not fit. A restart is needed, and it can be undone.",
                        self.system_font
                    ),
                    Ask::SystemFont(self.system_font.clone()),
                );
            }
            if ui
                .add_enabled(
                    ctx.elevated && current_sub.is_some(),
                    egui::Button::new("Restore Segoe UI"),
                )
                .on_disabled_hover_text(&hint)
                .clicked()
            {
                self.confirm
                    .ask("Make Segoe UI the system font again? A restart is needed.", Ask::RestoreSystemFont);
            }
            match current_sub {
                Some(s) => ui.weak(format!("Now: {s}")),
                None => ui.weak("Now: Segoe UI (the Windows default)"),
            };
        });
    }

    fn system_icons_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Icons of the desktop items. Changes apply to your account only.");
        ui.add_space(4.0);
        let mut action = None;
        egui::Grid::new("visual-sysicons")
            .num_columns(4)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for (i, s) in icons::SYSTEM_ICONS.iter().enumerate() {
                    let (spec, changed) = s.current();
                    self.icon(ui, &spec);
                    ui.label(s.label);
                    if ui.button("Change…").clicked() {
                        action = Some((i, true, spec.clone()));
                    }
                    if ui.add_enabled(changed, egui::Button::new("Restore")).clicked() {
                        action = Some((i, false, spec));
                    }
                    ui.end_row();
                }
            });
        if let Some((i, change, spec)) = action {
            let item = &icons::SYSTEM_ICONS[i];
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else if change {
                if let Some(new) = pick_icon(&spec) {
                    let r = item.set(ctx, &new);
                    self.report(r, format!("{} icon changed.", item.label));
                }
            } else {
                let r = item.reset(ctx);
                self.report(r, format!("{} icon restored.", item.label));
            }
        }
        ui.add_space(6.0);
        if ui.link("Choose which of these appear on the desktop").clicked() {
            self.report(
                wm_core::shell::shell_open("desk.cpl", ",,5"),
                "Desktop Icon Settings opened.",
            );
        }
    }

    fn other_icons_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Windows' stock icons for folders, drives and overlays. Changes apply to every account on this PC.");
        if !ctx.elevated {
            ui.colored_label(ui.visuals().warn_fg_color, ctx.i18n.tr("shell-needs-admin"));
        }
        ui.add_space(4.0);
        let mut action = None;
        egui::Grid::new("visual-shellicons")
            .num_columns(4)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for (index, label) in icons::SHELL_ICONS {
                    let replaced = icons::shell_icon(index);
                    let spec = replaced
                        .clone()
                        .unwrap_or_else(|| icons::shell_icon_default(index));
                    self.icon(ui, &spec);
                    ui.label(label);
                    if ui
                        .add_enabled(ctx.elevated, egui::Button::new("Change…"))
                        .clicked()
                    {
                        action = Some((index, label, Some(spec.clone())));
                    }
                    if ui
                        .add_enabled(ctx.elevated && replaced.is_some(), egui::Button::new("Restore"))
                        .clicked()
                    {
                        action = Some((index, label, None));
                    }
                    ui.end_row();
                }
            });
        if let Some((index, label, change)) = action {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                match change {
                    Some(spec) => {
                        if let Some(new) = pick_icon(&spec) {
                            let r = icons::set_shell_icon(ctx, index, label, Some(&new));
                            self.report(
                                r,
                                format!("{label}: replaced. Explorer shows it after its icons refresh."),
                            );
                        }
                    }
                    None => {
                        let r = icons::set_shell_icon(ctx, index, label, None);
                        self.report(r, format!("{label}: restored."));
                    }
                }
            }
        }
    }

    fn file_types_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let exts = self.extensions.get_or_insert_with(icons::extensions).clone();
        ui.horizontal(|ui| {
            ui.label("Find a file type:");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(".txt, pdf…")
                    .desired_width(160.0),
            );
            ui.weak(format!("{} registered types", exts.len()));
        });
        if !self.saved.changed_types.is_empty() {
            ui.weak(format!(
                "Changed with this tool: {}",
                self.saved.changed_types.join(", ")
            ));
        }
        let needle = self.filter.trim().trim_start_matches('.').to_ascii_lowercase();
        let shown: Vec<String> = if needle.is_empty() {
            self.saved.changed_types.clone()
        } else {
            exts.into_iter().filter(|e| e[1..].contains(&needle)).collect()
        };
        if shown.is_empty() {
            ui.weak(if needle.is_empty() {
                "Type part of an extension to list matching file types."
            } else {
                "No registered file type matches."
            });
            return;
        }
        let mut action: Option<(String, bool, String)> = None;
        let row_h = 38.0;
        egui::ScrollArea::vertical()
            .id_salt("visual-types")
            .max_height((ui.available_height() - 40.0).max(200.0))
            .show_rows(ui, row_h, shown.len(), |ui, range| {
                for ext in &shown[range] {
                    let spec = icons::file_type_icon(ext);
                    // No registered icon: the default app supplies one; show a generic file.
                    let shown_spec = if spec.is_empty() {
                        r"%SystemRoot%\System32\imageres.dll,2".to_owned()
                    } else {
                        spec.clone()
                    };
                    ui.horizontal(|ui| {
                        ui.set_min_height(row_h - 4.0);
                        self.icon(ui, &shown_spec);
                        ui.add_sized([90.0, 20.0], egui::Label::new(egui::RichText::new(ext).strong()));
                        ui.add_sized(
                            [220.0, 20.0],
                            egui::Label::new(
                                egui::RichText::new(icons::prog_id(ext).unwrap_or_default()).weak(),
                            )
                            .truncate(),
                        );
                        let own = icons::per_file_icon(&spec);
                        if ui
                            .add_enabled(!own, egui::Button::new("Change…"))
                            .on_disabled_hover_text("Files of this type show their own icons.")
                            .clicked()
                        {
                            action = Some((ext.clone(), true, spec.clone()));
                        }
                        if self.saved.changed_types.contains(ext) && ui.button("Restore").clicked() {
                            action = Some((ext.clone(), false, spec.clone()));
                        }
                    });
                }
            });
        if let Some((ext, change, spec)) = action {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                return;
            }
            let result = if change {
                let Some(new) = pick_icon(&spec) else { return };
                icons::set_file_type_icon(ctx, &ext, Some(&new)).map(|_| {
                    if !self.saved.changed_types.contains(&ext) {
                        self.saved.changed_types.push(ext.clone());
                        self.saved.changed_types.sort();
                    }
                    format!("{ext} icon changed.")
                })
            } else {
                icons::set_file_type_icon(ctx, &ext, None).map(|_| {
                    self.saved.changed_types.retain(|e| *e != ext);
                    format!("{ext} icon restored.")
                })
            };
            self.message = Some(result.map_err(|e| format!("{e:#}")));
            if let Err(e) = tool_settings::save(ID, &self.saved) {
                self.message = Some(Err(format!("{e:#}")));
            }
        }
    }
}

impl Tool for VisualCustomizer {
    fn id(&self) -> &'static str {
        ID
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.handle_confirm(ui, ctx);
        self.gallery.poll(ui.ctx(), ctx, &mut self.message);
        // The original's heading band.
        egui::Panel::top("visual-band")
            .resizable(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Customize Windows visual appearance and interface styles")
                            .strong()
                            .size(15.0),
                    );
                });
            });
        if frame::left(ui, 210.0, |ui| section_nav(ui, &SECTIONS, &mut self.section)) {
            self.message = None;
        }
        if self.section == 0 {
            frame::nested(ui, |ui| self.panel.ui_framed(ui, ctx));
            return;
        }
        frame::central(ui, |ui| self.page(ui, ctx));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl VisualCustomizer {
    fn page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        show_message(ui, &self.message);
        match self.section {
            0 => {}
            1 => self.fonts_page(ui, ctx),
            2 => self.system_icons_page(ui, ctx),
            3 => self.other_icons_page(ui, ctx),
            4 => self.file_types_page(ui, ctx),
            5 => self.gallery.folders_page(ui, ctx, &mut self.message),
            6 => self.gallery.themes_page(ui, ctx, &mut self.message),
            7 => self.gallery.lock_screen_page(ui, ctx, &mut self.message),
            _ => self.gallery.bing_page(ui, ctx, &mut self.message),
        }
    }
}
