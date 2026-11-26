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
    AppCtx, Category, Layout, Tool,
    command::run_streaming,
    egui, icons, paths,
    process::{hidden_command, restart_explorer},
    registry,
    safety::UndoAction,
    shell::{KnownFolder, known_folder},
    task::BackgroundTask,
    ui::{CheckItem, Confirm, Message, checklist, frame, request_open_tool, show_message},
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
        "Scan and restore component store health",
        "DISM RestoreHealth: replaces damaged Windows components from Windows Update. 10–30 minutes.",
        true,
    ),
    fix(
        "sysfiles",
        "Scan and repair system files",
        "SFC: checks protected Windows files and restores damaged ones. 10–20 minutes.",
        true,
    ),
    fix(
        "wmi",
        "Rebuild WMI repository",
        "Verifies the WMI repository (used by many tools and installers) and salvages it if inconsistent.",
        true,
    ),
    fix(
        "syspart",
        "Scan and repair the system partition",
        "CHKDSK online scan of the Windows drive; problems found are fixed at the next restart.",
        true,
    ),
    fix(
        "volumes",
        "Scan and repair the health of all volumes",
        "CHKDSK online scan of every other fixed NTFS drive.",
        true,
    ),
    fix(
        "internet",
        "Detect and repair internet connection",
        "Flushes DNS, resets the system proxy (WinHTTP), renews addresses, then tests name lookup.",
        true,
    ),
    fix(
        "assoc",
        "Detect and repair file association errors",
        "Removes per-user overrides that hijack .exe, .lnk, .bat, .cmd, .com, .msi, .reg and similar files.",
        false,
    ),
    fix(
        "folders",
        "Detect and repair system folder permissions",
        "Desktop, Documents, Downloads, Music, Pictures and Videos inherit from your profile again (saved first).",
        false,
    ),
    fix(
        "regperm",
        "Detect and repair registry key permissions",
        "Keys in your part of the registry that you cannot read inherit their permissions again (saved first).",
        false,
    ),
    fix(
        "shell",
        "Detect and repair File Explorer / Shell issues",
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

pub(crate) fn run(program: &str, args: &[&str], log: &mut Vec<String>) -> Result<String> {
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
pub(crate) fn run_long(
    program: &str,
    args: &[&str],
    log: &mut Vec<String>,
    status: &mut dyn FnMut(&str),
) -> Result<()> {
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

pub(crate) fn delete_matching(dir: &str, pattern: &str, log: &mut Vec<String>) {
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

pub(crate) fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S%.3f").to_string()
}

/// Exports a key to the backups folder; the undo re-imports it.
pub(crate) fn export_key(key: &str) -> Result<UndoAction> {
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

pub(crate) fn delete_key(key: &str) -> Result<()> {
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
    /// The fixes of the running (or last) run, in order.
    running: Vec<Fix>,
    log: Vec<String>,
    /// Each fix's result from the last run, by id.
    results: Vec<(&'static str, String)>,
    confirm: Confirm<Vec<Fix>>,
    message: Message,
}

impl Default for OneClickFixer {
    fn default() -> Self {
        Self {
            // Like the original, every repair starts ticked; the quick fixes start off.
            repairs: vec![true; REPAIRS.len()],
            quick: vec![false; QUICK.len()],
            task: None,
            running: Vec::new(),
            log: Vec::new(),
            results: Vec::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

/// Each fix's result, read from the run log ("== title ==" starts a fix's section;
/// a line "  failed: …" marks it failed).
pub fn results_from_log(fixes: &[Fix], log: &[String]) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for line in log {
        if let Some(title) = line.strip_prefix("== ").and_then(|l| l.strip_suffix(" ==")) {
            if let Some(f) = fixes.iter().find(|f| f.title == title) {
                out.push((f.id, "Done".into()));
            }
        } else if let Some(err) = line.strip_prefix("  failed: ")
            && let Some(last) = out.last_mut()
        {
            last.1 = format!("Failed: {err}");
        }
    }
    out
}

impl OneClickFixer {
    fn poll(&mut self, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                match r {
                    Ok(out) => {
                        for (what, undo) in out.undo {
                            let _ = ctx.journal.record("fixer", what, undo);
                        }
                        self.results = results_from_log(&self.running, &out.log);
                        let failed = self.results.iter().any(|(_, r)| r.starts_with("Failed"));
                        self.log = out.log;
                        self.message =
                            failed.then(|| Err("Finished with errors. See the repair logs.".into()));
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
    }

    fn start(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>, fixes: Vec<Fix>) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        for f in &fixes {
            let _ = ctx
                .journal
                .record("fixer", format!("Ran fix: {}", f.title), UndoAction::None);
        }
        self.log.clear();
        self.results.clear();
        self.message = None;
        self.running = fixes.clone();
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

    /// The Result column for one fix.
    fn result_of(&self, f: &Fix) -> String {
        if let Some((_, r)) = self.results.iter().find(|(id, _)| *id == f.id) {
            return r.clone();
        }
        match &self.task {
            Some(t) if t.status.starts_with(f.title) => "Running…".into(),
            Some(_) if self.running.iter().any(|r| r.id == f.id) => "Waiting…".into(),
            _ if f.restart => "Restart required afterwards".into(),
            _ => String::new(),
        }
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
        self.poll(ctx);
        if let Some(fixes) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.start(ui, ctx, fixes);
        }
        let elevated = ctx.elevated;
        let usable = |f: &Fix| !f.admin || elevated;
        let chosen: Vec<Fix> = REPAIRS
            .iter()
            .zip(&self.repairs)
            .chain(QUICK.iter().zip(&self.quick))
            .filter(|(f, s)| **s && usable(f))
            .map(|(f, _)| *f)
            .collect();
        let busy = self.task.is_some();

        let mut start = false;
        frame::footer(
            ui,
            |ui| {
                let all_fixes = REPAIRS.iter().chain(&QUICK);
                let states = self.repairs.iter().chain(&self.quick);
                let usable_states: Vec<bool> = all_fixes
                    .zip(states)
                    .filter(|(f, _)| usable(f))
                    .map(|(_, s)| *s)
                    .collect();
                let mut all = usable_states.iter().all(|s| *s);
                let partial = !all && usable_states.iter().any(|s| *s);
                if ui
                    .add_enabled(
                        !busy,
                        egui::Checkbox::new(&mut all, "Select All").indeterminate(partial),
                    )
                    .changed()
                {
                    for (f, s) in REPAIRS
                        .iter()
                        .chain(&QUICK)
                        .zip(self.repairs.iter_mut().chain(self.quick.iter_mut()))
                    {
                        *s = all && usable(f);
                    }
                }
                ui.add_space(12.0);
                if ui
                    .button(format!("{}  Repair Center", icons::FIRST_AID_KIT))
                    .clicked()
                {
                    request_open_tool(ui.ctx(), "repair");
                }
            },
            |ui| {
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                start = ui
                    .add_enabled(
                        !busy && !chosen.is_empty(),
                        egui::Button::new(format!("Start ({})", chosen.len())),
                    )
                    .clicked();
                if let Some(t) = &self.task {
                    ui.add(
                        egui::ProgressBar::new(t.progress)
                            .desired_width(160.0)
                            .show_percentage(),
                    );
                }
            },
        );
        if start {
            let names: Vec<&str> = chosen.iter().map(|f| f.title).collect();
            self.confirm
                .ask(format!("Run: {}?", names.join(", ")), chosen.clone());
        }

        let fill = ui.visuals().panel_fill;
        egui::Panel::bottom("fixer-logs")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(egui::Margin::symmetric(12, 4)),
            )
            .show(ui, |ui| {
                egui::CollapsingHeader::new(format!("Repair Logs ({} lines)", self.log.len()))
                    .id_salt("fixer-log")
                    .open(if busy { Some(false) } else { None })
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(170.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(self.log.join("\n")).monospace().small(),
                                    )
                                    .selectable(true),
                                );
                            });
                    });
            });

        let results: Vec<String> = REPAIRS.iter().chain(&QUICK).map(|f| self.result_of(f)).collect();
        let hint = ctx.i18n.tr("shell-needs-admin");
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            let fixes: Vec<&Fix> = REPAIRS.iter().chain(&QUICK).collect();
            // Fixes this account can't run show unticked.
            let mut shown: Vec<bool> = self
                .repairs
                .iter()
                .chain(&self.quick)
                .zip(&fixes)
                .map(|(s, f)| *s && usable(f))
                .collect();
            let mut rows: Vec<CheckItem<'_>> = shown
                .iter_mut()
                .zip(&fixes)
                .zip(&results)
                .map(|((on, f), result)| CheckItem {
                    on,
                    icon: fix_icon(f.id),
                    label: f.title.to_owned(),
                    options: false,
                    result: result.clone(),
                    enabled: !busy && usable(f),
                    disabled_hint: hint.clone(),
                    tooltip: f.description.to_owned(),
                })
                .collect();
            checklist(ui, "Select the items to run", "Result", &mut rows);
            drop(rows);
            for ((s, f), on) in self
                .repairs
                .iter_mut()
                .chain(self.quick.iter_mut())
                .zip(&fixes)
                .zip(shown)
            {
                if usable(f) {
                    *s = on;
                }
            }
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

fn fix_icon(id: &str) -> &'static str {
    match id {
        "component" => icons::STACK,
        "sysfiles" => icons::FILE_TEXT,
        "wmi" => icons::DATABASE,
        "syspart" | "volumes" => icons::HARD_DRIVES,
        "internet" | "network" => icons::GLOBE,
        "assoc" => icons::LINK,
        "folders" => icons::FOLDERS,
        "regperm" => icons::KEY,
        "shell" | "explorer" => icons::FOLDER_OPEN,
        "dns" => icons::NETWORK,
        "icons" | "thumbs" => icons::IMAGE,
        "store" => icons::GRID_FOUR,
        "time" => icons::CLOCK,
        "print" => icons::PRINTER,
        "update" => icons::ARROWS_CLOCKWISE,
        _ => icons::WRENCH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_are_read_from_the_log() {
        let fixes = [REPAIRS[2], QUICK[0]];
        let log: Vec<String> = [
            format!("== {} ==", fixes[0].title),
            "> winmgmt /verifyrepository".into(),
            "  failed: access denied".into(),
            format!("== {} ==", fixes[1].title),
            "> something".into(),
        ]
        .into();
        let r = results_from_log(&fixes, &log);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], (fixes[0].id, "Failed: access denied".to_owned()));
        assert_eq!(r[1], (fixes[1].id, "Done".to_owned()));
    }

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
