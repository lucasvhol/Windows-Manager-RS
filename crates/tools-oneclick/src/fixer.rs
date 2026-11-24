//! 1-Click Fixer: standard repairs for common Windows problems, run in sequence with a
//! log. Ten repairs (system integrity, disks, WMI, internet, file associations,
//! permissions, the shell) plus quick fixes (caches, print queue, network stack…).
//!
//! Repairs run in the background; the ones that change settings hand their undo steps
//! back so the change journal can record them.

use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use wm_core::{
    AppCtx, Category, Tool,
    command::run_streaming,
    egui, paths,
    process::{hidden_command, restart_explorer},
    registry,
    safety::UndoAction,
    shell::{KnownFolder, known_folder},
    task::BackgroundTask,
    ui::{Confirm, Message, request_open_tool, show_message},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fix {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub admin: bool,
    pub restart: bool,
}

const fn fix(id: &'static str, title: &'static str, description: &'static str, admin: bool) -> Fix {
    Fix {
        id,
        title,
        description,
        admin,
        restart: false,
    }
}

/// The main repairs.
pub const REPAIRS: [Fix; 10] = [
    fix(
        "component",
        "Repair the component store",
        "DISM RestoreHealth: replaces damaged Windows components from Windows Update. 10–30 minutes.",
        true,
    ),
    fix(
        "sysfiles",
        "Repair system files",
        "SFC: checks protected Windows files and restores damaged ones. 10–20 minutes.",
        true,
    ),
    fix(
        "wmi",
        "Check and repair WMI",
        "Verifies the WMI repository (used by many tools and installers) and salvages it if inconsistent.",
        true,
    ),
    fix(
        "syspart",
        "Scan the system drive",
        "CHKDSK online scan of the Windows drive; problems found are fixed at the next restart.",
        true,
    ),
    fix(
        "volumes",
        "Scan the other drives",
        "CHKDSK online scan of every other fixed NTFS drive.",
        true,
    ),
    fix(
        "internet",
        "Repair the internet connection",
        "Flushes DNS, resets the system proxy (WinHTTP), renews addresses, then tests name lookup.",
        true,
    ),
    fix(
        "assoc",
        "Repair program file associations",
        "Removes per-user overrides that hijack .exe, .lnk, .bat, .cmd, .com, .msi, .reg and similar files.",
        false,
    ),
    fix(
        "folders",
        "Reset personal folder permissions",
        "Desktop, Documents, Downloads, Music, Pictures and Videos inherit from your profile again (saved first).",
        false,
    ),
    fix(
        "regperm",
        "Reset broken registry permissions",
        "Keys in your part of the registry that you cannot read inherit their permissions again (saved first).",
        false,
    ),
    fix(
        "shell",
        "Repair the Windows shell",
        "Restores the shell and sign-in programs malware may replace (Winlogon Shell, Userinit), then restarts Explorer.",
        true,
    ),
];

/// Smaller, quick fixes.
pub const QUICK: [Fix; 9] = [
    fix(
        "dns",
        "Flush DNS cache",
        "Fixes websites that moved or fail to resolve.",
        true,
    ),
    fix(
        "explorer",
        "Restart File Explorer",
        "Fixes a frozen taskbar, Start or desktop.",
        false,
    ),
    fix(
        "icons",
        "Rebuild icon cache",
        "Fixes blank or wrong icons.",
        false,
    ),
    fix(
        "thumbs",
        "Rebuild thumbnail cache",
        "Fixes wrong or missing picture previews.",
        false,
    ),
    fix(
        "store",
        "Reset Microsoft Store cache",
        "Fixes Store apps that will not download or update.",
        false,
    ),
    fix(
        "time",
        "Synchronise the clock",
        "Fixes certificate errors caused by a wrong time.",
        true,
    ),
    fix(
        "print",
        "Clear stuck print jobs",
        "Empties the print queue and restarts the Print Spooler.",
        true,
    ),
    Fix {
        restart: true,
        ..fix(
            "network",
            "Reset network stack",
            "Resets Winsock and TCP/IP settings. Custom network settings are lost.",
            true,
        )
    },
    fix(
        "update",
        "Reset Windows Update",
        "Stops update services and sets aside the update cache (kept as *.bak).",
        true,
    ),
];

/// What a run produced: its log, and undo steps to record (description, action).
#[derive(Default)]
pub struct Outcome {
    pub log: Vec<String>,
    pub undo: Vec<(String, UndoAction)>,
}

fn run(program: &str, args: &[&str], log: &mut Vec<String>) -> Result<String> {
    log.push(format!("> {program} {}", args.join(" ")));
    let out = hidden_command(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("starting {program}"))?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        log.push(format!("  {}", l.trim()));
    }
    Ok(text)
}

