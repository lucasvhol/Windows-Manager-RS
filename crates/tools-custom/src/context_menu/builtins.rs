//! Ready-made context menu commands, and adding commands (built-in or any program)
//! to chosen locations. Added keys are named `WinManager.<name>` and journaled.

use anyhow::{Result, bail};
use wm_core::{AppCtx, paths::expand_env, registry, safety::UndoAction};

use super::{LOCATIONS, OURS, TOOL};

/// `{p}` in `args` is replaced by `%1` (selected item) or `%V` (folder background).
pub struct Builtin {
    pub id: &'static str,
    pub label: &'static str,
    pub exe: &'static str,
    pub args: &'static str,
    pub admin: bool,
    /// A shell extension (CLSID) instead of a command.
    pub handler: Option<&'static str>,
    /// Works on a file, folder or folder background (needs `{p}`).
    pub needs_path: bool,
}

const fn cmd(id: &'static str, label: &'static str, exe: &'static str, args: &'static str) -> Builtin {
    Builtin {
        id,
        label,
        exe,
        args,
        admin: false,
        handler: None,
        needs_path: false,
    }
}

const fn on_path(id: &'static str, label: &'static str, exe: &'static str, args: &'static str) -> Builtin {
    Builtin {
        id,
        label,
        exe,
        args,
        admin: false,
        handler: None,
        needs_path: true,
    }
}

