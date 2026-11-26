//! Custom System settings too rich for a switch: light and dark mode on a schedule,
//! and what the Caps Lock key does.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use windows::core::HSTRING;
use wm_core::{
    AppCtx, egui, registry,
    safety::UndoAction,
    scheduler::{self, Schedule, valid_time},
    tool_settings,
    tweaks::ExtraSetting,
    ui::{Message, show_message},
};

// ---------------------------------------------------------------------------------
// Light and dark on a schedule

const PERSONALIZE: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const SCHEDULE_ID: &str = "themeschedule";
/// The scheduled action, run as `win-manager.exe --run theme-auto`.
pub const THEME_ACTION: &str = "theme-auto";
const TASKS: [&str; 3] = [
    "Theme schedule - light",
    "Theme schedule - dark",
    "Theme schedule - sign-in",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSchedule {
    pub enabled: bool,
    pub light_at: String,
    pub dark_at: String,
}

impl Default for ThemeSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            light_at: "07:00".into(),
            dark_at: "19:00".into(),
        }
    }
}

fn minutes(t: &str) -> Option<u32> {
    let (h, m) = t.split_once(':')?;
    Some(h.parse::<u32>().ok()? * 60 + m.parse::<u32>().ok()?)
}

impl ThemeSchedule {
    /// Whether light mode is due at `now` (minutes after midnight). Works when the
    /// dark time is after midnight too.
    pub fn light_at_minute(&self, now: u32) -> bool {
        let (Some(l), Some(d)) = (minutes(&self.light_at), minutes(&self.dark_at)) else {
            return true;
        };
        if l <= d {
            now >= l && now < d
        } else {
            now >= l || now < d
        }
    }
}

/// Sets apps and Windows to light or dark mode now and tells open windows.
pub fn set_light(light: bool) -> Result<()> {
    let v = registry::dword(u32::from(light));
    registry::write_raw(PERSONALIZE, "AppsUseLightTheme", &v)?;
    registry::write_raw(PERSONALIZE, "SystemUsesLightTheme", &v)?;
    let area = HSTRING::from("ImmersiveColorSet");
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(area.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            1000,
            None,
        );
    }
    Ok(())
}

fn is_light_now() -> bool {
    registry::read_raw(PERSONALIZE, "AppsUseLightTheme")
        .and_then(|v| registry::decode_dword(&v))
        .is_none_or(|v| v != 0)
}

/// The scheduled action: applies the mode the saved schedule calls for now.
pub fn auto_apply() -> Result<String> {
    let s: ThemeSchedule = tool_settings::load(SCHEDULE_ID);
    if !s.enabled {
        return Ok("theme schedule is off".into());
    }
    use chrono::Timelike;
    let now = chrono::Local::now();
    let light = s.light_at_minute(now.hour() * 60 + now.minute());
    if light != is_light_now() {
        set_light(light)?;
    }
    Ok(format!("{} mode", if light { "light" } else { "dark" }))
}

pub struct ThemeScheduleSetting {
    edit: ThemeSchedule,
    saved: ThemeSchedule,
    message: Message,
}

impl Default for ThemeScheduleSetting {
    fn default() -> Self {
        let saved: ThemeSchedule = tool_settings::load(SCHEDULE_ID);
        Self {
            edit: saved.clone(),
            saved,
            message: None,
        }
    }
}

impl ThemeScheduleSetting {
    fn save(&mut self, ctx: &AppCtx<'_>) -> Result<String> {
        let s = self.edit.clone();
        if s.enabled {
            if !valid_time(&s.light_at) || !valid_time(&s.dark_at) {
                bail!("Enter both times as HH:MM (24-hour).");
            }
            if s.light_at == s.dark_at {
                bail!("Light and dark need different times.");
            }
        }
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        tool_settings::save(SCHEDULE_ID, &s)?;
        self.saved = s.clone();
        if s.enabled {
            scheduler::schedule(ctx, TASKS[0], THEME_ACTION, Schedule::Daily, &s.light_at, false)?;
            scheduler::schedule(ctx, TASKS[1], THEME_ACTION, Schedule::Daily, &s.dark_at, false)?;
            scheduler::schedule(ctx, TASKS[2], THEME_ACTION, Schedule::AtLogon, "", false)?;
            let mode = auto_apply()?;
            Ok(format!(
                "Scheduled: light at {}, dark at {}. Now in {mode}.",
                s.light_at, s.dark_at
            ))
        } else {
            for t in TASKS {
                scheduler::unschedule(ctx, t)?;
            }
            Ok("Theme schedule turned off.".into())
        }
    }
}

impl ExtraSetting for ThemeScheduleSetting {
    fn group(&self) -> &str {
        "Colours"
    }

    fn title(&self) -> &str {
        "Switch between light and dark mode on a schedule"
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let title = self.title().to_owned();
        ui.checkbox(&mut self.edit.enabled, title);
        ui.indent("theme-schedule", |ui| {
            ui.weak("Light mode from the first time, dark mode from the second. Also checked when you sign in, so a missed switch catches up.");
            ui.add_enabled_ui(self.edit.enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Light from");
                    ui.add(egui::TextEdit::singleline(&mut self.edit.light_at).desired_width(50.0));
                    ui.label("Dark from");
                    ui.add(egui::TextEdit::singleline(&mut self.edit.dark_at).desired_width(50.0));
                });
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.edit != self.saved, egui::Button::new("Save schedule"))
                    .clicked()
                {
                    self.message = Some(self.save(ctx).map_err(|e| format!("{e:#}")));
                }
                if self.saved.enabled {
                    ui.weak(format!(
                        "On: light at {}, dark at {}",
                        self.saved.light_at, self.saved.dark_at
                    ));
                }
            });
            show_message(ui, &self.message);
        });
    }
}

