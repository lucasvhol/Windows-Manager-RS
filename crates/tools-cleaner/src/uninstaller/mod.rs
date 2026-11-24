//! Smart Uninstaller: installed programs (single or batch uninstall, quiet mode,
//! leftovers, remove entry, rename, details, export), installed updates, Windows apps
//! and Program Files folders that no installed program refers to.
//!
//! Leftover folders go to the Recycle Bin; registry keys are backed up, then deleted.

mod orphans;
mod view;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use wm_core::{
    AppCtx,
    paths::expand_env,
    process::{hidden_command, json_list, powershell_json, run_checked, split_command_line},
    registry,
    safety::UndoAction,
};

pub use view::SmartUninstaller;

pub(crate) const UNINSTALL_KEYS: [&str; 3] = [
    r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
    r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall",
];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    pub key: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub installed: String,
    pub size: Option<u64>,
    pub uninstall: Option<String>,
    pub quiet_uninstall: Option<String>,
    pub install_location: Option<PathBuf>,
    pub display_icon: Option<String>,
    pub help_link: Option<String>,
    pub per_user: bool,
}

pub(crate) fn reg_string(key: &winreg::RegKey, name: &str) -> Option<String> {
    key.get_value::<String, _>(name)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

pub fn enumerate() -> Vec<Program> {
    let mut programs: Vec<Program> = Vec::new();
    for base in UNINSTALL_KEYS {
        let Ok(root) = registry::open_read(base) else {
            continue;
        };
        for sub in root.enum_keys().flatten() {
            let Ok(k) = root.open_subkey(&sub) else { continue };
            let Some(name) = reg_string(&k, "DisplayName") else {
                continue;
            };
            let system: u32 = k.get_value("SystemComponent").unwrap_or(0);
            let is_update =
                reg_string(&k, "ReleaseType").is_some_and(|r| r.contains("Update") || r == "Hotfix");
            if system == 1 || is_update || reg_string(&k, "ParentKeyName").is_some() {
                continue;
            }
            let installed = reg_string(&k, "InstallDate")
                .and_then(|d| format_install_date(&d))
                .unwrap_or_default();
            programs.push(Program {
                key: format!(r"{base}\{sub}"),
                publisher: reg_string(&k, "Publisher").unwrap_or_default(),
                version: reg_string(&k, "DisplayVersion").unwrap_or_default(),
                installed,
                size: k
                    .get_value::<u32, _>("EstimatedSize")
                    .ok()
                    .map(|kb| u64::from(kb) * 1024),
                uninstall: reg_string(&k, "UninstallString"),
                quiet_uninstall: reg_string(&k, "QuietUninstallString"),
                install_location: reg_string(&k, "InstallLocation")
                    .map(|p| PathBuf::from(expand_env(p.trim_matches('"')))),
                display_icon: reg_string(&k, "DisplayIcon"),
                help_link: reg_string(&k, "URLInfoAbout").or_else(|| reg_string(&k, "HelpLink")),
                per_user: base.starts_with("HKCU"),
                name,
            });
        }
    }
    programs.sort_by_key(|p| p.name.to_lowercase());
    programs.dedup_by(|a, b| a.name == b.name && a.version == b.version);
    programs
}

/// Every value of an uninstall entry, for the details pane.
pub fn details(key: &str) -> Vec<(String, String)> {
    let Ok(k) = registry::open_read(key) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = k
        .enum_values()
        .flatten()
        .map(|(name, v)| {
            let text = match v.vtype {
                winreg::enums::REG_DWORD => k
                    .get_value::<u32, _>(&name)
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
                winreg::enums::REG_SZ | winreg::enums::REG_EXPAND_SZ => {
                    k.get_value::<String, _>(&name).unwrap_or_default()
                }
                _ => format!("({} bytes)", v.bytes.len()),
            };
            (if name.is_empty() { "(Default)".into() } else { name }, text)
        })
        .collect();
    out.sort_by_key(|(n, _)| n.to_lowercase());
    out
}

/// `YYYYMMDD` → `YYYY-MM-DD`, rejecting malformed values some installers write.
pub fn format_install_date(raw: &str) -> Option<String> {
    chrono::NaiveDate::parse_from_str(raw.trim(), "%Y%m%d")
        .ok()
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// MSI entries often register `MsiExec /I{GUID}` (modify); `/X{GUID}` uninstalls.
pub fn normalize_uninstall(cmd: &str) -> String {
    let lower = cmd.to_ascii_lowercase();
    if lower.contains("msiexec")
        && let Some(i) = lower.find("/i{")
    {
        let mut s = cmd.to_owned();
        s.replace_range(i..i + 2, "/X");
        return s;
    }
    cmd.to_owned()
}

/// The command to run and whether it is silent. In quiet mode this is the program's
/// own silent command, or an MSI uninstall with `/qn`; otherwise (or when neither
/// exists) it is the normal, interactive uninstaller.
pub fn uninstall_command(p: &Program, quiet: bool) -> Option<(String, bool)> {
    if quiet {
        if let Some(q) = &p.quiet_uninstall {
            return Some((expand_env(q), true));
        }
        if let Some(u) = &p.uninstall {
            let cmd = normalize_uninstall(&expand_env(u));
            let lower = cmd.to_ascii_lowercase();
            if lower.contains("msiexec") && lower.contains("/x") {
                let extra = if lower.contains("/qn") {
                    ""
                } else {
                    " /qn /norestart"
                };
                return Some((format!("{cmd}{extra}"), true));
            }
        }
    }
    p.uninstall
        .as_ref()
        .map(|u| (normalize_uninstall(&expand_env(u)), false))
}

/// Runs an uninstall command and waits for the launched process to exit.
/// (Some uninstallers hand off to a copy of themselves and return early.)
pub fn run_command(cmd: &str) -> Result<i32> {
    use std::os::windows::process::CommandExt;
    let Some((exe, args)) = split_command_line(cmd) else {
        bail!("invalid uninstall command: {cmd}")
    };
    let status = std::process::Command::new(&exe)
        .raw_arg(&args)
        .status()
        .with_context(|| format!("starting {exe}"))?;
    Ok(status.code().unwrap_or(-1))
}

/// Program name without version / architecture decorations: "7-Zip 23.01 (x64)" → "7-Zip".
pub fn clean_name(name: &str) -> String {
    let mut s = name.to_owned();
    if let Some(i) = s.find(" (") {
        s.truncate(i);
    }
    let mut words: Vec<&str> = s.split_whitespace().collect();
    while words.len() > 1
        && words.last().is_some_and(|w| {
            w.chars().any(|c| c.is_ascii_digit())
                && w.chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == 'v' || c == '-')
                || matches!(
                    w.to_ascii_lowercase().as_str(),
                    "x64" | "x86" | "64-bit" | "32-bit"
                )
        })
    {
        words.pop();
    }
    words.join(" ")
}

/// Folder/key names too generic to ever remove on a name match.
const TOO_GENERIC: &[&str] = &[
    "microsoft",
    "windows",
    "classes",
    "policies",
    "intel",
    "nvidia",
    "amd",
    "google",
    "common files",
    "packages",
    "temp",
    "programs",
    "app",
    "apps",
    "software",
    "update",
    "driver",
    "drivers",
    "tools",
];

pub fn safe_name(name: &str) -> bool {
    name.chars().filter(|c| c.is_alphanumeric()).count() >= 3
        && !TOO_GENERIC.contains(&name.to_lowercase().as_str())
        && !name.contains(['\\', '/', ':', '*', '?'])
}

#[derive(Debug, Clone, PartialEq)]
pub enum Leftover {
    Folder(PathBuf),
    RegistryKey(String),
}

impl Leftover {
    pub fn label(&self) -> String {
        match self {
            Leftover::Folder(p) => p.display().to_string(),
            Leftover::RegistryKey(k) => k.clone(),
        }
    }

    /// Machine-wide locations need administrator rights to remove.
    pub fn needs_admin(&self) -> bool {
        match self {
            Leftover::RegistryKey(k) => registry::needs_admin(k),
            Leftover::Folder(p) => {
                let p = p.to_string_lossy().to_lowercase();
                ["%ProgramFiles%", "%ProgramFiles(x86)%", "%ProgramData%"]
                    .iter()
                    .any(|v| p.starts_with(&expand_env(v).to_lowercase()))
            }
        }
    }
}

/// Protected locations that must never be offered as leftovers themselves.
pub(crate) fn is_protected_dir(p: &Path) -> bool {
    let norm = |s: &str| s.trim_end_matches('\\').to_lowercase();
    let p = norm(&p.to_string_lossy());
    p.len() <= 3
        || [
            "%ProgramFiles%",
            "%ProgramFiles(x86)%",
            "%ProgramData%",
            "%APPDATA%",
            "%LOCALAPPDATA%",
            "%WINDIR%",
            "%USERPROFILE%",
        ]
        .iter()
        .any(|v| norm(&expand_env(v)) == p)
}

pub fn find_leftovers(program: &Program) -> Vec<Leftover> {
    let mut out = Vec::new();
    let name = clean_name(&program.name);
    let publisher = clean_name(&program.publisher.replace([',', '.'], " "));
    let names: Vec<&str> = [name.as_str(), program.name.as_str()]
        .into_iter()
        .filter(|n| safe_name(n))
        .collect();

    if let Some(loc) = &program.install_location
        && loc.exists()
        && !is_protected_dir(loc)
    {
        out.push(Leftover::Folder(loc.clone()));
    }
    for base in [
        "%ProgramFiles%",
        "%ProgramFiles(x86)%",
        "%ProgramData%",
        "%APPDATA%",
        "%LOCALAPPDATA%",
    ] {
        let base = PathBuf::from(expand_env(base));
        for n in &names {
            let mut candidates = vec![base.join(n)];
            if safe_name(&publisher) {
                candidates.push(base.join(&publisher).join(n));
            }
            for c in candidates {
                if c.is_dir() && !is_protected_dir(&c) && !out.contains(&Leftover::Folder(c.clone())) {
                    out.push(Leftover::Folder(c));
                }
            }
        }
    }
    for root in [r"HKCU\Software", r"HKLM\SOFTWARE", r"HKLM\SOFTWARE\WOW6432Node"] {
        for n in &names {
            let mut keys = vec![format!(r"{root}\{n}")];
            if safe_name(&publisher) {
                keys.push(format!(r"{root}\{publisher}\{n}"));
            }
            for k in keys {
                if registry::open_read(&k).is_ok() && !out.contains(&Leftover::RegistryKey(k.clone())) {
                    out.push(Leftover::RegistryKey(k));
                }
            }
        }
    }
    if registry::open_read(&program.key).is_ok() {
        out.push(Leftover::RegistryKey(program.key.clone()));
    }
    out
}

fn delete_key_journaled(ctx: &AppCtx<'_>, key: &str, description: String) -> Result<()> {
    let undo = ctx.journal.backup_reg_key(key)?;
    run_checked(hidden_command("reg").args(["delete", key, "/f"]))?;
    ctx.journal.record("uninstaller", description, undo)?;
    Ok(())
}

/// Removes (program name, leftover) pairs; returns how many were removed.
pub(crate) fn remove_leftovers(ctx: &AppCtx<'_>, items: &[(String, Leftover)]) -> Result<usize> {
    let mut folders = Vec::new();
    let mut removed = 0;
    for (program, item) in items {
        match item {
            Leftover::Folder(p) => folders.push(p.clone()),
            Leftover::RegistryKey(k) => {
                delete_key_journaled(ctx, k, format!("Removed leftover key {k} ({program})"))?;
                removed += 1;
            }
        }
    }
    if !folders.is_empty() {
        removed += folders.len();
        ctx.journal.recycle(
            "uninstaller",
            format!("Removed {} leftover folders", folders.len()),
            folders,
        )?;
    }
    Ok(removed)
}

/// Removes only the entry from the list (for programs that are already gone).
pub(crate) fn remove_entry(ctx: &AppCtx<'_>, p: &Program) -> Result<()> {
    delete_key_journaled(ctx, &p.key, format!("Removed the uninstall entry of {}", p.name))
}

/// Changes the name shown in this list and in Windows' Installed apps.
pub(crate) fn rename(ctx: &AppCtx<'_>, p: &Program, new_name: &str) -> Result<()> {
    let before = registry::snapshot(&p.key, "DisplayName");
    registry::write_raw(&p.key, "DisplayName", &registry::string(new_name))?;
    ctx.journal.record(
        "uninstaller",
        format!("Renamed {} to {new_name}", p.name),
        UndoAction::RestoreRegValues { values: vec![before] },
    )?;
    Ok(())
}

/// An installed Windows update (from `Get-HotFix`).
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub id: String,
    pub kind: String,
    pub installed: String,
}

