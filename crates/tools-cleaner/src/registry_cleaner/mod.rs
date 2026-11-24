//! Registry Cleaner (conservative): only flags entries that point at files, folders or
//! keys which *definitely* no longer exist (on a drive that is present). Every fix is
//! backed up: values via exact snapshots, keys via `.reg` export, optionally also one
//! combined `.reg` file and a restore point. An exclusion list keeps chosen entries
//! out of every scan, and `.reg` files can be imported with an undoable backup.

mod view;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx,
    file_icon::split_icon_location,
    paths::{expand_env, wildcard_match},
    process::{exe_from_command, hidden_command, run_checked},
    registry::{self, ValueSnapshot},
    safety::UndoAction,
    tool_settings,
};

pub use view::RegistryCleaner;

const TOOL: &str = "regclean";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Kind {
    // Programs
    AppPath,
    Uninstall,
    Applications,
    RegisteredApp,
    IfeoDebugger,
    // Startup
    StartupRun,
    // File types and shell
    FileTypeCommand,
    ContextMenu,
    DefaultIcon,
    // Components
    SharedDll,
    ComObject,
    TypeLib,
    InstallerFolder,
    // History and caches
    MuiCache,
    CompatStore,
    CompatLayers,
    FeatureUsage,
    // System
    Font,
    HelpFile,
    Sound,
    FirewallRule,
    // Other
    EmptySoftwareKey,
}

impl Kind {
    pub const ALL: [Kind; 22] = [
        Kind::AppPath,
        Kind::Uninstall,
        Kind::Applications,
        Kind::RegisteredApp,
        Kind::IfeoDebugger,
        Kind::StartupRun,
        Kind::FileTypeCommand,
        Kind::ContextMenu,
        Kind::DefaultIcon,
        Kind::SharedDll,
        Kind::ComObject,
        Kind::TypeLib,
        Kind::InstallerFolder,
        Kind::MuiCache,
        Kind::CompatStore,
        Kind::CompatLayers,
        Kind::FeatureUsage,
        Kind::Font,
        Kind::HelpFile,
        Kind::Sound,
        Kind::FirewallRule,
        Kind::EmptySoftwareKey,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::AppPath => "Application paths to missing programs",
            Kind::Uninstall => "Uninstall entries for removed programs",
            Kind::Applications => "\"Open with\" programs that are gone",
            Kind::RegisteredApp => "Default-app registrations without settings",
            Kind::IfeoDebugger => "Program launch redirects to missing debuggers",
            Kind::StartupRun => "Startup entries for missing programs",
            Kind::FileTypeCommand => "File type commands for missing programs",
            Kind::ContextMenu => "Context menu commands for missing programs",
            Kind::DefaultIcon => "File type icons from missing files",
            Kind::SharedDll => "Shared DLL references to missing files",
            Kind::ComObject => "COM objects whose file is gone",
            Kind::TypeLib => "Type libraries whose file is gone",
            Kind::InstallerFolder => "Installer folders that no longer exist",
            Kind::MuiCache => "Cached names of deleted programs",
            Kind::CompatStore => "Compatibility records of deleted programs",
            Kind::CompatLayers => "Compatibility settings of deleted programs",
            Kind::FeatureUsage => "Taskbar usage records of deleted programs",
            Kind::Font => "Font entries for missing font files",
            Kind::HelpFile => "Help file entries for missing files",
            Kind::Sound => "Sound events using missing sound files",
            Kind::FirewallRule => "Firewall rules for deleted programs",
            Kind::EmptySoftwareKey => "Empty program settings keys",
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            Kind::AppPath
            | Kind::Uninstall
            | Kind::Applications
            | Kind::RegisteredApp
            | Kind::IfeoDebugger => "Programs",
            Kind::StartupRun => "Startup",
            Kind::FileTypeCommand | Kind::ContextMenu | Kind::DefaultIcon => "File types and shell",
            Kind::SharedDll | Kind::ComObject | Kind::TypeLib | Kind::InstallerFolder => "Components",
            Kind::MuiCache | Kind::CompatStore | Kind::CompatLayers | Kind::FeatureUsage => {
                "History and caches"
            }
            Kind::Font | Kind::HelpFile | Kind::Sound | Kind::FirewallRule => "System",
            Kind::EmptySoftwareKey => "Other",
        }
    }

    /// Scanned unless the user turns them off. COM, type library and firewall entries
    /// are left to the user to opt into.
    pub fn default_on(self) -> bool {
        !matches!(self, Kind::ComObject | Kind::TypeLib | Kind::FirewallRule)
    }

    pub fn note(self) -> &'static str {
        match self {
            Kind::FirewallRule => "Takes effect after the firewall service restarts (or a reboot).",
            Kind::ComObject | Kind::TypeLib => {
                "Only entries whose registered file is missing on a present drive."
            }
            Kind::EmptySoftwareKey => "Top-level keys under your Software key with no values and no subkeys.",
            _ => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub kind: Kind,
    pub key: String,
    /// `None` = the whole key is the issue; `Some(name)` = a single value.
    pub value: Option<String>,
    pub detail: String,
}

