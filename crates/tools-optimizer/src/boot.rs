//! Boot Configuration: boot menu timeout and default entry, plus per-entry options
//! (safe mode, boot logging, no GUI boot, base video, boot menu style).
//!
//! Uses `bcdedit` (administrator only). The whole store is exported to the backups
//! folder before the first change of a session; each change is journaled with its
//! exact reverse command.

use anyhow::{Context, Result};
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BcdEntry {
    pub header: String,
    pub props: Vec<(String, String)>,
}

impl BcdEntry {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub fn identifier(&self) -> &str {
        self.get("identifier").unwrap_or_default()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| v.eq_ignore_ascii_case("yes"))
    }
}

/// Parses `bcdedit /enum` output: blocks of "Header / ----- / key  value" lines.
/// Indented lines continue the previous value (e.g. multi-entry `displayorder`).
pub fn parse_bcdedit(text: &str) -> Vec<BcdEntry> {
    let mut entries = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let Some(header) = lines.next() else { continue };
        let mut entry = BcdEntry {
            header: header.trim().to_owned(),
            props: Vec::new(),
        };
        for line in lines {
            if line.trim_start().starts_with("---") {
                continue;
            }
            if line.starts_with(char::is_whitespace) {
                if let Some((_, v)) = entry.props.last_mut() {
                    v.push('\n');
                    v.push_str(line.trim());
                }
                continue;
            }
            let line = line.trim();
            let (k, v) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            entry.props.push((k.to_owned(), v.trim().to_owned()));
        }
        if !entry.props.is_empty() {
            entries.push(entry);
        }
    }
    entries
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BootConfig {
    pub manager: BcdEntry,
    pub loaders: Vec<BcdEntry>,
}

impl BootConfig {
    pub fn from_entries(entries: Vec<BcdEntry>) -> Result<Self> {
        let mut manager = None;
        let mut loaders = Vec::new();
        for e in entries {
            if e.identifier() == "{bootmgr}" {
                manager = Some(e);
            } else if e
                .get("path")
                .is_some_and(|p| p.to_ascii_lowercase().contains("winload"))
            {
                loaders.push(e);
            }
        }
        Ok(Self {
            manager: manager.context("Windows Boot Manager entry not found")?,
            loaders,
        })
    }

    pub fn timeout(&self) -> u32 {
        self.manager
            .get("timeout")
            .and_then(|t| t.parse().ok())
            .unwrap_or(30)
    }

    pub fn default_id(&self) -> &str {
        self.manager.get("default").unwrap_or_default()
    }
}