/// Runs a long tool, reporting its progress lines, and logs its output.
fn run_long(program: &str, args: &[&str], log: &mut Vec<String>, status: &mut dyn FnMut(&str)) -> Result<()> {
    log.push(format!("> {program} {}", args.join(" ")));
    let (code, lines) = run_streaming(program, args, |s| status(s))?;
    log.extend(
        lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("  {}", l.trim())),
    );
    if code != 0 {
        bail!("{program} exited with code {code}");
    }
    Ok(())
}

fn kill_explorer_then(log: &mut Vec<String>, f: impl FnOnce(&mut Vec<String>)) -> Result<()> {
    run("taskkill", &["/f", "/im", "explorer.exe"], log)?;
    std::thread::sleep(std::time::Duration::from_millis(1200));
    f(log);
    // Windows usually restarts the shell itself; start it if it did not.
    let running = hidden_command("tasklist")
        .args(["/fi", "imagename eq explorer.exe", "/nh"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("explorer.exe")
        });
    if !running {
        std::process::Command::new("explorer.exe")
            .spawn()
            .context("starting explorer.exe")?;
    }
    Ok(())
}

fn delete_matching(dir: &str, pattern: &str, log: &mut Vec<String>) {
    for p in paths::expand_pattern(&format!(r"{dir}\{pattern}")) {
        match std::fs::remove_file(&p) {
            Ok(()) => log.push(format!("  deleted {}", p.display())),
            Err(e) => log.push(format!("  skipped {} ({e})", p.display())),
        }
    }
}

fn set_aside(path: &str, log: &mut Vec<String>) {
    let src = PathBuf::from(paths::expand_env(path));
    if !src.exists() {
        return;
    }
    let dest = PathBuf::from(format!(
        "{}.bak-{}",
        src.display(),
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    match std::fs::rename(&src, &dest) {
        Ok(()) => log.push(format!("  renamed {} → {}", src.display(), dest.display())),
        Err(e) => log.push(format!("  could not rename {} ({e})", src.display())),
    }
}

fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S%.3f").to_string()
}

/// Exports a key to the backups folder; the undo re-imports it.
fn export_key(key: &str) -> Result<UndoAction> {
    let safe: String = key
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .take(80)
        .collect();
    let file = paths::backups_dir()?.join(format!("{}-{safe}.reg", stamp()));
    let out = hidden_command("reg")
        .args(["export", key])
        .arg(&file)
        .arg("/y")
        .output()?;
    if !out.status.success() {
        bail!("could not back up {key}");
    }
    Ok(UndoAction::ImportRegFile { path: file })
}

fn delete_key(key: &str) -> Result<()> {
    let (root, sub) = registry::split_path(key)?;
    root.delete_subkey_all(sub)
        .with_context(|| format!("deleting {key}"))
}

/// File types whose per-user overrides can stop programs from starting.
const ASSOC_TYPES: [(&str, &str); 10] = [
    (".exe", "exefile"),
    (".com", "comfile"),
    (".bat", "batfile"),
    (".cmd", "cmdfile"),
    (".lnk", "lnkfile"),
    (".pif", "piffile"),
    (".scr", "scrfile"),
    (".msi", "Msi.Package"),
    (".msc", "MSCFile"),
    (".reg", "regfile"),
];

/// Per-user keys that override program file types (none exist on a healthy PC).
pub fn association_overrides() -> Vec<String> {
    let mut keys = Vec::new();
    for (ext, class) in ASSOC_TYPES {
        for key in [
            format!(r"HKCU\Software\Classes\{ext}"),
            format!(r"HKCU\Software\Classes\{class}"),
            format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}\UserChoice"),
        ] {
            if registry::open_read(&key).is_ok() {
                keys.push(key);
            }
        }
    }
    keys
}

