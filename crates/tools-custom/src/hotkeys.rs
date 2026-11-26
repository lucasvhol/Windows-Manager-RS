//! Hotkey Manager: global keyboard shortcuts that open programs, files, folders, web
//! addresses, Start items, Windows tools and Store apps.
//!
//! Uses the hotkey field of Start menu / desktop shortcuts, which Windows itself
//! listens for, so hotkeys keep working when this app is closed. New hotkeys are
//! shortcuts in a dedicated Start menu folder. Hotkeys can be turned off (the value
//! is remembered), backed up and restored; conflicts with other shortcuts and with
//! hotkeys other programs hold are reported.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
};
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    icons, paths,
    process::{json_list, powershell_json},
    safety::UndoAction,
    shell::{KnownFolder, ShortcutSpec, create_shortcut, known_folder, read_shortcut},
    task::BackgroundTask,
    tool_settings,
    ui::{Confirm, Message, ToolButton, frame, show_message},
};

const TOOL: &str = "hotkeys";
const HOTKEYF_SHIFT: u16 = 0x01;
const HOTKEYF_CONTROL: u16 = 0x02;
const HOTKEYF_ALT: u16 = 0x04;

pub const FOLDER: &str = "Win Manager RS Hotkeys";

/// Keys offered in the editor: (label, virtual-key code).
pub fn keys() -> Vec<(String, u8)> {
    let mut k: Vec<(String, u8)> = (b'A'..=b'Z').map(|c| ((c as char).to_string(), c)).collect();
    k.extend((b'0'..=b'9').map(|c| ((c as char).to_string(), c)));
    k.extend((1..=12u8).map(|n| (format!("F{n}"), 0x6F + n)));
    k.extend((0..=9u8).map(|n| (format!("Num {n}"), 0x60 + n)));
    for (l, v) in [
        ("Insert", 0x2D),
        ("Delete", 0x2E),
        ("Home", 0x24),
        ("End", 0x23),
        ("Page Up", 0x21),
        ("Page Down", 0x22),
        ("Left", 0x25),
        ("Up", 0x26),
        ("Right", 0x27),
        ("Down", 0x28),
        ("Space", 0x20),
    ] {
        k.push((l.to_owned(), v));
    }
    k
}

pub fn encode(ctrl: bool, alt: bool, shift: bool, vk: u8) -> u16 {
    let mut m = 0;
    if ctrl {
        m |= HOTKEYF_CONTROL;
    }
    if alt {
        m |= HOTKEYF_ALT;
    }
    if shift {
        m |= HOTKEYF_SHIFT;
    }
    (m << 8) | u16::from(vk)
}

pub fn format_hotkey(hk: u16) -> String {
    if hk == 0 {
        return String::new();
    }
    let m = hk >> 8;
    let vk = (hk & 0xFF) as u8;
    let mut parts = Vec::new();
    if m & HOTKEYF_CONTROL != 0 {
        parts.push("Ctrl".to_owned());
    }
    if m & HOTKEYF_ALT != 0 {
        parts.push("Alt".to_owned());
    }
    if m & HOTKEYF_SHIFT != 0 {
        parts.push("Shift".to_owned());
    }
    parts.push(
        keys()
            .into_iter()
            .find(|(_, k)| *k == vk)
            .map(|(l, _)| l)
            .unwrap_or_else(|| format!("0x{vk:02X}")),
    );
    parts.join("+")
}

/// Windows ignores shortcut hotkeys without Ctrl or Alt.
pub fn validate(hk: u16) -> Result<()> {
    if (hk >> 8) & (HOTKEYF_CONTROL | HOTKEYF_ALT) == 0 {
        bail!("Hotkeys need Ctrl and/or Alt.");
    }
    Ok(())
}

/// False when another running program has registered this combination.
pub fn free_system_wide(hk: u16) -> bool {
    let m = hk >> 8;
    let mut mods = MOD_NOREPEAT;
    if m & HOTKEYF_CONTROL != 0 {
        mods |= MOD_CONTROL;
    }
    if m & HOTKEYF_ALT != 0 {
        mods |= MOD_ALT;
    }
    if m & HOTKEYF_SHIFT != 0 {
        mods |= MOD_SHIFT;
    }
    const PROBE_ID: i32 = 0xB7E1;
    let ok =
        unsafe { RegisterHotKey(None, PROBE_ID, HOT_KEY_MODIFIERS(mods.0), u32::from(hk & 0xFF)) }.is_ok();
    if ok {
        let _ = unsafe { UnregisterHotKey(None, PROBE_ID) };
    }
    ok
}