pub fn load() -> Result<BootConfig> {
    let out = run_checked(hidden_command("bcdedit").args(["/enum", "ACTIVE"]))?;
    BootConfig::from_entries(parse_bcdedit(&String::from_utf8_lossy(&out.stdout)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeBoot {
    Off,
    Minimal,
    Network,
}

impl SafeBoot {
    pub fn of(entry: &BcdEntry) -> SafeBoot {
        match entry.get("safeboot").map(str::to_ascii_lowercase).as_deref() {
            Some("minimal") => SafeBoot::Minimal,
            Some("network") => SafeBoot::Network,
            _ => SafeBoot::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SafeBoot::Off => "Normal",
            SafeBoot::Minimal => "Safe mode",
            SafeBoot::Network => "Safe mode with networking",
        }
    }
}

/// A single bcdedit change: forward args, reverse args and description.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub args: Vec<String>,
    pub undo: Vec<String>,
    pub description: String,
}

fn set_or_delete(id: &str, key: &str, value: Option<&str>) -> Vec<String> {
    match value {
        Some(v) => vec!["/set".into(), id.into(), key.into(), v.into()],
        None => vec!["/deletevalue".into(), id.into(), key.into()],
    }
}

/// Builds a change for a yes/no option (absent ≙ "no").
pub fn flag_change(entry: &BcdEntry, key: &str, on: bool, label: &str) -> Change {
    let id = entry.identifier();
    let previous = entry.get(key).map(str::to_owned);
    Change {
        args: set_or_delete(id, key, on.then_some("yes")),
        undo: set_or_delete(id, key, previous.as_deref()),
        description: format!(
            "{} {label} for {}",
            if on { "Enabled" } else { "Disabled" },
            describe(entry)
        ),
    }
}

pub fn safeboot_change(entry: &BcdEntry, mode: SafeBoot) -> Change {
    let id = entry.identifier();
    let value = match mode {
        SafeBoot::Off => None,
        SafeBoot::Minimal => Some("minimal"),
        SafeBoot::Network => Some("network"),
    };
    Change {
        args: set_or_delete(id, "safeboot", value),
        undo: set_or_delete(id, "safeboot", entry.get("safeboot")),
        description: format!("Set next boot of {} to {}", describe(entry), mode.label()),
    }
}

fn describe(entry: &BcdEntry) -> String {
    entry.get("description").unwrap_or(entry.identifier()).to_owned()
}

fn bcdedit(args: &[String]) -> Result<()> {
    run_checked(hidden_command("bcdedit").args(args)).map(|_| ())
}

/// Exports the full BCD store to the backups folder.
fn export_backup() -> Result<std::path::PathBuf> {
    let file = paths::backups_dir()?.join(format!(
        "{}-bcd.bak",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    bcdedit(&["/export".into(), file.display().to_string()]).context("backing up the boot store")?;
    Ok(file)
}

#[derive(Default)]
pub struct BootConfiguration {
    load: Option<BackgroundTask<BootConfig>>,
    config: Option<Result<BootConfig, String>>,
    backup: Option<std::path::PathBuf>,
    selected: usize,
    timeout_edit: Option<u32>,
    confirm: Confirm<Change>,
    message: Message,
}

impl BootConfiguration {
    fn apply(&mut self, ctx: &AppCtx<'_>, change: Change) {
        if ctx.config.dry_run {
            self.message = Some(Ok(format!(
                "{} (bcdedit {})",
                ctx.i18n.tr("common-dry-run-skip"),
                change.args.join(" ")
            )));
            return;
        }
        let result = (|| -> Result<String> {
            if self.backup.is_none() {
                self.backup = Some(export_backup()?);
            }
            bcdedit(&change.args)?;
            ctx.journal.record(
                "boot",
                change.description.clone(),
                UndoAction::RunCommand {
                    program: "bcdedit".into(),
                    args: change.undo.clone(),
                },
            )?;
            Ok(format!("{}.", change.description))
        })();
        self.message = Some(result.map_err(|e| format!("{e:#}")));
        self.config = None;
        self.timeout_edit = None;
    }
}

impl Tool for BootConfiguration {
    fn id(&self) -> &'static str {
        "boot"
    }

    fn category(&self) -> Category {
        Category::Optimizer
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if !ctx.elevated {
            ui.label("Reading and changing the boot configuration requires administrator rights.");
            return;
        }
        if self.load.is_none() && self.config.is_none() {
            self.load = Some(BackgroundTask::spawn(ui.ctx(), |_| load()));
        }
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                self.config = Some(r);
                self.load = None;
            }
        }
        if let Some(change) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.apply(ctx, change);
        }
        show_message(ui, &self.message);
        if let Some(b) = &self.backup {
            ui.weak(format!("Boot store backup for this session: {}", b.display()));
        }

        let cfg = match &self.config {
            Some(Ok(c)) => c.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => {
                ui.spinner();
                return;
            }
        };

        ui.heading("Boot menu");
        ui.horizontal(|ui| {
            ui.label("Show the boot menu for");
            let timeout = self.timeout_edit.get_or_insert(cfg.timeout());
            ui.add(egui::DragValue::new(timeout).range(0..=999).suffix(" s"));
            if *timeout != cfg.timeout() && ui.button("Apply").clicked() {
                let change = Change {
                    args: vec!["/timeout".into(), timeout.to_string()],
                    undo: vec!["/timeout".into(), cfg.timeout().to_string()],
                    description: format!(
                        "Changed boot menu timeout from {} s to {timeout} s",
                        cfg.timeout()
                    ),
                };
                self.apply(ctx, change);
            }
        });
        if cfg.loaders.len() > 1 {
            ui.horizontal(|ui| {
                ui.label("Default operating system:");
                let current = cfg.loaders.iter().find(|l| l.identifier() == cfg.default_id());
                egui::ComboBox::from_id_salt("default_os")
                    .selected_text(
                        current
                            .map(describe)
                            .unwrap_or_else(|| cfg.default_id().to_owned()),
                    )
                    .show_ui(ui, |ui| {
                        for l in &cfg.loaders {
                            if ui
                                .selectable_label(l.identifier() == cfg.default_id(), describe(l))
                                .clicked()
                                && l.identifier() != cfg.default_id()
                            {
                                let change = Change {
                                    args: vec!["/default".into(), l.identifier().into()],
                                    undo: vec!["/default".into(), cfg.default_id().into()],
                                    description: format!("Set default boot entry to {}", describe(l)),
                                };
                                self.apply(ctx, change);
                            }
                        }
                    });
            });
        }

        ui.add_space(12.0);
        ui.heading("Boot entries");
        if cfg.loaders.is_empty() {
            ui.label("No Windows boot loader entries found.");
            return;
        }
        self.selected = self.selected.min(cfg.loaders.len() - 1);
        ui.horizontal(|ui| {
            for (i, l) in cfg.loaders.iter().enumerate() {
                let label = if l.identifier() == cfg.default_id() {
                    format!("{} (default)", describe(l))
                } else {
                    describe(l)
                };
                ui.selectable_value(&mut self.selected, i, label);
            }
        });
        let entry = cfg.loaders[self.selected].clone();
        egui::Grid::new("boot_entry")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for key in ["identifier", "device", "path", "locale"] {
                    if let Some(v) = entry.get(key) {
                        ui.weak(key);
                        ui.label(v);
                        ui.end_row();
                    }
                }
            });
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label("Next boot:");
            let current = SafeBoot::of(&entry);
            for mode in [SafeBoot::Off, SafeBoot::Minimal, SafeBoot::Network] {
                if ui.selectable_label(current == mode, mode.label()).clicked() && current != mode {
                    let change = safeboot_change(&entry, mode);
                    if mode == SafeBoot::Off {
                        self.apply(ctx, change);
                    } else {
                        self.confirm.ask(
                            "Windows will start in Safe Mode on every boot until you switch back to \
                             Normal here (or undo from the Change Journal). Continue?",
                            change,
                        );
                    }
                }
            }
        });

        let mut legacy = entry
            .get("bootmenupolicy")
            .is_some_and(|p| p.eq_ignore_ascii_case("legacy"));
        if ui
            .checkbox(
                &mut legacy,
                "Classic text boot menu (allows F8 for advanced options; slower boot)",
            )
            .changed()
        {
            let id = entry.identifier();
            let change = Change {
                args: vec![
                    "/set".into(),
                    id.into(),
                    "bootmenupolicy".into(),
                    if legacy { "legacy" } else { "standard" }.into(),
                ],
                undo: set_or_delete(id, "bootmenupolicy", entry.get("bootmenupolicy")),
                description: format!(
                    "Set boot menu style of {} to {}",
                    describe(&entry),
                    if legacy { "classic" } else { "standard" }
                ),
            };
            self.apply(ctx, change);
        }
        for (key, label, help) in [
            (
                "quietboot",
                "No GUI boot",
                "Hides the Windows logo and spinner during startup.",
            ),
            (
                "bootlog",
                "Boot log",
                r"Writes loaded drivers to C:\Windows\ntbtlog.txt.",
            ),
            (
                "basevideo",
                "Base video",
                "Starts with the basic display driver (for display driver problems).",
            ),
        ] {
            let mut on = entry.flag(key);
            if ui.checkbox(&mut on, label).on_hover_text(help).changed() {
                self.apply(ctx, flag_change(&entry, key, on, label));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\r\nWindows Boot Manager\r\n--------------------\r\nidentifier              {bootmgr}\r\n\
        device                  partition=\\Device\\HarddiskVolume1\r\ndescription             Windows Boot Manager\r\n\
        default                 {current}\r\ndisplayorder            {current}\r\n                        {1111-2222}\r\n\
        timeout                 30\r\n\r\nWindows Boot Loader\r\n-------------------\r\nidentifier              {current}\r\n\
        device                  partition=C:\r\npath                    \\WINDOWS\\system32\\winload.efi\r\n\
        description             Windows 11\r\nbootmenupolicy          Standard\r\nquietboot               Yes\r\n\
        \r\nWindows Boot Loader\r\n-------------------\r\nidentifier              {1111-2222}\r\n\
        path                    \\Windows\\system32\\winload.exe\r\ndescription             Old Windows\r\nsafeboot                Minimal\r\n";

    #[test]
    fn parse_sample_store() {
        let cfg = BootConfig::from_entries(parse_bcdedit(SAMPLE)).unwrap();
        assert_eq!(cfg.timeout(), 30);
        assert_eq!(cfg.default_id(), "{current}");
        assert_eq!(cfg.manager.get("displayorder"), Some("{current}\n{1111-2222}"));
        assert_eq!(cfg.loaders.len(), 2);
        assert!(cfg.loaders[0].flag("quietboot"));
        assert!(!cfg.loaders[0].flag("bootlog"));
        assert_eq!(SafeBoot::of(&cfg.loaders[1]), SafeBoot::Minimal);
    }

    #[test]
    fn changes_have_exact_reverses() {
        let cfg = BootConfig::from_entries(parse_bcdedit(SAMPLE)).unwrap();
        let current = &cfg.loaders[0];
        let c = flag_change(current, "bootlog", true, "Boot log");
        assert_eq!(c.args, ["/set", "{current}", "bootlog", "yes"]);
        assert_eq!(c.undo, ["/deletevalue", "{current}", "bootlog"]);

        let c = flag_change(current, "quietboot", false, "No GUI boot");
        assert_eq!(c.args, ["/deletevalue", "{current}", "quietboot"]);
        assert_eq!(c.undo, ["/set", "{current}", "quietboot", "Yes"]);

        let c = safeboot_change(&cfg.loaders[1], SafeBoot::Off);
        assert_eq!(c.args, ["/deletevalue", "{1111-2222}", "safeboot"]);
        assert_eq!(c.undo, ["/set", "{1111-2222}", "safeboot", "Minimal"]);
    }

    #[test]
    fn missing_boot_manager_is_an_error() {
        assert!(BootConfig::from_entries(Vec::new()).is_err());
    }
}