const fn admin(b: Builtin) -> Builtin {
    Builtin { admin: true, ..b }
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "CopyTo",
        label: "Copy to folder…",
        exe: "",
        args: "",
        admin: false,
        handler: Some("{C2FBB630-2971-11D1-A18C-00C04FD75D13}"),
        needs_path: true,
    },
    Builtin {
        id: "MoveTo",
        label: "Move to folder…",
        exe: "",
        args: "",
        admin: false,
        handler: Some("{C2FBB631-2971-11D1-A18C-00C04FD75D13}"),
        needs_path: true,
    },
    on_path(
        "CmdHere",
        "Open Command Prompt here",
        r"%SystemRoot%\System32\cmd.exe",
        r#"/s /k pushd "{p}""#,
    ),
    admin(on_path(
        "CmdAdminHere",
        "Open Command Prompt here as administrator",
        r"%SystemRoot%\System32\cmd.exe",
        r#"/s /k pushd "{p}""#,
    )),
    on_path(
        "PowerShellHere",
        "Open PowerShell here",
        r"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe",
        r#"-NoExit -Command "Set-Location -LiteralPath '{p}'""#,
    ),
    admin(on_path(
        "PowerShellAdminHere",
        "Open PowerShell here as administrator",
        r"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe",
        r#"-NoExit -Command "Set-Location -LiteralPath '{p}'""#,
    )),
    on_path(
        "TerminalHere",
        "Open Windows Terminal here",
        r"%LOCALAPPDATA%\Microsoft\WindowsApps\wt.exe",
        r#"-d "{p}""#,
    ),
    on_path(
        "CopyPath",
        "Copy full path",
        r"%SystemRoot%\System32\cmd.exe",
        r#"/c echo|set /p="{p}"|clip"#,
    ),
    admin(on_path(
        "TakeOwnership",
        "Take ownership",
        r"%SystemRoot%\System32\cmd.exe",
        r#"/c takeown /f "{p}" /r /d y && icacls "{p}" /grant *S-1-5-32-544:F /t /c && pause"#,
    )),
    on_path(
        "OpenNotepad",
        "Open with Notepad",
        r"%SystemRoot%\System32\notepad.exe",
        r#""{p}""#,
    ),
    on_path(
        "Sha256",
        "Show SHA-256 checksum",
        r"%SystemRoot%\System32\cmd.exe",
        r#"/k certutil -hashfile "{p}" SHA256"#,
    ),
    on_path(
        "ListFolder",
        "Copy folder listing",
        r"%SystemRoot%\System32\cmd.exe",
        r#"/c dir /b "{p}" | clip"#,
    ),
    cmd(
        "RestartExplorer",
        "Restart Explorer",
        r"%SystemRoot%\System32\cmd.exe",
        "/c taskkill /f /im explorer.exe & start explorer.exe",
    ),
    cmd(
        "EmptyRecycleBin",
        "Empty the Recycle Bin",
        r"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe",
        "-NoProfile -WindowStyle Hidden -Command Clear-RecycleBin -Force",
    ),
    cmd(
        "ControlPanel",
        "Control Panel",
        r"%SystemRoot%\System32\control.exe",
        "",
    ),
    cmd(
        "Settings",
        "Settings",
        r"%SystemRoot%\explorer.exe",
        "ms-settings:",
    ),
    cmd(
        "TaskManager",
        "Task Manager",
        r"%SystemRoot%\System32\Taskmgr.exe",
        "",
    ),
    cmd(
        "RegistryEditor",
        "Registry Editor",
        r"%SystemRoot%\regedit.exe",
        "",
    ),
    cmd(
        "Services",
        "Services",
        r"%SystemRoot%\System32\mmc.exe",
        "services.msc",
    ),
    cmd(
        "DeviceManager",
        "Device Manager",
        r"%SystemRoot%\System32\mmc.exe",
        "devmgmt.msc",
    ),
    cmd(
        "ComputerManagement",
        "Computer Management",
        r"%SystemRoot%\System32\mmc.exe",
        "compmgmt.msc",
    ),
    cmd(
        "EventViewer",
        "Event Viewer",
        r"%SystemRoot%\System32\mmc.exe",
        "eventvwr.msc",
    ),
    cmd(
        "DiskManagement",
        "Disk Management",
        r"%SystemRoot%\System32\mmc.exe",
        "diskmgmt.msc",
    ),
    cmd(
        "SystemProperties",
        "System Properties",
        r"%SystemRoot%\System32\SystemPropertiesAdvanced.exe",
        "",
    ),
    cmd(
        "EnvironmentVariables",
        "Environment Variables",
        r"%SystemRoot%\System32\rundll32.exe",
        "sysdm.cpl,EditEnvironmentVariables",
    ),
    cmd(
        "NetworkConnections",
        "Network Connections",
        r"%SystemRoot%\System32\control.exe",
        "ncpa.cpl",
    ),
    cmd(
        "PowerOptions",
        "Power Options",
        r"%SystemRoot%\System32\control.exe",
        "powercfg.cpl",
    ),
    cmd(
        "ProgramsFeatures",
        "Programs and Features",
        r"%SystemRoot%\System32\control.exe",
        "appwiz.cpl",
    ),
    cmd(
        "DiskCleanup",
        "Disk Cleanup",
        r"%SystemRoot%\System32\cleanmgr.exe",
        "",
    ),
    cmd(
        "Defragment",
        "Optimize Drives",
        r"%SystemRoot%\System32\dfrgui.exe",
        "",
    ),
    cmd("Calculator", "Calculator", r"%SystemRoot%\System32\calc.exe", ""),
    cmd("Notepad", "Notepad", r"%SystemRoot%\System32\notepad.exe", ""),
    cmd("Paint", "Paint", r"%SystemRoot%\explorer.exe", "ms-paint:"),
    cmd(
        "Snip",
        "Take a screenshot",
        r"%SystemRoot%\explorer.exe",
        "ms-screenclip:",
    ),
    cmd(
        "Lock",
        "Lock this PC",
        r"%SystemRoot%\System32\rundll32.exe",
        "user32.dll,LockWorkStation",
    ),
    cmd("SignOut", "Sign out", r"%SystemRoot%\System32\shutdown.exe", "/l"),
    cmd(
        "Restart",
        "Restart",
        r"%SystemRoot%\System32\shutdown.exe",
        "/r /t 0",
    ),
    cmd(
        "ShutDown",
        "Shut down",
        r"%SystemRoot%\System32\shutdown.exe",
        "/s /t 0",
    ),
    cmd(
        "KillNotResponding",
        "End programs that are not responding",
        r"%SystemRoot%\System32\taskkill.exe",
        r#"/f /fi "status eq not responding""#,
    ),
    admin(cmd(
        "FlushDns",
        "Flush the DNS cache",
        r"%SystemRoot%\System32\ipconfig.exe",
        "/flushdns",
    )),
    cmd("WinManager", "Win Manager RS", "", ""),
];