/// Windows tools offered as hotkey targets: (label, program, arguments).
pub fn utilities() -> Vec<(&'static str, &'static str, &'static str)> {
    let mut v: Vec<_> = crate::winx::UTILITIES.to_vec();
    v.push((
        "End programs that are not responding",
        r"%SystemRoot%\System32\taskkill.exe",
        r#"/f /fi "status eq not responding""#,
    ));
    v.push(("Task Manager", r"%SystemRoot%\System32\Taskmgr.exe", ""));
    v.push((
        "Show the desktop",
        r"%SystemRoot%\explorer.exe",
        "shell:::{3080F90D-D7AD-11D9-BD98-0000947B0257}",
    ));
    v
}

/// What a new hotkey opens.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A program, document or folder (shortcuts point at them directly).
    Path {
        path: String,
        args: String,
    },
    Url(String),
    /// An existing Start menu shortcut, copied with its arguments.
    StartItem(PathBuf),
    Utility(usize),
    /// A Store app by its AppUserModelID.
    App(String),
}

pub fn target_spec(t: &Target) -> Result<ShortcutSpec> {
    let explorer = || PathBuf::from(paths::expand_env(r"%SystemRoot%\explorer.exe"));
    Ok(match t {
        Target::Path { path, args } => {
            let p = path.trim().trim_matches('"');
            if p.is_empty() {
                bail!("Choose what the hotkey opens");
            }
            ShortcutSpec {
                target: PathBuf::from(p),
                args: args.trim().to_owned(),
                ..Default::default()
            }
        }
        Target::Url(u) => {
            let u = u.trim();
            if !u.contains("://") && !u.contains(':') {
                bail!("Enter a full web address, such as https://example.com");
            }
            ShortcutSpec {
                target: explorer(),
                args: format!("\"{u}\""),
                ..Default::default()
            }
        }
        Target::StartItem(lnk) => {
            let s = read_shortcut(lnk).with_context(|| format!("reading {}", lnk.display()))?;
            ShortcutSpec { hotkey: 0, ..s }
        }
        Target::Utility(i) => {
            let u = utilities();
            let (_, exe, args) = u.get(*i).context("unknown tool")?;
            ShortcutSpec {
                target: PathBuf::from(paths::expand_env(exe)),
                args: (*args).to_owned(),
                ..Default::default()
            }
        }
        Target::App(aumid) => ShortcutSpec {
            target: explorer(),
            args: format!(r"shell:AppsFolder\{aumid}"),
            ..Default::default()
        },
    })
}

/// Store apps for the target picker: (name, AppUserModelID).
pub fn store_apps() -> Result<Vec<(String, String)>> {
    let v = powershell_json(
        "@(Get-StartApps | Where-Object { $_.AppID -like '*!*' }) | ConvertTo-Json -Compress",
    )?;
    let mut apps: Vec<(String, String)> = json_list(v)
        .iter()
        .filter_map(|o| Some((o["Name"].as_str()?.to_owned(), o["AppID"].as_str()?.to_owned())))
        .collect();
    apps.sort_by_key(|(n, _)| n.to_lowercase());
    Ok(apps)
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotkeyItem {
    pub lnk: PathBuf,
    pub name: String,
    pub target: String,
    pub hotkey: u16,
    pub ours: bool,
    pub all_users: bool,
    /// Turned off here; `hotkey` holds the remembered combination.
    pub off: bool,
}

/// Remembered hotkeys of shortcuts that are turned off.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub off: Vec<(PathBuf, u16)>,
}

pub fn our_folder() -> PathBuf {
    PathBuf::from(paths::expand_env(
        r"%APPDATA%\Microsoft\Windows\Start Menu\Programs",
    ))
    .join(FOLDER)
}

