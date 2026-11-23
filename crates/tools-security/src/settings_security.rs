//! Settings Security: per-user restriction policies and basic hardening.

use wm_core::{AppCtx, Category, Tool, egui, tweaks::TweakPanel};

pub struct SettingsSecurity {
    panel: TweakPanel,
}

impl Default for SettingsSecurity {
    fn default() -> Self {
        Self {
            panel: TweakPanel::new("lockdown", include_str!("../tweaks/lockdown.toml")),
        }
    }
}

impl Tool for SettingsSecurity {
    fn id(&self) -> &'static str {
        "lockdown"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Restrictions apply to the account running this app. To restrict another account (for \
             example a child's), sign in as that user and run Win Manager RS there.",
        );
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "This app can always switch these off again, even when Control Panel, Task Manager \
             or the Registry Editor are blocked.",
        );
        ui.add_space(8.0);
        self.panel.ui(ui, ctx);
    }
}