/// What the new command runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Builtin(usize),
    Program { path: String, args: String, admin: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewCommand {
    pub label: String,
    /// Location ids from [`LOCATIONS`].
    pub locations: Vec<&'static str>,
    pub source: Source,
    pub extended: bool,
    pub position: String,
    /// Install for all users (HKLM, administrator) instead of only this account.
    pub all_users: bool,
}

/// `WinManager.<letters and digits of the label>`.
pub fn key_name(label: &str) -> String {
    let slug: String = label.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("{OURS}{}", if slug.is_empty() { "Command".into() } else { slug })
}

/// Runs `exe args` elevated through PowerShell's `Start-Process -Verb RunAs`.
fn elevated(exe: &str, args: &str) -> String {
    let ps = expand_env(r"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe");
    let list = if args.is_empty() {
        String::new()
    } else {
        format!(
            " -ArgumentList '{}'",
            args.replace('\'', "''").replace('"', "\\\"")
        )
    };
    format!(
        r#""{ps}" -NoProfile -WindowStyle Hidden -Command "Start-Process -FilePath '{exe}'{list} -Verb RunAs""#
    )
}

/// The command line stored in the registry.
pub fn command_line(exe: &str, args: &str, admin: bool, placeholder: &str) -> String {
    let exe = expand_env(exe);
    let args = args.replace("{p}", placeholder);
    if admin {
        elevated(&exe, &args)
    } else if args.is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {args}")
    }
}