fn roots() -> Vec<(PathBuf, bool)> {
    let mut roots = vec![
        (
            PathBuf::from(paths::expand_env(r"%APPDATA%\Microsoft\Windows\Start Menu")),
            false,
        ),
        (
            PathBuf::from(paths::expand_env(r"%ProgramData%\Microsoft\Windows\Start Menu")),
            true,
        ),
    ];
    if let Ok(d) = known_folder(KnownFolder::Desktop) {
        roots.push((d, false));
    }
    if let Ok(d) = known_folder(KnownFolder::PublicDesktop) {
        roots.push((d, true));
    }
    roots
}

/// Shortcuts that have (or had, when turned off here) a hotkey.
pub fn scan() -> Vec<HotkeyItem> {
    let saved: Saved = tool_settings::load(TOOL);
    let ours = our_folder();
    let mut items = Vec::new();
    for (root, all_users) in roots() {
        for e in WalkDir::new(root).follow_links(false).into_iter().flatten() {
            let p = e.path();
            if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
                continue;
            }
            let Ok(spec) = read_shortcut(p) else { continue };
            let remembered = saved.off.iter().find(|(l, _)| l == p).map(|(_, h)| *h);
            let is_ours = p.starts_with(&ours);
            if spec.hotkey == 0 && remembered.is_none() && !is_ours {
                continue;
            }
            items.push(HotkeyItem {
                name: p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                target: format!("{} {}", spec.target.display(), spec.args)
                    .trim()
                    .to_owned(),
                hotkey: if spec.hotkey == 0 {
                    remembered.unwrap_or(0)
                } else {
                    spec.hotkey
                },
                off: spec.hotkey == 0 && remembered.is_some(),
                lnk: p.to_owned(),
                ours: is_ours,
                all_users,
            });
        }
    }
    items.sort_by_key(|i| format_hotkey(i.hotkey));
    items
}

/// Changes the hotkey of an existing shortcut, preserving everything else.
fn write_hotkey(lnk: &Path, hotkey: u16) -> Result<()> {
    let mut spec = read_shortcut(lnk)?;
    spec.hotkey = hotkey;
    create_shortcut(lnk, &spec)
}

/// Sets a shortcut's hotkey (0 clears it), backing the file up first.
pub fn set_hotkey(ctx: &AppCtx<'_>, item: &HotkeyItem, hotkey: u16) -> Result<()> {
    let undo = ctx.journal.backup_file(&item.lnk)?;
    write_hotkey(&item.lnk, hotkey)?;
    let what = if hotkey == 0 {
        format!("Removed the hotkey of {}", item.name)
    } else {
        format!("Set {} for {}", format_hotkey(hotkey), item.name)
    };
    ctx.journal.record(TOOL, what, undo)?;
    Ok(())
}

/// Turns a hotkey off (remembering it) or back on.
pub fn set_on(ctx: &AppCtx<'_>, item: &HotkeyItem, on: bool) -> Result<()> {
    let mut saved: Saved = tool_settings::load(TOOL);
    saved.off.retain(|(l, _)| l != &item.lnk);
    if !on {
        saved.off.push((item.lnk.clone(), item.hotkey));
    }
    set_hotkey(ctx, item, if on { item.hotkey } else { 0 })?;
    tool_settings::save(TOOL, &saved)
}

pub fn create(ctx: &AppCtx<'_>, name: &str, target: &Target, hotkey: u16) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        bail!("Enter a name without \\ / : * ? \" < > |");
    }
    validate(hotkey)?;
    std::fs::create_dir_all(our_folder())?;
    let lnk = our_folder().join(format!("{name}.lnk"));
    if lnk.exists() {
        bail!("A hotkey named \"{name}\" already exists");
    }
    let spec = ShortcutSpec {
        hotkey,
        ..target_spec(target)?
    };
    create_shortcut(&lnk, &spec)?;
    ctx.journal.record(
        TOOL,
        format!("Added hotkey {} for {name}", format_hotkey(hotkey)),
        UndoAction::DeleteCreatedFiles {
            paths: vec![lnk.clone()],
        },
    )?;
    Ok(lnk)
}