pub fn list_updates() -> Result<Vec<Update>> {
    let v = powershell_json(
        r#"@(Get-HotFix | Select-Object HotFixID, Description, @{ n = 'InstalledOn'; e = { if ($_.InstalledOn) { $_.InstalledOn.ToString('yyyy-MM-dd') } } } | Sort-Object InstalledOn -Descending) | ConvertTo-Json -Compress"#,
    )?;
    let s = |o: &serde_json::Value, k: &str| o[k].as_str().unwrap_or_default().to_owned();
    Ok(json_list(v)
        .iter()
        .map(|o| Update {
            id: s(o, "HotFixID"),
            kind: s(o, "Description"),
            installed: s(o, "InstalledOn"),
        })
        .filter(|u| !u.id.is_empty())
        .collect())
}

pub fn remove_update(id: &str) -> Result<()> {
    let digits: String = id.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        bail!("{id} is not a KB number");
    }
    let out = hidden_command("wusa.exe")
        .args([
            "/uninstall".to_owned(),
            format!("/kb:{digits}"),
            "/quiet".into(),
            "/norestart".into(),
        ])
        .output()?;
    match out.status.code() {
        Some(0 | 3010) => Ok(()),
        Some(c) if c == 0x80240006u32 as i32 => {
            bail!("{id} is not installed or cannot be removed on its own")
        }
        Some(c) if c == 0x80070422u32 as i32 => bail!("the Windows Update service is disabled"),
        c => bail!("wusa failed (exit code {:#x})", c.unwrap_or(-1)),
    }
}

