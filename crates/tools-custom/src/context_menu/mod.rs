//! Context Menu Manager: the classic context menus of 10 shell locations plus any
//! file type, the Open With list, the New menu and Send To.
//!
//! Commands are disabled with the documented `LegacyDisable` value; handlers by
//! prefixing their CLSID with `-` (the convention used by Windows tools). New-menu
//! items are disabled by renaming `ShellNew`, Send To items by moving them aside.
//! Everything is journaled and can be undone.

mod builtins;
mod view;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::UI::Shell::SHLoadIndirectString;
use windows::core::HSTRING;
use wm_core::{
    AppCtx, icons,
    paths::{definitely_missing, expand_env},
    process::{exe_from_command, hidden_command, run_checked},
    registry,
    safety::UndoAction,
};

pub use view::ContextMenuManager;

pub(crate) const TOOL: &str = "contextmenu";
/// Prefix of the keys this tool creates.
pub(crate) const OURS: &str = "WinManager.";

/// A shell location and the classes whose menus appear there.
pub struct Location {
    pub id: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub classes: &'static [&'static str],
    /// Commands receive the folder itself (`%V`) rather than a selected item (`%1`).
    pub background: bool,
}

pub const LOCATIONS: [Location; 10] = [
    Location {
        id: "files",
        label: "File",
        icon: icons::FILE_TEXT,
        classes: &["*"],
        background: false,
    },
    Location {
        id: "all",
        label: "All File System Objects",
        icon: icons::COPY,
        classes: &["AllFilesystemObjects"],
        background: false,
    },
    Location {
        id: "folders",
        label: "Folder",
        icon: icons::FOLDERS,
        classes: &["Directory", "Folder"],
        background: false,
    },
    Location {
        id: "background",
        label: "Directory Background",
        icon: icons::FOLDER_OPEN,
        classes: &[r"Directory\Background"],
        background: true,
    },
    Location {
        id: "desktop",
        label: "Desktop",
        icon: icons::DESKTOP,
        classes: &["DesktopBackground"],
        background: true,
    },
    Location {
        id: "drives",
        label: "Drive",
        icon: icons::HARD_DRIVES,
        classes: &["Drive"],
        background: false,
    },
    Location {
        id: "thispc",
        label: "This PC",
        icon: icons::MONITOR,
        classes: &[r"CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}"],
        background: true,
    },
    Location {
        id: "recyclebin",
        label: "Recycle Bin",
        icon: icons::RECYCLE,
        classes: &[r"CLSID\{645FF040-5081-101B-9F08-00AA002F954E}"],
        background: true,
    },
    Location {
        id: "network",
        label: "Network",
        icon: icons::NETWORK,
        classes: &["Network", r"CLSID\{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}"],
        background: true,
    },
    Location {
        id: "libraries",
        label: "Library Folder",
        icon: icons::STACK,
        classes: &["LibraryFolder", r"LibraryFolder\background"],
        background: false,
    },
];

/// Commands that Explorer itself relies on; never offered for disabling or deleting.
const CORE_VERBS: &[&str] = &[
    "open",
    "opennewwindow",
    "opennewprocess",
    "opennewtab",
    "explore",
    "runas",
    "find",
    "pintohome",
    "properties",
];

const CLASSIC_MENU_KEY: &str = r"HKCU\Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}";
const ROOTS: [&str; 2] = [r"HKCU\Software\Classes", r"HKLM\SOFTWARE\Classes"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Handler,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Class path the entry is registered under (`*`, `Directory\Background`, `.txt`…).
    pub class: String,
    pub kind: Kind,
    /// Full registry key of the entry (HKCU\Software\Classes\… or HKLM\SOFTWARE\Classes\…).
    pub key: String,
    pub name: String,
    pub display: String,
    pub enabled: bool,
    /// Handler CLSID (for handlers whose default value holds it).
    pub clsid: Option<String>,
    /// Only shown with Shift held (`Extended`).
    pub extended: bool,
    /// `Top`, `Bottom` or empty.
    pub position: String,
    /// The program or DLL the entry runs.
    pub target: Option<String>,
    pub broken: bool,
}

