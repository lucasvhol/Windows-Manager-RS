//! Win+X Menu Editor: the Win+X (Start button right-click) menu, stored as shortcuts
//! in `%LOCALAPPDATA%\Microsoft\Windows\WinX\Group1..N`.
//!
//! Hide or show entries, add programs, files, Start Menu items and Windows tools,
//! rename and reorder entries, add and remove groups, and restore Windows' defaults.
//!
//! Windows only shows shortcuts that carry a hash in their property store: the
//! `HashData` of lowercase(target, with the System32 / Windows folder replaced by its
//! known-folder id, + arguments + a fixed phrase). New entries therefore launch
//! through `explorer.exe` or PowerShell (both in the Windows folder, where the
//! scheme is verified against Windows' own shortcuts by a test). Every change is
//! journaled.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
    STGM_READ, StructuredStorage::PROPVARIANT,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{HashData, IShellLinkW, ShellLink};
use windows::core::{GUID, HSTRING, Interface};
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    icons, paths,
    process::restart_explorer,
    safety::UndoAction,
    ui::{Confirm, Message, ToolButton, frame, show_message},
};

const TOOL: &str = "winx";
const HASH_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xFB8D2D7B_90D1_4E34_BF60_6EAC09922BBF),
    pid: 2,
};
const SALT: &str = "do not prehash links.  this should only be done by the user.";

/// Windows tools offered as ready-made entries: (label, program in the Windows
/// folder, arguments).
pub const UTILITIES: &[(&str, &str, &str)] = &[
    ("Command Prompt", r"%SystemRoot%\System32\cmd.exe", ""),
    ("Registry Editor", r"%SystemRoot%\regedit.exe", ""),
    ("Services", r"%SystemRoot%\System32\services.msc", ""),
    ("Control Panel", r"%SystemRoot%\System32\control.exe", ""),
    ("Group Policy Editor", r"%SystemRoot%\System32\gpedit.msc", ""),
    ("Task Scheduler", r"%SystemRoot%\System32\taskschd.msc", ""),
    ("System Configuration", r"%SystemRoot%\System32\msconfig.exe", ""),
    ("System Information", r"%SystemRoot%\System32\msinfo32.exe", ""),
    ("Resource Monitor", r"%SystemRoot%\System32\resmon.exe", ""),
    ("Performance Monitor", r"%SystemRoot%\System32\perfmon.exe", ""),
    ("Disk Cleanup", r"%SystemRoot%\System32\cleanmgr.exe", ""),
    ("Optimize Drives", r"%SystemRoot%\System32\dfrgui.exe", ""),
    (
        "Network Connections",
        r"%SystemRoot%\System32\control.exe",
        "ncpa.cpl",
    ),
    (
        "Programs and Features",
        r"%SystemRoot%\System32\control.exe",
        "appwiz.cpl",
    ),
    (
        "System Properties",
        r"%SystemRoot%\System32\SystemPropertiesAdvanced.exe",
        "",
    ),
    (
        "Environment Variables",
        r"%SystemRoot%\System32\rundll32.exe",
        "sysdm.cpl,EditEnvironmentVariables",
    ),
    ("Notepad", r"%SystemRoot%\System32\notepad.exe", ""),
    ("Calculator", r"%SystemRoot%\System32\calc.exe", ""),
    (
        "Windows Security",
        r"%SystemRoot%\explorer.exe",
        "windowsdefender:",
    ),
    ("Settings", r"%SystemRoot%\explorer.exe", "ms-settings:"),
    (
        "Restart Explorer",
        r"%SystemRoot%\System32\cmd.exe",
        "/c taskkill /f /im explorer.exe & start explorer.exe",
    ),
    (
        "Lock",
        r"%SystemRoot%\System32\rundll32.exe",
        "user32.dll,LockWorkStation",
    ),
];

#[derive(Debug, Clone, PartialEq)]
pub struct WinXItem {
    pub group: String,
    pub path: PathBuf,
    pub label: String,
    pub target: String,
    pub args: String,
    pub hidden: bool,
    /// The stored hash matches (or the entry is Windows' own non-file item).
    pub signed: bool,
}

pub fn winx_dir() -> PathBuf {
    PathBuf::from(paths::expand_env(r"%LOCALAPPDATA%\Microsoft\Windows\WinX"))
}

fn hidden_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("winx-hidden"))
}

