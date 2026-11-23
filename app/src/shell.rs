//! Main window: top bar, categorised sidebar with search, and the active tool.

use wm_core::{
    APP_NAME, AppCtx, Category, Tool,
    config::{Config, Theme},
    egui, elevation,
    i18n::I18n,
    safety::ChangeJournal,
};

pub struct Shell {
    tools: Vec<Box<dyn Tool>>,
    selected: usize,
    search: String,
    config: Config,
    saved_config: Config,
    config_path: Option<std::path::PathBuf>,
    journal: ChangeJournal,
    i18n: I18n,
    elevated: bool,
    error: Option<String>,
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
        let tools = crate::tools::all();
        let selected = config
            .last_tool
            .as_deref()
            .and_then(|id| tools.iter().position(|t| t.id() == id))
            .unwrap_or(0);
        apply_theme(&cc.egui_ctx, config.theme);
        Self {
            i18n: I18n::new(&config.language),
            tools,
            selected,
            search: String::new(),
            saved_config: config.clone(),
            config,
            config_path,
            journal,
            elevated: elevation::is_elevated(),
            error,
        }
    }

    fn top_bar(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        egui::Panel::top("top_bar").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.heading(APP_NAME);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.elevated {
                        ui.label(egui::RichText::new(self.i18n.tr("shell-admin")).strong());
                    } else {
                        if ui.button(self.i18n.tr("shell-restart-admin")).clicked() {
                            self.restart_as_admin(&ctx);
                        }
                        ui.label(self.i18n.tr("shell-standard-user"));
                    }
                });
            });
        });
    }

    fn sidebar(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(220.0)
            .show(root, |ui| {
                ui.add_space(6.0);
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text(self.i18n.tr("shell-search")));
                ui.add_space(6.0);
                let query = self.search.to_lowercase();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for cat in Category::ALL {
                        let items: Vec<(usize, String)> = self
                            .tools
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| t.category() == cat)
                            .map(|(i, t)| (i, t.title(&self.i18n)))
                            .filter(|(_, title)| query.is_empty() || title.to_lowercase().contains(&query))
                            .collect();
                        if items.is_empty() {
                            continue;
                        }
                        ui.label(egui::RichText::new(self.i18n.tr(cat.i18n_key())).small().weak());
                        for (i, title) in items {
                            if ui.selectable_label(self.selected == i, title).clicked() {
                                self.selected = i;
                                self.config.last_tool = Some(self.tools[i].id().to_owned());
                            }
                        }
                        ui.add_space(8.0);
                    }
                });
            });
    }

    fn central(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        egui::CentralPanel::default().show(root, |ui| {
            if let Some(err) = self.error.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, err);
                    if ui.small_button("✕").clicked() {
                        self.error = None;
                    }
                });
            }
            let tool = &mut self.tools[self.selected];
            if tool.requires_admin() && !self.elevated {
                let mut restart = false;
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, self.i18n.tr("shell-needs-admin"));
                    restart = ui.button(self.i18n.tr("shell-restart-admin")).clicked();
                });
                if restart {
                    self.restart_as_admin(&ctx);
                    return;
                }
            }
            if self.config.dry_run {
                ui.colored_label(ui.visuals().warn_fg_color, self.i18n.tr("shell-dry-run-active"));
            }
            let tool = &mut self.tools[self.selected];
            ui.heading(tool.title(&self.i18n));
            ui.separator();
            let mut app_ctx = AppCtx {
                config: &mut self.config,
                journal: &self.journal,
                i18n: &self.i18n,
                elevated: self.elevated,
            };
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                tool.ui(ui, &mut app_ctx);
            });
        });
    }

    fn restart_as_admin(&mut self, ctx: &egui::Context) {
        self.save_config();
        match elevation::relaunch_as_admin() {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Persists config and applies side effects (language, theme) when it changed.
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

fn apply_theme(ctx: &egui::Context, theme: Theme) {
    ctx.set_theme(match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.top_bar(ui);
        self.sidebar(ui);
        self.central(ui);
        let ctx = ui.ctx().clone();
        self.sync_config(&ctx);
    }
}
