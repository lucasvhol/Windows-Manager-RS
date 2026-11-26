//! Startup Manager: programs launched at sign-in from the Run / RunOnce keys, the
//! Startup folders and Windows apps' startup tasks, plus a read-only view of the
//! advanced auto-start locations malware likes to use.
//!
//! Enable/disable uses the same `StartupApproved` flags as Task Manager (and the
//! `State` value Settings uses for apps), so it is non-destructive and stays in sync
//! with Windows' own UI.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui,
    fileinfo::{self, FileInfo, Signature},
    icons,
    process::exe_from_command,
    registry::{self, ValueSnapshot},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, frame, show_message},
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const RUNONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
const APP_TASKS: &str =
    r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData";
const POLICY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    UserRegistry,
    MachineRegistry,
    MachineRegistry32,
    UserRunOnce,
    MachineRunOnce,
    UserFolder,
    CommonFolder,
    WindowsApp,
}

impl Source {
    pub const ALL: [Source; 8] = [
        Source::UserRegistry,
        Source::MachineRegistry,
        Source::MachineRegistry32,
        Source::UserRunOnce,
        Source::MachineRunOnce,
        Source::UserFolder,
        Source::CommonFolder,
        Source::WindowsApp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Source::UserRegistry => "Registry (current user)",
            Source::MachineRegistry => "Registry (all users)",
            Source::MachineRegistry32 => "Registry (all users, 32-bit)",
            Source::UserRunOnce => "Run once (current user)",
            Source::MachineRunOnce => "Run once (all users)",
            Source::UserFolder => "Startup folder (current user)",
            Source::CommonFolder => "Startup folder (all users)",
            Source::WindowsApp => "Windows app",
        }
    }

    pub fn needs_admin(self) -> bool {
        !matches!(
            self,
            Source::UserRegistry | Source::UserRunOnce | Source::UserFolder | Source::WindowsApp
        )
    }

    /// Registry key holding the entries (registry sources only).
    fn run_key(self) -> Option<String> {
        match self {
            Source::UserRegistry => Some(format!(r"HKCU\{RUN}")),
            Source::MachineRegistry => Some(format!(r"HKLM\{RUN}")),
            Source::MachineRegistry32 => Some(format!(r"HKLM\{RUN32}")),
            Source::UserRunOnce => Some(format!(r"HKCU\{RUNONCE}")),
            Source::MachineRunOnce => Some(format!(r"HKLM\{RUNONCE}")),
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

    /// Key with the Task-Manager-compatible enabled/disabled flags (none for RunOnce / apps).
    fn approved_key(self) -> Option<String> {
        Some(match self {
            Source::UserRegistry => format!(r"HKCU\{APPROVED}\Run"),
            Source::MachineRegistry => format!(r"HKLM\{APPROVED}\Run"),
            Source::MachineRegistry32 => format!(r"HKLM\{APPROVED}\Run32"),
            Source::UserFolder => format!(r"HKCU\{APPROVED}\StartupFolder"),
            Source::CommonFolder => format!(r"HKLM\{APPROVED}\StartupFolder"),
            _ => return None,
        })
    }

    pub fn can_toggle(self) -> bool {
        !matches!(self, Source::UserRunOnce | Source::MachineRunOnce)
    }

    /// The other scope a registry entry can be moved to.
    fn move_target(self) -> Option<Source> {
        match self {
            Source::UserRegistry => Some(Source::MachineRegistry),
            Source::MachineRegistry | Source::MachineRegistry32 => Some(Source::UserRegistry),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartupItem {
    pub source: Source,
    /// Registry value name, file name in the Startup folder, or app package name.
    pub name: String,
    pub command: String,
    pub file: Option<PathBuf>,
    pub enabled: bool,
    /// For Windows apps: the key holding the task's `State`.
    pub state_key: Option<String>,
}

impl StartupItem {
    /// The program the entry starts, when it can be found on disk.
    pub fn program(&self) -> Option<PathBuf> {
        if self.source == Source::WindowsApp {
            return None;
        }
        let p = match &self.file {
            Some(f) if f.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) => {
                return Some(f.clone());
            }
            Some(f) => f.clone(),
            None => PathBuf::from(expand_env(&exe_from_command(&self.command)?)),
        };
        p.is_absolute().then_some(p).filter(|p| p.exists())
    }

    /// The entry's program is gone: a Startup-folder shortcut whose target is missing, or a
    /// command naming a missing program (see [`missing_program`]). Windows apps never are.
    pub fn is_invalid(&self) -> bool {
        match (&self.source, &self.file) {
            (Source::WindowsApp, _) => false,
            (_, Some(f)) if f.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) => {
                wm_core::shell::is_broken_shortcut(f)
            }
            // A Startup-folder file other than a shortcut is the program itself.
            (_, Some(_)) => false,
            (_, None) => missing_program(&self.command),
        }
    }
}

/// Whether a command line names a program that no longer exists. Deliberately
/// conservative, as the answer is used to delete entries: the program must be an
/// absolute path that was quoted or ends in `.exe` (so an unquoted path with spaces is
/// never cut short), on a drive that is present, and the file itself must be missing.
pub fn missing_program(command: &str) -> bool {
    let Some(exe) = exe_from_command(command) else {
        return false;
    };
    let quoted = command.trim_start().starts_with('"');
    if !quoted && !exe.to_ascii_lowercase().ends_with(".exe") {
        return false;
    }
    let p = PathBuf::from(expand_env(&exe));
    let drive_present = p.components().next().is_some_and(|root| {
        std::path::Path::new(&format!("{}\\", root.as_os_str().to_string_lossy())).exists()
    });
    p.is_absolute() && drive_present && !p.exists()
}

/// Removes a startup entry (journaled; see [`delete`]).
pub fn remove(ctx: &AppCtx<'_>, item: &StartupItem) -> Result<()> {
    delete(ctx, item)
}

/// Expands `%VAR%` references.
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let var = &after[..end];
                match std::env::var(var) {
                    Ok(v) if !var.is_empty() => out.push_str(&v),
                    _ => out.push_str(&rest[start..start + end + 2]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
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

fn approved(source: Source, name: &str) -> bool {
    let Some(key) = source.approved_key() else {
        return true;
    };
    let data = registry::read_raw(&key, name);
    approved_is_enabled(data.as_ref().map(|v| v.bytes.as_slice()))
}

/// App startup task state: 2 = enabled, 4 = enabled by policy; others are disabled.
pub fn app_state_enabled(state: u32) -> bool {
    matches!(state, 2 | 4)
}

fn app_tasks() -> Vec<StartupItem> {
    let mut out = Vec::new();
    let Ok(root) = registry::open_read(APP_TASKS) else {
        return out;
    };
    for pkg in root.enum_keys().flatten() {
        let Ok(pk) = root.open_subkey(&pkg) else { continue };
        for task in pk.enum_keys().flatten() {
            let key = format!(r"{APP_TASKS}\{pkg}\{task}");
            let Some(state) = registry::read_raw(&key, "State")
                .as_ref()
                .and_then(registry::decode_dword)
            else {
                continue;
            };
            let short = pkg.split('_').next().unwrap_or(&pkg).to_owned();
            out.push(StartupItem {
                source: Source::WindowsApp,
                name: short,
                command: format!("{pkg} ({task})"),
                file: None,
                enabled: app_state_enabled(state),
                state_key: Some(key),
            });
        }
    }
    out
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
                    state_key: None,
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
                    state_key: None,
                });
            }
        }
    }
    items.extend(app_tasks());
    items.sort_by_key(|i| i.name.to_lowercase());
    items
}