fn default_dir() -> PathBuf {
    PathBuf::from(paths::expand_env(
        r"%SystemDrive%\Users\Default\AppData\Local\Microsoft\Windows\WinX",
    ))
}

/// "3 - Windows Explorer.lnk" → ("3", "Windows Explorer").
pub fn split_name(file: &Path) -> (Option<String>, String) {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.split_once(" - ") {
        // Ordering prefixes look like "3", "01a" or "04-1".
        Some((n, rest)) if n.starts_with(|c: char| c.is_ascii_digit()) && !n.contains(' ') => {
            (Some(n.to_owned()), rest.to_owned())
        }
        _ => (None, stem),
    }
}

pub fn label_of(file: &Path) -> String {
    split_name(file).1
}

/// The text Windows hashes: target with System32 / Windows replaced by their
/// known-folder ids, then arguments and the fixed phrase, all lowercase.
pub fn hash_input(target: &str, args: &str) -> String {
    let t = paths::expand_env(target).to_lowercase();
    let sys = paths::expand_env(r"%SystemRoot%\System32").to_lowercase();
    let win = paths::expand_env("%SystemRoot%").to_lowercase();
    let general = if let Some(r) = t.strip_prefix(&sys) {
        format!("{{1ac14e77-02e7-4e5d-b744-2eb1ae5198b7}}{r}")
    } else if let Some(r) = t.strip_prefix(&win) {
        format!("{{f38bf404-1d43-42f2-9305-67de0b28fc23}}{r}")
    } else {
        t
    };
    format!("{general}{args}{SALT}").to_lowercase()
}

pub fn link_hash(target: &str, args: &str) -> u32 {
    let bytes: Vec<u8> = hash_input(target, args)
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut out = [0u8; 4];
    let _ = unsafe { HashData(&bytes, &mut out) };
    u32::from_le_bytes(out)
}

fn text(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Target (expanded), arguments and stored hash of a shortcut.
pub fn read_link(lnk: &Path) -> Result<(String, String, Option<u32>)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.cast::<IPersistFile>()?
            .Load(&HSTRING::from(lnk.as_os_str()), STGM_READ)?;
        let mut path = [0u16; 1024];
        let _ = link.GetPath(&mut path, std::ptr::null_mut(), 0);
        let mut args = [0u16; 2048];
        let _ = link.GetArguments(&mut args);
        let store: IPropertyStore = link.cast()?;
        let stored = store
            .GetValue(&HASH_KEY)
            .ok()
            .and_then(|v| u32::try_from(&v).ok());
        Ok((text(&path), text(&args), stored))
    }
}

/// Writes a shortcut Windows will show in the Win+X menu.
pub fn write_link(lnk: &Path, target: &str, args: &str, description: &str) -> Result<()> {
    let target = paths::expand_env(target);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_str()))?;
        if !args.is_empty() {
            link.SetArguments(&HSTRING::from(args))?;
        }
        if !description.is_empty() {
            link.SetDescription(&HSTRING::from(description))?;
        }
        let store: IPropertyStore = link.cast()?;
        store.SetValue(&HASH_KEY, &PROPVARIANT::from(link_hash(&target, args)))?;
        store.Commit()?;
        link.cast::<IPersistFile>()?
            .Save(&HSTRING::from(lnk.as_os_str()), true)?;
    }
    Ok(())
}

/// What a new entry opens.
#[derive(Debug, Clone, PartialEq)]
pub enum NewEntry {
    /// A program, document or folder, optionally with arguments or elevated.
    Path { path: String, args: String, admin: bool },
    /// One of [`UTILITIES`].
    Utility(usize),
}

/// (target, arguments) for an entry, using only launchers in the Windows folder.
pub fn launcher(entry: &NewEntry) -> (String, String) {
    let ps = r"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe".to_owned();
    match entry {
        NewEntry::Utility(i) => {
            let (_, exe, args) = UTILITIES[*i];
            (exe.to_owned(), args.to_owned())
        }
        NewEntry::Path { path, args, admin } => {
            let p = path.trim().trim_matches('"').replace('\'', "''");
            if *admin || !args.trim().is_empty() {
                let list = if args.trim().is_empty() {
                    String::new()
                } else {
                    format!(" -ArgumentList '{}'", args.trim().replace('\'', "''"))
                };
                let verb = if *admin { " -Verb RunAs" } else { "" };
                (
                    ps,
                    format!(
                        "-NoProfile -WindowStyle Hidden -Command \"Start-Process -FilePath '{p}'{list}{verb}\""
                    ),
                )
            } else {
                (
                    r"%SystemRoot%\explorer.exe".to_owned(),
                    format!("\"{}\"", path.trim().trim_matches('"')),
                )
            }
        }
    }
}