/// Creates the command in every chosen location. Returns the keys created.
pub fn add_command(ctx: &AppCtx<'_>, c: &NewCommand) -> Result<Vec<String>> {
    if c.label.trim().is_empty() {
        bail!("Enter a name for the command");
    }
    if c.locations.is_empty() {
        bail!("Choose at least one place for the command");
    }
    if c.all_users && !ctx.elevated {
        bail!("Adding a command for all users needs administrator rights");
    }
    let root = if c.all_users {
        r"HKLM\SOFTWARE\Classes"
    } else {
        r"HKCU\Software\Classes"
    };
    let name = key_name(&c.label);
    let mut plan = Vec::new();
    for loc in LOCATIONS.iter().filter(|l| c.locations.contains(&l.id)) {
        let placeholder = if loc.background { "%V" } else { "%1" };
        for class in loc.classes.iter().take(1) {
            let (key, writes): (String, Vec<(String, &str, registry::RawValue)>) = match &c.source {
                Source::Builtin(i) if BUILTINS[*i].handler.is_some() => {
                    let key = format!(r"{root}\{class}\shellex\ContextMenuHandlers\{name}");
                    (
                        key.clone(),
                        vec![(
                            key,
                            "",
                            registry::string(BUILTINS[*i].handler.unwrap_or_default()),
                        )],
                    )
                }
                source => {
                    let (exe, args, admin) = match source {
                        Source::Builtin(i) => {
                            let b = &BUILTINS[*i];
                            let exe = if b.id == "WinManager" {
                                std::env::current_exe()?.display().to_string()
                            } else {
                                b.exe.to_owned()
                            };
                            (exe, b.args.to_owned(), b.admin)
                        }
                        Source::Program { path, args, admin } => (path.clone(), args.clone(), *admin),
                    };
                    if exe.trim().is_empty() {
                        bail!("Choose a program for the command");
                    }
                    let key = format!(r"{root}\{class}\shell\{name}");
                    let mut w = vec![
                        (key.clone(), "MUIVerb", registry::string(c.label.trim())),
                        (key.clone(), "Icon", registry::string(&expand_env(&exe))),
                        (
                            format!(r"{key}\command"),
                            "",
                            registry::string(&command_line(&exe, &args, admin, placeholder)),
                        ),
                    ];
                    if admin {
                        w.push((key.clone(), "HasLUAShield", registry::string("")));
                    }
                    if c.extended {
                        w.push((key.clone(), "Extended", registry::string("")));
                    }
                    if !c.position.is_empty() {
                        w.push((key.clone(), "Position", registry::string(&c.position)));
                    }
                    (key, w)
                }
            };
            if registry::open_read(&key).is_ok() {
                bail!("\"{}\" already exists in {}", c.label.trim(), loc.label);
            }
            plan.push((key, writes));
        }
    }
    // Everything is checked before the first write, so a refusal changes nothing.
    let mut created = Vec::new();
    let mut undo = Vec::new();
    for (key, writes) in plan {
        undo.push(ctx.journal.backup_reg_key(&key)?);
        for (k, n, v) in &writes {
            registry::write_raw(k, n, v)?;
        }
        created.push(key);
    }
    let places: Vec<&str> = LOCATIONS
        .iter()
        .filter(|l| c.locations.contains(&l.id))
        .map(|l| l.label)
        .collect();
    ctx.journal.record(
        TOOL,
        format!(
            "Added \"{}\" to the context menu ({})",
            c.label.trim(),
            places.join(", ")
        ),
        UndoAction::Sequence { steps: undo },
    )?;
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::super::tests::with_ctx;
    use super::*;

    #[test]
    fn builtins_are_well_formed() {
        assert!(BUILTINS.len() >= 40, "{}", BUILTINS.len());
        let mut ids: Vec<&str> = BUILTINS.iter().map(|b| b.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), BUILTINS.len(), "unique ids");
        for b in BUILTINS {
            assert_eq!(
                b.needs_path,
                b.args.contains("{p}") || b.handler.is_some(),
                "{}",
                b.id
            );
            if b.handler.is_none() && b.id != "WinManager" && b.id != "TerminalHere" {
                assert!(
                    std::path::Path::new(&expand_env(b.exe)).exists(),
                    "{} → {} missing",
                    b.id,
                    b.exe
                );
            }
        }
    }

    #[test]
    fn command_lines() {
        assert_eq!(key_name("Open CMD here!"), "WinManager.OpenCMDhere");
        let c = command_line(r"C:\x\a.exe", r#""{p}""#, false, "%V");
        assert_eq!(c, r#""C:\x\a.exe" "%V""#);
        let e = command_line(r"C:\x\a.exe", r#"/k "{p}""#, true, "%1");
        assert!(
            e.contains(r#"Start-Process -FilePath 'C:\x\a.exe' -ArgumentList '/k \"%1\"' -Verb RunAs"#),
            "{e}"
        );
    }

    #[test]
    fn add_builtin_and_handler_then_undo() {
        let _serial = crate::context_menu::TEST_REG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let label = format!("WM Test Cmd {}", std::process::id());
        /// Removes the test keys even if an assertion fails.
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for class in [
                    r"Directory\Background\shell",
                    r"Directory\shell",
                    r"AllFilesystemObjects\shellex\ContextMenuHandlers",
                ] {
                    for suffix in ["", "copy"] {
                        let k = format!(r"HKCU\Software\Classes\{class}\{}{suffix}", self.0);
                        if let Ok((root, sub)) = registry::split_path(&k) {
                            let _ = root.delete_subkey_all(sub);
                        }
                    }
                }
            }
        }
        let _cleanup = Cleanup(key_name(&label));
        let cmd_idx = BUILTINS.iter().position(|b| b.id == "CmdHere").unwrap();
        let copy_idx = BUILTINS.iter().position(|b| b.id == "CopyTo").unwrap();
        with_ctx(|ctx, journal| {
            let keys = add_command(
                ctx,
                &NewCommand {
                    label: label.clone(),
                    locations: vec!["background", "folders"],
                    source: Source::Builtin(cmd_idx),
                    extended: true,
                    position: "Top".into(),
                    all_users: false,
                },
            )
            .unwrap();
            assert_eq!(keys.len(), 2);
            let bg = keys.iter().find(|k| k.contains("Background")).unwrap();
            let line = registry::decode_string(&registry::read_raw(&format!(r"{bg}\command"), "").unwrap());
            assert!(line.ends_with(r#"/s /k pushd "%V""#), "{line}");
            assert!(registry::read_raw(bg, "Extended").is_some());
            let hkeys = add_command(
                ctx,
                &NewCommand {
                    label: format!("{label} copy"),
                    locations: vec!["all"],
                    source: Source::Builtin(copy_idx),
                    extended: false,
                    position: String::new(),
                    all_users: false,
                },
            )
            .unwrap();
            assert!(hkeys[0].contains(r"shellex\ContextMenuHandlers"));
            assert!(
                add_command(
                    ctx,
                    &NewCommand {
                        label: label.clone(),
                        locations: vec!["background"],
                        source: Source::Builtin(cmd_idx),
                        extended: false,
                        position: String::new(),
                        all_users: false
                    }
                )
                .is_err(),
                "duplicates refused"
            );
            for e in journal.entries() {
                journal.undo(e.id).unwrap();
            }
            for k in keys.iter().chain(&hkeys) {
                assert!(registry::open_read(k).is_err(), "{k} removed by undo");
            }
        });
    }
}