/// Saves this app's hotkey shortcuts and the hotkeys of all other shortcuts.
pub fn backup(parent: &Path, items: &[HotkeyItem]) -> Result<PathBuf> {
    let dir = parent.join(format!(
        "Hotkeys backup {}",
        chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
    ));
    std::fs::create_dir_all(&dir)?;
    let mut others = Vec::new();
    for i in items {
        if i.ours {
            std::fs::copy(&i.lnk, dir.join(i.lnk.file_name().context("no name")?))?;
        } else if !i.off && i.hotkey != 0 {
            others.push((i.lnk.clone(), i.hotkey));
        }
    }
    std::fs::write(
        dir.join("other-shortcuts.json"),
        serde_json::to_vec_pretty(&others)?,
    )?;
    Ok(dir)
}

/// Restores a backup: missing hotkey shortcuts are copied back, and other shortcuts
/// that still exist get their hotkey again. Returns how many were restored.
pub fn restore(ctx: &AppCtx<'_>, dir: &Path) -> Result<usize> {
    let others_file = dir.join("other-shortcuts.json");
    if !others_file.is_file() {
        bail!("{} is not a hotkey backup", dir.display());
    }
    let others: Vec<(PathBuf, u16)> = serde_json::from_slice(&std::fs::read(&others_file)?)?;
    std::fs::create_dir_all(our_folder())?;
    let mut steps = Vec::new();
    let mut n = 0;
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
            let dest = our_folder().join(e.file_name());
            if !dest.exists() {
                std::fs::copy(&p, &dest)?;
                steps.push(UndoAction::DeleteCreatedFiles { paths: vec![dest] });
                n += 1;
            }
        }
    }
    for (lnk, hk) in others {
        if lnk.is_file() && read_shortcut(&lnk).is_ok_and(|s| s.hotkey != hk) {
            steps.push(ctx.journal.backup_file(&lnk)?);
            write_hotkey(&lnk, hk)?;
            n += 1;
        }
    }
    ctx.journal.record(
        TOOL,
        format!("Restored {n} hotkeys from {}", dir.display()),
        UndoAction::Sequence { steps },
    )?;
    Ok(n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Path,
    Url,
    StartItem,
    Utility,
    App,
}

struct Draft {
    name: String,
    kind: Kind,
    path: String,
    args: String,
    url: String,
    start_item: Option<PathBuf>,
    utility: usize,
    app: Option<usize>,
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: usize,
    /// Changing the hotkey of this existing shortcut instead of creating one.
    editing: Option<HotkeyItem>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: Kind::Path,
            path: String::new(),
            args: String::new(),
            url: String::new(),
            start_item: None,
            utility: 0,
            app: None,
            ctrl: true,
            alt: true,
            shift: false,
            key: 0,
            editing: None,
        }
    }
}

enum Ask {
    Delete(Box<HotkeyItem>),
    Restore(PathBuf),
}

#[derive(Default)]
pub struct HotkeyManager {
    scan: Option<BackgroundTask<Vec<HotkeyItem>>>,
    items: Option<Vec<HotkeyItem>>,
    selected: Option<PathBuf>,
    draft: Option<Draft>,
    apps: Option<BackgroundTask<Vec<(String, String)>>>,
    app_list: Vec<(String, String)>,
    confirm: Confirm<Ask>,
    message: Message,
}

impl HotkeyManager {
    fn done(&mut self, r: Result<String>) {
        self.message = Some(r.map_err(|e| format!("{e:#}")));
        self.items = None;
    }

    fn draft_hotkey(d: &Draft) -> u16 {
        encode(d.ctrl, d.alt, d.shift, keys()[d.key].1)
    }