impl Issue {
    /// `key` or `key\value`, as matched by the exclusion list.
    pub fn location(&self) -> String {
        match &self.value {
            Some(v) => format!(r"{}\{v}", self.key),
            None => self.key.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Backup {
    /// Change Journal only (every fix can be undone there).
    #[default]
    Journal,
    /// Also save one `.reg` file of every changed key to a folder.
    RegFile,
}

/// Options remembered between sessions; 1-Click Cleaner uses them too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Categories to scan; `None` = the defaults.
    pub kinds: Option<Vec<Kind>>,
    /// Keys, `key\value` entries or wildcard patterns that are never reported.
    pub exclude: Vec<String>,
    pub backup: Backup,
    pub backup_folder: Option<PathBuf>,
    /// Create a System Restore point before fixing (administrator).
    pub restore_point: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            kinds: None,
            exclude: Vec::new(),
            backup: Backup::Journal,
            backup_folder: None,
            restore_point: false,
        }
    }
}

impl Options {
    pub fn saved() -> Self {
        tool_settings::load(TOOL)
    }

    pub fn save(&self) -> Result<()> {
        tool_settings::save(TOOL, self)
    }

    pub fn kinds(&self) -> Vec<Kind> {
        self.kinds
            .clone()
            .unwrap_or_else(|| Kind::ALL.iter().copied().filter(|k| k.default_on()).collect())
    }

    pub fn excludes(&self, issue: &Issue) -> bool {
        let loc = issue.location().to_lowercase();
        let key = issue.key.to_lowercase();
        self.exclude.iter().any(|e| {
            let e = e.trim().trim_end_matches('\\').to_lowercase();
            if e.is_empty() {
                false
            } else if e.contains(['*', '?']) {
                wildcard_match(&e, &loc) || wildcard_match(&e, &issue.detail.to_lowercase())
            } else {
                loc == e || key == e || loc.starts_with(&format!("{e}\\"))
            }
        })
    }
}

pub use wm_core::paths::definitely_missing;