// ---------------------------------------------------------------------------------
// Caps Lock (Scancode Map)

use wm_core::keyboard::{self, current_map};

const CAPS: u16 = 0x3A;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsMode {
    Normal,
    Off,
    Ctrl,
    Shift,
    Esc,
}

impl CapsMode {
    pub const ALL: [CapsMode; 5] = [
        CapsMode::Normal,
        CapsMode::Off,
        CapsMode::Ctrl,
        CapsMode::Shift,
        CapsMode::Esc,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CapsMode::Normal => "Caps Lock (Windows default)",
            CapsMode::Off => "Nothing (turned off)",
            CapsMode::Ctrl => "Left Ctrl",
            CapsMode::Shift => "Left Shift",
            CapsMode::Esc => "Esc",
        }
    }

    fn scancode(self) -> Option<u16> {
        match self {
            CapsMode::Normal => None,
            CapsMode::Off => Some(0x00),
            CapsMode::Ctrl => Some(0x1D),
            CapsMode::Shift => Some(0x2A),
            CapsMode::Esc => Some(0x01),
        }
    }

    fn from_scancode(code: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.scancode() == Some(code))
    }
}

pub fn caps_mode() -> Option<CapsMode> {
    match current_map().iter().find(|(_, from)| *from == CAPS) {
        None => Some(CapsMode::Normal),
        Some((to, _)) => CapsMode::from_scancode(*to),
    }
}

/// Changes only the Caps Lock entry; other key remaps are kept.
pub fn set_caps_mode(ctx: &AppCtx<'_>, mode: CapsMode) -> Result<()> {
    let new: Vec<(u16, u16)> = mode.scancode().map(|to| (to, CAPS)).into_iter().collect();
    let snap = keyboard::replace_keys(&[CAPS], &new)?;
    ctx.journal.record(
        "customsystem",
        format!("Caps Lock key set to: {} (restart needed)", mode.label()),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

pub struct CapsLockSetting {
    choice: CapsMode,
    message: Message,
}

impl Default for CapsLockSetting {
    fn default() -> Self {
        Self {
            choice: caps_mode().unwrap_or(CapsMode::Normal),
            message: None,
        }
    }
}

impl ExtraSetting for CapsLockSetting {
    fn group(&self) -> &str {
        "Keyboard"
    }

    fn title(&self) -> &str {
        "What the Caps Lock key does"
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let current = caps_mode();
        ui.horizontal(|ui| {
            ui.label(self.title());
            egui::ComboBox::from_id_salt("caps-mode")
                .selected_text(self.choice.label())
                .show_ui(ui, |ui| {
                    for m in CapsMode::ALL {
                        ui.selectable_value(&mut self.choice, m, m.label());
                    }
                });
            let changed = current != Some(self.choice);
            if ui
                .add_enabled(ctx.elevated && changed, egui::Button::new("Apply"))
                .on_disabled_hover_text(if ctx.elevated {
                    "Already in effect."
                } else {
                    "Needs the app to run as administrator."
                })
                .clicked()
            {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    set_caps_mode(ctx, self.choice)
                        .map(|()| "Saved. Restart Windows for the key to change.".into())
                        .map_err(|e| format!("{e:#}"))
                });
            }
            ui.weak("· admin");
        });
        ui.indent("caps-lock", |ui| {
            let others = current_map().iter().filter(|(_, f)| *f != CAPS).count();
            let mut note = "Applies to every account after a restart.".to_owned();
            if others > 0 {
                note += &format!(" {others} other key remap(s) on this PC are kept.");
            }
            if current.is_none() {
                note += " Caps Lock is currently remapped to a key not listed here.";
            }
            ui.weak(note);
            show_message(ui, &self.message);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_picks_the_right_mode() {
        let day = ThemeSchedule {
            enabled: true,
            light_at: "07:00".into(),
            dark_at: "19:00".into(),
        };
        assert!(day.light_at_minute(12 * 60));
        assert!(!day.light_at_minute(6 * 60 + 59));
        assert!(!day.light_at_minute(19 * 60));
        let night = ThemeSchedule {
            light_at: "20:00".into(),
            dark_at: "02:00".into(),
            ..day
        };
        assert!(night.light_at_minute(23 * 60));
        assert!(night.light_at_minute(60));
        assert!(!night.light_at_minute(3 * 60));
    }

    #[test]
    fn scancode_maps_round_trip_and_keep_other_keys() {
        use wm_core::keyboard::{build_map, merge, parse_map};
        // Right Alt -> Left Ctrl, already on the PC.
        let existing = build_map(&[(0x1D, 0xE038)]);
        assert_eq!(existing.len(), 8 + 4 + 4 + 4);
        let parsed = parse_map(&existing);
        assert_eq!(parsed, vec![(0x1D, 0xE038)]);
        let with_caps = merge(&parsed, &[CAPS], &[(CapsMode::Esc.scancode().unwrap(), CAPS)]);
        let bytes = build_map(&with_caps);
        assert_eq!(parse_map(&bytes), vec![(0x1D, 0xE038), (0x01, 0x3A)]);
        assert_eq!(CapsMode::from_scancode(0x1D), Some(CapsMode::Ctrl));
        let _ = caps_mode();
    }
}