impl Entry {
    pub fn protected(&self) -> bool {
        self.kind == Kind::Command && CORE_VERBS.iter().any(|v| v.eq_ignore_ascii_case(&self.name))
    }

    /// Handlers whose key *name* is the CLSID (empty default) cannot be toggled by value.
    pub fn toggleable(&self) -> bool {
        !self.protected() && (self.kind == Kind::Command || self.clsid.is_some())
    }

    pub fn ours(&self) -> bool {
        self.name.starts_with(OURS)
    }

    pub fn writable(&self, elevated: bool) -> bool {
        elevated || !registry::needs_admin(&self.key)
    }
}

/// Resolves `@dll,-123` resource strings to text.
pub(crate) fn resolve_indirect(s: &str) -> String {
    if !s.starts_with('@') {
        return s.to_owned();
    }
    let mut buf = [0u16; 512];
    match unsafe { SHLoadIndirectString(&HSTRING::from(s), &mut buf, None) } {
        Ok(()) => {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        }
        Err(_) => s.to_owned(),
    }
}

/// Strips menu accelerators: "&Open with Code" → "Open with Code".
pub fn strip_accelerator(s: &str) -> String {
    s.replace("&&", "\u{0}").replace('&', "").replace('\u{0}', "&")
}

pub(crate) fn read_string(key: &str, name: &str) -> Option<String> {
    registry::read_raw(key, name)
        .map(|v| registry::decode_string(&v))
        .filter(|s| !s.trim().is_empty())
}

fn clsid_name(clsid: &str) -> Option<String> {
    let clean = clsid.trim_start_matches('-');
    read_string(&format!(r"HKCR\CLSID\{clean}"), "").map(|n| resolve_indirect(&n))
}

pub fn is_clsid(s: &str) -> bool {
    let s = s.trim_start_matches('-');
    s.len() == 38 && s.starts_with('{') && s.ends_with('}')
}

/// Missing program, also trying `.exe` for names without an extension.
pub(crate) fn target_missing(path: &str) -> bool {
    definitely_missing(path)
        && (Path::new(path).extension().is_some() || definitely_missing(&format!("{path}.exe")))
}

fn handler_dll(clsid: &str) -> Option<String> {
    let clean = clsid.trim_start_matches('-');
    read_string(&format!(r"HKCR\CLSID\{clean}\InprocServer32"), "").map(|p| expand_env(p.trim_matches('"')))
}

fn command_target(verb_key: &str) -> Option<String> {
    let cmd_key = format!(r"{verb_key}\command");
    if let Some(cmd) = read_string(&cmd_key, "") {
        // A plain path with spaces is a valid command too.
        let whole = expand_env(cmd.trim().trim_matches('"'));
        if !whole.contains('"') && Path::new(&whole).exists() {
            return Some(whole);
        }
        return exe_from_command(&cmd).map(|e| expand_env(&e));
    }
    read_string(&cmd_key, "DelegateExecute").and_then(|c| handler_dll(&c))
}

fn verb_entry(class: &str, key: String, verb: String) -> Entry {
    let display = read_string(&key, "MUIVerb")
        .or_else(|| read_string(&key, ""))
        .map(|d| strip_accelerator(&resolve_indirect(&d)))
        .unwrap_or_else(|| verb.clone());
    let target = command_target(&key);
    Entry {
        class: class.to_owned(),
        kind: Kind::Command,
        enabled: registry::read_raw(&key, "LegacyDisable").is_none(),
        extended: registry::read_raw(&key, "Extended").is_some(),
        position: read_string(&key, "Position").unwrap_or_default(),
        broken: target.as_deref().is_some_and(target_missing),
        target,
        key,
        name: verb,
        display,
        clsid: None,
    }
}

