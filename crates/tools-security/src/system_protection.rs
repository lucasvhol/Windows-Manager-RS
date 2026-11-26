//! System Protection: restore points (list, create, delete), protection per drive,
//! and shortcuts to Windows' recovery tools.

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use serde::Deserialize;
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, registry,
    safety::{UndoAction, restore_point},
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, empty_state, frame, section, section_nav, show_message},
};

/// SPP client entry that lists the volumes with System Protection turned on.
const SPP_CLIENTS: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\SPP\Clients";
const SPP_SYSTEM_RESTORE: &str = "{09F7EDC5-294E-4180-AF6A-FB0E6A0E9513}";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RestorePoint {
    pub sequence_number: u32,
    pub description: String,
    /// WMI datetime, e.g. `20261124093000.000000-000`.
    pub creation_time: String,
    pub restore_point_type: u32,
}

impl RestorePoint {
    pub fn created(&self) -> String {
        let t = &self.creation_time;
        match (t.get(0..4), t.get(4..6), t.get(6..8), t.get(8..10), t.get(10..12)) {
            (Some(y), Some(m), Some(d), Some(h), Some(mi)) => format!("{y}-{m}-{d} {h}:{mi}"),
            _ => t.clone(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self.restore_point_type {
            0 => "App installed",
            1 => "App removed",
            10 => "Driver installed",
            12 => "Settings changed",
            13 => "Cancelled operation",
            _ => "Manual or system",
        }
    }
}

/// Lists restore points through Windows PowerShell (needs administrator rights).
fn list_restore_points() -> Result<Vec<RestorePoint>> {
    let out = wm_core::process::run_checked(wm_core::process::hidden_command("powershell").args([
        "-NoProfile",
        "-Command",
        "ConvertTo-Json -Compress -InputObject @(Get-ComputerRestorePoint | Select-Object SequenceNumber, Description, CreationTime, RestorePointType)",
    ]))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut points: Vec<RestorePoint> =
        serde_json::from_str(text).context("reading the restore point list")?;
    points.sort_by_key(|p| std::cmp::Reverse(p.sequence_number));
    Ok(points)
}

fn delete_restore_point(seq: u32) -> Result<()> {
    let rc = unsafe { windows::Win32::System::Restore::SRRemoveRestorePoint(seq) };
    if rc != 0 {
        bail!("Windows could not delete restore point {seq} (error {rc}).");
    }
    Ok(())
}

/// Drive letters whose System Protection is on, parsed from entries like
/// `\\?\Volume{…}\:(C%3A)`.
pub fn protected_letters(entries: &[String]) -> Vec<char> {
    entries
        .iter()
        .filter_map(|e| {
            let i = e.rfind(":(")?;
            e[i + 2..]
                .chars()
                .next()
                .filter(char::is_ascii_alphabetic)
                .map(|c| c.to_ascii_uppercase())
        })
        .collect()
}

fn read_protected() -> Vec<char> {
    let entries: Vec<String> = registry::open_read(SPP_CLIENTS)
        .ok()
        .and_then(|k| k.get_value(SPP_SYSTEM_RESTORE).ok())
        .unwrap_or_default();
    protected_letters(&entries)
}

fn fixed_ntfs_letters() -> Vec<char> {
    ('C'..='Z')
        .filter(|c| {
            let root = format!(r"{c}:\");
            let w: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            let kind = unsafe {
                windows::Win32::Storage::FileSystem::GetDriveTypeW(windows::core::PCWSTR(w.as_ptr()))
            };
            kind == 3 // DRIVE_FIXED
        })
        .collect()
}

fn protection_command(letter: char, on: bool) -> (String, Vec<String>) {
    let verb = if on {
        "Enable-ComputerRestore"
    } else {
        "Disable-ComputerRestore"
    };
    (
        "powershell".into(),
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            format!("{verb} -Drive '{letter}:\\'"),
        ],
    )
}

const SECTIONS: [(&str, &str); 3] = [
    (icons::CLOCK_COUNTER_CLOCKWISE, "Restore points"),
    (icons::HARD_DRIVES, "Protection per drive"),
    (icons::FIRST_AID_KIT, "Recovery"),
];

enum Ask {
    Delete(Vec<u32>),
    Protection(char, bool),
    AdvancedStartup,
}

#[derive(Default)]
pub struct SystemProtection {
    section: usize,
    points: Option<Result<Vec<RestorePoint>, String>>,
    loading: Option<BackgroundTask<Vec<RestorePoint>>>,
    creating: Option<BackgroundTask<()>>,
    selected: std::collections::BTreeSet<u32>,
    protected: Option<Vec<char>>,
    confirm: Confirm<Ask>,
    message: Message,
}

impl SystemProtection {
    fn restore_points(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if ctx.elevated && self.points.is_none() && self.loading.is_none() {
            self.loading = Some(BackgroundTask::spawn(ui.ctx(), |_| list_restore_points()));
        }
        if let Some(t) = &mut self.loading {
            t.poll();
            if let Some(r) = t.take_result() {
                self.points = Some(r);
                self.loading = None;
            }
        }
        if let Some(t) = &mut self.creating {
            t.poll();
            if let Some(r) = t.take_result() {
                self.creating = None;
                self.message = Some(r.map(|()| {
                    let _ = ctx
                        .journal
                        .record("systemprotection", "Created restore point", UndoAction::None);
                    ctx.i18n.tr("settings-restore-point-created")
                }));
                self.points = None;
            }
        }
        let busy = self.loading.is_some() || self.creating.is_some();
        let ok = ctx.elevated && !busy;
        let n = self.selected.len();
        let delete_label = format!("Delete ({n})");
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Create").enabled(ok)),
                Some(ToolButton::new(icons::TRASH, &delete_label).enabled(ok && n > 0)),
                None,
                Some(ToolButton::new(icons::CLOCK_COUNTER_CLOCKWISE, "System Restore")),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(ok)),
            ],
            |ui| {
                if busy {
                    ui.spinner();
                }
            },
        ) {
            Some(0) => {
                self.creating = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                    restore_point::create("Win Manager RS manual checkpoint")
                }));
            }
            Some(1) => {
                self.confirm.ask(
                    format!(
                        "Delete {n} restore point{}? This can't be undone.",
                        if n == 1 { "" } else { "s" }
                    ),
                    Ask::Delete(self.selected.iter().copied().collect()),
                );
            }
            Some(3) => {
                self.message = std::process::Command::new("rstrui.exe")
                    .spawn()
                    .err()
                    .map(|e| Err(format!("rstrui.exe: {e}")));
            }
            Some(4) => self.points = None,
            _ => {}
        }
        let status = match &self.points {
            Some(Ok(p)) => format!("{} restore points, {n} selected", p.len()),
            _ if busy => "Reading restore points…".to_owned(),
            _ => String::new(),
        };
        frame::status(ui, &status, |ui| {
            ui.weak("Windows also creates restore points before updates and installs.");
        });
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            if !ctx.elevated {
                empty_state(
                    ui,
                    icons::SHIELD,
                    "Administrator rights needed",
                    &ctx.i18n.tr("shell-needs-admin"),
                );
                return;
            }
            match &self.points {
                None => {}
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                Some(Ok(points)) if points.is_empty() => empty_state(
                    ui,
                    icons::CLOCK_COUNTER_CLOCKWISE,
                    "No restore points",
                    "Turn on protection for the Windows drive to have Windows create them.",
                ),
                Some(Ok(points)) => {
                    let selected = &mut self.selected;
                    TableBuilder::new(ui)
                        .striped(true)
                        .column(Column::exact(28.0))
                        .column(Column::initial(150.0))
                        .column(Column::initial(380.0).clip(true))
                        .column(Column::remainder())
                        .header(22.0, |mut h| {
                            for t in ["", "Created", "Description", "Type"] {
                                h.col(|ui| {
                                    ui.strong(t);
                                });
                            }
                        })
                        .body(|body| {
                            body.rows(24.0, points.len(), |mut row| {
                                let p = &points[row.index()];
                                row.col(|ui| {
                                    let mut on = selected.contains(&p.sequence_number);
                                    if ui.checkbox(&mut on, "").changed() {
                                        if on {
                                            selected.insert(p.sequence_number);
                                        } else {
                                            selected.remove(&p.sequence_number);
                                        }
                                    }
                                });
                                row.col(|ui| {
                                    ui.label(p.created());
                                });
                                row.col(|ui| {
                                    ui.label(&p.description);
                                });
                                row.col(|ui| {
                                    ui.weak(p.kind());
                                });
                            });
                        });
                }
            }
        });
    }

    fn protection(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::GEAR, "Windows settings")),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
            |_| {},
        ) {
            Some(0) => {
                let _ = std::process::Command::new("SystemPropertiesProtection.exe").spawn();
            }
            Some(1) => self.protected = None,
            _ => {}
        }
        let protected = self.protected.get_or_insert_with(read_protected).clone();
        let letters = fixed_ntfs_letters();
        frame::status(
            ui,
            &format!("{} of {} drives protected", protected.len(), letters.len()),
            |_| {},
        );
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            ui.weak("With protection on, Windows keeps restore points of system files and settings for that drive.");
            ui.add_space(8.0);
            for c in letters {
                let on = protected.contains(&c);
                section(
                    ui,
                    "",
                    |_| {},
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icons::HARD_DRIVES).size(20.0));
                            ui.strong(format!("Drive {c}:"));
                            if on {
                                ui.colored_label(ui.visuals().selection.bg_fill, "Protected");
                            } else {
                                ui.weak("Not protected");
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let (label, want) = if on { ("Turn off", false) } else { ("Turn on", true) };
                                if ui
                                    .add_enabled(ctx.elevated, egui::Button::new(label))
                                    .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"))
                                    .clicked()
                                {
                                    if want {
                                        self.apply_protection(c, true, ctx);
                                    } else {
                                        self.confirm.ask(
                                            format!("Turn off System Protection for {c}:? Windows deletes every restore point on that drive."),
                                            Ask::Protection(c, false),
                                        );
                                    }
                                }
                            });
                        });
                    },
                );
                ui.add_space(6.0);
            }
        });
    }

    fn apply_protection(&mut self, letter: char, on: bool, ctx: &mut AppCtx<'_>) {
        self.message = Some(if ctx.config.dry_run {
            Ok(ctx.i18n.tr("common-dry-run-skip"))
        } else {
            let (p, a) = protection_command(letter, on);
            wm_core::process::run_checked(wm_core::process::hidden_command(&p).args(&a))
                .and_then(|_| {
                    let (up, ua) = protection_command(letter, !on);
                    ctx.journal.record(
                        "systemprotection",
                        format!(
                            "Turned System Protection {} for {letter}:",
                            if on { "on" } else { "off" }
                        ),
                        if on {
                            UndoAction::RunCommand {
                                program: up,
                                args: ua,
                            }
                        } else {
                            UndoAction::None
                        },
                    )?;
                    Ok(format!(
                        "System Protection is {} for {letter}:.",
                        if on { "on" } else { "off" }
                    ))
                })
                .map_err(|e| format!("{e:#}"))
        });
        self.protected = None;
    }

    fn recovery(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let open = |program: &str, args: &[&str]| -> Message {
            std::process::Command::new(program)
                .args(args)
                .spawn()
                .err()
                .map(|e| Err(format!("{program}: {e}")))
        };
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            let items: [(&str, &str, &str, bool); 4] = [
                (
                    icons::CLOCK_COUNTER_CLOCKWISE,
                    "System Restore",
                    "Return Windows to an earlier restore point.",
                    true,
                ),
                (
                    icons::FIRST_AID_KIT,
                    "Recovery options",
                    "Reset this PC, go back to the previous Windows version, or start from a USB drive.",
                    true,
                ),
                (
                    icons::USB,
                    "Create a recovery drive",
                    "Copy Windows' recovery tools to a USB drive.",
                    true,
                ),
                (
                    icons::POWER,
                    "Restart to advanced startup",
                    "Restarts into the blue recovery menu (Safe Mode, Startup Repair, firmware settings).",
                    ctx.elevated,
                ),
            ];
            for (i, (icon, title, hint, enabled)) in items.into_iter().enumerate() {
                let clicked = section(
                    ui,
                    "",
                    |_| {},
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icon).size(22.0));
                            ui.vertical(|ui| {
                                ui.strong(title);
                                ui.weak(hint);
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add_enabled(enabled, egui::Button::new("Open…"))
                                    .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"))
                                    .clicked()
                            })
                            .inner
                        })
                        .inner
                    },
                );
                ui.add_space(6.0);
                if clicked {
                    match i {
                        0 => self.message = open("rstrui.exe", &[]),
                        1 => self.message = open("explorer.exe", &["ms-settings:recovery"]),
                        2 => self.message = open("RecoveryDrive.exe", &[]),
                        _ => self.confirm.ask(
                            "Restart now into Windows' advanced startup options (Safe Mode, Startup Repair, firmware settings)? Save your work first.",
                            Ask::AdvancedStartup,
                        ),
                    }
                }
            }
        });
    }
}

