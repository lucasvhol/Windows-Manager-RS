//! Boot Configuration: boot menu timeout, default entry and order; add, rename and
//! delete entries; per-entry options (safe mode variants, boot logging, no GUI boot,
//! base video, boot menu style, driver-signing switches); back up and restore the
//! whole boot store; restart into Advanced startup or the UEFI firmware settings.
//!
//! Uses `bcdedit` (administrator only). The whole store is exported to the backups
//! folder before the first change of a session; each change is journaled with its
//! exact reverse commands, and destructive ones with a fresh export to re-import.

use anyhow::{Context, Result};
use wm_core::{
    AppCtx, Category, Tool, egui, icons, paths,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, show_message, toolbar},
};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BcdEntry {
    pub header: String,
    pub props: Vec<(String, String)>,
}

/// "Yes" as bcdedit prints it in common display languages (values are localized,
/// element names are not).
const YES: &[&str] = &[
    "yes", "sim", "ja", "oui", "sí", "si", "sì", "tak", "evet", "да", "是", "はい", "예",
];

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
        self.get(key)
            .is_some_and(|v| YES.iter().any(|y| v.trim().to_lowercase() == *y))
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
    /// Windows boot loaders in boot-menu order.
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
        let manager = manager.context("Windows Boot Manager entry not found")?;
        let order: Vec<String> = manager
            .get("displayorder")
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        loaders.sort_by_key(|l| {
            order
                .iter()
                .position(|o| o == l.identifier())
                .unwrap_or(usize::MAX)
        });
        Ok(Self { manager, loaders })
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

    pub fn display_order(&self) -> Vec<String> {
        self.manager
            .get("displayorder")
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
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
    CommandPrompt,
}

impl SafeBoot {
    pub const ALL: [SafeBoot; 4] = [
        SafeBoot::Off,
        SafeBoot::Minimal,
        SafeBoot::Network,
        SafeBoot::CommandPrompt,
    ];

    pub fn of(entry: &BcdEntry) -> SafeBoot {
        match entry.get("safeboot").map(str::to_ascii_lowercase).as_deref() {
            Some("minimal") if entry.flag("safebootalternateshell") => SafeBoot::CommandPrompt,
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
            SafeBoot::CommandPrompt => "Safe mode with Command Prompt",
        }
    }
}

/// A set of bcdedit commands with the commands that reverse them.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub steps: Vec<Vec<String>>,
    pub undo: Vec<Vec<String>>,
    pub description: String,
    /// Export the store right before the change and undo by re-importing it
    /// (for changes without a precise reverse, such as deleting an entry).
    pub reimport: bool,
}