fn fix_associations(out: &mut Outcome) -> Result<()> {
    let keys = association_overrides();
    if keys.is_empty() {
        out.log.push("  no overrides found".into());
        return Ok(());
    }
    for key in keys {
        let undo = export_key(&key)?;
        match delete_key(&key) {
            Ok(()) => {
                out.log.push(format!("  removed {key}"));
                out.undo
                    .push((format!("Removed the file type override {key}"), undo));
            }
            Err(e) => out.log.push(format!("  {e:#}")),
        }
    }
    Ok(())
}

fn personal_folders() -> Vec<PathBuf> {
    let profile = PathBuf::from(paths::expand_env("%USERPROFILE%"));
    [
        KnownFolder::Desktop,
        KnownFolder::Documents,
        KnownFolder::Downloads,
        KnownFolder::Music,
        KnownFolder::Pictures,
        KnownFolder::Videos,
    ]
    .into_iter()
    .filter_map(|f| known_folder(f).ok())
    // Folders moved elsewhere would inherit that drive's permissions instead.
    .filter(|p| p.starts_with(&profile) && p.is_dir())
    .collect()
}

/// Whether an SDDL DACL has entries of its own (protected, or any non-inherited ACE).
fn has_explicit_entries(sddl: &str) -> bool {
    let flags = sddl.trim_start_matches("D:");
    if flags.split('(').next().is_some_and(|f| f.contains('P')) {
        return true;
    }
    sddl.split('(')
        .skip(1)
        .any(|ace| !ace.split(';').nth(1).unwrap_or("").contains("ID"))
}

/// Items in `dir` (and `dir` itself) with permissions of their own, parents first.
/// Links and junctions are not followed.
fn explicit_acls(dir: &Path) -> Vec<(PathBuf, String)> {
    use std::os::windows::fs::MetadataExt;
    const REPARSE: u32 = 0x400;
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        if let Ok(sddl) = wm_core::acl::dacl_sddl(&p)
            && has_explicit_entries(&sddl)
        {
            found.push((p.clone(), sddl));
        }
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir()
            && meta.file_attributes() & REPARSE == 0
            && let Ok(entries) = std::fs::read_dir(&p)
        {
            let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            children.sort();
            stack.extend(children.into_iter().rev());
        }
    }
    found
}

/// Resets a folder tree to inherit from its parent, saving every item that had
/// permissions of its own. Returns the undo (restores those, parents first), or None
/// if nothing differed from the inherited permissions.
fn reset_folder(dir: &Path, log: &mut Vec<String>) -> Result<Option<(String, UndoAction)>> {
    let saved = explicit_acls(dir);
    let d = dir.display().to_string();
    if saved.is_empty() {
        log.push(format!("  {d}: already inherits its permissions"));
        return Ok(None);
    }
    log.push(format!(
        "  {d}: {} items have permissions of their own",
        saved.len()
    ));
    run("icacls", &[&d, "/reset", "/T", "/C", "/Q"], log)?;
    let steps = saved
        .into_iter()
        .map(|(path, sddl)| UndoAction::RestoreAcl { path, sddl })
        .collect();
    Ok(Some((
        format!("Reset the permissions of {d}"),
        UndoAction::Sequence { steps },
    )))
}

fn fix_folders(out: &mut Outcome, status: &mut dyn FnMut(&str)) -> Result<()> {
    for dir in personal_folders() {
        status(&dir.display().to_string());
        if let Some(undo) = reset_folder(&dir, &mut out.log)? {
            out.undo.push(undo);
        }
    }
    Ok(())
}

/// Per-user policy keys are read-only by design; they are not "broken".
fn is_policy(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.starts_with(r"hkcu\software\policies")
        || k.starts_with(r"hkcu\software\microsoft\windows\currentversion\policies")
}

