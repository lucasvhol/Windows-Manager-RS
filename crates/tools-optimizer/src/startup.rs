//! Startup Manager: programs launched at sign-in from the Run keys and Startup folders.
//!
//! Enable/disable uses the same `StartupApproved` flags as Task Manager, so it is
//! non-destructive and stays in sync with Windows' own UI.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Tool, egui,
    registry::{self, ValueSnapshot},
    safety::UndoAction,
    ui::{Confirm, Message, show_message},
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    UserRegistry,
    MachineRegistry,
    MachineRegistry32,
    UserFolder,
    CommonFolder,
}

impl Source {
    pub const ALL: [Source; 5] = [
        Source::UserRegistry,
        Source::MachineRegistry,
        Source::MachineRegistry32,
        Source::UserFolder,
        Source::CommonFolder,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Source::UserRegistry => "Registry (current user)",
            Source::MachineRegistry => "Registry (all users)",
            Source::MachineRegistry32 => "Registry (all users, 32-bit)",
            Source::UserFolder => "Startup folder (current user)",
            Source::CommonFolder => "Startup folder (all users)",
        }
    }

    pub fn needs_admin(self) -> bool {
        !matches!(self, Source::UserRegistry | Source::UserFolder)
    }

    /// Registry key holding the entries (registry sources only).
    fn run_key(self) -> Option<String> {
        match self {
            Source::UserRegistry => Some(format!(r"HKCU\{RUN}")),
            Source::MachineRegistry => Some(format!(r"HKLM\{RUN}")),
            Source::MachineRegistry32 => Some(format!(r"HKLM\{RUN32}")),
            _ => None,
        }
    }

    fn folder(self) -> Option<PathBuf> {
        let base = |var: &str| std::env::var_os(var).map(PathBuf::from);
        match self {
            Source::UserFolder => {
                Some(base("APPDATA")?.join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
            }
            Source::CommonFolder => {
                Some(base("ProgramData")?.join(r"Microsoft\Windows\Start Menu\Programs\StartUp"))
            }
            _ => None,
        }
    }

    /// Key with the Task-Manager-compatible enabled/disabled flags.
    fn approved_key(self) -> String {
        match self {
            Source::UserRegistry => format!(r"HKCU\{APPROVED}\Run"),
            Source::MachineRegistry => format!(r"HKLM\{APPROVED}\Run"),
            Source::MachineRegistry32 => format!(r"HKLM\{APPROVED}\Run32"),
            Source::UserFolder => format!(r"HKCU\{APPROVED}\StartupFolder"),
            Source::CommonFolder => format!(r"HKLM\{APPROVED}\StartupFolder"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartupItem {
    pub source: Source,
    /// Registry value name or file name in the Startup folder.
    pub name: String,
    pub command: String,
    pub file: Option<PathBuf>,
    pub enabled: bool,
}

/// `StartupApproved` data: missing or even first byte = enabled, odd = disabled.
pub fn approved_is_enabled(data: Option<&[u8]>) -> bool {
    data.and_then(|d| d.first()).is_none_or(|b| b & 1 == 0)
}

/// 12-byte `StartupApproved` value (flag + FILETIME of the change).
pub fn approved_bytes(enabled: bool, now: SystemTime) -> Vec<u8> {
    let mut v = vec![if enabled { 2 } else { 3 }, 0, 0, 0];
    let filetime = if enabled {
        0u64
    } else {
        let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default();
        (secs.as_secs() + 11_644_473_600) * 10_000_000 + u64::from(secs.subsec_nanos() / 100)
    };
    v.extend_from_slice(&filetime.to_le_bytes());
    v
}

/// Extracts the executable from a command line, e.g. `"C:\a b\x.exe" -min` → `C:\a b\x.exe`.
pub fn exe_from_command(command: &str) -> Option<String> {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        return rest
            .split('"')
            .next()
            .map(str::to_owned)
            .filter(|s| !s.is_empty());
    }
    let lower = c.to_ascii_lowercase();
    if let Some(i) = lower.find(".exe") {
        return Some(c[..i + 4].to_owned());
    }
    c.split_whitespace().next().map(str::to_owned)
}

fn approved(source: Source, name: &str) -> bool {
    let data = registry::read_raw(&source.approved_key(), name);
    approved_is_enabled(data.as_ref().map(|v| v.bytes.as_slice()))
}

pub fn enumerate() -> Vec<StartupItem> {
    let mut items = Vec::new();
    for source in Source::ALL {
        if let Some(key) = source.run_key()
            && let Ok(k) = registry::open_read(&key)
        {
            for (name, value) in k.enum_values().flatten() {
                if name.is_empty() {
                    continue;
                }
                let raw = registry::RawValue {
                    vtype: value.vtype.clone() as u32,
                    bytes: value.bytes.into_owned(),
                };
                items.push(StartupItem {
                    source,
                    enabled: approved(source, &name),
                    command: registry::decode_string(&raw),
                    file: None,
                    name,
                });
            }
        }
        if let Some(dir) = source.folder()
            && let Ok(entries) = std::fs::read_dir(&dir)
        {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.eq_ignore_ascii_case("desktop.ini") || e.path().is_dir() {
                    continue;
                }
                items.push(StartupItem {
                    source,
                    enabled: approved(source, &name),
                    command: e.path().display().to_string(),
                    file: Some(e.path()),
                    name,
                });
            }
        }
    }
    items.sort_by_key(|i| i.name.to_lowercase());
    items
}

fn set_enabled(item: &StartupItem, enabled: bool) -> Result<ValueSnapshot> {
    let key = item.source.approved_key();
    let snap = registry::snapshot(&key, &item.name);
    registry::write_raw(
        &key,
        &item.name,
        &registry::binary(&approved_bytes(enabled, SystemTime::now())),
    )?;
    Ok(snap)
}

/// Removes a registry entry (journaled exactly) or recycles a Startup-folder file.
fn delete(ctx: &AppCtx<'_>, item: &StartupItem) -> Result<()> {
    if let Some(file) = &item.file {
        ctx.journal.recycle(
            "startup",
            format!("Removed startup item {}", item.name),
            vec![file.clone()],
        )?;
        return Ok(());
    }
    let key = item.source.run_key().expect("registry source");
    let snaps = vec![
        registry::snapshot(&key, &item.name),
        registry::snapshot(&item.source.approved_key(), &item.name),
    ];
    for s in &snaps {
        registry::delete_value(&s.key, &s.name)?;
    }
    ctx.journal.record(
        "startup",
        format!("Removed startup item {}", item.name),
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

fn open_location(item: &StartupItem) {
    let target = item
        .file
        .clone()
        .or_else(|| exe_from_command(&item.command).map(PathBuf::from));
    if let Some(t) = target.filter(|p| Path::new(p).exists()) {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", t.display()))
            .spawn();
    }
}

#[derive(Default)]
pub struct StartupManager {
    items: Option<Vec<StartupItem>>,
    selected: Option<(Source, String)>,
    confirm: Confirm<StartupItem>,
    message: Message,
    new_name: String,
    new_command: String,
}

impl StartupManager {
    fn add_entry(&mut self, ctx: &AppCtx<'_>) {
        let (name, command) = (
            self.new_name.trim().to_owned(),
            self.new_command.trim().to_owned(),
        );
        if name.is_empty() || command.is_empty() {
            self.message = Some(Err("Enter both a name and a command.".into()));
            return;
        }
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let key = format!(r"HKCU\{RUN}");
        let snap = registry::snapshot(&key, &name);
        if snap.previous.is_some() {
            self.message = Some(Err(format!("A startup entry named \"{name}\" already exists.")));
            return;
        }
        self.message = Some(
            registry::write_raw(&key, &name, &registry::string(&command))
                .and_then(|()| {
                    ctx.journal.record(
                        "startup",
                        format!("Added startup item {name}"),
                        UndoAction::RestoreRegValues { values: vec![snap] },
                    )
                })
                .map(|_| {
                    self.new_name.clear();
                    self.new_command.clear();
                    self.items = None;
                    format!("Added {name}.")
                })
                .map_err(|e| format!("{e:#}")),
        );
    }
}

impl Tool for StartupManager {
    fn id(&self) -> &'static str {
        "startup"
    }

    fn category(&self) -> Category {
        Category::Optimizer
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.items.get_or_insert_with(enumerate).clone();

        if let Some(item) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                delete(ctx, &item)
                    .map(|()| format!("Removed {}.", item.name))
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }

        let selected = self
            .selected
            .as_ref()
            .and_then(|(s, n)| items.iter().find(|i| i.source == *s && &i.name == n))
            .cloned();

        ui.horizontal(|ui| {
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.items = None;
                self.message = None;
            }
            let enabled = items.iter().filter(|i| i.enabled).count();
            ui.weak(format!("{} items, {enabled} enabled", items.len()));
            ui.separator();
            ui.add_enabled_ui(selected.is_some(), |ui| {
                let locked = selected
                    .as_ref()
                    .is_some_and(|i| i.source.needs_admin() && !ctx.elevated);
                if ui.add_enabled(!locked, egui::Button::new("Remove…")).clicked()
                    && let Some(item) = &selected
                {
                    self.confirm.ask(
                        format!(
                            "Remove \"{}\" from startup? Disabling it is usually enough. \
                             Removal can be undone from the Change Journal.",
                            item.name
                        ),
                        item.clone(),
                    );
                }
                if ui.button("Open file location").clicked()
                    && let Some(item) = &selected
                {
                    open_location(item);
                }
            });
        });
        show_message(ui, &self.message);
        ui.add_space(4.0);

        let mut toggle: Option<(StartupItem, bool)> = None;
        let mut clicked = None;
        let table_height = ui.available_height() - 90.0;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .max_scroll_height(table_height)
            .column(Column::exact(64.0))
            .column(Column::initial(200.0).clip(true))
            .column(Column::initial(190.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["Enabled", "Name", "Location", "Command"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, items.len(), |mut row| {
                    let item = &items[row.index()];
                    let is_sel = self
                        .selected
                        .as_ref()
                        .is_some_and(|(s, n)| *s == item.source && n == &item.name);
                    row.set_selected(is_sel);
                    let locked = item.source.needs_admin() && !ctx.elevated;
                    row.col(|ui| {
                        let mut on = item.enabled;
                        let r = ui.add_enabled(!locked, egui::Checkbox::without_text(&mut on));
                        if r.changed() {
                            toggle = Some((item.clone(), on));
                        }
                        if locked {
                            r.on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&item.name);
                    });
                    row.col(|ui| {
                        ui.weak(item.source.label());
                    });
                    row.col(|ui| {
                        ui.label(&item.command).on_hover_text(&item.command);
                    });
                    if row.response().clicked() {
                        clicked = Some((item.source, item.name.clone()));
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }

        if let Some((item, enabled)) = toggle {
            let verb = if enabled { "Enabled" } else { "Disabled" };
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                set_enabled(&item, enabled)
                    .and_then(|snap| {
                        ctx.journal.record(
                            "startup",
                            format!("{verb} startup item {}", item.name),
                            UndoAction::RestoreRegValues { values: vec![snap] },
                        )
                    })
                    .map(|_| format!("{verb} {}.", item.name))
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Add for current user:");
            ui.add(
                egui::TextEdit::singleline(&mut self.new_name)
                    .hint_text("Name")
                    .desired_width(140.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.new_command)
                    .hint_text(r#""C:\Path\App.exe" --args"#)
                    .desired_width(320.0),
            );
            if ui.button("Browse…").clicked()
                && let Some(p) = rfd_pick_exe()
            {
                self.new_command = format!("\"{}\"", p.display());
                if self.new_name.is_empty() {
                    self.new_name = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
            }
            if ui.button("Add").clicked() {
                self.add_entry(ctx);
            }
        });
    }
}

fn rfd_pick_exe() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("Programs", &["exe", "bat", "cmd", "lnk"])
        .pick_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_flags() {
        assert!(approved_is_enabled(None));
        assert!(approved_is_enabled(Some(&[])));
        assert!(approved_is_enabled(Some(&[2, 0, 0, 0])));
        assert!(approved_is_enabled(Some(&[6, 0, 0, 0])));
        assert!(!approved_is_enabled(Some(&[3, 0, 0, 0])));
        let on = approved_bytes(true, SystemTime::now());
        let off = approved_bytes(false, SystemTime::now());
        assert_eq!(on.len(), 12);
        assert!(approved_is_enabled(Some(&on)));
        assert!(!approved_is_enabled(Some(&off)));
        assert_ne!(&off[4..], &[0u8; 8], "disabled entries carry a timestamp");
    }

    #[test]
    fn command_parsing() {
        assert_eq!(
            exe_from_command(r#""C:\Program Files\X\x.exe" -min"#).as_deref(),
            Some(r"C:\Program Files\X\x.exe")
        );
        assert_eq!(
            exe_from_command(r"C:\Program Files\Y\y.EXE /background").as_deref(),
            Some(r"C:\Program Files\Y\y.EXE")
        );
        assert_eq!(
            exe_from_command("rundll32 foo.dll,Bar").as_deref(),
            Some("rundll32")
        );
        assert_eq!(exe_from_command("  ").as_deref(), None);
    }

    #[test]
    fn enumerate_does_not_fail() {
        let items = enumerate();
        assert!(items.iter().all(|i| !i.name.is_empty()));
    }

    #[test]
    fn disable_and_restore_via_snapshot() {
        let item = StartupItem {
            source: Source::UserRegistry,
            name: format!("wm-test-{}", std::process::id()),
            command: "x".into(),
            file: None,
            enabled: true,
        };
        let snap = set_enabled(&item, false).unwrap();
        assert!(!approved(item.source, &item.name));
        registry::restore(&snap).unwrap();
        assert!(approved(item.source, &item.name));
        assert_eq!(registry::read_raw(&item.source.approved_key(), &item.name), None);
    }
}
