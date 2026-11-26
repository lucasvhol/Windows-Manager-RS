//! System Speed: well-known, reversible responsiveness tweaks (see `tweaks/speed.toml`).

use wm_core::{AppCtx, Category, Tool, egui, tweaks::TweakPanel};

pub const DEFINITIONS: &str = include_str!("../tweaks/speed.toml");

pub struct SystemSpeed {
    panel: TweakPanel,
}

impl Default for SystemSpeed {
    fn default() -> Self {
        Self {
            panel: TweakPanel::new("speed", DEFINITIONS),
        }
    }
}

impl Tool for SystemSpeed {
    fn id(&self) -> &'static str {
        "speed"
    }

    fn category(&self) -> Category {
        Category::Optimizer
    }

    fn layout(&self) -> wm_core::Layout {
        wm_core::Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        wm_core::ui::frame::options(ui, |ui| {
            ui.weak(
                "Each tweak changes documented Windows settings. Turning one off restores the Windows \
                 default, and every change can be undone from the Change Journal.",
            );
        });
        self.panel.ui_framed(ui, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::tweaks::parse;

    #[test]
    fn definitions_parse_and_are_unique() {
        let tweaks = parse(DEFINITIONS).unwrap();
        assert!(tweaks.len() >= 35);
        let mut ids: Vec<_> = tweaks.iter().map(|t| t.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), tweaks.len(), "duplicate tweak ids");
        for t in &tweaks {
            // Reading state must not fail for any definition.
            let _ = t.state();
        }
    }
}
