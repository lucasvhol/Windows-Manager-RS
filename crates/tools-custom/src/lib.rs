//! Customization tools (Phase 4): context menu, Win+X, Start menu, hotkeys, Run
//! aliases, jump list launcher, and visual / Explorer / This PC / navigation pane tweaks.

mod context_menu;
pub mod custom_extras;
mod hotkeys;
mod jumplist;
mod navpane;
mod run_shortcuts;
mod start_menu;
mod thispc;
mod visual;
mod winx;

use wm_core::tweaks::TweakTool;
use wm_core::{Category, Tool};

/// Tweak definitions, also used by the Optimization Wizard.
pub const VISUAL_TWEAKS: &str = include_str!("../tweaks/visual.toml");
pub const EXPLORER_TWEAKS: &str = include_str!("../tweaks/explorer.toml");
pub const START_TWEAKS: &str = include_str!("../tweaks/start.toml");
pub const THISPC_TWEAKS: &str = include_str!("../tweaks/thispc.toml");
pub const NAVPANE_TWEAKS: &str = include_str!("../tweaks/navpane.toml");
pub const CUSTOM_TWEAKS: &str = include_str!("../tweaks/custom.toml");
pub const APP_SETTINGS_TWEAKS: &str = include_str!("../tweaks/appsettings.toml");

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(visual::VisualCustomizer::new(VISUAL_TWEAKS)),
        Box::new(TweakTool::new(
            Category::Customization,
            "explorertweaks",
            "",
            include_str!("../tweaks/explorer.toml"),
        )),
        Box::new(
            TweakTool::new(Category::Customization, "customsystem", "", CUSTOM_TWEAKS)
                .with_extra(Box::new(custom_extras::ThemeScheduleSetting::default()))
                .with_extra(Box::new(custom_extras::CapsLockSetting::default())),
        ),
        Box::new(TweakTool::new(
            Category::Customization,
            "appsettings",
            "",
            APP_SETTINGS_TWEAKS,
        )),
        Box::new(context_menu::ContextMenuManager::default()),
        Box::new(winx::WinXEditor::default()),
        Box::new(start_menu::StartMenuManager::default()),
        Box::new(hotkeys::HotkeyManager::default()),
        Box::new(run_shortcuts::RunShortcuts::default()),
        Box::new(jumplist::JumpListLauncher::default()),
        Box::new(navpane::NavPaneManager::default()),
        Box::new(thispc::ThisPcManager::default()),
    ]
}

#[cfg(test)]
mod tests {
    use wm_core::tweaks::parse;

    #[test]
    fn all_tweak_files_parse_with_unique_ids() {
        let mut ids = Vec::new();
        for text in [
            include_str!("../tweaks/visual.toml"),
            include_str!("../tweaks/explorer.toml"),
            include_str!("../tweaks/thispc.toml"),
            include_str!("../tweaks/navpane.toml"),
            include_str!("../tweaks/start.toml"),
            include_str!("../tweaks/custom.toml"),
            include_str!("../tweaks/appsettings.toml"),
        ] {
            for t in parse(text).unwrap() {
                if t.kind != wm_core::tweaks::Kind::Toggle {
                    ids.push(t.id);
                    continue;
                }
                assert!(t.settings.iter().all(|s| s.on != s.off), "{}", t.id);
                let _ = t.state();
                ids.push(t.id);
            }
        }
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate tweak ids across files");
    }
}