/// Every command and handler registered for these classes (both hives).
pub fn enumerate(classes: &[String]) -> Vec<Entry> {
    let mut out = Vec::new();
    for class in classes {
        for root in ROOTS {
            let shell = format!(r"{root}\{class}\shell");
            if let Ok(k) = registry::open_read(&shell) {
                for verb in k.enum_keys().flatten() {
                    out.push(verb_entry(class, format!(r"{shell}\{verb}"), verb));
                }
            }
            let handlers = format!(r"{root}\{class}\shellex\ContextMenuHandlers");
            if let Ok(k) = registry::open_read(&handlers) {
                for name in k.enum_keys().flatten() {
                    let key = format!(r"{handlers}\{name}");
                    let default = read_string(&key, "").filter(|d| is_clsid(d));
                    let clsid_for_name = default.clone().or_else(|| is_clsid(&name).then(|| name.clone()));
                    let display = clsid_for_name
                        .as_deref()
                        .and_then(clsid_name)
                        .unwrap_or_else(|| name.clone());
                    let target = clsid_for_name.as_deref().and_then(handler_dll);
                    out.push(Entry {
                        class: class.clone(),
                        kind: Kind::Handler,
                        enabled: !default.as_deref().is_some_and(|d| d.starts_with('-')),
                        extended: false,
                        position: String::new(),
                        broken: target.as_deref().is_some_and(definitely_missing),
                        target,
                        key,
                        name,
                        display,
                        clsid: default,
                    });
                }
            }
        }
    }
    out
}

/// Classes whose menus apply to files with this extension: the extension, its
/// program id, the user's chosen program id, and the per-extension / per-type
/// `SystemFileAssociations` entries.
pub fn file_type_classes(ext: &str) -> Vec<String> {
    let ext = format!(".{}", ext.trim().trim_start_matches('.').to_lowercase());
    let mut v = vec![ext.clone()];
    if let Some(p) = read_string(&format!(r"HKCR\{ext}"), "") {
        v.push(p);
    }
    let choice =
        format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}\UserChoice");
    if let Some(p) = read_string(&choice, "ProgId") {
        v.push(p);
    }
    v.push(format!(r"SystemFileAssociations\{ext}"));
    if let Some(t) = read_string(&format!(r"HKCR\{ext}"), "PerceivedType") {
        v.push(format!(r"SystemFileAssociations\{t}"));
    }
    v.dedup();
    v
}

fn set_value_journaled(
    ctx: &AppCtx<'_>,
    key: &str,
    name: &str,
    data: Option<registry::RawValue>,
    what: String,
) -> Result<()> {
    let snap = registry::snapshot(key, name);
    match &data {
        Some(v) => registry::write_raw(key, name, v)?,
        None => registry::delete_value(key, name)?,
    }
    ctx.journal
        .record(TOOL, what, UndoAction::RestoreRegValues { values: vec![snap] })?;
    Ok(())
}

/// Value change that toggles an entry: (value name, new data or None to delete).
pub fn toggle_change(entry: &Entry, enable: bool) -> Option<(String, Option<registry::RawValue>)> {
    match entry.kind {
        Kind::Command => Some(("LegacyDisable".into(), (!enable).then(|| registry::string("")))),
        Kind::Handler => {
            let clsid = entry.clsid.as_deref()?.trim_start_matches('-').to_owned();
            let value = if enable { clsid } else { format!("-{clsid}") };
            Some((String::new(), Some(registry::string(&value))))
        }
    }
}

pub fn set_enabled(ctx: &AppCtx<'_>, e: &Entry, enable: bool) -> Result<()> {
    let Some((name, data)) = toggle_change(e, enable) else {
        bail!("\"{}\" cannot be switched off this way", e.display)
    };
    set_value_journaled(
        ctx,
        &e.key,
        &name,
        data,
        format!(
            "{} context menu item \"{}\" ({})",
            if enable { "Enabled" } else { "Disabled" },
            e.display,
            e.class
        ),
    )
}

pub fn set_extended(ctx: &AppCtx<'_>, e: &Entry, shift_only: bool) -> Result<()> {
    set_value_journaled(
        ctx,
        &e.key,
        "Extended",
        shift_only.then(|| registry::string("")),
        format!(
            "\"{}\" now shows {}",
            e.display,
            if shift_only { "only with Shift" } else { "always" }
        ),
    )
}

pub fn set_position(ctx: &AppCtx<'_>, e: &Entry, position: &str) -> Result<()> {
    set_value_journaled(
        ctx,
        &e.key,
        "Position",
        (!position.is_empty()).then(|| registry::string(position)),
        format!(
            "Moved \"{}\" to {}",
            e.display,
            if position.is_empty() {
                "its default place"
            } else {
                position
            }
        ),
    )
}