fn set_enabled(item: &StartupItem, enabled: bool) -> Result<ValueSnapshot> {
    if let Some(key) = &item.state_key {
        let snap = registry::snapshot(key, "State");
        registry::write_raw(key, "State", &registry::dword(if enabled { 2 } else { 1 }))?;
        return Ok(snap);
    }
    let Some(key) = item.source.approved_key() else {
        bail!("run-once entries can't be disabled, only removed")
    };
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
    let Some(key) = item.source.run_key() else {
        bail!("Windows apps can only be turned off, not removed")
    };
    let mut snaps = vec![registry::snapshot(&key, &item.name)];
    if let Some(ak) = item.source.approved_key() {
        snaps.push(registry::snapshot(&ak, &item.name));
    }
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

/// Moves a Run entry between the current-user and all-users keys (with its enabled flag).
fn move_entry(ctx: &AppCtx<'_>, item: &StartupItem, to: Source) -> Result<()> {
    let (Some(from_key), Some(to_key)) = (item.source.run_key(), to.run_key()) else {
        bail!("this entry can't be moved")
    };
    if registry::read_raw(&to_key, &item.name).is_some() {
        bail!("{} already has an entry named \"{}\"", to.label(), item.name);
    }
    let mut snaps = vec![
        registry::snapshot(&from_key, &item.name),
        registry::snapshot(&to_key, &item.name),
    ];
    let value = registry::read_raw(&from_key, &item.name)
        .ok_or_else(|| anyhow::anyhow!("the entry no longer exists"))?;
    registry::write_raw(&to_key, &item.name, &value)?;
    registry::delete_value(&from_key, &item.name)?;
    if let (Some(fa), Some(ta)) = (item.source.approved_key(), to.approved_key()) {
        snaps.push(registry::snapshot(&fa, &item.name));
        snaps.push(registry::snapshot(&ta, &item.name));
        registry::delete_value(&fa, &item.name)?;
        if !item.enabled {
            registry::write_raw(
                &ta,
                &item.name,
                &registry::binary(&approved_bytes(false, SystemTime::now())),
            )?;
        }
    }
    ctx.journal.record(
        "startup",
        format!("Moved startup item {} to {}", item.name, to.label()),
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

fn edit_command(ctx: &AppCtx<'_>, item: &StartupItem, command: &str) -> Result<()> {
    let Some(key) = item.source.run_key() else {
        bail!("only registry entries can be edited here")
    };
    let snap = registry::snapshot(&key, &item.name);
    registry::write_raw(&key, &item.name, &registry::string(command))?;
    ctx.journal.record(
        "startup",
        format!("Changed the command of startup item {}", item.name),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

fn open_location(item: &StartupItem) {
    if let Some(t) = item.program().or_else(|| item.file.clone()) {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", t.display()))
            .spawn();
    }
}

// --- Block all automatic startup (Explorer policies) ---------------------------

const BLOCK_VALUES: [&str; 2] = ["DisableCurrentUserRun", "DisableLocalMachineRun"];

fn block_key(machine: bool) -> String {
    format!(r"{}\{POLICY}", if machine { "HKLM" } else { "HKCU" })
}

pub fn auto_start_blocked(machine: bool) -> bool {
    BLOCK_VALUES.iter().all(|v| {
        registry::read_raw(&block_key(machine), v)
            .as_ref()
            .and_then(registry::decode_dword)
            == Some(1)
    })
}

fn set_auto_start_blocked(ctx: &AppCtx<'_>, machine: bool, block: bool) -> Result<()> {
    let key = block_key(machine);
    let names = [
        "DisableCurrentUserRun",
        "DisableCurrentUserRunOnce",
        "DisableLocalMachineRun",
        "DisableLocalMachineRunOnce",
    ];
    let snaps: Vec<ValueSnapshot> = names.iter().map(|n| registry::snapshot(&key, n)).collect();
    for n in names {
        if block {
            registry::write_raw(&key, n, &registry::dword(1))?;
        } else {
            registry::delete_value(&key, n)?;
        }
    }
    ctx.journal.record(
        "startup",
        format!(
            "{} automatic startup from the Run keys ({})",
            if block { "Blocked" } else { "Allowed" },
            if machine { "all users" } else { "current user" }
        ),
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

// --- Advanced locations (read-only) --------------------------------------------

#[derive(Debug, Clone)]
pub struct Advanced {
    pub location: String,
    pub name: String,
    pub value: String,
    /// True when the value differs from what a clean Windows has.
    pub unusual: bool,
    pub key: String,
}

fn winlogon_expected(name: &str, value: &str) -> bool {
    let v = value.trim().trim_end_matches(',').to_ascii_lowercase();
    match name {
        "Shell" => v == "explorer.exe",
        "Userinit" => v.ends_with(r"system32\userinit.exe"),
        _ => true,
    }
}

pub fn advanced_locations() -> Vec<Advanced> {
    let mut out = Vec::new();
    let winlogon = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon";
    for name in ["Shell", "Userinit"] {
        if let Some(v) = registry::read_raw(winlogon, name) {
            let value = registry::decode_string(&v);
            out.push(Advanced {
                location: "Winlogon".into(),
                name: name.into(),
                unusual: !winlogon_expected(name, &value),
                value,
                key: winlogon.into(),
            });
        }
    }
    for (key, label) in [
        (
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Windows",
            "AppInit DLLs",
        ),
        (
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows NT\CurrentVersion\Windows",
            "AppInit DLLs (32-bit)",
        ),
    ] {
        let dlls = registry::read_raw(key, "AppInit_DLLs")
            .map(|v| registry::decode_string(&v))
            .unwrap_or_default();
        let load = registry::read_raw(key, "LoadAppInit_DLLs")
            .as_ref()
            .and_then(registry::decode_dword)
            .unwrap_or(0);
        if !dlls.trim().is_empty() || load != 0 {
            out.push(Advanced {
                location: label.into(),
                name: "AppInit_DLLs".into(),
                value: format!("{dlls} (loading {})", if load != 0 { "on" } else { "off" }),
                unusual: load != 0 && !dlls.trim().is_empty(),
                key: key.into(),
            });
        }
    }
    for scope in ["HKCU", "HKLM"] {
        let key = format!(r"{scope}\{POLICY}\Run");
        if let Ok(k) = registry::open_read(&key) {
            for (name, value) in k.enum_values().flatten() {
                let raw = registry::RawValue {
                    vtype: value.vtype.clone() as u32,
                    bytes: value.bytes.into_owned(),
                };
                out.push(Advanced {
                    location: format!("Policy Run ({scope})"),
                    name,
                    value: registry::decode_string(&raw),
                    unusual: true,
                    key: key.clone(),
                });
            }
        }
    }
    let active = r"HKLM\SOFTWARE\Microsoft\Active Setup\Installed Components";
    if let Ok(k) = registry::open_read(active) {
        for id in k.enum_keys().flatten() {
            let sub = format!(r"{active}\{id}");
            let stub = registry::read_raw(&sub, "StubPath")
                .map(|v| registry::decode_string(&v))
                .unwrap_or_default();
            if stub.trim().is_empty() {
                continue;
            }
            let name = registry::read_raw(&sub, "")
                .map(|v| registry::decode_string(&v))
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            out.push(Advanced {
                location: "Active Setup".into(),
                name,
                value: stub,
                unusual: false,
                key: sub,
            });
        }
    }
    let ifeo = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";
    if let Ok(k) = registry::open_read(ifeo) {
        for exe in k.enum_keys().flatten() {
            let sub = format!(r"{ifeo}\{exe}");
            if let Some(d) = registry::read_raw(&sub, "Debugger") {
                out.push(Advanced {
                    location: "Image hijack (IFEO)".into(),
                    name: exe,
                    value: registry::decode_string(&d),
                    unusual: true,
                    key: sub,
                });
            }
        }
    }
    out
}

// --- Tool ------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Items,
    Advanced,
}

enum Ask {
    Remove(StartupItem),
    DisableAll(Vec<StartupItem>),
    Block(bool),
}

pub struct StartupManager {
    page: Page,
    /// Selected node of the location tree (index into `NAV`).
    node: usize,
    adding: bool,
    settings_open: bool,
    items: Option<Vec<StartupItem>>,
    advanced: Option<Vec<Advanced>>,
    info: HashMap<PathBuf, FileInfo>,
    info_task: Option<BackgroundTask<HashMap<PathBuf, FileInfo>>>,
    asked: HashSet<PathBuf>,
    running: HashSet<String>,
    selected: Option<(Source, String)>,
    editing: Option<String>,
    confirm: Confirm<Ask>,
    message: Message,
    new_name: String,
    new_command: String,
}

impl Default for StartupManager {
    fn default() -> Self {
        Self {
            page: Page::Items,
            node: 0,
            adding: false,
            settings_open: false,
            items: None,
            advanced: None,
            info: HashMap::new(),
            info_task: None,
            asked: HashSet::new(),
            running: HashSet::new(),
            selected: None,
            editing: None,
            confirm: Confirm::default(),
            message: None,
            new_name: String::new(),
            new_command: String::new(),
        }
    }
}

/// Lower-cased full paths of running programs.
fn running_programs() -> HashSet<String> {
    let Ok(mut sys) = sysinfo::System::new() else {
        return HashSet::new();
    };
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
    );
    sys.processes()
        .values()
        .filter_map(|p| p.exe())
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .collect()
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

    fn refresh(&mut self) {
        self.items = None;
        self.advanced = None;
        self.running = running_programs();
    }

    fn info_of(&self, item: &StartupItem) -> Option<&FileInfo> {
        item.program().and_then(|p| self.info.get(&p))
    }

    fn request_info(&mut self, ctx: &egui::Context, items: &[StartupItem]) {
        if let Some(t) = &mut self.info_task {
            t.poll();
            if let Some(r) = t.take_result() {
                if let Ok(m) = r {
                    self.info.extend(m);
                }
                self.info_task = None;
            }
            return;
        }
        let new: Vec<PathBuf> = items
            .iter()
            .filter_map(StartupItem::program)
            .filter(|p| !self.asked.contains(p))
            .collect();
        if !new.is_empty() {
            self.asked.extend(new.iter().cloned());
            self.info_task = Some(BackgroundTask::spawn(ctx, move |_| fileinfo::describe(&new)));
        }
    }

    fn is_microsoft(&self, item: &StartupItem) -> bool {
        self.info_of(item)
            .is_some_and(|i| i.signature == Signature::Valid && i.signer.contains("Microsoft"))
    }
}

/// What a node of the location tree shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Sources(&'static [Source]),
    Advanced,
    Open(&'static str),
}

const ALL_SOURCES: &[Source] = &Source::ALL;

/// The original's location tree: (depth, icon, label, target).
const NAV: [(usize, &str, &str, Target); 17] = [
    (0, icons::FOLDER_OPEN, "General", Target::Sources(ALL_SOURCES)),
    (
        1,
        icons::FOLDER_OPEN,
        "Startup Folder",
        Target::Sources(&[Source::CommonFolder, Source::UserFolder]),
    ),
    (
        2,
        icons::USER,
        "All Users",
        Target::Sources(&[Source::CommonFolder]),
    ),
    (
        2,
        icons::USER,
        "Current User",
        Target::Sources(&[Source::UserFolder]),
    ),
    (
        1,
        icons::DATABASE,
        "Registry",
        Target::Sources(&[
            Source::MachineRegistry,
            Source::MachineRegistry32,
            Source::MachineRunOnce,
            Source::UserRegistry,
            Source::UserRunOnce,
        ]),
    ),
    (
        2,
        icons::USER,
        "All Users",
        Target::Sources(&[
            Source::MachineRegistry,
            Source::MachineRegistry32,
            Source::MachineRunOnce,
        ]),
    ),
    (
        3,
        icons::FOLDERS,
        "Run",
        Target::Sources(&[Source::MachineRegistry]),
    ),
    (
        3,
        icons::FOLDERS,
        "Run Once",
        Target::Sources(&[Source::MachineRunOnce]),
    ),
    (
        3,
        icons::FOLDERS,
        "Run (Wow64)",
        Target::Sources(&[Source::MachineRegistry32]),
    ),
    (
        2,
        icons::USER,
        "Current User",
        Target::Sources(&[Source::UserRegistry, Source::UserRunOnce]),
    ),
    (3, icons::FOLDERS, "Run", Target::Sources(&[Source::UserRegistry])),
    (
        3,
        icons::FOLDERS,
        "Run Once",
        Target::Sources(&[Source::UserRunOnce]),
    ),
    (
        1,
        icons::GRID_FOUR,
        "Windows Apps",
        Target::Sources(&[Source::WindowsApp]),
    ),
    (0, icons::FOLDER_OPEN, "Other", Target::Advanced),
    (1, icons::PLAY, "Advanced Startup", Target::Advanced),
    (1, icons::PLAY, "Service Manager", Target::Open("services")),
    (1, icons::PLAY, "Task Scheduler Manager", Target::Open("tasks")),
];

/// Runs a startup entry's command now, as Windows would at sign-in.
fn start_now(item: &StartupItem) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    if let Some(f) = &item.file {
        wm_core::shell::shell_open(&f.display().to_string(), "")?;
        return Ok(());
    }
    if item.source == Source::WindowsApp {
        bail!("Windows apps start from the Start menu");
    }
    std::process::Command::new("cmd")
        .arg("/c")
        .raw_arg(format!("start \"\" {}", item.command))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()?;
    Ok(())
}

impl StartupManager {
    fn items_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>, all: &[StartupItem]) {
        let sources = match NAV[self.node].3 {
            Target::Sources(s) => s,
            _ => ALL_SOURCES,
        };
        let items: Vec<StartupItem> = all
            .iter()
            .filter(|i| sources.contains(&i.source))
            .cloned()
            .collect();
        let selected = self
            .selected
            .as_ref()
            .and_then(|(s, n)| items.iter().find(|i| i.source == *s && &i.name == n))
            .cloned();
        let locked = selected
            .as_ref()
            .is_some_and(|i| i.source.needs_admin() && !ctx.elevated);
        let has = selected.is_some() && !locked;
        let can_move = selected
            .as_ref()
            .and_then(|i| i.source.move_target())
            .is_some_and(|t| !t.needs_admin() || ctx.elevated);
        let disabled = selected.as_ref().is_some_and(|i| !i.enabled);
        let can_toggle = has && selected.as_ref().is_some_and(|i| i.source.can_toggle());
        let buttons = [
            Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            None,
            Some(
                ToolButton::new(icons::PLAY, "Start")
                    .enabled(selected.as_ref().is_some_and(|i| i.source != Source::WindowsApp)),
            ),
            Some(
                ToolButton::new(
                    if disabled {
                        icons::CHECK
                    } else {
                        icons::STOP_CIRCLE
                    },
                    if disabled { "Enable" } else { "Disable" },
                )
                .enabled(can_toggle),
            ),
            Some(ToolButton::new(icons::LIST_CHECKS, "Batch")),
            None,
            Some(ToolButton::new(icons::PLUS, "Add")),
            Some(
                ToolButton::new(icons::X, "Delete")
                    .enabled(has && selected.as_ref().is_some_and(|i| i.source != Source::WindowsApp)),
            ),
            Some(ToolButton::new(icons::SWAP, "Move to").enabled(has && can_move)),
            None,
            Some(
                ToolButton::new(icons::FOLDER_OPEN, "Open File Location").enabled(
                    selected
                        .as_ref()
                        .is_some_and(|i| i.program().is_some() || i.file.is_some()),
                ),
            ),
            Some(
                ToolButton::new(icons::PENCIL_SIMPLE, "Edit")
                    .enabled(has && selected.as_ref().is_some_and(|i| i.source.run_key().is_some())),
            ),
            Some(ToolButton::new(icons::GLOBE, "Query").enabled(selected.is_some())),
            None,
            Some(ToolButton::new(icons::GEAR, "Settings")),
            Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
        ];
        match frame::toolbar(ui, &buttons, |_| {}) {
            Some(0) => {
                self.refresh();
                self.message = None;
            }
            Some(2) => {
                if let Some(item) = &selected {
                    self.message = Some(
                        start_now(item)
                            .map(|()| format!("Started {}.", item.name))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(3) => {
                if let Some(item) = &selected {
                    self.toggle(ctx, item, !item.enabled);
                }
            }
            Some(4) => {
                let targets: Vec<StartupItem> = all
                    .iter()
                    .filter(|i| {
                        i.enabled && i.source.can_toggle() && (!i.source.needs_admin() || ctx.elevated)
                    })
                    .filter(|i| !self.is_microsoft(i))
                    .cloned()
                    .collect();
                if targets.is_empty() {
                    self.message = Some(Ok(
                        "Nothing to disable: every enabled item is signed by Microsoft.".into(),
                    ));
                } else {
                    let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
                    self.confirm.ask(
                        format!(
                            "Batch: disable these {} startup items that are not signed by Microsoft?\n\n{}",
                            targets.len(),
                            names.join("\n")
                        ),
                        Ask::DisableAll(targets),
                    );
                }
            }
            Some(6) => self.adding = true,
            Some(7) => {
                if let Some(item) = &selected {
                    self.confirm.ask(
                        format!(
                            "Delete \"{}\" from startup? Disabling it is usually enough. Deleting can be undone from the Change Journal.",
                            item.name
                        ),
                        Ask::Remove(item.clone()),
                    );
                }
            }
            Some(8) => {
                if let Some(item) = &selected
                    && let Some(to) = item.source.move_target()
                {
                    self.message = Some(if ctx.config.dry_run {
                        Ok(ctx.i18n.tr("common-dry-run-skip"))
                    } else {
                        move_entry(ctx, item, to)
                            .map(|()| format!("Moved {} to {}.", item.name, to.label()))
                            .map_err(|e| format!("{e:#}"))
                    });
                    self.selected = Some((to, item.name.clone()));
                    self.items = None;
                }
            }
            Some(10) => {
                if let Some(item) = &selected {
                    open_location(item);
                }
            }
            Some(11) => self.editing = selected.as_ref().map(|i| i.command.clone()),
            Some(12) => {
                if let Some(item) = &selected {
                    let q: String = item
                        .name
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '+' })
                        .collect();
                    let _ = std::process::Command::new("explorer")
                        .arg(format!("https://www.bing.com/search?q={q}+startup"))
                        .spawn();
                }
            }
            Some(14) => self.settings_open = true,
            Some(15) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
        self.dialogs(ui, ctx, selected.as_ref());

        let checking = self.info_task.is_some();
        frame::status(ui, &format!("Total: {}", items.len()), |ui| {
            if checking {
                ui.weak("checking signatures…");
                ui.spinner();
            }
        });
        // The original's details pane under the list.
        if let Some(item) = &selected {
            let info = self.info_of(item).cloned().unwrap_or_default();
            egui::Panel::bottom("startup-details")
                .resizable(true)
                .default_size(120.0)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)))
                .show(ui, |ui| {
                    egui::Grid::new("startup-detail-grid")
                        .num_columns(2)
                        .spacing([16.0, 3.0])
                        .show(ui, |ui| {
                            for (k, v) in [
                                ("Name", item.name.clone()),
                                ("Location", item.source.label().to_owned()),
                                ("Command", item.command.clone()),
                                ("Description", info.description.clone()),
                                ("Company", info.company.clone()),
                                ("Signed by", info.signer.clone()),
                            ] {
                                if !v.is_empty() {
                                    ui.weak(k);
                                    ui.add(egui::Label::new(v).selectable(true));
                                    ui.end_row();
                                }
                            }
                        });
                });
        }
        frame::central_fixed(ui, |ui| self.items_table(ui, ctx, &items));
    }

    fn toggle(&mut self, ctx: &AppCtx<'_>, item: &StartupItem, enabled: bool) {
        let verb = if enabled { "Enabled" } else { "Disabled" };
        self.message = Some(if ctx.config.dry_run {
            Ok(ctx.i18n.tr("common-dry-run-skip"))
        } else {
            set_enabled(item, enabled)
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

    /// Add, Edit and Settings dialogs.
    fn dialogs(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>, selected: Option<&StartupItem>) {
        if self.adding {
            let mut add = false;
            let modal = egui::Modal::new(egui::Id::unique("startup-add")).show(ui.ctx(), |ui| {
                ui.set_min_width(460.0);
                ui.strong("Add a startup item for the current user");
                ui.add_space(6.0);
                egui::Grid::new("startup-add-grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(320.0));
                        ui.end_row();
                        ui.label("Command");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.new_command)
                                    .hint_text(r#""C:\Path\App.exe" --args"#)
                                    .desired_width(240.0),
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
                        });
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    add = ui.button("Add").clicked();
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        self.adding = false;
                    }
                });
            });
            if modal.should_close() {
                self.adding = false;
            }
            if add {
                self.add_entry(ctx);
                if self.message.as_ref().is_some_and(|m| m.is_ok()) {
                    self.adding = false;
                }
            }
        }
        if let (Some(item), Some(cmd)) = (selected, &mut self.editing) {
            let (mut save, mut cancel) = (false, false);
            let modal = egui::Modal::new(egui::Id::unique("startup-edit")).show(ui.ctx(), |ui| {
                ui.set_min_width(520.0);
                ui.strong(format!("Edit {}", item.name));
                ui.add_space(6.0);
                ui.add(egui::TextEdit::singleline(cmd).desired_width(500.0));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    save = ui.button(format!("{}  Save", icons::FLOPPY_DISK)).clicked();
                    cancel = ui.button(ctx.i18n.tr("common-cancel")).clicked();
                });
            });
            if save {
                let cmd = cmd.clone();
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    edit_command(ctx, item, &cmd)
                        .map(|()| format!("Saved {}.", item.name))
                        .map_err(|e| format!("{e:#}"))
                });
                self.editing = None;
                self.items = None;
            } else if cancel || modal.should_close() {
                self.editing = None;
            }
        }
        if self.settings_open {
            let modal = egui::Modal::new(egui::Id::unique("startup-settings")).show(ui.ctx(), |ui| {
                ui.set_min_width(420.0);
                ui.strong("Settings");
                ui.add_space(6.0);
                for machine in [false, true] {
                    let mut blocked = auto_start_blocked(machine);
                    let label = if machine {
                        "Block the Run keys for all users"
                    } else {
                        "Block the Run keys for me"
                    };
                    let r = ui.add_enabled(!machine || ctx.elevated, egui::Checkbox::new(&mut blocked, label));
                    if r.changed() {
                        if blocked {
                            self.confirm.ask(
                                "Stop Windows starting every program in the Run and RunOnce keys at sign-in? \
                                 Startup-folder shortcuts and services are not affected. Undo it here or in the Change Journal.",
                                Ask::Block(machine),
                            );
                        } else {
                            self.message = Some(
                                set_auto_start_blocked(ctx, machine, false)
                                    .map(|()| "Automatic startup is allowed again.".into())
                                    .map_err(|e| format!("{e:#}")),
                            );
                        }
                    }
                }
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.settings_open = false;
                }
            });
            if modal.should_close() {
                self.settings_open = false;
            }
        }
    }

    fn items_table(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>, items: &[StartupItem]) {
        show_message(ui, &self.message);
        let mut toggle: Option<(StartupItem, bool)> = None;
        let mut clicked = None;
        let warn = ui.visuals().warn_fg_color;
        let good = egui::Color32::from_rgb(90, 180, 110);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(220.0).clip(true))
            .column(Column::initial(260.0).clip(true))
            .column(Column::initial(75.0))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["Name", "Path", "Available", "Running", "Digital Signature"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, items.len(), |mut row| {
                    let item = &items[row.index()];
                    let info = self.info_of(item);
                    let is_sel = self
                        .selected
                        .as_ref()
                        .is_some_and(|(s, n)| *s == item.source && n == &item.name);
                    row.set_selected(is_sel);
                    let locked = item.source.needs_admin() && !ctx.elevated;
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            let mut on = item.enabled;
                            let r = ui.add_enabled(
                                !locked && item.source.can_toggle(),
                                egui::Checkbox::without_text(&mut on),
                            );
                            if r.changed() {
                                toggle = Some((item.clone(), on));
                            }
                            if locked {
                                r.on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                            }
                            ui.label(&item.name);
                        });
                    });
                    row.col(|ui| {
                        ui.label(&item.command).on_hover_text(&item.command);
                    });
                    row.col(|ui| {
                        if item.is_invalid() {
                            ui.colored_label(warn, "No");
                        } else {
                            ui.label("Yes");
                        }
                    });
                    row.col(|ui| {
                        let running = item.program().is_some_and(|p| {
                            self.running.contains(&p.to_string_lossy().to_ascii_lowercase())
                        });
                        if running {
                            ui.colored_label(good, "Yes");
                        } else {
                            ui.weak("No");
                        }
                    });
                    row.col(|ui| {
                        let s = info.map(|i| i.signature).unwrap_or_default();
                        let text = match (s, info) {
                            (Signature::Valid, Some(i)) if !i.signer.is_empty() => {
                                format!("{} (Trusted)", i.signer)
                            }
                            _ => s.label().to_owned(),
                        };
                        if matches!(s, Signature::Unsigned | Signature::Untrusted) {
                            ui.colored_label(warn, text);
                        } else {
                            ui.label(text);
                        }
                    });
                    if row.response().clicked() {
                        clicked = Some((item.source, item.name.clone()));
                    }
                });
            });
        if clicked.is_some() && clicked != self.selected {
            self.selected = clicked;
            self.editing = None;
        }
        if let Some((item, enabled)) = toggle {
            self.toggle(ctx, &item, enabled);
        }
    }

    fn advanced_page(&mut self, ui: &mut egui::Ui) {
        if frame::toolbar(
            ui,
            &[Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh"))],
            |_| {},
        ) == Some(0)
        {
            self.advanced = None;
        }
        let list = self.advanced.get_or_insert_with(advanced_locations).clone();
        let unusual = list.iter().filter(|a| a.unusual).count();
        frame::status(
            ui,
            &format!(
                "{} entries, {unusual} unusual · read-only: open an entry in Registry Editor to change it",
                list.len()
            ),
            |_| {},
        );
        frame::central(ui, |ui| self.advanced_grid(ui, &list));
    }

    fn advanced_grid(&mut self, ui: &mut egui::Ui, list: &[Advanced]) {
        ui.weak(
            "Other places Windows starts programs from. Unusual entries are highlighted: they are often \
             left by malware or old software.",
        );
        ui.add_space(6.0);
        let warn = ui.visuals().warn_fg_color;
        egui::Grid::new("advanced")
            .striped(true)
            .spacing([14.0, 4.0])
            .show(ui, |ui| {
                for h in ["Location", "Name", "Value", ""] {
                    ui.strong(h);
                }
                ui.end_row();
                for a in list {
                    if a.unusual {
                        ui.colored_label(warn, format!("{}  {}", icons::WARNING, a.location));
                    } else {
                        ui.label(&a.location);
                    }
                    ui.label(&a.name);
                    ui.label(&a.value);
                    if ui.small_button("Registry Editor").clicked() {
                        self.message = registry::open_in_regedit(&a.key)
                            .err()
                            .map(|e| Err(format!("{e:#}")));
                    }
                    ui.end_row();
                }
            });
        show_message(ui, &self.message);
    }
}