fn list(root: &Path, hidden: bool) -> Vec<WinXItem> {
    let mut items = Vec::new();
    let Ok(groups) = std::fs::read_dir(root) else {
        return items;
    };
    for g in groups.flatten().filter(|g| g.path().is_dir()) {
        let group = g.file_name().to_string_lossy().into_owned();
        let Ok(files) = std::fs::read_dir(g.path()) else {
            continue;
        };
        for f in files
            .flatten()
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")))
        {
            let (target, args, stored) = read_link(&f).unwrap_or_default();
            let signed = match stored {
                None => false,
                // Non-file entries (Settings pages, shell items): Windows' own.
                Some(_) if target.is_empty() => true,
                Some(h) => h == link_hash(&target, &args),
            };
            items.push(WinXItem {
                group: group.clone(),
                label: label_of(&f),
                path: f,
                target,
                args,
                hidden,
                signed,
            });
        }
    }
    items
}

fn group_number(g: &str) -> u32 {
    g.trim_start_matches("Group").parse().unwrap_or(0)
}

pub fn enumerate() -> Vec<WinXItem> {
    let mut items = list(&winx_dir(), false);
    if let Ok(h) = hidden_dir() {
        items.extend(list(&h, true));
    }
    // Windows shows the highest group at the top of the menu, highest number first.
    items.sort_by(|a, b| {
        group_number(&b.group)
            .cmp(&group_number(&a.group))
            .then(b.path.file_name().cmp(&a.path.file_name()))
    });
    items
}