    fn draft_window(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.apps {
            t.poll();
            if let Some(r) = t.take_result() {
                self.app_list = r.unwrap_or_default();
                self.apps = None;
            }
        }
        let items = self.items.clone().unwrap_or_default();
        let Some(d) = &mut self.draft else { return };
        let mut result = None;
        let mut want_apps = false;
        let title = if d.editing.is_some() {
            "Change hotkey"
        } else {
            "New hotkey"
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                if let Some(e) = &d.editing {
                    ui.label(format!("{} — {}", e.name, e.target));
                } else {
                    ui.horizontal(|ui| {
                        for (k, l) in [
                            (Kind::Path, "Program, file or folder"),
                            (Kind::Url, "Web address"),
                            (Kind::StartItem, "Start item"),
                            (Kind::Utility, "Windows tool"),
                            (Kind::App, "Store app"),
                        ] {
                            if ui.radio_value(&mut d.kind, k, l).changed() && k == Kind::App {
                                want_apps = true;
                            }
                        }
                    });
                    egui::Grid::new("hk-draft")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            match d.kind {
                                Kind::Path => {
                                    ui.label("Opens");
                                    ui.horizontal(|ui| {
                                        ui.add(egui::TextEdit::singleline(&mut d.path).desired_width(250.0));
                                        let mut picked = None;
                                        if ui.button("File…").clicked() {
                                            picked = rfd::FileDialog::new().pick_file();
                                        }
                                        if ui.button("Folder…").clicked() {
                                            picked = rfd::FileDialog::new().pick_folder();
                                        }
                                        if let Some(p) = picked {
                                            d.path = p.display().to_string();
                                            if d.name.is_empty() {
                                                d.name = p
                                                    .file_stem()
                                                    .or(p.file_name())
                                                    .map(|s| s.to_string_lossy().into_owned())
                                                    .unwrap_or_default();
                                            }
                                        }
                                    });
                                    ui.end_row();
                                    ui.label("Arguments");
                                    ui.text_edit_singleline(&mut d.args);
                                    ui.end_row();
                                }
                                Kind::Url => {
                                    ui.label("Address");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut d.url)
                                            .hint_text("https://…")
                                            .desired_width(300.0),
                                    );
                                    ui.end_row();
                                }
                                Kind::StartItem => {
                                    ui.label("Start item");
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            d.start_item
                                                .as_ref()
                                                .map(|p| p.display().to_string())
                                                .unwrap_or_default(),
                                        );
                                        if ui.button("Choose…").clicked()
                                            && let Some(p) = rfd::FileDialog::new()
                                                .set_directory(paths::expand_env(
                                                    r"%ProgramData%\Microsoft\Windows\Start Menu\Programs",
                                                ))
                                                .add_filter("Shortcuts", &["lnk"])
                                                .pick_file()
                                        {
                                            if d.name.is_empty() {
                                                d.name = p
                                                    .file_stem()
                                                    .map(|s| s.to_string_lossy().into_owned())
                                                    .unwrap_or_default();
                                            }
                                            d.start_item = Some(p);
                                        }
                                    });
                                    ui.end_row();
                                }
                                Kind::Utility => {
                                    ui.label("Tool");
                                    let list = utilities();
                                    egui::ComboBox::from_id_salt("hk-util")
                                        .width(300.0)
                                        .selected_text(list[d.utility].0)
                                        .show_ui(ui, |ui| {
                                            for (i, (n, _, _)) in list.iter().enumerate() {
                                                if ui.selectable_value(&mut d.utility, i, *n).clicked() {
                                                    d.name = (*n).to_owned();
                                                }
                                            }
                                        });
                                    ui.end_row();
                                }
                                Kind::App => {
                                    ui.label("App");
                                    if self.app_list.is_empty() {
                                        ui.spinner();
                                    } else {
                                        egui::ComboBox::from_id_salt("hk-app")
                                            .width(300.0)
                                            .selected_text(
                                                d.app.map(|i| self.app_list[i].0.clone()).unwrap_or_default(),
                                            )
                                            .show_ui(ui, |ui| {
                                                for (i, (n, _)) in self.app_list.iter().enumerate() {
                                                    if ui.selectable_label(d.app == Some(i), n).clicked() {
                                                        d.app = Some(i);
                                                        d.name = n.clone();
                                                    }
                                                }
                                            });
                                    }
                                    ui.end_row();
                                }
                            }
                            ui.label("Name");
                            ui.text_edit_singleline(&mut d.name);
                            ui.end_row();
                        });
                }
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.ctrl, "Ctrl");
                    ui.checkbox(&mut d.alt, "Alt");
                    ui.checkbox(&mut d.shift, "Shift");
                    let keys = keys();
                    egui::ComboBox::from_id_salt("hk-key")
                        .selected_text(&keys[d.key].0)
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for (i, (label, _)) in keys.iter().enumerate() {
                                ui.selectable_value(&mut d.key, i, label);
                            }
                        });
                });
                let hk = Self::draft_hotkey(d);
                let clash = items
                    .iter()
                    .find(|i| i.hotkey == hk && !i.off && d.editing.as_ref().is_none_or(|e| e.lnk != i.lnk));
                match (validate(hk), clash) {
                    (Err(e), _) => {
                        ui.colored_label(ui.visuals().warn_fg_color, e.to_string());
                    }
                    (Ok(()), Some(c)) => {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!("{} is already used by \"{}\".", format_hotkey(hk), c.name),
                        );
                    }
                    (Ok(()), None) if !free_system_wide(hk) => {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!(
                                "{} is taken by a running program; Windows may not pass it on.",
                                format_hotkey(hk)
                            ),
                        );
                    }
                    _ => {
                        ui.weak(format!("{} is free.", format_hotkey(hk)));
                    }
                }
                ui.horizontal(|ui| {
                    let ok = validate(hk).is_ok() && clash.is_none();
                    if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
                        result = Some(true);
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        result = Some(false);
                    }
                });
            });
        if want_apps && self.app_list.is_empty() && self.apps.is_none() {
            self.apps = Some(BackgroundTask::spawn(ui.ctx(), |_| store_apps()));
        }
        match result {
            Some(true) => {
                let d = self.draft.take().expect("open");
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    return;
                }
                let hk = Self::draft_hotkey(&d);
                let r = if let Some(item) = &d.editing {
                    set_hotkey(ctx, item, hk)
                        .map(|_| format!("{} now opens {}.", format_hotkey(hk), item.name))
                } else {
                    let target = match d.kind {
                        Kind::Path => Some(Target::Path {
                            path: d.path.clone(),
                            args: d.args.clone(),
                        }),
                        Kind::Url => Some(Target::Url(d.url.clone())),
                        Kind::StartItem => d.start_item.clone().map(Target::StartItem),
                        Kind::Utility => Some(Target::Utility(d.utility)),
                        Kind::App => d
                            .app
                            .and_then(|i| self.app_list.get(i))
                            .map(|(_, id)| Target::App(id.clone())),
                    };
                    match target {
                        Some(t) => create(ctx, &d.name, &t, hk).map(|_| {
                            format!(
                                "{} now opens {}. New hotkeys can need a sign-out to start working.",
                                format_hotkey(hk),
                                d.name.trim()
                            )
                        }),
                        None => Err(anyhow::anyhow!("Choose what the hotkey opens")),
                    }
                };
                self.done(r);
            }
            Some(false) => self.draft = None,
            None => {}
        }
    }
}