/// For 32-bit registrations (`WOW6432Node`, `win32` type libraries) the same text
/// can mean several places: `System32` / `SysWOW64`, and the 64-bit or `(x86)`
/// program folders (`%ProgramFiles%` expands differently for 32-bit programs).
/// Missing only if absent from every one of them.
pub fn definitely_missing_32(raw: &str) -> bool {
    let raw = raw.trim().trim_matches('"');
    let x86 = raw
        .replace("%CommonProgramFiles%", "%CommonProgramFiles(x86)%")
        .replace("%ProgramFiles%", "%ProgramFiles(x86)%");
    let mut candidates = vec![expand_env(raw).to_lowercase(), expand_env(&x86).to_lowercase()];
    let pf = expand_env("%ProgramFiles%").to_lowercase();
    let pf86 = expand_env("%ProgramFiles(x86)%").to_lowercase();
    for c in candidates.clone() {
        for (a, b) in [
            (r"\system32\", r"\syswow64\"),
            (r"\syswow64\", r"\system32\"),
            (pf.as_str(), pf86.as_str()),
            (pf86.as_str(), pf.as_str()),
        ] {
            if !a.is_empty() && c.contains(a) {
                candidates.push(c.replacen(a, b, 1));
            }
        }
    }
    candidates.iter().all(|c| definitely_missing(c))
}

/// Registrations that belong to Windows itself (left to Windows to maintain).
/// Store apps' files (`WindowsApps`) count too: app deployment maintains them.
fn windows_owned(path: &str) -> bool {
    let p = expand_env(path.trim().trim_matches('"')).to_lowercase();
    let win = expand_env("%WINDIR%").to_lowercase();
    let apps = expand_env(r"%ProgramFiles%\WindowsApps").to_lowercase();
    (!win.is_empty() && p.starts_with(&format!("{win}\\"))) || p.starts_with(&format!("{apps}\\"))
}

fn values(key: &str) -> Vec<(String, String)> {
    let Ok(k) = registry::open_read(key) else {
        return Vec::new();
    };
    k.enum_values()
        .flatten()
        .map(|(name, v)| {
            let raw = registry::RawValue {
                vtype: v.vtype.clone() as u32,
                bytes: v.bytes.into_owned(),
            };
            (name, registry::decode_string(&raw))
        })
        .collect()
}

fn default_value(key: &str) -> Option<String> {
    values(key)
        .into_iter()
        .find(|(n, _)| n.is_empty())
        .map(|(_, v)| v)
}

fn subkeys(key: &str) -> Vec<String> {
    registry::open_read(key)
        .map(|k| k.enum_keys().flatten().collect())
        .unwrap_or_default()
}

fn has_children(key: &str) -> bool {
    registry::open_read(key).is_ok_and(|k| k.enum_keys().next().is_some() || k.enum_values().next().is_some())
}

fn gone(path: &str, wow: bool) -> bool {
    if wow {
        definitely_missing_32(path)
    } else {
        definitely_missing(path)
    }
}

/// The program a command line (or a plain path with spaces) starts, if it is
/// definitely missing. Windows adds `.exe` / `.com` to names without an extension.
fn missing_exe(cmd: &str, wow: bool) -> Option<String> {
    let whole = cmd.trim().trim_matches('"');
    if !whole.contains('"') && !gone(whole, wow) && Path::new(&expand_env(whole)).is_absolute() {
        return None; // the whole value is an existing path (e.g. unquoted with spaces)
    }
    let exe = exe_from_command(cmd)?;
    let has_ext = Path::new(&exe).extension().is_some();
    let all_gone = gone(&exe, wow)
        && (has_ext || (gone(&format!("{exe}.exe"), wow) && gone(&format!("{exe}.com"), wow)));
    all_gone.then_some(exe)
}

/// A type library path without its trailing resource index (`srm.dll\2` → `srm.dll`).
pub fn typelib_file(raw: &str) -> &str {
    let t = raw.trim().trim_matches('"');
    match t.rsplit_once('\\') {
        Some((head, idx)) if !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()) => head,
        _ => t,
    }
}

fn issue(kind: Kind, key: impl Into<String>, value: Option<String>, detail: impl Into<String>) -> Issue {
    Issue {
        kind,
        key: key.into(),
        value,
        detail: detail.into(),
    }
}

fn scan_app_paths(out: &mut Vec<Issue>) {
    for root in [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths",
    ] {
        for sub in subkeys(root) {
            let key = format!(r"{root}\{sub}");
            if let Some(path) = default_value(&key)
                && definitely_missing(&path)
            {
                out.push(issue(Kind::AppPath, key, None, path));
            }
        }
    }
}

fn scan_uninstall(out: &mut Vec<Issue>) {
    for root in [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall",
    ] {
        let wow = root.contains("WOW6432Node");
        for sub in subkeys(root) {
            let key = format!(r"{root}\{sub}");
            let vals = values(&key);
            let get = |n: &str| vals.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
            let Some(cmd) = get("UninstallString") else {
                continue;
            };
            let Some(exe) = exe_from_command(&cmd) else {
                continue;
            };
            let lower = exe.to_lowercase();
            if lower.contains("msiexec") || lower.contains("rundll32") {
                continue; // system tools; validity depends on the MSI database instead
            }
            let location_gone =
                get("InstallLocation").is_none_or(|l| l.trim().is_empty() || definitely_missing(&l));
            if missing_exe(&cmd, wow).is_some() && location_gone {
                let name = get("DisplayName").unwrap_or(sub.clone());
                out.push(issue(Kind::Uninstall, key, None, format!("{name} — {exe}")));
            }
        }
    }
}

fn scan_applications(out: &mut Vec<Issue>) {
    for root in [
        r"HKCU\Software\Classes\Applications",
        r"HKLM\SOFTWARE\Classes\Applications",
    ] {
        for app in subkeys(root) {
            let key = format!(r"{root}\{app}");
            if let Some(cmd) = default_value(&format!(r"{key}\shell\open\command"))
                && let Some(exe) = missing_exe(&cmd, false)
            {
                out.push(issue(Kind::Applications, key, None, format!("{app} — {exe}")));
            }
        }
    }
}

fn scan_registered_apps(out: &mut Vec<Issue>) {
    for (hive, key) in [
        ("HKLM", r"HKLM\SOFTWARE\RegisteredApplications"),
        ("HKCU", r"HKCU\Software\RegisteredApplications"),
    ] {
        for (name, target) in values(key) {
            // Store apps' registrations are maintained by Windows' app deployment.
            if name.is_empty() || target.trim().is_empty() || target.contains(r"\AppModel\Repository\") {
                continue;
            }
            let rel = target.trim().trim_start_matches('\\');
            // Look in both hives: some programs register machine-wide but keep settings per user.
            // 32-bit programs keep machine-wide keys under WOW6432Node.
            let wow = rel
                .strip_prefix("Software\\")
                .or_else(|| rel.strip_prefix("SOFTWARE\\"))
                .map(|r| format!(r"HKLM\SOFTWARE\WOW6432Node\{r}"));
            let missing = [hive, "HKCU", "HKLM"]
                .iter()
                .map(|h| format!(r"{h}\{rel}"))
                .chain(wow)
                .all(|k| registry::open_read(&k).is_err());
            if missing {
                out.push(issue(
                    Kind::RegisteredApp,
                    key,
                    Some(name.clone()),
                    format!("{name} — {target}"),
                ));
            }
        }
    }
}

fn scan_ifeo(out: &mut Vec<Issue>) {
    let root = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";
    for exe in subkeys(root) {
        let key = format!(r"{root}\{exe}");
        if let Some((_, cmd)) = values(&key)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("Debugger"))
            && let Some(missing) = missing_exe(&cmd, false)
        {
            out.push(issue(
                Kind::IfeoDebugger,
                key,
                Some("Debugger".into()),
                format!("{exe} → {missing}"),
            ));
        }
    }
}