pub(crate) fn delete_key_journaled(ctx: &AppCtx<'_>, key: &str, what: String) -> Result<()> {
    let undo = ctx.journal.backup_reg_key(key)?;
    run_checked(hidden_command("reg").args(["delete", key, "/f"]))?;
    ctx.journal.record(TOOL, what, undo)?;
    Ok(())
}

pub fn delete_entry(ctx: &AppCtx<'_>, e: &Entry) -> Result<()> {
    if e.protected() {
        bail!("\"{}\" is a core Explorer command", e.display);
    }
    delete_key_journaled(
        ctx,
        &e.key,
        format!("Removed context menu item \"{}\" ({})", e.display, e.class),
    )
}

pub fn classic_menu_enabled() -> bool {
    registry::read_raw(&format!(r"{CLASSIC_MENU_KEY}\InprocServer32"), "").is_some()
}

pub fn set_classic_menu(ctx: &AppCtx<'_>, on: bool) -> Result<()> {
    let undo = ctx.journal.backup_reg_key(CLASSIC_MENU_KEY)?;
    if on {
        registry::write_raw(
            &format!(r"{CLASSIC_MENU_KEY}\InprocServer32"),
            "",
            &registry::string(""),
        )?;
    } else {
        run_checked(hidden_command("reg").args(["delete", CLASSIC_MENU_KEY, "/f"]))?;
    }
    ctx.journal.record(
        TOOL,
        if on {
            "Enabled the classic context menu"
        } else {
            "Restored the Windows 11 context menu"
        },
        undo,
    )?;
    Ok(())
}

// --- Open With -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct OpenWithApp {
    pub key: String,
    pub exe: String,
    pub display: String,
    pub target: Option<String>,
    /// `NoOpenWith`: left out of every "Open with" list.
    pub hidden: bool,
    pub broken: bool,
}

pub fn enumerate_open_with() -> Vec<OpenWithApp> {
    let mut out: Vec<OpenWithApp> = Vec::new();
    for root in ROOTS {
        let base = format!(r"{root}\Applications");
        let Ok(k) = registry::open_read(&base) else {
            continue;
        };
        for exe in k.enum_keys().flatten() {
            let key = format!(r"{base}\{exe}");
            let target = command_target(&format!(r"{key}\shell\open"));
            if target.is_none() && registry::open_read(&format!(r"{key}\shell")).is_err() {
                continue; // capability-only entries never show in Open with
            }
            let display = read_string(&key, "FriendlyAppName")
                .map(|n| resolve_indirect(&n))
                .unwrap_or_else(|| exe.clone());
            out.push(OpenWithApp {
                hidden: registry::read_raw(&key, "NoOpenWith").is_some(),
                broken: target.as_deref().is_some_and(target_missing),
                target,
                key,
                exe,
                display,
            });
        }
    }
    out.sort_by_key(|a| a.display.to_lowercase());
    out
}

pub fn set_open_with_hidden(ctx: &AppCtx<'_>, app: &OpenWithApp, hide: bool) -> Result<()> {
    // Written to the app's own key: a per-user copy of a machine-wide key would hide
    // the machine-wide settings it relies on.
    set_value_journaled(
        ctx,
        &app.key,
        "NoOpenWith",
        hide.then(|| registry::string("")),
        format!(
            "{} \"{}\" in Open with",
            if hide { "Hid" } else { "Showed" },
            app.display
        ),
    )
}

// --- New menu --------------------------------------------------------------------

/// Name the `ShellNew` key gets while its item is turned off.
pub const SHELLNEW_OFF: &str = "ShellNew.WinManagerOff";

#[derive(Debug, Clone, PartialEq)]
pub struct NewItem {
    pub ext: String,
    /// Key that holds (or held) `ShellNew`.
    pub parent: String,
    pub display: String,
    pub enabled: bool,
}

impl NewItem {
    pub fn key(&self) -> String {
        format!(
            r"{}\{}",
            self.parent,
            if self.enabled { "ShellNew" } else { SHELLNEW_OFF }
        )
    }
}