pub fn groups() -> Vec<String> {
    let mut g: Vec<String> = std::fs::read_dir(winx_dir())
        .map(|r| {
            r.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    g.sort_by_key(|n| std::cmp::Reverse(group_number(n)));
    g
}

/// Next free two-digit ordering prefix in a group folder (shown at the top).
fn next_prefix(dir: &Path) -> String {
    let max = std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .filter_map(|e| split_name(&e.path()).0)
                .filter_map(|p| {
                    p.chars()
                        .take_while(char::is_ascii_digit)
                        .collect::<String>()
                        .parse::<u32>()
                        .ok()
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    format!("{:02}", max + 1)
}

fn safe_label(label: &str) -> Result<String> {
    let l = label.trim();
    if l.is_empty() || l.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        bail!("\"{l}\" cannot be used as a name");
    }
    Ok(l.to_owned())
}

pub fn add_entry(ctx: &AppCtx<'_>, group: &str, label: &str, entry: &NewEntry) -> Result<PathBuf> {
    let label = safe_label(label)?;
    let dir = winx_dir().join(group);
    std::fs::create_dir_all(&dir)?;
    let lnk = dir.join(format!("{} - {label}.lnk", next_prefix(&dir)));
    let (target, args) = launcher(entry);
    write_link(&lnk, &target, &args, &label)?;
    ctx.journal.record(
        TOOL,
        format!("Added \"{label}\" to the Win+X menu"),
        UndoAction::DeleteCreatedFiles {
            paths: vec![lnk.clone()],
        },
    )?;
    Ok(lnk)
}

fn journal_moves(ctx: &AppCtx<'_>, what: String, moves: Vec<(PathBuf, PathBuf)>) -> Result<()> {
    // Undo walks the moves in reverse.
    let steps = moves
        .into_iter()
        .rev()
        .map(|m| UndoAction::MoveBack { moves: vec![m] })
        .collect();
    ctx.journal.record(TOOL, what, UndoAction::Sequence { steps })?;
    Ok(())
}

fn rename_file(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    std::fs::rename(from, to).with_context(|| format!("renaming {}", from.display()))
}

/// Moves an item between the WinX folder and the hidden folder.
pub fn set_shown(ctx: &AppCtx<'_>, item: &WinXItem, show: bool) -> Result<()> {
    let base = if show { winx_dir() } else { hidden_dir()? };
    let dest_dir = base.join(&item.group);
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(item.path.file_name().unwrap_or_default());
    rename_file(&item.path, &dest)?;
    journal_moves(
        ctx,
        format!(
            "{} Win+X entry \"{}\"",
            if show { "Restored" } else { "Hid" },
            item.label
        ),
        vec![(item.path.clone(), dest)],
    )
}

pub fn rename(ctx: &AppCtx<'_>, item: &WinXItem, new_label: &str) -> Result<()> {
    let label = safe_label(new_label)?;
    let prefix = split_name(&item.path)
        .0
        .map(|p| format!("{p} - "))
        .unwrap_or_default();
    let dest = item.path.with_file_name(format!("{prefix}{label}.lnk"));
    rename_file(&item.path, &dest)?;
    journal_moves(
        ctx,
        format!("Renamed Win+X entry \"{}\" to \"{label}\"", item.label),
        vec![(item.path.clone(), dest)],
    )
}

/// Swaps the ordering prefixes of two entries in the same group.
pub fn swap(ctx: &AppCtx<'_>, a: &WinXItem, b: &WinXItem) -> Result<()> {
    let (pa, la) = split_name(&a.path);
    let (pb, lb) = split_name(&b.path);
    let (Some(pa), Some(pb)) = (pa, pb) else {
        bail!("these entries have no position number")
    };
    let tmp = a.path.with_extension("lnk.moving");
    let new_a = a.path.with_file_name(format!("{pb} - {la}.lnk"));
    let new_b = b.path.with_file_name(format!("{pa} - {lb}.lnk"));
    rename_file(&a.path, &tmp)?;
    rename_file(&b.path, &new_b)?;
    rename_file(&tmp, &new_a)?;
    journal_moves(
        ctx,
        format!("Reordered Win+X entries \"{la}\" and \"{lb}\""),
        vec![
            (a.path.clone(), tmp.clone()),
            (b.path.clone(), new_b),
            (tmp, new_a),
        ],
    )
}

pub fn move_to_group(ctx: &AppCtx<'_>, item: &WinXItem, group: &str) -> Result<()> {
    let dir = winx_dir().join(group);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{} - {}.lnk", next_prefix(&dir), item.label));
    rename_file(&item.path, &dest)?;
    journal_moves(
        ctx,
        format!("Moved Win+X entry \"{}\" to {group}", item.label),
        vec![(item.path.clone(), dest)],
    )
}

pub fn add_group(ctx: &AppCtx<'_>) -> Result<String> {
    let n = groups().iter().map(|g| group_number(g)).max().unwrap_or(0) + 1;
    let name = format!("Group{n}");
    let dir = winx_dir().join(&name);
    std::fs::create_dir_all(&dir)?;
    ctx.journal.record(
        TOOL,
        format!("Added Win+X {name}"),
        UndoAction::DeleteCreatedFiles { paths: vec![dir] },
    )?;
    Ok(name)
}

/// Moves a group (and its entries) out of the menu; undo puts it back.
pub fn remove_group(ctx: &AppCtx<'_>, group: &str) -> Result<()> {
    let from = winx_dir().join(group);
    let to = hidden_dir()?.join(format!(
        "removed {} {group}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::create_dir_all(to.parent().context("no parent")?)?;
    wm_core::paths::move_path(&from, &to)?;
    journal_moves(ctx, format!("Removed Win+X {group}"), vec![(from, to)])
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let dest = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

/// Replaces the menu with Windows' defaults; the current menu is kept for undo.
pub fn restore_defaults(ctx: &AppCtx<'_>) -> Result<()> {
    let defaults = default_dir();
    if !defaults.is_dir() {
        bail!(
            "Windows' default Win+X menu was not found at {}",
            defaults.display()
        );
    }
    let current = winx_dir();
    let backup = paths::data_dir()?.join(format!(
        "winx-before-defaults-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::rename(&current, &backup).context("moving the current menu aside")?;
    if let Err(e) = copy_dir(&defaults, &current) {
        let _ = std::fs::remove_dir_all(&current);
        let _ = std::fs::rename(&backup, &current);
        return Err(e.context("copying the defaults"));
    }
    ctx.journal.record(
        TOOL,
        "Restored the default Win+X menu",
        UndoAction::Sequence {
            steps: vec![
                UndoAction::DeleteCreatedFiles {
                    paths: vec![current.clone()],
                },
                UndoAction::MoveBack {
                    moves: vec![(current, backup)],
                },
            ],
        },
    )?;
    Ok(())
}

enum Ask {
    RemoveGroup(String),
    Defaults,
}

struct AddDialog {
    group: String,
    label: String,
    utility: Option<usize>,
    path: String,
    args: String,
    admin: bool,
}

#[derive(Default)]
pub struct WinXEditor {
    items: Option<Vec<WinXItem>>,
    selected: Option<PathBuf>,
    add: Option<AddDialog>,
    renaming: Option<(WinXItem, String)>,
    restart_pending: bool,
    confirm: Confirm<Ask>,
    message: Message,
}

impl WinXEditor {
    fn done(&mut self, r: Result<String>) {
        if r.is_ok() {
            self.restart_pending = true;
        }
        self.message = Some(r.map_err(|e| format!("{e:#}")));
        self.items = None;
    }

    fn add_dialog(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(d) = &mut self.add else { return };
        let mut close = false;
        let mut add = false;
        egui::Window::new("Add a Win+X entry")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                egui::Grid::new("winx-add")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Opens");
                        egui::ComboBox::from_id_salt("winx-util")
                            .width(280.0)
                            .selected_text(d.utility.map_or("A program, file or folder", |i| UTILITIES[i].0))
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(d.utility.is_none(), "A program, file or folder")
                                    .clicked()
                                {
                                    d.utility = None;
                                }
                                for (i, (name, _, _)) in UTILITIES.iter().enumerate() {
                                    if ui.selectable_label(d.utility == Some(i), *name).clicked() {
                                        d.utility = Some(i);
                                        d.label = (*name).to_owned();
                                    }
                                }
                            });
                        ui.end_row();
                        if d.utility.is_none() {
                            ui.label("Path");
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut d.path).desired_width(200.0));
                                let mut picked = None;
                                if ui.button("File…").clicked() {
                                    picked = rfd::FileDialog::new().pick_file();
                                }
                                if ui.button("Folder…").clicked() {
                                    picked = rfd::FileDialog::new().pick_folder();
                                }
                                if ui.button("Start item…").clicked() {
                                    picked = rfd::FileDialog::new()
                                        .set_directory(paths::expand_env(
                                            r"%ProgramData%\Microsoft\Windows\Start Menu\Programs",
                                        ))
                                        .add_filter("Shortcuts", &["lnk"])
                                        .pick_file();
                                }
                                if let Some(p) = picked {
                                    d.path = p.display().to_string();
                                    d.label = p
                                        .file_stem()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_default();
                                }
                            });
                            ui.end_row();
                            ui.label("Arguments");
                            ui.text_edit_singleline(&mut d.args);
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(&mut d.admin, "Run as administrator");
                            ui.end_row();
                        }
                        ui.label("Name");
                        ui.text_edit_singleline(&mut d.label);
                        ui.end_row();
                        ui.label("Group");
                        egui::ComboBox::from_id_salt("winx-group")
                            .selected_text(d.group.clone())
                            .show_ui(ui, |ui| {
                                for g in groups() {
                                    ui.selectable_value(&mut d.group, g.clone(), g);
                                }
                            });
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        add = true;
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        close = true;
                    }
                });
            });
        if add {
            let d = self.add.as_ref().expect("open");
            let entry = match d.utility {
                Some(i) => NewEntry::Utility(i),
                None => NewEntry::Path {
                    path: d.path.clone(),
                    args: d.args.clone(),
                    admin: d.admin,
                },
            };
            if matches!(&entry, NewEntry::Path { path, .. } if path.trim().is_empty()) {
                self.message = Some(Err("Choose what the entry opens.".into()));
            } else if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                close = true;
            } else {
                let (group, label) = (d.group.clone(), d.label.clone());
                let r = add_entry(ctx, &group, &label, &entry)
                    .map(|_| format!("Added \"{}\" to {group}.", label.trim()));
                close = r.is_ok();
                self.done(r);
            }
        }
        if close {
            self.add = None;
        }
    }

    fn rename_dialog(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some((_, name)) = &mut self.renaming else {
            return;
        };
        let mut result = None;
        egui::Window::new("Rename entry")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.text_edit_singleline(name).request_focus();
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        result = Some(true);
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        result = Some(false);
                    }
                });
            });
        match result {
            Some(true) => {
                let (item, name) = self.renaming.take().expect("open");
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                } else {
                    let r = rename(ctx, &item, &name).map(|_| "Renamed.".to_owned());
                    self.done(r);
                }
            }
            Some(false) => self.renaming = None,
            None => {}
        }
    }
}