fn scan_run(out: &mut Vec<Issue>) {
    for key in [
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\RunOnce",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\RunOnce",
    ] {
        let wow = key.contains("WOW6432Node");
        for (name, cmd) in values(key) {
            if !name.is_empty()
                && let Some(exe) = missing_exe(&cmd, wow)
            {
                out.push(issue(Kind::StartupRun, key, Some(name), exe));
            }
        }
    }
}

/// Classes whose verbs show in Explorer's context menus.
const MENU_CLASSES: &[&str] = &["*", "AllFilesystemObjects", "Directory", "Folder", "Drive"];

fn scan_classes(kinds: &HashSet<Kind>, out: &mut Vec<Issue>) {
    let (commands, menus, icons) = (
        kinds.contains(&Kind::FileTypeCommand),
        kinds.contains(&Kind::ContextMenu),
        kinds.contains(&Kind::DefaultIcon),
    );
    for root in [r"HKCU\Software\Classes", r"HKLM\SOFTWARE\Classes"] {
        let mut classes: Vec<String> = subkeys(root)
            .into_iter()
            .filter(|c| !c.eq_ignore_ascii_case("CLSID") && !c.eq_ignore_ascii_case("TypeLib"))
            .collect();
        classes.push(r"Directory\Background".into());
        for class in classes {
            let base = format!(r"{root}\{class}");
            let menu = MENU_CLASSES.iter().any(|m| m.eq_ignore_ascii_case(&class)) || class.contains('\\');
            if (menu && menus) || (!menu && commands) {
                for verb in subkeys(&format!(r"{base}\shell")) {
                    let vkey = format!(r"{base}\shell\{verb}");
                    if let Some(cmd) = default_value(&format!(r"{vkey}\command"))
                        && let Some(exe) = missing_exe(&cmd, false)
                    {
                        let kind = if menu {
                            Kind::ContextMenu
                        } else {
                            Kind::FileTypeCommand
                        };
                        out.push(issue(kind, vkey, None, format!("{class} \"{verb}\" — {exe}")));
                    }
                }
            }
            if icons
                && let Some(icon) = default_value(&format!(r"{base}\DefaultIcon"))
                && !icon.contains("%1")
            {
                let (path, _) = split_icon_location(&icon);
                if !path.is_empty() && definitely_missing(&path) {
                    out.push(issue(
                        Kind::DefaultIcon,
                        format!(r"{base}\DefaultIcon"),
                        None,
                        format!("{class} — {path}"),
                    ));
                }
            }
        }
    }
}

fn scan_shared_dlls(out: &mut Vec<Issue>) {
    for key in [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\SharedDLLs",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\SharedDLLs",
    ] {
        let wow = key.contains("WOW6432Node");
        for (name, _) in values(key) {
            let gone = if wow {
                definitely_missing_32(&name)
            } else {
                definitely_missing(&name)
            };
            if gone {
                out.push(issue(Kind::SharedDll, key, Some(name.clone()), name));
            }
        }
    }
}

fn scan_com(out: &mut Vec<Issue>) {
    for (root, wow) in [
        (r"HKCU\Software\Classes\CLSID", false),
        (r"HKLM\SOFTWARE\Classes\CLSID", false),
        (r"HKLM\SOFTWARE\WOW6432Node\Classes\CLSID", true),
    ] {
        for clsid in subkeys(root) {
            let key = format!(r"{root}\{clsid}");
            let servers: Vec<String> = ["InprocServer32", "LocalServer32"]
                .iter()
                .filter_map(|s| default_value(&format!(r"{key}\{s}")))
                .filter(|v| !v.trim().is_empty())
                .collect();
            if servers.is_empty() {
                continue;
            }
            // Every registered server must be gone (an object may have both kinds).
            let all_gone = servers
                .iter()
                .all(|s| !windows_owned(s) && missing_exe(s, wow).is_some());
            if all_gone {
                let name = default_value(&key)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(clsid.clone());
                out.push(issue(
                    Kind::ComObject,
                    key,
                    None,
                    format!("{name} — {}", servers[0]),
                ));
            }
        }
    }
}

fn scan_typelibs(out: &mut Vec<Issue>) {
    let root = r"HKLM\SOFTWARE\Classes\TypeLib";
    for guid in subkeys(root) {
        for ver in subkeys(&format!(r"{root}\{guid}")) {
            let vkey = format!(r"{root}\{guid}\{ver}");
            let mut files = Vec::new();
            for lcid in subkeys(&vkey)
                .into_iter()
                .filter(|l| l.chars().all(|c| c.is_ascii_hexdigit()))
            {
                for plat in subkeys(&format!(r"{vkey}\{lcid}")) {
                    if let Some(f) = default_value(&format!(r"{vkey}\{lcid}\{plat}")) {
                        files.push((plat.eq_ignore_ascii_case("win32"), typelib_file(&f).to_owned()));
                    }
                }
            }
            if !files.is_empty()
                && files.iter().all(|(_, f)| !windows_owned(f))
                && files.iter().all(|(w32, f)| {
                    if *w32 {
                        definitely_missing_32(f)
                    } else {
                        definitely_missing(f)
                    }
                })
            {
                let name = default_value(&vkey)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(guid.clone());
                out.push(issue(
                    Kind::TypeLib,
                    vkey,
                    None,
                    format!("{name} — {}", files[0].1),
                ));
            }
        }
    }
}