fn type_name(ext: &str, parent: &str) -> String {
    let progid = read_string(&format!(r"HKCR\{ext}"), "");
    let leaf = parent.rsplit('\\').next().unwrap_or_default();
    let id = if leaf.starts_with('.') {
        progid
    } else {
        Some(leaf.to_owned())
    };
    id.and_then(|p| {
        read_string(&format!(r"HKCR\{p}"), "FriendlyTypeName")
            .or_else(|| read_string(&format!(r"HKCR\{p}"), ""))
    })
    .map(|n| resolve_indirect(&n))
    .unwrap_or_else(|| format!("{} file", ext.trim_start_matches('.').to_uppercase()))
}

pub fn enumerate_new_items() -> Vec<NewItem> {
    let mut out = Vec::new();
    for root in ROOTS {
        let Ok(classes) = registry::open_read(root) else {
            continue;
        };
        for ext in classes.enum_keys().flatten().filter(|k| k.starts_with('.')) {
            let base = format!(r"{root}\{ext}");
            let mut parents = vec![base.clone()];
            parents.extend(
                subkeys(&base)
                    .into_iter()
                    .filter(|s| !s.starts_with("ShellNew"))
                    .map(|s| format!(r"{base}\{s}")),
            );
            for parent in parents {
                for (name, enabled) in [("ShellNew", true), (SHELLNEW_OFF, false)] {
                    if registry::open_read(&format!(r"{parent}\{name}")).is_ok() {
                        out.push(NewItem {
                            display: type_name(&ext, &parent),
                            ext: ext.clone(),
                            parent: parent.clone(),
                            enabled,
                        });
                    }
                }
            }
        }
    }
    out.sort_by_key(|i| i.display.to_lowercase());
    out
}

fn subkeys(key: &str) -> Vec<String> {
    registry::open_read(key)
        .map(|k| k.enum_keys().flatten().collect())
        .unwrap_or_default()
}

pub fn set_new_item_enabled(ctx: &AppCtx<'_>, item: &NewItem, enable: bool) -> Result<()> {
    let (from, to) = if enable {
        (SHELLNEW_OFF, "ShellNew")
    } else {
        ("ShellNew", SHELLNEW_OFF)
    };
    let src = format!(r"{}\{from}", item.parent);
    let dst = format!(r"{}\{to}", item.parent);
    // Undo: put the source back, then remove the renamed copy.
    let undo = UndoAction::Sequence {
        steps: vec![
            ctx.journal.backup_reg_key(&src)?,
            ctx.journal.backup_reg_key(&dst)?,
        ],
    };
    run_checked(hidden_command("reg").args(["copy", &src, &dst, "/s", "/f"]))?;
    run_checked(hidden_command("reg").args(["delete", &src, "/f"]))?;
    ctx.journal.record(
        TOOL,
        format!(
            "{} \"{}\" in the New menu",
            if enable { "Showed" } else { "Hid" },
            item.display
        ),
        undo,
    )?;
    Ok(())
}

/// Adds an empty-file entry for `ext` to the New menu, in the hive that already
/// holds the extension (a new per-user key could hide the machine-wide file type).
pub fn add_new_item(ctx: &AppCtx<'_>, ext: &str) -> Result<String> {
    let ext = format!(".{}", ext.trim().trim_start_matches('.').to_lowercase());
    if ext.len() < 2 || ext[1..].contains(['\\', '/', ' ', '.']) {
        bail!("\"{ext}\" is not a file extension");
    }
    let user = format!(r"HKCU\Software\Classes\{ext}");
    let machine = format!(r"HKLM\SOFTWARE\Classes\{ext}");
    let parent = if registry::open_read(&user).is_ok() || registry::open_read(&machine).is_err() {
        user
    } else if ctx.elevated {
        machine
    } else {
        bail!("{ext} is registered for all users; restart as administrator to add it to the New menu");
    };
    let key = format!(r"{parent}\ShellNew");
    let undo = ctx.journal.backup_reg_key(&key)?;
    registry::write_raw(&key, "NullFile", &registry::string(""))?;
    ctx.journal
        .record(TOOL, format!("Added {ext} to the New menu"), undo)?;
    Ok(type_name(&ext, &parent))
}

