//! Lock system keys: turn the left and right Windows keys off completely through the
//! keyboard Scancode Map (every account, after a restart). Other remaps in the map, such
//! as Caps Lock from Customization, are kept.

use anyhow::Result;
use wm_core::{
    AppCtx, egui,
    keyboard::{self, LEFT_WIN, RIGHT_WIN},
    safety::UndoAction,
    tweaks::ExtraSetting,
    ui::{Message, show_message},
};

const KEYS: [u16; 2] = [LEFT_WIN, RIGHT_WIN];

/// True when both Windows keys are mapped to nothing.
pub fn windows_keys_off() -> bool {
    let map = keyboard::current_map();
    KEYS.iter().all(|k| map.contains(&(0, *k)))
}

pub fn set_windows_keys_off(ctx: &AppCtx<'_>, off: bool) -> Result<()> {
    let new: Vec<(u16, u16)> = if off {
        KEYS.iter().map(|k| (0, *k)).collect()
    } else {
        Vec::new()
    };
    let snap = keyboard::replace_keys(&KEYS, &new)?;
    ctx.journal.record(
        "lockdown",
        if off {
            "Turned the Windows keys off (restart needed)"
        } else {
            "Turned the Windows keys back on (restart needed)"
        },
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

#[derive(Default)]
pub struct WindowsKeysSetting {
    message: Message,
}

impl ExtraSetting for WindowsKeysSetting {
    fn group(&self) -> &str {
        "Lock system keys"
    }

    fn title(&self) -> &str {
        "Turn the Windows keys off completely (all accounts)"
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let mut off = windows_keys_off();
        ui.horizontal(|ui| {
            let r = ui
                .add_enabled(ctx.elevated, egui::Checkbox::new(&mut off, self.title()))
                .on_disabled_hover_text("Needs the app to run as administrator.");
            if r.changed() {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    set_windows_keys_off(ctx, off)
                        .map(|()| "Saved. Restart Windows for it to take effect.".into())
                        .map_err(|e| format!("{e:#}"))
                });
            }
            ui.weak("· admin · restart");
        });
        ui.indent("win-keys", |ui| {
            ui.weak(
                "Neither Windows key does anything, alone or in shortcuts. Start still opens with \
                 Ctrl+Esc or a click. Other key remaps on this PC are kept.",
            );
            show_message(ui, &self.message);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::registry;

    #[test]
    fn windows_keys_off_and_undo_keep_other_remaps() {
        if !registry::writable(keyboard::KEYBOARD_LAYOUT) {
            return; // needs administrator rights
        }
        let before = registry::snapshot(keyboard::KEYBOARD_LAYOUT, keyboard::SCANCODE_MAP);
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: true,
        };
        let others: Vec<(u16, u16)> = keyboard::current_map()
            .into_iter()
            .filter(|(_, f)| !KEYS.contains(f))
            .collect();
        let target = !windows_keys_off();
        let r = set_windows_keys_off(&ctx, target);
        let state = windows_keys_off();
        let kept: Vec<(u16, u16)> = keyboard::current_map()
            .into_iter()
            .filter(|(_, f)| !KEYS.contains(f))
            .collect();
        let undo = journal.entries().first().map(|e| journal.undo(e.id));
        let undone = registry::snapshot(keyboard::KEYBOARD_LAYOUT, keyboard::SCANCODE_MAP);
        let _ = registry::restore(&before);
        r.unwrap();
        assert_eq!(state, target);
        assert_eq!(kept, others, "other remaps are kept");
        undo.unwrap().unwrap();
        assert_eq!(undone, before, "undo restores the map exactly");
    }
}