fn scan_installer_folders(out: &mut Vec<Issue>) {
    let key = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Installer\Folders";
    for (name, _) in values(key) {
        if definitely_missing(&name) {
            out.push(issue(Kind::InstallerFolder, key, Some(name.clone()), name));
        }
    }
}

fn scan_history(kinds: &HashSet<Kind>, out: &mut Vec<Issue>) {
    if kinds.contains(&Kind::MuiCache) {
        let mui = r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\Shell\MuiCache";
        for (name, _) in values(mui) {
            // Value names look like "C:\path\app.exe.FriendlyAppName".
            let lower = name.to_lowercase();
            if let Some(i) = lower.find(".exe.")
                && definitely_missing(&name[..i + 4])
            {
                // Windows stores several values per program (name, company); show which one.
                let detail = format!("{} ({})", &name[..i + 4], &name[i + 5..]);
                out.push(issue(Kind::MuiCache, mui, Some(name), detail));
            }
        }
    }
    if kinds.contains(&Kind::CompatStore) {
        let compat =
            r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Compatibility Assistant\Store";
        for (name, _) in values(compat) {
            if definitely_missing(&name) {
                out.push(issue(Kind::CompatStore, compat, Some(name.clone()), name));
            }
        }
    }
    if kinds.contains(&Kind::CompatLayers) {
        for key in [
            r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers",
        ] {
            for (name, mode) in values(key) {
                if definitely_missing(&name) {
                    out.push(issue(
                        Kind::CompatLayers,
                        key,
                        Some(name.clone()),
                        format!("{name} ({})", mode.trim()),
                    ));
                }
            }
        }
    }
    if kinds.contains(&Kind::FeatureUsage) {
        let base = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\FeatureUsage";
        for sub in ["AppSwitched", "ShowJumpView", "AppBadgeUpdated", "AppLaunch"] {
            let key = format!(r"{base}\{sub}");
            for (name, _) in values(&key) {
                if definitely_missing(&name) {
                    out.push(issue(Kind::FeatureUsage, key.clone(), Some(name.clone()), name));
                }
            }
        }
    }
}

fn scan_fonts(out: &mut Vec<Issue>) {
    let fonts_dir = PathBuf::from(expand_env(r"%WINDIR%\Fonts"));
    for key in [
        r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts",
        r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts",
    ] {
        for (name, file) in values(key) {
            let file = file.trim();
            if name.is_empty() || file.is_empty() {
                continue;
            }
            let full = if Path::new(file).is_absolute() {
                file.to_owned()
            } else {
                fonts_dir.join(file).display().to_string()
            };
            if definitely_missing(&full) {
                out.push(issue(
                    Kind::Font,
                    key,
                    Some(name.clone()),
                    format!("{name} — {full}"),
                ));
            }
        }
    }
}

fn scan_help(out: &mut Vec<Issue>) {
    for key in [
        r"HKLM\SOFTWARE\Microsoft\Windows\Help",
        r"HKLM\SOFTWARE\Microsoft\Windows\HTML Help",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\Help",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\HTML Help",
    ] {
        for (name, dir) in values(key) {
            if name.is_empty() || dir.trim().is_empty() {
                continue;
            }
            let full = Path::new(&expand_env(dir.trim()))
                .join(&name)
                .display()
                .to_string();
            if definitely_missing(&full) {
                out.push(issue(Kind::HelpFile, key, Some(name), full));
            }
        }
    }
}

fn scan_sounds(out: &mut Vec<Issue>) {
    let root = r"HKCU\AppEvents\Schemes\Apps";
    for app in subkeys(root) {
        for event in subkeys(&format!(r"{root}\{app}")) {
            let key = format!(r"{root}\{app}\{event}\.Current");
            if let Some(wav) = default_value(&key)
                && !wav.trim().is_empty()
                && definitely_missing(&wav)
            {
                out.push(issue(
                    Kind::Sound,
                    key,
                    Some(String::new()),
                    format!("{event} — {wav}"),
                ));
            }
        }
    }
}

/// `App=C:\path\app.exe` inside a firewall rule string.
pub fn firewall_rule_app(rule: &str) -> Option<&str> {
    rule.split('|').find_map(|part| part.strip_prefix("App="))
}

fn scan_firewall(out: &mut Vec<Issue>) {
    let key = r"HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
    for (name, rule) in values(key) {
        if let Some(app) = firewall_rule_app(&rule)
            && definitely_missing(app)
        {
            let label = rule
                .split('|')
                .find_map(|p| p.strip_prefix("Name="))
                .unwrap_or(&name)
                .to_owned();
            out.push(issue(
                Kind::FirewallRule,
                key,
                Some(name),
                format!("{label} — {app}"),
            ));
        }
    }
}