impl Change {
    fn one(args: Vec<String>, undo: Vec<String>, description: String) -> Self {
        Self {
            steps: vec![args],
            undo: vec![undo],
            description,
            reimport: false,
        }
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

fn set_or_delete(id: &str, key: &str, value: Option<&str>) -> Vec<String> {
    match value {
        Some(v) => s(&["/set", id, key, v]),
        None => s(&["/deletevalue", id, key]),
    }
}

/// Builds a change for a yes/no option (absent ≙ "no").
pub fn flag_change(entry: &BcdEntry, key: &str, on: bool, label: &str) -> Change {
    let id = entry.identifier();
    Change::one(
        set_or_delete(id, key, on.then_some("yes")),
        set_or_delete(id, key, entry.get(key)),
        format!(
            "{} {label} for {}",
            if on { "Enabled" } else { "Disabled" },
            describe(entry)
        ),
    )
}

pub fn safeboot_change(entry: &BcdEntry, mode: SafeBoot) -> Change {
    let id = entry.identifier();
    let (safeboot, shell) = match mode {
        SafeBoot::Off => (None, None),
        SafeBoot::Minimal => (Some("minimal"), None),
        SafeBoot::Network => (Some("network"), None),
        SafeBoot::CommandPrompt => (Some("minimal"), Some("yes")),
    };
    let mut steps = vec![set_or_delete(id, "safeboot", safeboot)];
    let mut undo = vec![set_or_delete(id, "safeboot", entry.get("safeboot"))];
    // Only touch the alternate shell when it is set now or wanted.
    if shell.is_some() || entry.get("safebootalternateshell").is_some() {
        steps.push(set_or_delete(id, "safebootalternateshell", shell));
        undo.push(set_or_delete(
            id,
            "safebootalternateshell",
            entry.get("safebootalternateshell"),
        ));
    }
    Change {
        steps,
        undo,
        description: format!("Set next boot of {} to {}", describe(entry), mode.label()),
        reimport: false,
    }
}

/// `order` with `id` moved one place up (`-1`) or down (`+1`).
pub fn moved(order: &[String], id: &str, delta: isize) -> Option<Vec<String>> {
    let i = order.iter().position(|o| o == id)?;
    let j = i.checked_add_signed(delta).filter(|j| *j < order.len())?;
    let mut new = order.to_vec();
    new.swap(i, j);
    Some(new)
}

pub fn order_change(cfg: &BootConfig, new: Vec<String>, entry: &BcdEntry) -> Change {
    let mut args = s(&["/displayorder"]);
    args.extend(new);
    let mut undo = s(&["/displayorder"]);
    undo.extend(cfg.display_order());
    Change::one(args, undo, format!("Moved {} in the boot menu", describe(entry)))
}

/// The first `{xxxxxxxx-xxxx-…}` GUID in bcdedit output.
pub fn find_guid(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    (0..bytes.len()).find_map(|i| {
        let cand = text.get(i..i + 38)?;
        let inner = &cand[1..37];
        (cand.starts_with('{')
            && cand.ends_with('}')
            && inner.char_indices().all(|(k, c)| {
                if [8, 13, 18, 23].contains(&k) {
                    c == '-'
                } else {
                    c.is_ascii_hexdigit()
                }
            }))
        .then(|| cand.to_owned())
    })
}

fn describe(entry: &BcdEntry) -> String {
    entry.get("description").unwrap_or(entry.identifier()).to_owned()
}

fn bcdedit(args: &[String]) -> Result<String> {
    let out = run_checked(hidden_command("bcdedit").args(args))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Exports the full BCD store to `file`, or to the backups folder.
fn export_store(file: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    let file = match file {
        Some(f) => f,
        None => paths::backups_dir()?.join(format!(
            "{}-bcd.bak",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        )),
    };
    bcdedit(&["/export".into(), file.display().to_string()]).context("backing up the boot store")?;
    Ok(file)
}

fn undo_action(undo: &[Vec<String>]) -> UndoAction {
    let cmd = |args: &Vec<String>| UndoAction::RunCommand {
        program: "bcdedit".into(),
        args: args.clone(),
    };
    match undo {
        [one] => cmd(one),
        many => UndoAction::Sequence {
            steps: many.iter().map(cmd).collect(),
        },
    }
}

enum Ask {
    Change(Change),
    Add(String),
    Import(std::path::PathBuf),
    Restart { firmware: bool },
}

#[derive(Default)]
pub struct BootConfiguration {
    load: Option<BackgroundTask<BootConfig>>,
    config: Option<Result<BootConfig, String>>,
    backup: Option<std::path::PathBuf>,
    selected: usize,
    timeout_edit: Option<u32>,
    rename: Option<String>,
    new_name: Option<String>,
    confirm: Confirm<Ask>,
    message: Message,
}

impl BootConfiguration {
    fn session_backup(&mut self) -> Result<()> {
        if self.backup.is_none() {
            self.backup = Some(export_store(None)?);
        }
        Ok(())
    }

    fn apply(&mut self, ctx: &AppCtx<'_>, change: Change) {
        if ctx.config.dry_run {
            let cmds: Vec<String> = change
                .steps
                .iter()
                .map(|a| format!("bcdedit {}", a.join(" ")))
                .collect();
            self.message = Some(Ok(format!(
                "{} ({})",
                ctx.i18n.tr("common-dry-run-skip"),
                cmds.join("; ")
            )));
            return;
        }
        let result = (|| -> Result<String> {
            self.session_backup()?;
            let undo = if change.reimport {
                let fresh = export_store(None)?;
                vec![vec!["/import".to_owned(), fresh.display().to_string()]]
            } else {
                change.undo.clone()
            };
            for (i, step) in change.steps.iter().enumerate() {
                if let Err(e) = bcdedit(step) {
                    // Roll back the steps that did succeed.
                    if !change.reimport {
                        for u in change.undo.iter().take(i) {
                            let _ = bcdedit(u);
                        }
                    }
                    return Err(e);
                }
            }
            ctx.journal
                .record("boot", change.description.clone(), undo_action(&undo))?;
            Ok(format!("{}.", change.description))
        })();
        self.finish(result);
    }

    fn finish(&mut self, result: Result<String>) {
        self.message = Some(result.map_err(|e| format!("{e:#}")));
        self.config = None;
        self.timeout_edit = None;
        self.rename = None;
    }

    fn add_entry(&mut self, ctx: &AppCtx<'_>, name: &str) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let result = (|| -> Result<String> {
            self.session_backup()?;
            let out = bcdedit(&s(&["/copy", "{current}", "/d", name]))?;
            let id = find_guid(&out).context("bcdedit did not report the new entry's identifier")?;
            ctx.journal.record(
                "boot",
                format!("Added boot entry {name} {id}"),
                undo_action(&[s(&["/delete", &id])]),
            )?;
            Ok(format!(
                "Added boot entry \"{name}\", a copy of the running Windows."
            ))
        })();
        self.finish(result);
    }

    fn import(&mut self, ctx: &AppCtx<'_>, file: &std::path::Path) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let result = (|| -> Result<String> {
            let fresh = export_store(None)?;
            bcdedit(&["/import".into(), file.display().to_string()])?;
            ctx.journal.record(
                "boot",
                format!("Restored the boot store from {}", file.display()),
                undo_action(&[vec!["/import".into(), fresh.display().to_string()]]),
            )?;
            Ok(format!("Restored the boot store from {}.", file.display()))
        })();
        self.finish(result);
    }

    fn run_ask(&mut self, ctx: &AppCtx<'_>, ask: Ask) {
        match ask {
            Ask::Change(c) => self.apply(ctx, c),
            Ask::Add(name) => self.add_entry(ctx, &name),
            Ask::Import(f) => self.import(ctx, &f),
            Ask::Restart { firmware } => {
                let arg = if firmware { "/fw" } else { "/o" };
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    run_checked(hidden_command("shutdown").args(["/r", arg, "/t", "0"]))
                        .map(|_| "Restarting…".to_owned())
                        .map_err(|e| format!("{e:#}"))
                });
            }
        }
    }

    fn entry_options(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, cfg: &BootConfig, entry: &BcdEntry) {
        let id = entry.identifier().to_owned();
        ui.horizontal(|ui| {
            ui.heading(describe(entry));
            if id == cfg.default_id() {
                ui.weak("(default)");
            }
            if id == "{current}" {
                ui.weak("(running now)");
            }
        });
        if let Some(name) = &mut self.rename {
            let mut save = false;
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(name);
                save = ui.button(format!("{}  Save", icons::FLOPPY_DISK)).clicked();
                cancel = ui.button(ctx.i18n.tr("common-cancel")).clicked();
            });
            if save && !name.trim().is_empty() {
                let name = name.trim().to_owned();
                let change = Change::one(
                    s(&["/set", &id, "description", &name]),
                    set_or_delete(&id, "description", entry.get("description")),
                    format!("Renamed boot entry {} to {name}", describe(entry)),
                );
                self.apply(ctx, change);
            } else if cancel {
                self.rename = None;
            }
        }
        egui::Grid::new("boot_entry")
            .num_columns(2)
            .spacing([16.0, 3.0])
            .show(ui, |ui| {
                for key in ["identifier", "device", "path", "osdevice", "systemroot", "locale"] {
                    if let Some(v) = entry.get(key) {
                        ui.weak(key);
                        ui.label(v);
                        ui.end_row();
                    }
                }
            });
        ui.add_space(8.0);

        ui.strong("Next boot");
        ui.horizontal_wrapped(|ui| {
            let current = SafeBoot::of(entry);
            for mode in SafeBoot::ALL {
                if ui.selectable_label(current == mode, mode.label()).clicked() && current != mode {
                    let change = safeboot_change(entry, mode);
                    if mode == SafeBoot::Off {
                        self.apply(ctx, change);
                    } else {
                        self.confirm.ask(
                            "Windows will start in this mode on every boot until you switch back to Normal here (or undo from the Change Journal). Continue?",
                            Ask::Change(change),
                        );
                    }
                }
            }
        });
        ui.add_space(6.0);

        ui.strong("Options");
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
            let change = Change::one(
                s(&[
                    "/set",
                    &id,
                    "bootmenupolicy",
                    if legacy { "legacy" } else { "standard" },
                ]),
                set_or_delete(&id, "bootmenupolicy", entry.get("bootmenupolicy")),
                format!(
                    "Set boot menu style of {} to {}",
                    describe(entry),
                    if legacy { "classic" } else { "standard" }
                ),
            );
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
                self.apply(ctx, flag_change(entry, key, on, label));
            }
        }
        ui.add_space(6.0);

        ui.strong("Driver signing");
        ui.weak("These lower security and only work with Secure Boot turned off in the firmware.");
        for (key, label, warning) in [
            (
                "testsigning",
                "Test signing mode",
                "Test signing lets Windows load drivers signed with test certificates, including malicious ones, and shows a watermark on the desktop.",
            ),
            (
                "nointegritychecks",
                "Disable driver signature checks",
                "Windows will load unsigned drivers, which malware can use to run at kernel level.",
            ),
        ] {
            let mut on = entry.flag(key);
            if ui.checkbox(&mut on, label).changed() {
                let change = flag_change(entry, key, on, label);
                if on {
                    self.confirm.ask(
                        format!("{warning} Enable {label} for {}?", describe(entry)),
                        Ask::Change(change),
                    );
                } else {
                    self.apply(ctx, change);
                }
            }
        }
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
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.run_ask(ctx, ask);
        }

        let cfg = match &self.config {
            Some(Ok(c)) => c.clone(),
            Some(Err(e)) => {
                show_message(ui, &self.message);
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => {
                ui.spinner();
                return;
            }
        };
        self.selected = self.selected.min(cfg.loaders.len().saturating_sub(1));
        let entry = cfg.loaders.get(self.selected).cloned();
        let id = entry
            .as_ref()
            .map(|e| e.identifier().to_owned())
            .unwrap_or_default();
        let order = cfg.display_order();
        let deletable = entry.is_some() && id != "{current}" && id != cfg.default_id();

        let buttons = [
            Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            None,
            Some(ToolButton::new(icons::PLUS, "Add")),
            Some(ToolButton::new(icons::PENCIL_SIMPLE, "Rename").enabled(entry.is_some())),
            Some(ToolButton::new(icons::TRASH, "Delete").enabled(deletable)),
            Some(ToolButton::new(icons::ARROW_UP, "Move up").enabled(moved(&order, &id, -1).is_some())),
            Some(ToolButton::new(icons::ARROW_DOWN, "Move down").enabled(moved(&order, &id, 1).is_some())),
            None,
            Some(ToolButton::new(icons::FLOPPY_DISK, "Back up")),
            Some(ToolButton::new(icons::UPLOAD, "Restore")),
            None,
            Some(ToolButton::new(icons::WRENCH, "Advanced startup")),
            Some(ToolButton::new(icons::CPU, "UEFI settings")),
        ];
        match toolbar(ui, &buttons) {
            Some(0) => {
                self.config = None;
                self.message = None;
            }
            Some(2) => self.new_name = Some("Windows (copy)".into()),
            Some(3) => self.rename = entry.as_ref().map(describe),
            Some(4) => {
                if let Some(e) = &entry {
                    self.confirm.ask(
                        format!(
                            "Delete the boot entry \"{}\" {id}? The boot store is exported first and the deletion can be undone from the Change Journal.",
                            describe(e)
                        ),
                        Ask::Change(Change {
                            steps: vec![s(&["/delete", &id])],
                            undo: Vec::new(),
                            description: format!("Deleted boot entry {} {id}", describe(e)),
                            reimport: true,
                        }),
                    );
                }
            }
            Some(i @ (5 | 6)) => {
                if let Some(e) = &entry
                    && let Some(new) = moved(&order, &id, if i == 5 { -1 } else { 1 })
                {
                    self.apply(ctx, order_change(&cfg, new, e));
                }
            }
            Some(8) => {
                if let Some(file) = rfd::FileDialog::new()
                    .set_directory(paths::backups_dir().unwrap_or_default())
                    .set_file_name(format!("bcd-{}.bak", chrono::Local::now().format("%Y%m%d-%H%M")))
                    .save_file()
                {
                    self.message = Some(
                        export_store(Some(file))
                            .map(|f| format!("Saved the boot store to {}.", f.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(9) => {
                if let Some(file) = rfd::FileDialog::new().set_directory(paths::backups_dir().unwrap_or_default()).pick_file() {
                    self.confirm.ask(
                        format!(
                            "Replace the whole boot store with {}? A wrong file can stop Windows from starting. The current store is exported first so this can be undone.",
                            file.display()
                        ),
                        Ask::Import(file),
                    );
                }
            }
            Some(11) => self.confirm.ask(
                "Restart now into Advanced startup (recovery options, Startup Settings, System Restore)? Save your work first.",
                Ask::Restart { firmware: false },
            ),
            Some(12) => self.confirm.ask(
                "Restart now into the UEFI firmware settings? Save your work first.",
                Ask::Restart { firmware: true },
            ),
            _ => {}
        }
        show_message(ui, &self.message);
        if let Some(b) = &self.backup {
            ui.weak(format!("Boot store backup for this session: {}", b.display()));
        }

        if let Some(name) = &mut self.new_name {
            let mut add = false;
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.label("New entry (a copy of the running Windows):");
                ui.text_edit_singleline(name);
                add = ui.button(format!("{}  Add", icons::PLUS)).clicked();
                cancel = ui.button(ctx.i18n.tr("common-cancel")).clicked();
            });
            if add && !name.trim().is_empty() {
                let name = name.trim().to_owned();
                self.new_name = None;
                self.run_ask(ctx, Ask::Add(name));
                return;
            } else if cancel {
                self.new_name = None;
            }
        }

        ui.horizontal(|ui| {
            ui.label("Show the boot menu for");
            let timeout = self.timeout_edit.get_or_insert(cfg.timeout());
            ui.add(egui::DragValue::new(timeout).range(0..=999).suffix(" s"));
            if *timeout != cfg.timeout() && ui.button("Apply").clicked() {
                let change = Change::one(
                    vec!["/timeout".into(), timeout.to_string()],
                    vec!["/timeout".into(), cfg.timeout().to_string()],
                    format!(
                        "Changed boot menu timeout from {} s to {timeout} s",
                        cfg.timeout()
                    ),
                );
                self.apply(ctx, change);
            }
            if cfg.loaders.len() > 1 {
                ui.separator();
                ui.label("Default:");
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
                                let change = Change::one(
                                    s(&["/default", l.identifier()]),
                                    s(&["/default", cfg.default_id()]),
                                    format!("Set default boot entry to {}", describe(l)),
                                );
                                self.apply(ctx, change);
                            }
                        }
                    });
            }
        });
        ui.add_space(6.0);

        if cfg.loaders.is_empty() {
            ui.label("No Windows boot loader entries found.");
            return;
        }
        egui::Panel::left("boot_entries")
            .resizable(true)
            .default_size(240.0)
            .show(ui, |ui| {
                ui.strong("Boot menu");
                for (i, l) in cfg.loaders.iter().enumerate() {
                    let mut label = describe(l);
                    if l.identifier() == cfg.default_id() {
                        label += "  (default)";
                    }
                    if ui
                        .selectable_label(self.selected == i, format!("{}  {label}", icons::WINDOWS_LOGO))
                        .clicked()
                    {
                        self.selected = i;
                        self.rename = None;
                    }
                }
            });
        if let Some(e) = entry {
            egui::ScrollArea::vertical().show(ui, |ui| self.entry_options(ui, ctx, &cfg, &e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\r\nWindows Boot Manager\r\n--------------------\r\nidentifier              {bootmgr}\r\n\
        device                  partition=\\Device\\HarddiskVolume1\r\ndescription             Windows Boot Manager\r\n\
        default                 {current}\r\ndisplayorder            {1111-2222}\r\n                        {current}\r\n\
        timeout                 30\r\n\r\nWindows Boot Loader\r\n-------------------\r\nidentifier              {current}\r\n\
        device                  partition=C:\r\npath                    \\WINDOWS\\system32\\winload.efi\r\n\
        description             Windows 11\r\nbootmenupolicy          Standard\r\nquietboot               Yes\r\n\
        \r\nWindows Boot Loader\r\n-------------------\r\nidentifier              {1111-2222}\r\n\
        path                    \\Windows\\system32\\winload.exe\r\ndescription             Old Windows\r\nsafeboot                Minimal\r\n\
        safebootalternateshell  Sim\r\n";

    #[test]
    fn parse_sample_store() {
        let cfg = BootConfig::from_entries(parse_bcdedit(SAMPLE)).unwrap();
        assert_eq!(cfg.timeout(), 30);
        assert_eq!(cfg.default_id(), "{current}");
        assert_eq!(cfg.display_order(), ["{1111-2222}", "{current}"]);
        assert_eq!(cfg.loaders.len(), 2);
        assert_eq!(
            cfg.loaders[0].identifier(),
            "{1111-2222}",
            "loaders follow the display order"
        );
        assert!(cfg.loaders[1].flag("quietboot"));
        assert!(!cfg.loaders[1].flag("bootlog"));
        assert_eq!(
            SafeBoot::of(&cfg.loaders[0]),
            SafeBoot::CommandPrompt,
            "localized yes is understood"
        );
    }

    #[test]
    fn changes_have_exact_reverses() {
        let cfg = BootConfig::from_entries(parse_bcdedit(SAMPLE)).unwrap();
        let current = &cfg.loaders[1];
        let c = flag_change(current, "bootlog", true, "Boot log");
        assert_eq!(c.steps, [s(&["/set", "{current}", "bootlog", "yes"])]);
        assert_eq!(c.undo, [s(&["/deletevalue", "{current}", "bootlog"])]);

        let c = flag_change(current, "quietboot", false, "No GUI boot");
        assert_eq!(c.steps, [s(&["/deletevalue", "{current}", "quietboot"])]);
        assert_eq!(c.undo, [s(&["/set", "{current}", "quietboot", "Yes"])]);

        let old = &cfg.loaders[0];
        let c = safeboot_change(old, SafeBoot::Off);
        assert_eq!(
            c.steps,
            [
                s(&["/deletevalue", "{1111-2222}", "safeboot"]),
                s(&["/deletevalue", "{1111-2222}", "safebootalternateshell"])
            ]
        );
        assert_eq!(
            c.undo,
            [
                s(&["/set", "{1111-2222}", "safeboot", "Minimal"]),
                s(&["/set", "{1111-2222}", "safebootalternateshell", "Sim"])
            ]
        );
        let c = safeboot_change(current, SafeBoot::Network);
        assert_eq!(c.steps.len(), 1, "alternate shell untouched when unused");
        let c = safeboot_change(current, SafeBoot::CommandPrompt);
        assert_eq!(
            c.steps[1],
            s(&["/set", "{current}", "safebootalternateshell", "yes"])
        );
    }

    #[test]
    fn boot_order_moves() {
        let cfg = BootConfig::from_entries(parse_bcdedit(SAMPLE)).unwrap();
        let order = cfg.display_order();
        assert_eq!(
            moved(&order, "{current}", -1).unwrap(),
            ["{current}", "{1111-2222}"]
        );
        assert!(moved(&order, "{current}", 1).is_none());
        assert!(moved(&order, "{1111-2222}", -1).is_none());
        let c = order_change(&cfg, moved(&order, "{current}", -1).unwrap(), &cfg.loaders[1]);
        assert_eq!(c.steps, [s(&["/displayorder", "{current}", "{1111-2222}"])]);
        assert_eq!(c.undo, [s(&["/displayorder", "{1111-2222}", "{current}"])]);
    }

    #[test]
    fn guid_extraction() {
        assert_eq!(
            find_guid("The entry was successfully copied to {0b7f1c3e-2a4d-11ef-9a1b-00155d012345}.")
                .as_deref(),
            Some("{0b7f1c3e-2a4d-11ef-9a1b-00155d012345}")
        );
        assert_eq!(find_guid("{current} {not-a-guid}"), None);
    }

    #[test]
    fn missing_boot_manager_is_an_error() {
        assert!(BootConfig::from_entries(Vec::new()).is_err());
    }
}
