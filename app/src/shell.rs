//! Main window (hub) and per-tool native windows.
//!
//! The hub has a header band, a left category rail and a grid of tool tiles; each tool
//! opens in its own OS window (an egui immediate viewport), so several can be open at once.

use wm_core::{
    APP_NAME, AppCtx, Category, Tool,
    config::{Config, Theme},
    egui::{self, Color32, ViewportBuilder, ViewportClass, ViewportId},
    elevation,
    i18n::I18n,
    icons,
    safety::ChangeJournal,
};

use crate::hub::{self, Summary, catalog, widgets};

pub struct Shell {
    tools: Vec<Box<dyn Tool>>,
    open: Vec<bool>,
    category: Category,
    search: String,
    config: Config,
    saved_config: Config,
    config_path: Option<std::path::PathBuf>,
    journal: ChangeJournal,
    i18n: I18n,
    elevated: bool,
    error: Option<String>,
    summary: Summary,
    about_open: bool,
    updater: crate::updater::Updater,
    locked: bool,
    password_input: String,
    password_wrong: bool,
    /// Tells the minimise watcher whether to hide the window (set once the handle is known).
    tray_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Shell {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let config_path = Config::default_path().ok();
        let config = config_path
            .as_deref()
            .map(Config::load_or_default)
            .unwrap_or_default();
        let (journal, error) = match ChangeJournal::open_default() {
            Ok(j) => (j, None),
            Err(e) => {
                // Fall back to a temp journal so the app still runs; surface the problem.
                let tmp = std::env::temp_dir().join("win-manager-rs");
                let j = ChangeJournal::open(&tmp.join("journal.json"), &tmp.join("backups"))
                    .expect("temp journal");
                (j, Some(format!("Could not open change journal: {e:#}")))
            }
        };
        icons::install(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, config.theme);
        cc.egui_ctx.set_zoom_factor(config.ui_scale.clamp(0.75, 2.0));
        let tools = crate::tools::all();
        // `--open <tool-id>` (repeatable) opens tool windows at start, e.g. from a shortcut.
        let args: Vec<String> = std::env::args().collect();
        let requested: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "--open")
            .map(|w| w[1].as_str())
            .collect();
        let open = tools.iter().map(|t| requested.contains(&t.id())).collect();
        // `--category <n>` starts on the n-th rail entry (0 = Home).
        let category = args
            .windows(2)
            .find(|w| w[0] == "--category")
            .and_then(|w| w[1].parse::<usize>().ok())
            .and_then(|n| Category::ALL.get(n).copied())
            .unwrap_or(Category::Home);
        let config_locked = config.password_hash.is_some();
        Self {
            i18n: I18n::new(&config.language),
            tools,
            open,
            category,
            search: String::new(),
            saved_config: config.clone(),
            config,
            config_path,
            journal,
            elevated: elevation::is_elevated(),
            error,
            summary: Summary::collect(),
            about_open: false,
            updater: Default::default(),
            locked: config_locked,
            password_input: String::new(),
            password_wrong: false,
            tray_flag: None,
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.tools.iter().position(|t| t.id() == id)
    }

    fn open_tool(&mut self, ctx: &egui::Context, id: &str) {
        let Some(i) = self.index_of(id) else { return };
        if self.open[i] {
            ctx.send_viewport_cmd_to(viewport_id(id), egui::ViewportCommand::Focus);
        }
        self.open[i] = true;
        if id != "journal" && id != "settings" {
            self.config.touch_recent(id);
        }
    }

    fn title_of(&self, id: &str) -> String {
        self.i18n.tr(&format!("{id}-title"))
    }

    fn description_of(&self, id: &str) -> String {
        let key = format!("{id}-desc");
        let d = self.i18n.tr(&key);
        if d == key { String::new() } else { d }
    }