/// Top-level keys under `HKCU\Software` the system manages itself.
const KEEP_SOFTWARE_KEYS: &[&str] = &[
    "classes",
    "microsoft",
    "policies",
    "wow6432node",
    "appdatalow",
    "clients",
];

fn scan_empty_software(out: &mut Vec<Issue>) {
    let root = r"HKCU\Software";
    for sub in subkeys(root) {
        if KEEP_SOFTWARE_KEYS.contains(&sub.to_lowercase().as_str()) {
            continue;
        }
        let key = format!(r"{root}\{sub}");
        if !has_children(&key) {
            out.push(issue(Kind::EmptySoftwareKey, key, None, sub));
        }
    }
}

/// One scanner: the selected categories and the issue list to add to.
type ScanStep = fn(&HashSet<Kind>, &mut Vec<Issue>);

/// Scans the given categories, leaving out excluded entries.
pub fn scan_with(opts: &Options, report: &dyn Fn(&str)) -> Vec<Issue> {
    let kinds: HashSet<Kind> = opts.kinds().into_iter().collect();
    let mut out = Vec::new();
    let on = |k: Kind| kinds.contains(&k);
    let steps: [(&[Kind], &str, ScanStep); 14] = [
        (&[Kind::AppPath], "application paths", |_, o| scan_app_paths(o)),
        (&[Kind::Uninstall], "uninstall entries", |_, o| scan_uninstall(o)),
        (&[Kind::Applications], "\"Open with\" programs", |_, o| {
            scan_applications(o)
        }),
        (&[Kind::RegisteredApp], "default-app registrations", |_, o| {
            scan_registered_apps(o)
        }),
        (&[Kind::IfeoDebugger], "launch redirects", |_, o| scan_ifeo(o)),
        (&[Kind::StartupRun], "startup entries", |_, o| scan_run(o)),
        (
            &[Kind::FileTypeCommand, Kind::ContextMenu, Kind::DefaultIcon],
            "file types",
            scan_classes,
        ),
        (&[Kind::SharedDll], "shared DLLs", |_, o| scan_shared_dlls(o)),
        (&[Kind::ComObject], "COM objects", |_, o| scan_com(o)),
        (&[Kind::TypeLib], "type libraries", |_, o| scan_typelibs(o)),
        (&[Kind::InstallerFolder], "installer folders", |_, o| {
            scan_installer_folders(o)
        }),
        (
            &[
                Kind::MuiCache,
                Kind::CompatStore,
                Kind::CompatLayers,
                Kind::FeatureUsage,
            ],
            "history and caches",
            scan_history,
        ),
        (
            &[Kind::Font, Kind::HelpFile, Kind::Sound],
            "fonts, help and sounds",
            |k, o| {
                if k.contains(&Kind::Font) {
                    scan_fonts(o);
                }
                if k.contains(&Kind::HelpFile) {
                    scan_help(o);
                }
                if k.contains(&Kind::Sound) {
                    scan_sounds(o);
                }
            },
        ),
        (
            &[Kind::FirewallRule, Kind::EmptySoftwareKey],
            "firewall and settings keys",
            |k, o| {
                if k.contains(&Kind::FirewallRule) {
                    scan_firewall(o);
                }
                if k.contains(&Kind::EmptySoftwareKey) {
                    scan_empty_software(o);
                }
            },
        ),
    ];
    for (ks, label, f) in steps {
        if ks.iter().any(|k| on(*k)) {
            report(&format!("Checking {label}…"));
            f(&kinds, &mut out);
        }
    }
    out.retain(|i| on(i.kind) && !opts.excludes(i));
    out.sort_by(|a, b| (a.kind, &a.detail).cmp(&(b.kind, &b.detail)));
    out.dedup_by(|a, b| a.location().eq_ignore_ascii_case(&b.location()));
    out
}

/// Scan with the saved options (used by 1-Click Cleaner).
pub fn scan() -> Vec<Issue> {
    scan_with(&Options::saved(), &|_| {})
}