impl Tool for StartupManager {
    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn id(&self) -> &'static str {
        "startup"
    }

    fn category(&self) -> Category {
        Category::Optimizer
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.items.is_none() && self.running.is_empty() {
            self.running = running_programs();
        }
        let items = self.items.get_or_insert_with(enumerate).clone();
        self.request_info(ui.ctx(), &items);

        match self.confirm.show(ui.ctx(), ctx.i18n) {
            Some(Ask::Remove(item)) => {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    delete(ctx, &item)
                        .map(|()| format!("Removed {}.", item.name))
                        .map_err(|e| format!("{e:#}"))
                });
                self.items = None;
            }
            Some(Ask::DisableAll(list)) => {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    let mut snaps = Vec::new();
                    let mut errors = Vec::new();
                    for i in &list {
                        match set_enabled(i, false) {
                            Ok(s) => snaps.push(s),
                            Err(e) => errors.push(format!("{}: {e:#}", i.name)),
                        }
                    }
                    let n = snaps.len();
                    let _ = ctx.journal.record(
                        "startup",
                        format!("Disabled {n} startup items not from Microsoft"),
                        UndoAction::RestoreRegValues { values: snaps },
                    );
                    if errors.is_empty() {
                        Ok(format!("Disabled {n} items."))
                    } else {
                        Err(errors.join("; "))
                    }
                });
                self.items = None;
            }
            Some(Ask::Block(machine)) => {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    set_auto_start_blocked(ctx, machine, true)
                        .map(|()| "Programs in the Run keys no longer start automatically.".into())
                        .map_err(|e| format!("{e:#}"))
                });
            }
            None => {}
        }

        // The original's location tree.
        let mut picked = None;
        frame::left(ui, 210.0, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, (depth, icon, label, _)) in NAV.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_space(*depth as f32 * 14.0);
                    let text = egui::RichText::new(format!("{icon}  {label}"));
                    let text = if *depth == 0 { text.strong() } else { text };
                    if ui.selectable_label(self.node == i, text).clicked() {
                        picked = Some(i);
                    }
                });
            }
        });
        if let Some(i) = picked {
            match NAV[i].3 {
                Target::Open(tool) => wm_core::ui::request_open_tool(ui.ctx(), tool),
                Target::Advanced => {
                    self.node = i;
                    self.page = Page::Advanced;
                }
                Target::Sources(_) => {
                    self.node = i;
                    self.page = Page::Items;
                }
            }
        }
        match self.page {
            Page::Items => self.items_page(ui, ctx, &items),
            Page::Advanced => self.advanced_page(ui),
        }
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
    fn missing_programs_are_detected_conservatively() {
        assert!(missing_program(
            r#""C:\definitely-missing-wm-test\a b\app.exe" /min"#
        ));
        assert!(missing_program(r"C:\definitely-missing-wm-test\app.exe /min"));
        assert!(!missing_program(r"%SystemRoot%\System32\cmd.exe /c echo"));
        // Unquoted, no .exe: the program can't be told apart from its arguments.
        assert!(!missing_program(r"C:\definitely-missing-wm-test\run.bat now"));
        // Not an absolute path: found through PATH, never flagged.
        assert!(!missing_program("ctfmon.exe"));
        // A drive that isn't there (unplugged) is not a missing program.
        assert!(!missing_program(r"Q:\gone\app.exe"));
        assert!(!missing_program(""));
    }

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
        assert!(
            app_state_enabled(2) && app_state_enabled(4) && !app_state_enabled(1) && !app_state_enabled(0)
        );
    }

    #[test]
    fn command_parsing_and_env() {
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
        let windir = std::env::var("WINDIR").unwrap();
        assert_eq!(expand_env(r"%WINDIR%\x.exe"), format!(r"{windir}\x.exe"));
        assert_eq!(expand_env("50% off"), "50% off");
        assert_eq!(expand_env("%NOPE_NOT_SET%"), "%NOPE_NOT_SET%");
    }

    #[test]
    fn winlogon_checks() {
        assert!(winlogon_expected("Shell", "explorer.exe"));
        assert!(!winlogon_expected("Shell", "explorer.exe, evil.exe"));
        assert!(winlogon_expected(
            "Userinit",
            r"C:\WINDOWS\system32\userinit.exe,"
        ));
        assert!(!winlogon_expected(
            "Userinit",
            r"C:\WINDOWS\system32\userinit.exe,C:\x\bad.exe,"
        ));
    }

    #[test]
    fn enumerate_and_advanced_do_not_fail() {
        let items = enumerate();
        assert!(items.iter().all(|i| !i.name.is_empty()));
        let adv = advanced_locations();
        assert!(adv.iter().any(|a| a.name == "Shell"), "{adv:?}");
    }

    #[test]
    fn disable_and_restore_via_snapshot() {
        let item = StartupItem {
            source: Source::UserRegistry,
            name: format!("wm-test-{}", std::process::id()),
            command: "x".into(),
            file: None,
            enabled: true,
            state_key: None,
        };
        let snap = set_enabled(&item, false).unwrap();
        assert!(!approved(item.source, &item.name));
        registry::restore(&snap).unwrap();
        assert!(approved(item.source, &item.name));
        assert_eq!(
            registry::read_raw(&item.source.approved_key().unwrap(), &item.name),
            None
        );
    }
}