/// OneDrive's own uninstaller: the per-user entry if there is one, else the setup
/// program that ships with Windows.
pub fn onedrive_uninstall_command(programs: &[Program]) -> Option<String> {
    if let Some(p) = programs
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case("Microsoft OneDrive") && p.uninstall.is_some())
    {
        return p.uninstall.clone().map(|u| expand_env(&u));
    }
    for c in [
        r"%SystemRoot%\System32\OneDriveSetup.exe",
        r"%SystemRoot%\SysWOW64\OneDriveSetup.exe",
    ] {
        let path = expand_env(c);
        if Path::new(&path).exists() {
            return Some(format!("\"{path}\" /uninstall"));
        }
    }
    None
}

pub fn uninstall_onedrive(cmd: &str) -> Result<i32> {
    // The uninstaller fails while OneDrive is running.
    let _ = hidden_command("taskkill")
        .args(["/f", "/im", "OneDrive.exe"])
        .output();
    run_command(cmd)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Html,
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn export(programs: &[&Program], format: ExportFormat) -> String {
    let size = |p: &Program| p.size.map(|b| (b / 1024).to_string()).unwrap_or_default();
    let scope = |p: &Program| if p.per_user { "Current user" } else { "All users" };
    let mut out = String::new();
    match format {
        ExportFormat::Csv => {
            out.push_str("Name,Publisher,Version,Size (KB),Installed,For,Uninstall command\r\n");
            for p in programs {
                let _ = write!(
                    out,
                    "{},{},{},{},{},{},{}\r\n",
                    csv_field(&p.name),
                    csv_field(&p.publisher),
                    csv_field(&p.version),
                    size(p),
                    p.installed,
                    scope(p),
                    csv_field(p.uninstall.as_deref().unwrap_or_default())
                );
            }
        }
        ExportFormat::Html => {
            let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
            let _ = write!(
                out,
                "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>Installed programs</title>\n<style>\n\
                 body{{font:14px 'Segoe UI',sans-serif;margin:24px;color:#222}}\n\
                 table{{border-collapse:collapse;width:100%}}\n\
                 th,td{{text-align:left;padding:3px 8px;border-bottom:1px solid #e4e4e4}}\n\
                 td.n{{text-align:right;font-variant-numeric:tabular-nums}}\n\
                 @media(prefers-color-scheme:dark){{body{{background:#1e1e1e;color:#ddd}}th,td{{border-color:#333}}}}\n\
                 </style></head><body>\n<h1>Installed programs</h1>\n<p>{} programs, exported {stamp}.</p>\n\
                 <table><tr><th>Name</th><th>Publisher</th><th>Version</th><th>Size</th><th>Installed</th><th>For</th></tr>\n",
                programs.len()
            );
            for p in programs {
                let _ = writeln!(
                    out,
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"n\">{}</td><td>{}</td><td>{}</td></tr>",
                    html_escape(&p.name),
                    html_escape(&p.publisher),
                    html_escape(&p.version),
                    p.size.map(wm_core::ui::format_bytes).unwrap_or_default(),
                    p.installed,
                    scope(p)
                );
            }
            out.push_str("</table></body></html>\n");
        }
    }
    out
}

/// A web search for the program, for "what is this?".
pub fn lookup_url(p: &Program) -> String {
    let q = format!("{} {}", clean_name(&p.name), p.publisher);
    let q: String = q
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c.to_string()
            } else if c == ' ' {
                "+".into()
            } else {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b)
                    .bytes()
                    .map(|x| format!("%{x:02X}"))
                    .collect()
            }
        })
        .collect();
    format!("https://www.bing.com/search?q={q}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_dates() {
        assert_eq!(format_install_date("20261113").as_deref(), Some("2026-11-13"));
        assert_eq!(format_install_date("20265301"), None, "month 53 is rejected");
        assert_eq!(format_install_date("2026"), None);
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("7-Zip 23.01 (x64)"), "7-Zip");
        assert_eq!(clean_name("Mozilla Firefox (x64 en-US)"), "Mozilla Firefox");
        assert_eq!(clean_name("Notepad++ 8.6.2 x64"), "Notepad++");
        assert_eq!(clean_name("Python 3.12.1"), "Python");
        assert_eq!(clean_name("Steam"), "Steam");
        assert!(!safe_name("Microsoft"));
        assert!(!safe_name("Qt"));
        assert!(safe_name("Notepad++"));
    }

    #[test]
    fn msi_commands() {
        assert_eq!(
            normalize_uninstall("MsiExec.exe /I{AB-12}"),
            "MsiExec.exe /X{AB-12}"
        );
        assert_eq!(
            normalize_uninstall(r#""C:\x\unins000.exe""#),
            r#""C:\x\unins000.exe""#
        );
    }

    /// Rename and Remove entry on a throwaway per-user entry, each undone through the journal.
    #[test]
    fn rename_and_remove_entry_with_undo() {
        let sub = format!("WmUninstallTest{}", std::process::id());
        let key = format!(r"{}\{sub}", UNINSTALL_KEYS[2]);
        registry::write_raw(&key, "DisplayName", &registry::string("Wm Test App")).unwrap();
        registry::write_raw(&key, "UninstallString", &registry::string(r"C:\none\u.exe")).unwrap();
        let p = enumerate().into_iter().find(|p| p.key == key).expect("listed");
        assert!(p.per_user);

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
        let name = || registry::decode_string(&registry::read_raw(&key, "DisplayName").unwrap());

        rename(&ctx, &p, "Renamed App").unwrap();
        assert_eq!(name(), "Renamed App");
        journal.undo(journal.entries()[0].id).unwrap();
        assert_eq!(name(), "Wm Test App");

        remove_entry(&ctx, &p).unwrap();
        assert!(registry::open_read(&key).is_err());
        journal.undo(journal.entries()[0].id).unwrap();
        assert_eq!(name(), "Wm Test App", "undo re-imports the entry");

        let (root, path) = registry::split_path(&key).unwrap();
        root.delete_subkey_all(path).unwrap();
    }

    #[test]
    fn quiet_commands() {
        let msi = Program {
            uninstall: Some("MsiExec.exe /I{AB-12}".into()),
            ..Default::default()
        };
        assert_eq!(
            uninstall_command(&msi, true),
            Some(("MsiExec.exe /X{AB-12} /qn /norestart".into(), true))
        );
        assert_eq!(
            uninstall_command(&msi, false),
            Some(("MsiExec.exe /X{AB-12}".into(), false))
        );
        let own = Program {
            uninstall: Some(r#""C:\x\unins000.exe""#.into()),
            quiet_uninstall: Some(r#""C:\x\unins000.exe" /SILENT"#.into()),
            ..Default::default()
        };
        assert_eq!(
            uninstall_command(&own, true).unwrap().0,
            r#""C:\x\unins000.exe" /SILENT"#
        );
        let plain = Program {
            uninstall: Some(r#""C:\x\uninstall.exe""#.into()),
            ..Default::default()
        };
        assert_eq!(
            uninstall_command(&plain, true),
            Some((r#""C:\x\uninstall.exe""#.into(), false)),
            "no silent mode: falls back to the interactive uninstaller"
        );
        assert_eq!(uninstall_command(&Program::default(), true), None);
    }

    #[test]
    fn protected_dirs_never_leftovers() {
        assert!(is_protected_dir(Path::new(&expand_env("%ProgramFiles%"))));
        assert!(is_protected_dir(Path::new(r"C:\")));
        let p = Program {
            name: "Windows".into(),
            publisher: "Microsoft".into(),
            install_location: Some(PathBuf::from(expand_env("%WINDIR%"))),
            key: r"HKCU\Software\nope-wm-test".into(),
            ..Default::default()
        };
        assert!(find_leftovers(&p).is_empty());
    }

    #[test]
    fn real_programs_listed() {
        let programs = enumerate();
        assert!(!programs.is_empty());
        assert!(programs.iter().all(|p| !p.name.is_empty()));
        assert!(details(&programs[0].key).iter().any(|(n, _)| n == "DisplayName"));
    }

    #[test]
    fn leftover_registry_key_found() {
        let name = format!("WmLeftoverTest{}", std::process::id());
        let key = format!(r"HKCU\Software\{name}");
        registry::write_raw(&key, "x", &registry::dword(1)).unwrap();
        let p = Program {
            name: format!("{name} 1.2.3"),
            key: r"HKCU\Software\none".into(),
            ..Default::default()
        };
        assert!(find_leftovers(&p).contains(&Leftover::RegistryKey(key.clone())));
        let (root, sub) = registry::split_path(&key).unwrap();
        root.delete_subkey_all(sub).unwrap();
    }

    #[test]
    fn exports_and_lookup() {
        let p = Program {
            name: "Tool, \"Pro\" 2.0".into(),
            publisher: "<Acme>".into(),
            size: Some(2048),
            ..Default::default()
        };
        let csv = export(&[&p], ExportFormat::Csv);
        assert!(csv.contains("\"Tool, \"\"Pro\"\" 2.0\",<Acme>,,2,"), "{csv}");
        let html = export(&[&p], ExportFormat::Html);
        assert!(html.contains("&lt;Acme&gt;") && html.contains("2.0 KB"));
        assert_eq!(
            lookup_url(&Program {
                name: "Café 1.0".into(),
                publisher: "A&B".into(),
                ..Default::default()
            }),
            "https://www.bing.com/search?q=Caf%C3%A9+A%26B"
        );
    }

    #[test]
    fn real_updates_listed() {
        // Get-HotFix works without administrator rights.
        let updates = list_updates().unwrap();
        assert!(updates.iter().all(|u| !u.id.is_empty()));
    }
}