/// Writes one `.reg` file containing every key the issues touch (exported as they are now).
pub fn export_backup(issues: &[Issue], file: &Path) -> Result<usize> {
    let mut keys: Vec<&str> = issues.iter().map(|i| i.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut text = String::from("Windows Registry Editor Version 5.00\r\n");
    let mut n = 0;
    for (i, key) in keys.iter().enumerate() {
        let part = std::env::temp_dir().join(format!("wm-regclean-{}-{i}.reg", std::process::id()));
        if run_checked(hidden_command("reg").args(["export", key]).arg(&part).arg("/y")).is_err() {
            continue; // already gone
        }
        let bytes = std::fs::read(&part);
        let _ = std::fs::remove_file(&part);
        let bytes = bytes?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let body = String::from_utf16_lossy(&units);
        let body = body.trim_start_matches('\u{feff}');
        // Drop each export's own header line.
        let body = body.split_once('\n').map_or("", |(_, rest)| rest);
        text.push_str(body);
        n += 1;
    }
    let mut out = vec![0xFF, 0xFE];
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(file, out).with_context(|| format!("writing {}", file.display()))?;
    Ok(n)
}

/// Fixes issues; returns how many were fixed. Each kind is journaled separately.
pub fn fix(ctx: &AppCtx<'_>, issues: &[Issue]) -> Result<usize> {
    let mut fixed = 0;
    let mut snapshots: Vec<(Kind, ValueSnapshot)> = Vec::new();
    for issue in issues {
        match &issue.value {
            Some(name) => {
                let snap = registry::snapshot(&issue.key, name);
                registry::delete_value(&issue.key, name)?;
                snapshots.push((issue.kind, snap));
                fixed += 1;
            }
            None => {
                let undo = ctx.journal.backup_reg_key(&issue.key)?;
                run_checked(hidden_command("reg").args(["delete", &issue.key, "/f"]))?;
                ctx.journal.record(
                    TOOL,
                    format!("Removed key {} ({})", issue.key, issue.detail),
                    undo,
                )?;
                fixed += 1;
            }
        }
    }
    let mut kinds: Vec<Kind> = snapshots.iter().map(|(k, _)| *k).collect();
    kinds.dedup();
    for kind in kinds {
        let values: Vec<ValueSnapshot> = snapshots
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, s)| s.clone())
            .collect();
        ctx.journal.record(
            TOOL,
            format!(
                "Removed {} registry values: {}",
                values.len(),
                kind.label().to_lowercase()
            ),
            UndoAction::RestoreRegValues { values },
        )?;
    }
    Ok(fixed)
}

/// `[HKEY_…]` key names in a `.reg` file (UTF-16 or ANSI), without deletions (`[-…]`).
pub fn reg_file_keys(bytes: &[u8]) -> Vec<String> {
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    let mut keys: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix('[')?.strip_suffix(']').map(str::to_owned))
        .map(|k| k.trim_start_matches('-').to_owned())
        .collect();
    keys.dedup();
    keys
}