impl Tool for HotkeyManager {
    fn id(&self) -> &'static str {
        "hotkeys"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.scan.is_none() && self.items.is_none() {
            self.scan = Some(BackgroundTask::spawn(ui.ctx(), |_| Ok(scan())));
        }
        if let Some(t) = &mut self.scan {
            t.poll();
            if let Some(r) = t.take_result() {
                self.items = Some(r.unwrap_or_default());
                self.scan = None;
            }
        }
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let r = match ask {
                    Ask::Delete(item) => ctx
                        .journal
                        .recycle(
                            TOOL,
                            format!("Deleted hotkey {}", item.name),
                            vec![item.lnk.clone()],
                        )
                        .map(|_| format!("Deleted {}.", item.name)),
                    Ask::Restore(dir) => restore(ctx, &dir).map(|n| format!("Restored {n} hotkeys.")),
                };
                self.done(r);
            }
        }
        self.draft_window(ui, ctx);

        let items = self.items.clone().unwrap_or_default();
        let selected = self
            .selected
            .as_ref()
            .and_then(|p| items.iter().find(|i| &i.lnk == p))
            .cloned();
        let usable = selected.as_ref().is_some_and(|i| !i.all_users || ctx.elevated);
        let on_label = if selected.as_ref().is_some_and(|i| i.off) {
            "Turn on"
        } else {
            "Turn off"
        };
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "New hotkey")),
                Some(ToolButton::new(icons::KEYBOARD, "Change").enabled(usable)),
                Some(
                    ToolButton::new(icons::POWER, on_label)
                        .enabled(usable && selected.as_ref().is_some_and(|i| i.hotkey != 0)),
                ),
                Some(
                    ToolButton::new(icons::TRASH, "Delete")
                        .enabled(usable && selected.as_ref().is_some_and(|i| i.ours)),
                ),
                None,
                Some(ToolButton::new(icons::FLOPPY_DISK, "Back up").enabled(!items.is_empty())),
                Some(ToolButton::new(icons::CLOCK_COUNTER_CLOCKWISE, "Restore")),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
            |_| {},
        ) {
            Some(0) => self.draft = Some(Draft::default()),
            Some(1) => {
                if let Some(i) = &selected {
                    let keys = keys();
                    let m = i.hotkey >> 8;
                    self.draft = Some(Draft {
                        name: i.name.clone(),
                        ctrl: i.hotkey == 0 || m & HOTKEYF_CONTROL != 0,
                        alt: i.hotkey == 0 || m & HOTKEYF_ALT != 0,
                        shift: m & HOTKEYF_SHIFT != 0,
                        key: keys
                            .iter()
                            .position(|(_, k)| u16::from(*k) == i.hotkey & 0xFF)
                            .unwrap_or(0),
                        editing: Some(i.clone()),
                        ..Draft::default()
                    });
                }
            }
            Some(2) => {
                if let Some(i) = &selected {
                    if ctx.config.dry_run {
                        self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    } else {
                        let r = set_on(ctx, i, i.off).map(|_| {
                            format!(
                                "{} {}.",
                                if i.off { "Turned on" } else { "Turned off" },
                                format_hotkey(i.hotkey)
                            )
                        });
                        self.done(r);
                    }
                }
            }
            Some(3) => {
                if let Some(i) = &selected {
                    self.confirm.ask(
                        format!("Delete the hotkey \"{}\"?", i.name),
                        Ask::Delete(Box::new(i.clone())),
                    );
                }
            }
            Some(5) => {
                if let Some(parent) = rfd::FileDialog::new()
                    .set_title("Where to save the backup")
                    .pick_folder()
                {
                    self.message = Some(
                        backup(&parent, &items)
                            .map(|d| format!("Saved to {}.", d.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(6) => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a hotkey backup folder")
                    .pick_folder()
                {
                    self.confirm.ask(
                        format!("Restore hotkeys from {}?", dir.display()),
                        Ask::Restore(dir),
                    );
                }
            }
            Some(7) => self.items = None,
            _ => {}
        }
        let Some(items) = self.items.clone() else {
            frame::central(ui, |ui| {
                show_message(ui, &self.message);
                ui.spinner();
            });
            return;
        };
        let conflicts = items
            .iter()
            .filter(|i| {
                !i.off && i.hotkey != 0 && items.iter().filter(|o| !o.off && o.hotkey == i.hotkey).count() > 1
            })
            .count();
        let status = format!(
            "{} hotkeys, {} turned off{}",
            items.iter().filter(|i| i.hotkey != 0 && !i.off).count(),
            items.iter().filter(|i| i.off).count(),
            if conflicts > 0 {
                format!(", {conflicts} in conflict")
            } else {
                String::new()
            }
        );
        frame::status(ui, &status, |ui| {
            ui.weak("Hotkeys live in shortcuts, so Windows handles them even when this app is closed.");
        });
        frame::central_fixed(ui, |ui| self.table(ui, &items));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl HotkeyManager {
    fn table(&mut self, ui: &mut egui::Ui, items: &[HotkeyItem]) {
        show_message(ui, &self.message);
        let ours = our_folder();
        let mut clicked = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(150.0))
            .column(Column::initial(200.0).clip(true))
            .column(Column::initial(110.0))
            .column(Column::initial(90.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["Hotkey", "Name", "Where", "State", "Opens"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(21.0, items.len(), |mut row| {
                    let i = &items[row.index()];
                    row.set_selected(self.selected.as_ref() == Some(&i.lnk));
                    let dup = !i.off
                        && i.hotkey != 0
                        && items.iter().filter(|o| !o.off && o.hotkey == i.hotkey).count() > 1;
                    row.col(|ui| {
                        ui.strong(format_hotkey(i.hotkey));
                    });
                    row.col(|ui| {
                        ui.label(&i.name);
                    });
                    row.col(|ui| {
                        ui.weak(if i.lnk.starts_with(&ours) {
                            "This app"
                        } else if i.lnk.to_string_lossy().contains("Start Menu") {
                            "Start menu"
                        } else {
                            "Desktop"
                        });
                    });
                    row.col(|ui| {
                        if dup {
                            ui.colored_label(ui.visuals().warn_fg_color, "Conflict");
                        } else if i.off {
                            ui.weak("Off");
                        } else if i.hotkey == 0 {
                            ui.weak("No hotkey");
                        } else {
                            ui.label("On");
                        }
                    });
                    row.col(|ui| {
                        ui.weak(&i.target);
                    });
                    if row.response().clicked() {
                        clicked = Some(i.lnk.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_and_formatting() {
        let n = keys().iter().position(|(l, _)| l == "N").unwrap();
        let hk = encode(true, true, false, keys()[n].1);
        assert_eq!(hk, 0x064E);
        assert_eq!(format_hotkey(hk), "Ctrl+Alt+N");
        assert_eq!(format_hotkey(encode(true, false, true, 0x70)), "Ctrl+Shift+F1");
        assert_eq!(format_hotkey(encode(false, true, false, 0x24)), "Alt+Home");
        assert!(validate(hk).is_ok());
        assert!(validate(encode(false, false, true, b'A')).is_err());
        assert_eq!(format_hotkey(0), "");
        let mut labels: Vec<String> = keys().into_iter().map(|(l, _)| l).collect();
        let n = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), n);
    }

    #[test]
    fn system_wide_probe() {
        // An obscure combination is normally free, and probing does not keep it.
        let hk = encode(true, true, true, 0x87); // Ctrl+Alt+Shift+F24
        assert!(free_system_wide(hk));
        assert!(free_system_wide(hk), "the probe releases the hotkey");
    }

    #[test]
    fn target_specs() {
        let url = target_spec(&Target::Url("https://example.com".into())).unwrap();
        assert!(
            url.target
                .to_string_lossy()
                .to_lowercase()
                .ends_with("explorer.exe")
        );
        assert_eq!(url.args, "\"https://example.com\"");
        assert!(target_spec(&Target::Url("example".into())).is_err());
        let app = target_spec(&Target::App(
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into(),
        ))
        .unwrap();
        assert!(
            app.args
                .starts_with(r"shell:AppsFolder\Microsoft.WindowsCalculator")
        );
        let u = utilities();
        assert!(u.iter().any(|(n, _, _)| n.contains("not responding")));
        let t = target_spec(&Target::Utility(0)).unwrap();
        assert!(t.target.exists());
        assert!(
            target_spec(&Target::Path {
                path: " ".into(),
                args: String::new()
            })
            .is_err()
        );
    }

    #[test]
    fn create_turn_off_on_backup_restore_in_real_start_menu() {
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
        let name = format!("WM Test HK {}", std::process::id());
        let lnk_path = our_folder().join(format!("{name}.lnk"));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
                let mut s: Saved = tool_settings::load(TOOL);
                s.off.retain(|(l, _)| l != &self.0);
                let _ = tool_settings::save(TOOL, &s);
            }
        }
        let _c = Cleanup(lnk_path.clone());
        let hk = encode(true, true, true, 0x87);
        let lnk = create(&ctx, &name, &Target::Utility(0), hk).unwrap();
        let find = || scan().into_iter().find(|i| i.lnk == lnk).unwrap();
        let item = find();
        assert_eq!((item.hotkey, item.off, item.ours), (hk, false, true));
        set_on(&ctx, &item, false).unwrap();
        let off = find();
        assert!(off.off && off.hotkey == hk, "the hotkey is remembered while off");
        assert_eq!(read_shortcut(&lnk).unwrap().hotkey, 0);
        set_on(&ctx, &off, true).unwrap();
        assert!(!find().off);

        let b = backup(dir.path(), &[find()]).unwrap();
        std::fs::remove_file(&lnk).unwrap();
        assert_eq!(restore(&ctx, &b).unwrap(), 1);
        assert_eq!(read_shortcut(&lnk).unwrap().hotkey, hk);
        assert!(restore(&ctx, dir.path()).is_err(), "not a backup");
    }
}