    fn header(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        let fill = widgets::band_fill(root);
        egui::Panel::top("hub_header")
            .exact_size(70.0)
            .frame(
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(root, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new(icons::WRENCH)
                            .size(38.0)
                            .color(widgets::ACCENT),
                    );
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(APP_NAME).size(24.0).strong());
                        ui.weak(format!("for Windows 10/11 · v{}", env!("CARGO_PKG_VERSION")));
                    });
                    ui.add_space(24.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text(format!(
                                "{}  {}",
                                icons::MAGNIFYING_GLASS,
                                self.i18n.tr("shell-search")
                            ))
                            .desired_width(240.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(format!("{} {}", icons::INFO, self.i18n.tr("hub-about")))
                            .clicked()
                        {
                            self.about_open = true;
                        }
                        self.updater.button(ui);
                        if ui
                            .button(format!("{} {}", icons::GEAR, self.title_of("settings")))
                            .clicked()
                        {
                            self.open_tool(&ctx, "settings");
                        }
                        if ui
                            .button(format!(
                                "{} {}",
                                icons::CLOCK_COUNTER_CLOCKWISE,
                                self.title_of("journal")
                            ))
                            .clicked()
                        {
                            self.open_tool(&ctx, "journal");
                        }
                        ui.separator();
                        if self.elevated {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {}",
                                    icons::SHIELD_CHECK,
                                    self.i18n.tr("shell-admin")
                                ))
                                .strong(),
                            );
                        } else if ui
                            .button(format!(
                                "{} {}",
                                icons::SHIELD,
                                self.i18n.tr("shell-restart-admin")
                            ))
                            .clicked()
                        {
                            self.restart_as_admin(&ctx);
                        }
                    });
                });
            });
    }

    fn rail(&mut self, root: &mut egui::Ui) {
        let content_fill = root.visuals().panel_fill;
        let fill = widgets::rail_fill(root);
        egui::Panel::left("hub_rail")
            .exact_size(214.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(fill))
            .show(root, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(4.0);
                for cat in Category::ALL {
                    let selected = self.category == cat && self.search.is_empty();
                    if widgets::rail_item(
                        ui,
                        catalog::category_icon(cat),
                        &self.i18n.tr(cat.i18n_key()),
                        selected,
                        content_fill,
                    )
                    .clicked()
                    {
                        self.category = cat;
                        self.search.clear();
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.weak(&self.summary.build);
                    ui.weak(&self.summary.os);
                });
            });
    }

    fn central(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        let mut to_open: Vec<String> = Vec::new();
        egui::CentralPanel::default().show(root, |ui| {
            if let Some(err) = self.error.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, err);
                    if ui.small_button(icons::X).clicked() {
                        self.error = None;
                    }
                });
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let query = self.search.trim().to_lowercase();
                if !query.is_empty() {
                    let ids: Vec<String> = self
                        .tools
                        .iter()
                        .map(|t| t.id().to_owned())
                        .filter(|id| {
                            self.title_of(id).to_lowercase().contains(&query)
                                || self.description_of(id).to_lowercase().contains(&query)
                        })
                        .collect();
                    if ids.is_empty() {
                        ui.weak(self.i18n.tr("hub-no-results"));
                    }
                    to_open.extend(self.tile_grid(ui, &ids));
                } else if self.category == Category::Home {
                    let actions = hub::home(
                        ui,
                        &self.summary,
                        &self.journal,
                        &self.i18n,
                        &self.config.recent_tools,
                        self.elevated,
                    );
                    to_open.extend(actions.open);
                } else {
                    let ids: Vec<String> = catalog::TILES
                        .iter()
                        .find(|(c, _)| *c == self.category)
                        .map(|(_, ids)| ids.iter().map(|s| (*s).to_owned()).collect())
                        .unwrap_or_default();
                    to_open.extend(self.tile_grid(ui, &ids));
                }
            });
        });
        to_open.extend(wm_core::ui::take_open_requests(&ctx));
        for id in to_open {
            self.open_tool(&ctx, &id);
        }
    }

    /// Two-column grid of tiles; returns the ids the user opened.
    fn tile_grid(&self, ui: &mut egui::Ui, ids: &[String]) -> Vec<String> {
        let mut opened = Vec::new();
        ui.add_space(8.0);
        let width = ((ui.available_width() - 24.0) / 2.0).max(220.0);
        for row in ids.chunks(2) {
            ui.horizontal(|ui| {
                for id in row {
                    let r = widgets::tile(
                        ui,
                        width,
                        catalog::tool_icon(id),
                        &self.title_of(id),
                        &self.description_of(id),
                    );
                    if r.clicked() {
                        opened.push(id.clone());
                    }
                    let related = catalog::related(id);
                    if !related.is_empty() {
                        let caret = egui::Rect::from_min_size(
                            r.rect.right_top() + egui::vec2(-28.0, 6.0),
                            egui::vec2(22.0, 22.0),
                        );
                        ui.put(caret, |ui: &mut egui::Ui| {
                            ui.menu_button(icons::CARET_DOWN, |ui| {
                                for rel in related {
                                    if ui
                                        .button(format!("{} {}", catalog::tool_icon(rel), self.title_of(rel)))
                                        .clicked()
                                    {
                                        opened.push((*rel).to_owned());
                                        ui.close();
                                    }
                                }
                            })
                            .response
                        });
                    }
                    ui.add_space(12.0);
                }
            });
            ui.add_space(8.0);
        }
        opened
    }

    /// Draws every open tool in its own native window.
    fn tool_windows(&mut self, ctx: &egui::Context) {
        let mut restart = false;
        for i in 0..self.tools.len() {
            if !self.open[i] {
                continue;
            }
            let id = self.tools[i].id();
            let title = self.title_of(id);
            let description = self.description_of(id);
            let icon = catalog::tool_icon(id);
            let (tools, config, journal, i18n, elevated) = (
                &mut self.tools,
                &mut self.config,
                &self.journal,
                &self.i18n,
                self.elevated,
            );
            let tool = &mut tools[i];
            let window_title = format!("{APP_NAME} - {title}");
            let mut close = false;
            ctx.show_viewport_immediate(
                viewport_id(id),
                ViewportBuilder::default()
                    .with_title(&window_title)
                    .with_inner_size([1000.0, 700.0])
                    .with_min_inner_size([640.0, 420.0]),
                |ui, class| {
                    let mut body = |ui: &mut egui::Ui| {
                        widgets::tool_header(ui, icon, &title, &description);
                        if tool.requires_admin() && !elevated {
                            ui.horizontal(|ui| {
                                ui.add_space(14.0);
                                ui.colored_label(ui.visuals().warn_fg_color, i18n.tr("shell-needs-admin"));
                                restart |= ui.button(i18n.tr("shell-restart-admin")).clicked();
                            });
                        }
                        if config.dry_run {
                            ui.horizontal(|ui| {
                                ui.add_space(14.0);
                                ui.colored_label(ui.visuals().warn_fg_color, i18n.tr("shell-dry-run-active"));
                            });
                        }
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(14, 8))
                            .show(ui, |ui| {
                                let mut app_ctx = AppCtx {
                                    config,
                                    journal,
                                    i18n,
                                    elevated,
                                };
                                egui::ScrollArea::vertical()
                                    .auto_shrink(false)
                                    .show(ui, |ui| tool.ui(ui, &mut app_ctx));
                            });
                    };
                    if class == ViewportClass::EmbeddedWindow {
                        let mut still_open = true;
                        egui::Window::new(&window_title)
                            .open(&mut still_open)
                            .show(ui.ctx(), |ui| body(ui));
                        close = !still_open;
                    } else {
                        egui::CentralPanel::default()
                            .frame(egui::Frame::new().fill(ui.visuals().panel_fill))
                            .show(ui, body);
                        close = ui.ctx().input(|inp| inp.viewport().close_requested());
                    }
                },
            );
            if close {
                self.open[i] = false;
            }
        }
        if restart {
            self.restart_as_admin(ctx);
        }
    }

    fn about(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let modal = egui::Modal::new(egui::Id::unique("about")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(
                egui::RichText::new(format!("{} {APP_NAME}", icons::WRENCH))
                    .size(20.0)
                    .strong(),
            );
            ui.weak(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.add_space(8.0);
            ui.label(self.i18n.tr("hub-about-text"));
            ui.add_space(6.0);
            ui.weak("Icons: Phosphor (MIT). UI: egui (MIT/Apache-2.0).");
            ui.add_space(8.0);
            if ui.button(self.i18n.tr("common-close")).clicked() {
                self.about_open = false;
            }
        });
        if modal.should_close() {
            self.about_open = false;
        }
    }

    fn restart_as_admin(&mut self, ctx: &egui::Context) {
        self.save_config();
        match elevation::relaunch_as_admin() {
            Ok(()) => ctx.send_viewport_cmd_to(ViewportId::ROOT, egui::ViewportCommand::Close),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// With "hide in the notification area" on, a minimised main window is hidden (by a
    /// watcher thread, since nothing is drawn while minimised); the rule monitor's tray
    /// icon shows it again.
    fn hide_to_tray(&mut self, frame: &eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let flag = self.tray_flag.get_or_insert_with(|| {
            let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            if let Ok(h) = frame.window_handle()
                && let RawWindowHandle::Win32(w) = h.as_raw()
            {
                let hwnd = w.hwnd.get();
                wm_tools_oneclick::monitor::remember_app_window(hwnd);
                wm_tools_oneclick::monitor::watch_minimize(hwnd, flag.clone());
            }
            flag
        });
        flag.store(self.config.minimize_to_tray, std::sync::atomic::Ordering::Relaxed);
    }

    /// Persists config and applies side effects (language, theme, zoom) when it changed.
    fn sync_config(&mut self, ctx: &egui::Context) {
        if self.config == self.saved_config {
            return;
        }
        if self.config.language != self.saved_config.language {
            self.i18n = I18n::new(&self.config.language);
        }
        if self.config.theme != self.saved_config.theme {
            apply_theme(ctx, self.config.theme);
        }
        if self.config.ui_scale != self.saved_config.ui_scale {
            ctx.set_zoom_factor(self.config.ui_scale.clamp(0.75, 2.0));
        }
        self.save_config();
    }

    fn save_config(&mut self) {
        if let Some(path) = &self.config_path
            && let Err(e) = self.config.save(path)
        {
            tracing::warn!("saving config: {e:#}");
        }
        self.saved_config = self.config.clone();
    }
}

fn viewport_id(tool_id: &str) -> ViewportId {
    ViewportId::from_hash_of(("tool", tool_id))
}

/// Theme preference plus our accent colour for selections in both themes.
fn apply_theme(ctx: &egui::Context, theme: Theme) {
    ctx.set_theme(match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
    for t in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(t, |s| {
            s.visuals.selection.bg_fill = widgets::ACCENT;
            s.visuals.selection.stroke.color = Color32::WHITE;
            s.visuals.hyperlink_color = widgets::ACCENT;
            s.spacing.button_padding = egui::vec2(8.0, 4.0);
        });
    }
}

impl Shell {
    /// Shown instead of the hub while a program password is set and not yet entered.
    fn lock_screen(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show(root, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.28);
                ui.label(egui::RichText::new(icons::LOCK).size(48.0).color(widgets::ACCENT));
                ui.label(egui::RichText::new(APP_NAME).size(22.0).strong());
                ui.weak("Enter the program password to continue.");
                ui.add_space(10.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.password_input)
                        .password(true)
                        .desired_width(240.0),
                );
                resp.request_focus();
                let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Unlock").clicked() || submit {
                    let ok = self
                        .config
                        .password_hash
                        .as_deref()
                        .is_none_or(|h| wm_core::password::verify(&self.password_input, h));
                    self.password_input.clear();
                    if ok {
                        self.locked = false;
                        self.password_wrong = false;
                    } else {
                        self.password_wrong = true;
                    }
                }
                if self.password_wrong {
                    ui.colored_label(ui.visuals().error_fg_color, "Wrong password.");
                }
            });
        });
    }
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.locked {
            self.lock_screen(ui);
            return;
        }
        self.header(ui);
        self.rail(ui);
        self.central(ui);
        self.tool_windows(&ctx);
        self.about(&ctx);
        if self.updater.show(&ctx) {
            self.save_config();
            ctx.send_viewport_cmd_to(ViewportId::ROOT, egui::ViewportCommand::Close);
        }
        self.sync_config(&ctx);
        self.hide_to_tray(_frame);
    }
}