impl Tool for SystemProtection {
    fn id(&self) -> &'static str {
        "systemprotection"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        match self.confirm.show(ui.ctx(), ctx.i18n) {
            Some(Ask::Delete(seqs)) => {
                let errors: Vec<String> = if ctx.config.dry_run {
                    Vec::new()
                } else {
                    seqs.iter()
                        .filter_map(|&s| delete_restore_point(s).err().map(|e| format!("{e:#}")))
                        .collect()
                };
                if !ctx.config.dry_run {
                    let _ = ctx.journal.record(
                        "systemprotection",
                        format!("Deleted {} restore point(s)", seqs.len() - errors.len()),
                        UndoAction::None,
                    );
                }
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else if errors.is_empty() {
                    Ok("Deleted.".into())
                } else {
                    Err(errors.join("; "))
                });
                self.selected.clear();
                self.points = None;
            }
            Some(Ask::Protection(c, on)) => self.apply_protection(c, on, ctx),
            Some(Ask::AdvancedStartup) => {
                self.message = wm_core::process::run_checked(
                    wm_core::process::hidden_command("shutdown").args(["/r", "/o", "/t", "0"]),
                )
                .err()
                .map(|e| Err(format!("{e:#}")));
            }
            None => {}
        }
        if frame::left(ui, 190.0, |ui| section_nav(ui, &SECTIONS, &mut self.section)) {
            self.message = None;
        }
        match self.section {
            0 => self.restore_points(ui, ctx),
            1 => self.protection(ui, ctx),
            _ => self.recovery(ui, ctx),
        }
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_protected_volumes() {
        let entries = vec![
            r"\\?\Volume{0b1c2d3e-0000-0000-0000-100000000000}\:(C%3A)".to_owned(),
            r"\\?\Volume{0b1c2d3e-0000-0000-0000-200000000000}\:(D%3A)".to_owned(),
            "garbage".to_owned(),
        ];
        assert_eq!(protected_letters(&entries), vec!['C', 'D']);
    }

    #[test]
    fn parses_restore_point_json() {
        let json = r#"[{"SequenceNumber":12,"Description":"Installed X","CreationTime":"20261124093000.000000-000","RestorePointType":0}]"#;
        let p: Vec<RestorePoint> = serde_json::from_str(json).unwrap();
        assert_eq!(p[0].created(), "2026-11-24 09:30");
        assert_eq!(p[0].kind(), "App installed");
    }

    #[test]
    fn protection_commands_are_reversible() {
        let (_, on) = protection_command('D', true);
        let (_, off) = protection_command('D', false);
        assert!(on[2].starts_with("Enable-ComputerRestore -Drive 'D:"));
        assert!(off[2].starts_with("Disable-ComputerRestore"));
    }
}
