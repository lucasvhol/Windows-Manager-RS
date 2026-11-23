//! A tool that is just an introduction plus a [`TweakPanel`].

use wm_core::{AppCtx, Category, Tool, egui, tweaks::TweakPanel};

pub struct TweakTool {
    id: &'static str,
    intro: &'static str,
    panel: TweakPanel,
}

impl TweakTool {
    pub fn new(id: &'static str, intro: &'static str, definitions: &str) -> Self {
        Self {
            id,
            intro,
            panel: TweakPanel::new(id, definitions),
        }
    }
}

impl Tool for TweakTool {
    fn id(&self) -> &'static str {
        self.id
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(self.intro);
        ui.add_space(8.0);
        self.panel.ui(ui, ctx);
    }
}