/// Keys under `root` this account cannot read (policy keys excluded).
pub fn unreadable_keys(root: &str, max_depth: usize) -> Vec<String> {
    fn walk(key: &str, depth: usize, max: usize, found: &mut Vec<String>) {
        let Ok(k) = registry::open_read(key) else { return };
        for name in k.enum_keys().flatten() {
            let child = format!(r"{key}\{name}");
            if is_policy(&child) {
                continue;
            }
            if registry::open_read(&child).is_err() {
                found.push(child);
            } else if depth < max {
                walk(&child, depth + 1, max, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, 0, max_depth, &mut found);
    found
}

fn fix_registry_permissions(out: &mut Outcome, root: &str) -> Result<()> {
    let keys = unreadable_keys(root, 16);
    if keys.is_empty() {
        out.log.push("  every key is readable".into());
        return Ok(());
    }
    for key in keys {
        let sddl = match wm_core::acl::reg_dacl_sddl(&key) {
            Ok(s) => s,
            Err(e) => {
                out.log
                    .push(format!("  {key}: {e:#} (an administrator must take ownership)"));
                continue;
            }
        };
        match wm_core::acl::reset_reg_dacl(&key) {
            Ok(()) => {
                out.log.push(format!("  reset {key}"));
                out.undo.push((
                    format!("Reset the permissions of {key}"),
                    UndoAction::RestoreRegAcl { key, sddl },
                ));
            }
            Err(e) => out.log.push(format!("  {key}: {e:#}")),
        }
    }
    Ok(())
}

const WINLOGON: &str = r"HKLM\Software\Microsoft\Windows NT\CurrentVersion\Winlogon";
const USER_WINLOGON: &str = r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon";

/// Values that differ from Windows' defaults: (key, name, expected or None to delete).
pub fn shell_problems() -> Vec<(&'static str, &'static str, Option<String>)> {
    let userinit = format!(r"{}\system32\userinit.exe,", paths::expand_env("%SystemRoot%"));
    let read = |k: &str, n: &str| registry::read_raw(k, n).map(|v| registry::decode_string(&v));
    let mut out = Vec::new();
    if !read(WINLOGON, "Shell").is_some_and(|s| s.trim().eq_ignore_ascii_case("explorer.exe")) {
        out.push((WINLOGON, "Shell", Some("explorer.exe".to_owned())));
    }
    if !read(WINLOGON, "Userinit").is_some_and(|s| s.trim().eq_ignore_ascii_case(&userinit)) {
        out.push((WINLOGON, "Userinit", Some(userinit)));
    }
    if read(USER_WINLOGON, "Shell").is_some() {
        out.push((USER_WINLOGON, "Shell", None));
    }
    out
}

fn fix_shell(out: &mut Outcome) -> Result<()> {
    let problems = shell_problems();
    if problems.is_empty() {
        out.log
            .push("  shell and sign-in programs are the Windows defaults".into());
    } else {
        let snaps: Vec<_> = problems
            .iter()
            .map(|(k, n, _)| registry::snapshot(k, n))
            .collect();
        for (key, name, value) in &problems {
            match value {
                Some(v) => {
                    registry::write_raw(key, name, &registry::string(v))?;
                    out.log.push(format!("  set {key}\\{name} = {v}"));
                }
                None => {
                    registry::delete_value(key, name)?;
                    out.log.push(format!("  removed {key}\\{name}"));
                }
            }
        }
        out.undo.push((
            "Restored the Windows shell and sign-in programs".into(),
            UndoAction::RestoreRegValues { values: snaps },
        ));
    }
    restart_explorer()?;
    out.log.push("  Explorer restarted".into());
    Ok(())
}

fn other_ntfs_drives(log: &mut Vec<String>) -> Vec<String> {
    let system = paths::expand_env("%SystemDrive%").to_ascii_uppercase();
    let text = run(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "Get-Volume | Where-Object { $_.DriveType -eq 'Fixed' -and $_.DriveLetter -and $_.FileSystem -eq 'NTFS' } | ForEach-Object { \"DRIVE=$($_.DriveLetter):\" }",
        ],
        log,
    )
    .unwrap_or_default();
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("DRIVE="))
        .map(str::to_ascii_uppercase)
        .filter(|d| *d != system)
        .collect()
}

fn fix_internet(log: &mut Vec<String>) -> Result<()> {
    run("ipconfig", &["/flushdns"], log)?;
    run("netsh", &["winhttp", "reset", "proxy"], log)?;
    let _ = run("ipconfig", &["/renew"], log);
    match "www.msftconnecttest.com:80"
        .to_socket_addrs()
        .map(|mut a| a.next().is_some())
    {
        Ok(true) => log.push("  name lookup works".into()),
        _ => log.push("  name lookup still fails: check the cable or Wi-Fi, or restart the router".into()),
    }
    Ok(())
}

/// Runs one fix. `status` receives progress text.
pub fn apply(fix: &str, out: &mut Outcome, status: &mut dyn FnMut(&str)) -> Result<()> {
    let log = &mut out.log;
    match fix {
        "component" => run_long(
            "dism",
            &["/Online", "/Cleanup-Image", "/RestoreHealth"],
            log,
            status,
        ),
        "sysfiles" => run_long("sfc", &["/scannow"], log, status),
        "wmi" => {
            let text = run("winmgmt", &["/verifyrepository"], log)?;
            if text.to_ascii_lowercase().contains("inconsistent") {
                run("winmgmt", &["/salvagerepository"], log)?;
            }
            Ok(())
        }
        "syspart" => {
            let drive = paths::expand_env("%SystemDrive%");
            run_long("chkdsk", &[&drive, "/scan"], log, status)
        }
        "volumes" => {
            let drives = other_ntfs_drives(log);
            if drives.is_empty() {
                log.push("  no other fixed NTFS drives".into());
            }
            for d in drives {
                if let Err(e) = run_long("chkdsk", &[&d, "/scan"], log, status) {
                    log.push(format!("  {e:#}"));
                }
            }
            Ok(())
        }
        "internet" => fix_internet(log),
        "assoc" => fix_associations(out),
        "folders" => fix_folders(out, status),
        "regperm" => fix_registry_permissions(out, r"HKCU\Software"),
        "shell" => fix_shell(out),
        "dns" => run("ipconfig", &["/flushdns"], log).map(drop),
        "explorer" => {
            restart_explorer()?;
            log.push("  Explorer restarted".into());
            Ok(())
        }
        "icons" => kill_explorer_then(log, |log| {
            delete_matching("%LOCALAPPDATA%", "IconCache.db", log);
            delete_matching(
                r"%LOCALAPPDATA%\Microsoft\Windows\Explorer",
                "iconcache_*.db",
                log,
            );
        }),
        "thumbs" => kill_explorer_then(log, |log| {
            delete_matching(
                r"%LOCALAPPDATA%\Microsoft\Windows\Explorer",
                "thumbcache_*.db",
                log,
            );
        }),
        "store" => {
            log.push("> wsreset.exe (the Store opens when it finishes)".into());
            wm_core::shell::shell_open("wsreset.exe", "")
        }
        "time" => {
            let _ = run("net", &["start", "w32time"], log);
            run("w32tm", &["/resync", "/force"], log).map(drop)
        }
        "print" => {
            run("net", &["stop", "spooler"], log)?;
            delete_matching(r"%WINDIR%\System32\spool\PRINTERS", "*", log);
            run("net", &["start", "spooler"], log).map(drop)
        }
        "network" => {
            run("netsh", &["winsock", "reset"], log)?;
            run("netsh", &["int", "ip", "reset"], log).map(drop)
        }
        "update" => {
            for s in ["wuauserv", "bits", "cryptsvc"] {
                let _ = run("net", &["stop", s, "/y"], log);
            }
            set_aside(r"%WINDIR%\SoftwareDistribution", log);
            set_aside(r"%WINDIR%\System32\catroot2", log);
            for s in ["cryptsvc", "bits", "wuauserv"] {
                let _ = run("net", &["start", s], log);
            }
            Ok(())
        }
        other => bail!("unknown fix {other}"),
    }
}

pub struct OneClickFixer {
    repairs: Vec<bool>,
    quick: Vec<bool>,
    task: Option<BackgroundTask<Outcome>>,
    log: Vec<String>,
    confirm: Confirm<Vec<Fix>>,
    message: Message,
}

impl Default for OneClickFixer {
    fn default() -> Self {
        Self {
            repairs: REPAIRS
                .iter()
                .map(|f| matches!(f.id, "wmi" | "internet" | "assoc" | "shell"))
                .collect(),
            quick: QUICK.iter().map(|f| matches!(f.id, "dns" | "icons")).collect(),
            task: None,
            log: Vec::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

/// A group of fixes with a "Select all" box.
fn group(
    ui: &mut egui::Ui,
    title: &str,
    fixes: &[Fix],
    selected: &mut [bool],
    elevated: bool,
    admin_hint: &str,
) {
    let usable = |f: &Fix| !f.admin || elevated;
    ui.horizontal(|ui| {
        ui.strong(title);
        let mut all = fixes
            .iter()
            .zip(selected.iter())
            .filter(|(f, _)| usable(f))
            .all(|(_, s)| *s);
        if ui.checkbox(&mut all, "Select all").changed() {
            for (f, s) in fixes.iter().zip(selected.iter_mut()) {
                *s = all && usable(f);
            }
        }
    });
    for (f, sel) in fixes.iter().zip(selected.iter_mut()) {
        ui.horizontal(|ui| {
            let ok = usable(f);
            let mut off = false;
            let state = if ok { sel } else { &mut off };
            ui.add_enabled(ok, egui::Checkbox::new(state, f.title))
                .on_disabled_hover_text(admin_hint);
            ui.weak(f.description);
            if f.restart {
                ui.weak("· restart required");
            }
        });
    }
}

impl Tool for OneClickFixer {
    fn id(&self) -> &'static str {
        "fixer"
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                match r {
                    Ok(out) => {
                        for (what, undo) in out.undo {
                            let _ = ctx.journal.record("fixer", what, undo);
                        }
                        let failed = out.log.iter().any(|l| l.starts_with("  failed:"));
                        self.log = out.log;
                        self.message = Some(if failed {
                            Err("Finished with errors. See the log.".into())
                        } else {
                            Ok("Done. See the log for details.".into())
                        });
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if let Some(fixes) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                for f in &fixes {
                    let _ = ctx
                        .journal
                        .record("fixer", format!("Ran fix: {}", f.title), UndoAction::None);
                }
                self.log.clear();
                self.message = None;
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    let mut out = Outcome::default();
                    let n = fixes.len() as f32;
                    for (i, f) in fixes.iter().enumerate() {
                        p.report(i as f32 / n, f.title);
                        out.log.push(format!("== {} ==", f.title));
                        let mut status = |s: &str| p.report(i as f32 / n, format!("{}: {s}", f.title));
                        if let Err(e) = apply(f.id, &mut out, &mut status) {
                            out.log.push(format!("  failed: {e:#}"));
                        }
                    }
                    Ok(out)
                }));
            }
        }

        ui.horizontal(|ui| {
            ui.label("Tick the problems to fix, then press Start. Each repair is a standard Windows repair.");
            if ui.link("More repairs in Repair Center").clicked() {
                request_open_tool(ui.ctx(), "repair");
            }
        });
        if !ctx.elevated {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Greyed-out repairs need administrator rights (restart the app as administrator).",
            );
        }
        ui.add_space(4.0);
        let hint = ctx.i18n.tr("shell-needs-admin");
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - 120.0).max(160.0))
            .show(ui, |ui| {
                group(ui, "Repairs", &REPAIRS, &mut self.repairs, ctx.elevated, &hint);
                ui.add_space(8.0);
                group(ui, "Quick fixes", &QUICK, &mut self.quick, ctx.elevated, &hint);
            });
        let chosen: Vec<Fix> = REPAIRS
            .iter()
            .zip(&self.repairs)
            .chain(QUICK.iter().zip(&self.quick))
            .filter(|(f, s)| **s && (!f.admin || ctx.elevated))
            .map(|(f, _)| *f)
            .collect();
        ui.separator();
        ui.horizontal(|ui| {
            let label = match chosen.len() {
                0 => "Start".to_owned(),
                1 => "Start (1 repair)".to_owned(),
                n => format!("Start ({n} repairs)"),
            };
            if ui
                .add_enabled(
                    !chosen.is_empty() && self.task.is_none(),
                    egui::Button::new(label),
                )
                .clicked()
            {
                let names: Vec<&str> = chosen.iter().map(|f| f.title).collect();
                self.confirm
                    .ask(format!("Run: {}?", names.join(", ")), chosen.clone());
            }
            if let Some(t) = &self.task {
                ui.add(egui::ProgressBar::new(t.progress).desired_width(160.0));
                ui.label(&t.status);
            }
        });
        show_message(ui, &self.message);
        if !self.log.is_empty() {
            egui::CollapsingHeader::new(format!("Log ({} lines)", self.log.len()))
                .default_open(true)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(self.log.join("\n")).monospace().small())
                                .selectable(true),
                        );
                    });
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_ids_unique_and_known() {
        assert!(has_explicit_entries("D:PAI(A;OICIID;FA;;;SY)"));
        assert!(has_explicit_entries("D:AI(A;OICIID;FA;;;SY)(A;;FA;;;BA)"));
        assert!(!has_explicit_entries("D:AI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)"));
        let mut ids: Vec<&str> = REPAIRS.iter().chain(&QUICK).map(|f| f.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), REPAIRS.len() + QUICK.len());
        let mut out = Outcome::default();
        assert!(apply("nope", &mut out, &mut |_| {}).is_err());
    }

    #[test]
    fn healthy_pc_checks() {
        // On a healthy PC there is nothing to undo for these.
        assert!(personal_folders().iter().all(|p| p.is_dir()));
        assert!(unreadable_keys(r"HKCU\Software", 3).iter().all(|k| !is_policy(k)));
        assert!(is_policy(r"HKCU\Software\Policies\Microsoft"));
    }

    #[test]
    fn folder_reset_and_undo() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Docs");
        let child = target.join("sub");
        std::fs::create_dir_all(&child).unwrap();
        let backups = dir.path().join("b");
        std::fs::create_dir_all(&backups).unwrap();
        // Give the child its own protected permissions (owner keeps access to revert).
        let custom = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)";
        wm_core::acl::set_dacl_sddl(&child, custom).unwrap();
        let custom_now = wm_core::acl::dacl_sddl(&child).unwrap();
        assert!(custom_now.starts_with("D:P"), "{custom_now}");

        let mut log = Vec::new();
        let (_, undo) = reset_folder(&target, &mut log).unwrap().expect("reset");
        let reset = wm_core::acl::dacl_sddl(&child).unwrap();
        assert!(
            !reset.starts_with("D:P"),
            "inherits after reset: {reset}
{log:?}"
        );

        let journal = wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &backups).unwrap();
        let e = journal.record("fixer", "test", undo).unwrap();
        journal.undo(e.id).unwrap();
        assert_eq!(wm_core::acl::dacl_sddl(&child).unwrap(), custom_now);
    }

    #[test]
    fn registry_permission_repair_resets_and_undoes() {
        let root = format!(r"HKCU\Software\WM-fixer-test-{}", std::process::id());
        let locked = format!(r"{root}\locked");
        registry::write_raw(&locked, "v", &registry::dword(1)).unwrap();
        let before = wm_core::acl::reg_dacl_sddl(&locked).unwrap();
        wm_core::acl::set_reg_dacl_sddl(&locked, "D:P(A;;KA;;;SY)").unwrap();
        assert_eq!(unreadable_keys(&root, 4), vec![locked.clone()]);

        let mut out = Outcome::default();
        fix_registry_permissions(&mut out, &root).unwrap();
        assert!(registry::open_read(&locked).is_ok());
        assert!(unreadable_keys(&root, 4).is_empty());
        assert_eq!(out.undo.len(), 1);
        assert!(
            matches!(&out.undo[0].1, UndoAction::RestoreRegAcl { sddl, .. } if sddl.contains("(A;;KA;;;SY)") && sddl.starts_with("D:P"))
        );
        let _ = before;
        delete_key(&root).unwrap();
    }
}