impl Tool for WinXEditor {
    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn id(&self) -> &'static str {
        "winx"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let r = match ask {
                    Ask::RemoveGroup(g) => remove_group(ctx, &g).map(|_| format!("Removed {g}.")),
                    Ask::Defaults => {
                        restore_defaults(ctx).map(|_| "Restored Windows' default Win+X menu.".into())
                    }
                };
                self.done(r);
            }
        }
        self.add_dialog(ui, ctx);
        self.rename_dialog(ui, ctx);
        let items = self.items.get_or_insert_with(enumerate).clone();
        let selected = self
            .selected
            .as_ref()
            .and_then(|p| items.iter().find(|i| &i.path == p))
            .cloned();
        let group_items = |g: &str| -> Vec<WinXItem> {
            items
                .iter()
                .filter(|i| i.group == g && !i.hidden)
                .cloned()
                .collect()
        };
        let neighbour = |up: bool| -> Option<WinXItem> {
            let s = selected.as_ref().filter(|s| !s.hidden)?;
            let list = group_items(&s.group);
            let i = list.iter().position(|x| x.path == s.path)?;
            // The list runs top of the menu first.
            if up {
                i.checked_sub(1).map(|j| list[j].clone())
            } else {
                list.get(i + 1).cloned()
            }
        };
        let (up, down) = (neighbour(true), neighbour(false));
        let shown = selected.as_ref().is_some_and(|s| !s.hidden);

        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add entry")),
                Some(ToolButton::new(icons::PENCIL_SIMPLE, "Rename").enabled(shown)),
                Some(ToolButton::new(icons::ARROW_UP, "Move up").enabled(up.is_some())),
                Some(ToolButton::new(icons::ARROW_DOWN, "Move down").enabled(down.is_some())),
                None,
                Some(ToolButton::new(icons::STACK, "Add group")),
                Some(ToolButton::new(icons::CLOCK_COUNTER_CLOCKWISE, "Defaults")),
                None,
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Restart Explorer").enabled(self.restart_pending)),
            ],
            |_| {},
        ) {
            Some(0) => {
                self.add = Some(AddDialog {
                    group: selected.as_ref().map_or_else(|| groups().into_iter().next().unwrap_or("Group3".into()), |s| s.group.clone()),
                    label: String::new(),
                    utility: None,
                    path: String::new(),
                    args: String::new(),
                    admin: false,
                });
            }
            Some(1) => {
                if let Some(s) = &selected {
                    self.renaming = Some((s.clone(), s.label.clone()));
                }
            }
            Some(i @ (2 | 3)) => {
                let other = if i == 2 { up } else { down };
                if let (Some(s), Some(o)) = (&selected, other) {
                    if ctx.config.dry_run {
                        self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    } else {
                        // The selection follows the entry to its new file name.
                        let r = swap(ctx, s, &o).map(|_| "Moved.".to_owned());
                        let (op, _) = split_name(&o.path);
                        if let Some(op) = op {
                            self.selected = Some(s.path.with_file_name(format!("{op} - {}.lnk", s.label)));
                        }
                        self.done(r);
                    }
                }
            }
            Some(5) => {
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                } else {
                    let r = add_group(ctx).map(|g| format!("Added {g}; it appears at the top of the menu once it has entries."));
                    self.done(r);
                }
            }
            Some(6) => self.confirm.ask(
                "Replace your Win+X menu with Windows' default one? Your current menu is kept and comes back with Undo.",
                Ask::Defaults,
            ),
            Some(8) => {
                self.message = Some(restart_explorer().map(|()| "Explorer restarted.".into()).map_err(|e| format!("{e:#}")));
                self.restart_pending = false;
            }
            _ => {}
        }
        frame::options(ui, |ui| {
            ui.weak("Untick entries to hide them. Changes appear after Explorer restarts.");
            if let Some(s) = selected.as_ref().filter(|s| !s.hidden) {
                ui.separator();
                ui.label(format!("Move \"{}\" to", s.label));
                let mut target = None;
                egui::ComboBox::from_id_salt("winx-move")
                    .selected_text(s.group.clone())
                    .show_ui(ui, |ui| {
                        for g in groups().into_iter().filter(|g| *g != s.group) {
                            if ui.selectable_label(false, &g).clicked() {
                                target = Some(g);
                            }
                        }
                    });
                if let Some(g) = target {
                    if ctx.config.dry_run {
                        self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    } else {
                        let r = move_to_group(ctx, s, &g).map(|_| format!("Moved to {g}."));
                        self.selected = None;
                        self.done(r);
                    }
                }
            }
        });
        let status = format!(
            "{} entries in {} groups{}",
            items.iter().filter(|i| !i.hidden).count(),
            groups().len(),
            if items.iter().any(|i| !i.hidden && !i.signed) {
                "; some entries lack Windows' hash and will not be shown"
            } else {
                ""
            }
        );
        frame::status(ui, &status, |_| {});

        let mut toggle = None;
        let mut clicked = None;
        let mut group_action = None;
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            for g in groups() {
                ui.horizontal(|ui| {
                    ui.strong(&g);
                    if ui.small_button("Remove group").clicked() {
                        group_action = Some(g.clone());
                    }
                });
                let rows: Vec<&WinXItem> = items.iter().filter(|i| i.group == g).collect();
                ui.push_id(&g, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    TableBuilder::new(ui)
                        .striped(true)
                        .sense(egui::Sense::click())
                        .vscroll(false)
                        .column(Column::exact(24.0))
                        .column(Column::initial(220.0).clip(true))
                        .column(Column::initial(130.0))
                        .column(Column::remainder().clip(true))
                        .body(|mut body| {
                            for it in rows {
                                body.row(21.0, |mut row| {
                                    row.set_selected(self.selected.as_ref() == Some(&it.path));
                                    row.col(|ui| {
                                        let mut on = !it.hidden;
                                        if ui.checkbox(&mut on, "").changed() {
                                            toggle = Some((it.clone(), on));
                                        }
                                    });
                                    row.col(|ui| {
                                        ui.label(&it.label);
                                    });
                                    row.col(|ui| {
                                        if it.hidden {
                                            ui.weak("Hidden");
                                        } else if !it.signed {
                                            ui.colored_label(
                                                ui.visuals().warn_fg_color,
                                                "Not shown by Windows",
                                            );
                                        }
                                    });
                                    row.col(|ui| {
                                        ui.weak(format!("{} {}", it.target, it.args).trim().to_owned());
                                    });
                                    if row.response().clicked() {
                                        clicked = Some(it.path.clone());
                                    }
                                });
                            }
                        });
                });
                ui.add_space(8.0);
            }
            let hidden: Vec<&WinXItem> = items.iter().filter(|i| i.hidden).collect();
            if !hidden.is_empty() {
                ui.strong("Hidden entries");
                for it in hidden {
                    ui.horizontal(|ui| {
                        let mut on = false;
                        if ui.checkbox(&mut on, &it.label).changed() {
                            toggle = Some((it.clone(), true));
                        }
                        ui.weak(&it.group);
                    });
                }
            }
        });
        if clicked.is_some() {
            self.selected = clicked;
        }
        if let Some(g) = group_action {
            self.confirm.ask(
                format!("Remove {g} and its entries from the menu? Undo brings it back."),
                Ask::RemoveGroup(g),
            );
        }
        if let Some((it, show)) = toggle {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let r = set_shown(ctx, &it, show)
                    .map(|_| format!("{} \"{}\".", if show { "Restored" } else { "Hid" }, it.label));
                self.done(r);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(
            label_of(Path::new("3 - Windows Explorer.lnk")),
            "Windows Explorer"
        );
        assert_eq!(
            label_of(Path::new("01a - Windows PowerShell.lnk")),
            "Windows PowerShell"
        );
        assert_eq!(label_of(Path::new("04-1 - NetworkStatus.lnk")), "NetworkStatus");
        assert_eq!(label_of(Path::new("Tools - Misc.lnk")), "Tools - Misc");
        assert_eq!(label_of(Path::new("Custom.lnk")), "Custom");
    }

    /// Tests that read or change the real Win+X folder take turns, so one never reads
    /// another's temporary group while its shortcuts are half-written.
    static REAL_FOLDER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The hash scheme reproduces every hash Windows stored on its own file shortcuts.
    #[test]
    fn hash_matches_windows_own_shortcuts() {
        let _turn = REAL_FOLDER.lock().unwrap_or_else(|e| e.into_inner());
        let items = list(&winx_dir(), false);
        assert!(!items.is_empty(), "the Win+X folder should contain shortcuts");
        let mut checked = 0;
        for i in items.iter().filter(|i| !i.target.is_empty()) {
            let (_, _, stored) = read_link(&i.path).unwrap();
            if let Some(h) = stored {
                assert_eq!(h, link_hash(&i.target, &i.args), "{}", i.path.display());
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    #[test]
    fn launchers_stay_in_the_windows_folder() {
        let win = paths::expand_env("%SystemRoot%").to_lowercase();
        for e in [
            NewEntry::Path {
                path: r"C:\Program Files\App\app.exe".into(),
                args: String::new(),
                admin: false,
            },
            NewEntry::Path {
                path: r"D:\Docs".into(),
                args: "-x 'y'".into(),
                admin: false,
            },
            NewEntry::Path {
                path: r"C:\Tools\it's.exe".into(),
                args: String::new(),
                admin: true,
            },
            NewEntry::Utility(0),
        ] {
            let (t, a) = launcher(&e);
            assert!(paths::expand_env(&t).to_lowercase().starts_with(&win), "{t}");
            if let NewEntry::Path { admin: true, .. } = e {
                assert!(a.contains("-Verb RunAs") && a.contains("it''s.exe"), "{a}");
            }
        }
        for (name, exe, _) in UTILITIES {
            let p = paths::expand_env(exe);
            assert!(p.to_lowercase().starts_with(&win), "{name}");
            // gpedit.msc is missing on Home editions.
            assert!(Path::new(&p).exists() || exe.contains("gpedit"), "{name}: {p}");
        }
    }

    /// Add, rename, reorder and move entries in two temporary groups of the real
    /// Win+X folder, then undo each change through the journal.
    #[test]
    fn edit_entries_in_temporary_groups_with_undo() {
        let _turn = REAL_FOLDER.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        struct Cleanup(Vec<PathBuf>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for d in &self.0 {
                    let _ = std::fs::remove_dir_all(d);
                }
            }
        }
        let g1 = add_group(&ctx).unwrap();
        let g2 = add_group(&ctx).unwrap();
        let hidden = hidden_dir().unwrap();
        let _cleanup = Cleanup(vec![
            winx_dir().join(&g1),
            winx_dir().join(&g2),
            hidden.join(&g1),
            hidden.join(&g2),
        ]);
        let a = add_entry(&ctx, &g1, "Alpha", &NewEntry::Utility(0)).unwrap();
        let b = add_entry(
            &ctx,
            &g1,
            "Beta",
            &NewEntry::Path {
                path: r"C:\Windows".into(),
                args: String::new(),
                admin: false,
            },
        )
        .unwrap();
        assert!(a.ends_with("01 - Alpha.lnk") && b.ends_with("02 - Beta.lnk"));
        let find = |label: &str| {
            enumerate()
                .into_iter()
                .find(|i| i.label == label && (i.group == g1 || i.group == g2))
        };
        assert!(find("Alpha").unwrap().signed && find("Beta").unwrap().signed);

        swap(&ctx, &find("Alpha").unwrap(), &find("Beta").unwrap()).unwrap();
        assert!(winx_dir().join(&g1).join("02 - Alpha.lnk").exists());
        rename(&ctx, &find("Beta").unwrap(), "Gamma").unwrap();
        move_to_group(&ctx, &find("Gamma").unwrap(), &g2).unwrap();
        assert!(winx_dir().join(&g2).join("01 - Gamma.lnk").exists());
        set_shown(&ctx, &find("Alpha").unwrap(), false).unwrap();
        assert!(find("Alpha").unwrap().hidden);

        // Undo everything but the group creation, newest first.
        let entries = journal.entries();
        for e in entries.iter().take(entries.len() - 2) {
            journal.undo(e.id).unwrap();
        }
        assert!(!a.exists() && !b.exists() && find("Alpha").is_none() && find("Gamma").is_none());
    }

    #[test]
    fn written_links_carry_a_valid_hash() {
        let dir = tempfile::tempdir().unwrap();
        let lnk = dir.path().join("01 - Test.lnk");
        let (t, a) = launcher(&NewEntry::Path {
            path: r"C:\Program Files\App\app.exe".into(),
            args: String::new(),
            admin: false,
        });
        write_link(&lnk, &t, &a, "Test").unwrap();
        let (target, args, stored) = read_link(&lnk).unwrap();
        assert_eq!(stored, Some(link_hash(&target, &args)));
        assert!(target.to_lowercase().ends_with(r"\explorer.exe"));
        assert_eq!(next_prefix(dir.path()), "02");
    }
}