// --- Send To ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SendToItem {
    pub path: PathBuf,
    pub display: String,
    pub enabled: bool,
}

pub fn send_to_dir() -> PathBuf {
    PathBuf::from(expand_env(r"%APPDATA%\Microsoft\Windows\SendTo"))
}

fn send_to_off_dir() -> Result<PathBuf> {
    Ok(wm_core::paths::data_dir()?.join("SendTo (turned off)"))
}

fn list_dir(dir: &Path, enabled: bool) -> Vec<SendToItem> {
    std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    !p.file_name()
                        .is_some_and(|n| n.eq_ignore_ascii_case("desktop.ini"))
                })
                .map(|path| SendToItem {
                    display: path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path,
                    enabled,
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn enumerate_send_to() -> Vec<SendToItem> {
    let mut v = list_dir(&send_to_dir(), true);
    if let Ok(off) = send_to_off_dir() {
        v.extend(list_dir(&off, false));
    }
    v.sort_by_key(|i| i.display.to_lowercase());
    v
}

pub fn set_send_to_enabled(ctx: &AppCtx<'_>, item: &SendToItem, enable: bool) -> Result<()> {
    let dir = if enable { send_to_dir() } else { send_to_off_dir()? };
    std::fs::create_dir_all(&dir)?;
    let name = item.path.file_name().context("no file name")?;
    let to = dir.join(name);
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    wm_core::paths::move_path(&item.path, &to)?;
    ctx.journal.record(
        TOOL,
        format!(
            "{} \"{}\" in Send To",
            if enable { "Showed" } else { "Hid" },
            item.display
        ),
        UndoAction::MoveBack {
            moves: vec![(item.path.clone(), to)],
        },
    )?;
    Ok(())
}

/// Adds a shortcut to a program or folder to the Send To menu.
pub fn add_send_to(ctx: &AppCtx<'_>, target: &Path, name: &str) -> Result<PathBuf> {
    let lnk = send_to_dir().join(format!("{}.lnk", name.trim()));
    if lnk.exists() {
        bail!("{} already exists in Send To", name.trim());
    }
    wm_core::shell::create_shortcut(
        &lnk,
        &wm_core::shell::ShortcutSpec {
            target: target.to_owned(),
            ..Default::default()
        },
    )?;
    ctx.journal.record(
        TOOL,
        format!("Added \"{}\" to Send To", name.trim()),
        UndoAction::DeleteCreatedFiles {
            paths: vec![lnk.clone()],
        },
    )?;
    Ok(lnk)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn with_ctx<R>(f: impl FnOnce(&AppCtx<'_>, &wm_core::safety::ChangeJournal) -> R) -> R {
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
        f(&ctx, &journal)
    }

    #[test]
    fn helpers() {
        assert_eq!(strip_accelerator("&Open with Code"), "Open with Code");
        assert_eq!(strip_accelerator("Save && Exit"), "Save & Exit");
        assert!(is_clsid("{09A47860-11B0-4DA5-AFA5-26D86198A780}"));
        assert!(is_clsid("-{09A47860-11B0-4DA5-AFA5-26D86198A780}"));
        assert!(!is_clsid("EPP"));
        assert!(!target_missing(r"%SystemRoot%\system32\notepad"), ".exe implied");
        assert!(target_missing(r"C:\Windows\no-such-wm.exe"));
        let classes = file_type_classes("TXT");
        assert_eq!(classes[0], ".txt");
        assert!(classes.contains(&r"SystemFileAssociations\.txt".to_owned()));
    }

    #[test]
    fn toggle_values() {
        let cmd = Entry {
            class: "*".into(),
            kind: Kind::Command,
            key: r"HKCU\Software\Classes\*\shell\x".into(),
            name: "x".into(),
            display: "X".into(),
            enabled: true,
            clsid: None,
            extended: false,
            position: String::new(),
            target: None,
            broken: false,
        };
        assert_eq!(
            toggle_change(&cmd, false),
            Some(("LegacyDisable".into(), Some(registry::string(""))))
        );
        assert_eq!(toggle_change(&cmd, true), Some(("LegacyDisable".into(), None)));
        let h = Entry {
            kind: Kind::Handler,
            clsid: Some("{A}".into()),
            ..cmd.clone()
        };
        assert_eq!(
            toggle_change(&h, false).unwrap().1,
            Some(registry::string("-{A}"))
        );
        let disabled = Entry {
            clsid: Some("-{A}".into()),
            ..h
        };
        assert_eq!(
            toggle_change(&disabled, true).unwrap().1,
            Some(registry::string("{A}"))
        );
        assert!(
            !Entry {
                name: "open".into(),
                ..cmd
            }
            .toggleable()
        );
    }

    #[test]
    fn real_user_verb_toggle_shift_position_delete_and_undo() {
        let class = r"Directory\Background";
        let key = format!(
            r"HKCU\Software\Classes\{class}\shell\wm-test-{}",
            std::process::id()
        );
        registry::write_raw(&key, "MUIVerb", &registry::string("WM Test")).unwrap();
        registry::write_raw(
            &format!(r"{key}\command"),
            "",
            &registry::string(r#""C:\Windows\no-such-wm.exe" "%V""#),
        )
        .unwrap();
        let find = || enumerate(&[class.to_owned()]).into_iter().find(|e| e.key == key);
        let e = find().expect("test verb listed");
        assert!(e.enabled && e.broken && e.display == "WM Test");
        with_ctx(|ctx, journal| {
            set_enabled(ctx, &e, false).unwrap();
            set_extended(ctx, &e, true).unwrap();
            set_position(ctx, &e, "Top").unwrap();
            let now = find().unwrap();
            assert!(!now.enabled && now.extended && now.position == "Top");
            for entry in journal.entries() {
                journal.undo(entry.id).unwrap();
            }
            let back = find().unwrap();
            assert!(back.enabled && !back.extended && back.position.is_empty());
            delete_entry(ctx, &back).unwrap();
            assert!(find().is_none());
            journal.undo(journal.entries()[0].id).unwrap();
            assert!(find().is_some(), "undo re-imports the deleted verb");
        });
        let (root, sub) = registry::split_path(&key).unwrap();
        root.delete_subkey_all(sub).unwrap();
    }

    #[test]
    fn new_menu_item_add_hide_show_undo() {
        let ext = format!("wmt{}", std::process::id());
        let key = format!(r"HKCU\Software\Classes\.{ext}");
        with_ctx(|ctx, journal| {
            add_new_item(ctx, &ext).unwrap();
            let find = || {
                enumerate_new_items()
                    .into_iter()
                    .find(|i| i.ext == format!(".{ext}"))
            };
            let item = find().expect("listed");
            assert!(item.enabled);
            set_new_item_enabled(ctx, &item, false).unwrap();
            let off = find().unwrap();
            assert!(!off.enabled && registry::open_read(&format!(r"{key}\ShellNew")).is_err());
            set_new_item_enabled(ctx, &off, true).unwrap();
            assert!(find().unwrap().enabled);
            // Undo everything, newest first: the extension key ends up empty.
            for e in journal.entries() {
                journal.undo(e.id).unwrap();
            }
            assert!(find().is_none());
        });
        let (root, sub) = registry::split_path(&key).unwrap();
        let _ = root.delete_subkey_all(sub);
        assert!(add_new_item_rejects("a b"));
    }

    fn add_new_item_rejects(ext: &str) -> bool {
        with_ctx(|ctx, _| add_new_item(ctx, ext).is_err())
    }

    #[test]
    fn real_lists_load() {
        assert!(
            enumerate(&["Directory".to_owned()])
                .iter()
                .any(|e| e.kind == Kind::Command)
        );
        let apps = enumerate_open_with();
        assert!(apps.iter().all(|a| !a.exe.is_empty()));
        assert!(
            enumerate_new_items()
                .iter()
                .any(|i| i.ext == ".txt" || i.ext == ".zip" || i.ext == ".bmp")
        );
        let send = enumerate_send_to();
        assert!(send.iter().all(|s| !s.display.is_empty()));
    }
}