/// Imports a `.reg` file after backing up every key it touches, so the import can be undone.
pub fn import_reg(ctx: &AppCtx<'_>, file: &Path) -> Result<usize> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let keys = reg_file_keys(&bytes);
    if keys.is_empty() {
        bail!("{} contains no registry keys", file.display());
    }
    if !ctx.elevated
        && keys
            .iter()
            .any(|k| !k.to_uppercase().starts_with("HKEY_CURRENT_USER"))
    {
        bail!("this file changes machine-wide keys; restart as administrator to import it");
    }
    // Keys are backed up deepest first is unnecessary: each backup covers its subtree.
    let mut top: Vec<String> = Vec::new();
    for k in &keys {
        if !top
            .iter()
            .any(|t| k.to_lowercase().starts_with(&format!("{}\\", t.to_lowercase())))
        {
            top.push(k.clone());
        }
    }
    let mut steps = Vec::new();
    for k in &top {
        steps.push(ctx.journal.backup_reg_key(k)?);
    }
    run_checked(hidden_command("reg").arg("import").arg(file)).context("importing")?;
    ctx.journal.record(
        TOOL,
        format!("Imported {}", file.display()),
        UndoAction::Sequence { steps },
    )?;
    Ok(keys.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx<'a>(
        journal: &'a wm_core::safety::ChangeJournal,
        config: &'a mut wm_core::config::Config,
        i18n: &'a wm_core::i18n::I18n,
    ) -> AppCtx<'a> {
        AppCtx {
            config,
            journal,
            i18n,
            elevated: false,
        }
    }

    #[test]
    fn missing_detection_is_conservative() {
        assert!(
            !definitely_missing("notepad.exe"),
            "relative names are never reported"
        );
        assert!(
            !definitely_missing(r"\\server\share\x.exe"),
            "network paths are never reported"
        );
        assert!(!definitely_missing(r"C:\Windows\notepad.exe"));
        assert!(definitely_missing(r"C:\Windows\definitely-not-here-wm.exe"));
        assert!(definitely_missing(r#""%WINDIR%\nope-wm.exe""#));
        assert!(!definitely_missing("%UNSET_WM_VAR%\\x.exe"));
        // A drive letter that does not exist (unplugged USB) is never reported.
        let absent = ('D'..='Z')
            .find(|c| !Path::new(&format!("{c}:\\")).exists())
            .unwrap();
        assert!(!definitely_missing(&format!(r"{absent}:\gone\app.exe")));
    }

    #[test]
    fn wow64_paths_check_syswow64() {
        // A file only in SysWOW64, registered as System32 by a 32-bit program.
        let only32 = std::fs::read_dir(expand_env(r"%WINDIR%\SysWOW64"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| {
                n.ends_with(".dll") && !Path::new(&expand_env(&format!(r"%WINDIR%\System32\{n}"))).exists()
            });
        if let Some(n) = only32 {
            let raw = format!(r"%WINDIR%\System32\{n}");
            assert!(definitely_missing(&raw));
            assert!(!definitely_missing_32(&raw), "found in SysWOW64");
        }
        assert!(definitely_missing_32(r"C:\Windows\System32\no-such-wm.dll"));
    }

    #[test]
    fn exclusions_and_rule_parsing() {
        let i = issue(Kind::StartupRun, r"HKCU\Run", Some("App".into()), r"C:\x\app.exe");
        let with = |e: &str| Options {
            exclude: vec![e.into()],
            ..Default::default()
        };
        assert!(with(r"hkcu\run\app").excludes(&i));
        assert!(with(r"HKCU\Run").excludes(&i));
        assert!(with(r"*\app.exe").excludes(&i), "wildcards match the detail too");
        assert!(!with(r"HKCU\Ru").excludes(&i));
        assert_eq!(
            firewall_rule_app("v2.31|Action=Allow|Active=TRUE|App=C:\\x\\a.exe|Name=A|"),
            Some(r"C:\x\a.exe")
        );
        assert_eq!(firewall_rule_app("v2.31|Action=Allow|Svc=x|"), None);
        assert_eq!(
            typelib_file(r"C:\Windows\SysWOW64\srm.dll\2"),
            r"C:\Windows\SysWOW64\srm.dll"
        );
        assert_eq!(typelib_file(r"C:\x\a.tlb"), r"C:\x\a.tlb");
        let ado = r"C:\Program Files\Common Files\System\ado\msado15.dll";
        if Path::new(ado).exists() {
            assert!(missing_exe(ado, false).is_none(), "unquoted path with spaces");
        }
        assert!(
            missing_exe(r"%SystemRoot%\system32\perfmon /res", false).is_none(),
            ".exe is implied"
        );
        assert!(missing_exe(r"C:\Windows\no-such-wm /x", false).is_some());
        assert!(missing_exe(r#""C:\Windows\no-such-wm.exe" /x"#, false).is_some());
        assert_eq!(Kind::ALL.len(), 22);
        assert!(!Options::default().kinds().contains(&Kind::ComObject));
    }

    #[test]
    fn finds_and_fixes_dead_run_value_with_undo() {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let name = format!("wm-regclean-test-{}", std::process::id());
        /// Removes the test value even if an assertion fails.
        struct Cleanup<'a>(&'a str, &'a str);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = registry::delete_value(self.0, self.1);
            }
        }
        let _cleanup = Cleanup(key, &name);
        registry::write_raw(
            key,
            &name,
            &registry::string(r#""C:\Windows\no-such-wm-app.exe" /x"#),
        )
        .unwrap();
        let only_run = Options {
            kinds: Some(vec![Kind::StartupRun]),
            ..Default::default()
        };
        let found: Vec<Issue> = scan_with(&only_run, &|_| {})
            .into_iter()
            .filter(|i| i.value.as_deref() == Some(name.as_str()))
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, Kind::StartupRun);
        let excluded = Options {
            exclude: vec![found[0].location()],
            ..only_run.clone()
        };
        assert!(
            !scan_with(&excluded, &|_| {})
                .iter()
                .any(|i| i.value.as_deref() == Some(name.as_str()))
        );

        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.reg");
        assert_eq!(export_backup(&found, &backup).unwrap(), 1);
        let keys = reg_file_keys(&std::fs::read(&backup).unwrap());
        assert!(
            keys.iter().any(|k| k.ends_with(r"CurrentVersion\Run")),
            "{keys:?}"
        );

        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = test_ctx(&journal, &mut config, &i18n);
        assert_eq!(fix(&ctx, &found).unwrap(), 1);
        assert!(registry::read_raw(key, &name).is_none());
        journal.undo(journal.entries()[0].id).unwrap();
        assert!(
            registry::read_raw(key, &name).is_some(),
            "undo restores the value"
        );
    }

    #[test]
    fn import_reg_with_undo() {
        let sub = format!("WmRegImportTest{}", std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.reg");
        std::fs::write(
            &file,
            format!("Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\{sub}]\r\n\"V\"=dword:00000007\r\n"),
        )
        .unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = test_ctx(&journal, &mut config, &i18n);
        let key = format!(r"HKCU\Software\{sub}");
        assert_eq!(import_reg(&ctx, &file).unwrap(), 1);
        assert!(registry::read_raw(&key, "V").is_some());
        journal.undo(journal.entries()[0].id).unwrap();
        assert!(
            registry::open_read(&key).is_err(),
            "undo deletes the key the import created"
        );
    }

    #[test]
    fn real_scan_of_every_category_runs() {
        let all = Options {
            kinds: Some(Kind::ALL.to_vec()),
            ..Default::default()
        };
        let issues = scan_with(&all, &|_| {});
        assert!(issues.iter().all(|i| !i.key.is_empty()));
        // Every reported file-based issue really points at something missing.
        assert!(
            issues
                .iter()
                .filter(|i| i.kind == Kind::StartupRun)
                .all(|i| definitely_missing(&i.detail) || definitely_missing_32(&i.detail))
        );
    }
}
