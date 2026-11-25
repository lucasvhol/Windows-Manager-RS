//! The Repair Center's repairs: what each one is, how to tell whether it is needed,
//! and how to carry it out. Repairs shared with the 1-Click Fixer run its code.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use wm_core::{
    icons, paths,
    process::{hidden_command, restart_explorer},
    registry,
    safety::UndoAction,
};

use crate::fixer::{self, Outcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// Runs [`apply`] in the background.
    Run,
    /// Opens a program, console or settings page: (target, arguments).
    Open(&'static str, &'static str),
    /// Opens another tool of this app.
    Tool(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repair {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub admin: bool,
    pub restart: bool,
    pub how: How,
}

const fn run(id: &'static str, title: &'static str, description: &'static str, admin: bool) -> Repair {
    Repair {
        id,
        title,
        description,
        admin,
        restart: false,
        how: How::Run,
    }
}

const fn open(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    target: &'static str,
) -> Repair {
    Repair {
        how: How::Open(target, ""),
        ..run(id, title, description, false)
    }
}

const fn tool(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    tool: &'static str,
) -> Repair {
    Repair {
        how: How::Tool(tool),
        ..run(id, title, description, false)
    }
}

const fn restart(r: Repair) -> Repair {
    Repair { restart: true, ..r }
}

/// A repair the 1-Click Fixer also offers, with its wording.
fn shared(id: &str) -> Repair {
    let f = fixer::REPAIRS
        .iter()
        .chain(&fixer::QUICK)
        .find(|f| f.id == id)
        .unwrap_or_else(|| panic!("no fixer repair {id}"));
    Repair {
        id: f.id,
        title: f.title,
        description: f.description,
        admin: f.admin,
        restart: f.restart,
        how: How::Run,
    }
}

pub struct Section {
    pub icon: &'static str,
    pub title: &'static str,
    pub repairs: Vec<Repair>,
}

const SYS: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\System";
const EXP: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
const WSYS: &str = r"Software\Policies\Microsoft\Windows\System";

type Lock = (
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// Policies that block parts of Windows, set per user or for the whole PC.
pub const LOCKS: [Lock; 11] = [
    (
        "lock-taskmgr",
        "Unblock Task Manager",
        "Removes the policy that stops Task Manager from opening.",
        &[(SYS, "DisableTaskMgr")],
    ),
    (
        "lock-regedit",
        "Unblock Registry Editor",
        "Removes the policy that stops Registry Editor from opening.",
        &[(SYS, "DisableRegistryTools")],
    ),
    (
        "lock-cmd",
        "Unblock Command Prompt",
        "Removes the policy that blocks Command Prompt and batch files.",
        &[(WSYS, "DisableCMD")],
    ),
    (
        "lock-control",
        "Unblock Control Panel and Settings",
        "Removes the policy that hides Control Panel and the Settings app.",
        &[(EXP, "NoControlPanel")],
    ),
    (
        "lock-run",
        "Unblock the Run dialog",
        "Removes the policy that hides Run (Win+R).",
        &[(EXP, "NoRun")],
    ),
    (
        "lock-folderopts",
        "Unblock Folder Options",
        "Removes the policy that hides File Explorer's Folder Options.",
        &[(EXP, "NoFolderOptions")],
    ),
    (
        "lock-menus",
        "Unblock right-click menus",
        "Removes the policies that turn off the File Explorer and taskbar right-click menus.",
        &[(EXP, "NoViewContextMenu"), (EXP, "NoTrayContextMenu")],
    ),
    (
        "lock-drives",
        "Show hidden or blocked drives",
        "Removes the policies that hide drives or stop them from being opened.",
        &[(EXP, "NoDrives"), (EXP, "NoViewOnDrive")],
    ),
    (
        "lock-programs",
        "Lift program blocks",
        "Turns off the lists of programs that may not run, or that are the only ones allowed to run.",
        &[(EXP, "DisallowRun"), (EXP, "RestrictRun")],
    ),
    (
        "lock-power",
        "Unblock shut down and sign out",
        "Removes the policies that hide Shut down and Sign out.",
        &[(EXP, "NoClose"), (EXP, "NoLogoff")],
    ),
    (
        "lock-ctrlaltdel",
        "Unblock locking and password change",
        "Removes the policies that hide Lock and Change a password from Ctrl+Alt+Del.",
        &[(SYS, "DisableLockWorkstation"), (SYS, "DisableChangePassword")],
    ),
];

pub fn sections() -> Vec<Section> {
    let mut unlock = vec![run(
        "unlock-all",
        "Remove every block found",
        "Removes all the blocks below in one go, for your account and for the whole PC.",
        true,
    )];
    unlock.extend(
        LOCKS
            .iter()
            .map(|(id, title, desc, _)| run(id, title, desc, true)),
    );
    vec![
        Section {
            icon: icons::WRENCH,
            title: "System repair",
            repairs: vec![
                shared("component"),
                shared("sysfiles"),
                run(
                    "scanhealth",
                    "Check the component store",
                    "DISM ScanHealth: looks for damage without changing anything. 5–15 minutes.",
                    true,
                ),
                run(
                    "compcleanup",
                    "Clean up the component store",
                    "DISM StartComponentCleanup: removes replaced component versions. Updates installed so far can no longer be uninstalled.",
                    true,
                ),
                shared("wmi"),
                shared("syspart"),
                shared("volumes"),
                run(
                    "msiserver",
                    "Re-register Windows Installer",
                    "Fixes setup programs that report that the Windows Installer service cannot be reached.",
                    true,
                ),
                open(
                    "memdiag",
                    "Windows Memory Diagnostic",
                    "Tests the memory (RAM) at the next restart.",
                    "mdsched.exe",
                ),
            ],
        },
        Section {
            icon: icons::TOOLBOX,
            title: "Misc repair",
            repairs: vec![
                shared("explorer"),
                shared("time"),
                shared("print"),
                shared("update"),
                run(
                    "search",
                    "Rebuild the search index",
                    "Has Windows Search index everything again from scratch. Results are incomplete until it finishes.",
                    true,
                ),
                restart(run(
                    "fontcache",
                    "Rebuild the font cache",
                    "Fixes fonts that show as boxes or in the wrong typeface.",
                    true,
                )),
            ],
        },
        Section {
            icon: icons::APP_WINDOW,
            title: "Windows apps",
            repairs: vec![
                shared("store"),
                run(
                    "storereg",
                    "Re-register the Microsoft Store",
                    "Registers the Store app again for your account. Fixes a Store that will not open.",
                    false,
                ),
                run(
                    "startreg",
                    "Repair Start and the taskbar",
                    "Registers the Start menu, the taskbar's pop-ups and search again for your account.",
                    false,
                ),
                run(
                    "appsreg",
                    "Re-register all your apps",
                    "Registers every installed app again for your account. Takes several minutes; apps that are open may fail.",
                    false,
                ),
                tool(
                    "apptool",
                    "Windows Apps Uninstaller",
                    "Remove, repair or reinstall built-in and Store apps.",
                    "apps",
                ),
                open(
                    "installedapps",
                    "Installed apps settings",
                    "Repair or reset a single app from its Advanced options.",
                    "ms-settings:appsfeatures",
                ),
            ],
        },
        Section {
            icon: icons::LIST_CHECKS,
            title: "Event viewer",
            repairs: vec![
                tool(
                    "eventstool",
                    "Event Viewer",
                    "Browse, filter and export Windows' event logs.",
                    "events",
                ),
                open(
                    "eventvwr",
                    "Windows Event Viewer",
                    "Opens Windows' own Event Viewer console.",
                    "eventvwr.msc",
                ),
                Repair {
                    how: How::Open("perfmon.exe", "/rel"),
                    ..open(
                        "reliability",
                        "Reliability Monitor",
                        "A day-by-day history of crashes, failed updates and other problems.",
                        "",
                    )
                },
                run(
                    "evtclear",
                    "Save and clear the Windows logs",
                    "Saves the Application, System and Setup logs to the logs folder, then empties them, so only new problems show.",
                    true,
                ),
                run(
                    "eventlog",
                    "Repair the event log service",
                    "Sets the Windows Event Log service to start automatically again and starts it.",
                    true,
                ),
            ],
        },
        Section {
            icon: icons::LOCK,
            title: "Unlock system",
            repairs: unlock,
        },
        Section {
            icon: icons::SHIELD,
            title: "Security settings",
            repairs: vec![
                restart(run(
                    "uac",
                    "Turn User Account Control back on",
                    "Turns UAC on again and restores its prompt for administrators, on the secure desktop.",
                    true,
                )),
                run(
                    "defender",
                    "Remove policies that turn off Microsoft Defender",
                    "Lets Microsoft Defender and its real-time protection run again.",
                    true,
                ),
                run(
                    "firewall",
                    "Turn Windows Firewall back on",
                    "Turns the firewall on for every network type and removes policies that turned it off.",
                    true,
                ),
                run(
                    "smartscreen",
                    "Turn SmartScreen back on",
                    "Removes the settings that stop SmartScreen from checking downloaded programs.",
                    true,
                ),
                run(
                    "secnotify",
                    "Restore Windows Security notifications",
                    "Removes the policies that hide Windows Security's alerts and its taskbar icon.",
                    true,
                ),
                run(
                    "wupolicy",
                    "Remove policies that block Windows Update",
                    "Lets Windows Update check and install again. If you turned updates off on purpose, leave this alone.",
                    true,
                ),
                open(
                    "winsec",
                    "Windows Security",
                    "Opens Windows Security to check protection status.",
                    "windowsdefender:",
                ),
            ],
        },
        Section {
            icon: icons::LINK,
            title: "File associations",
            repairs: vec![
                shared("assoc"),
                run(
                    "assoc-folders",
                    "Repair opening folders and drives",
                    "Removes per-user commands that take over opening folders and drives. File manager replacements set these on purpose.",
                    false,
                ),
                open(
                    "defaultapps",
                    "Default apps",
                    "Choose which app opens each file type and link.",
                    "ms-settings:defaultapps",
                ),
            ],
        },
        Section {
            icon: icons::LIST,
            title: "Context menu",
            repairs: vec![
                tool(
                    "ctxtool",
                    "Context Menu Manager",
                    "Add, remove and hide right-click menu entries.",
                    "contextmenu",
                ),
                run(
                    "classicmenu",
                    "Restore the Windows 11 right-click menu",
                    "Removes the setting that brings back the full classic menu, then restarts Explorer.",
                    false,
                ),
                run(
                    "blocked-ext",
                    "Unblock shell extensions",
                    "Empties Windows' block list of shell extensions, including entries hidden on purpose by menu tools.",
                    true,
                ),
            ],
        },
        Section {
            icon: icons::IMAGE,
            title: "Icon repair",
            repairs: vec![
                shared("icons"),
                shared("thumbs"),
                run(
                    "shell-icons",
                    "Restore system icons and shortcut arrows",
                    "Removes replacements for Windows' own icons (such as a hidden shortcut arrow), then rebuilds the icon cache.",
                    true,
                ),
                run(
                    "drive-icons",
                    "Restore drive icons",
                    "Removes custom icons and labels set for drive letters.",
                    true,
                ),
            ],
        },
        Section {
            icon: icons::NETWORK,
            title: "Network problems",
            repairs: vec![
                shared("dns"),
                shared("internet"),
                shared("network"),
                run(
                    "firewall-reset",
                    "Reset firewall rules",
                    "Saves the current rules, then puts Windows Firewall back to its default rules. Rules added by programs are removed.",
                    true,
                ),
                run(
                    "hosts-reset",
                    "Reset the hosts file",
                    "Saves the hosts file, then removes all its entries (for example ones that block or redirect websites).",
                    true,
                ),
                tool(
                    "hoststool",
                    "Hosts File Editor",
                    "Review and edit hosts file entries one by one.",
                    "hosts",
                ),
                tool(
                    "nettools",
                    "Network Tools",
                    "Ping, trace route, name lookup and more.",
                    "nettools",
                ),
                open(
                    "netstatus",
                    "Network status",
                    "Windows' network status page, with its own network reset.",
                    "ms-settings:network-status",
                ),
            ],
        },
        Section {
            icon: icons::QUESTION,
            title: "Troubleshooting",
            repairs: vec![
                open(
                    "troubleshoot",
                    "Troubleshoot settings",
                    "Windows' own troubleshooters for common problems.",
                    "ms-settings:troubleshoot",
                ),
                open(
                    "ts-update",
                    "Windows Update",
                    "Check for, pause or retry updates.",
                    "ms-settings:windowsupdate",
                ),
                open(
                    "ts-sound",
                    "Sound",
                    "Output and input devices and their volume.",
                    "ms-settings:sound",
                ),
                open(
                    "ts-bluetooth",
                    "Bluetooth & devices",
                    "Pair, remove and check connected devices.",
                    "ms-settings:bluetooth",
                ),
                open(
                    "ts-printers",
                    "Printers & scanners",
                    "Printer status, queues and drivers.",
                    "ms-settings:printers",
                ),
                open(
                    "ts-display",
                    "Display",
                    "Resolution, scaling and multiple monitors.",
                    "ms-settings:display",
                ),
                open(
                    "resmon",
                    "Resource Monitor",
                    "Which programs use the processor, memory, disk and network.",
                    "resmon.exe",
                ),
                open(
                    "msinfo",
                    "System Information",
                    "Windows' detailed hardware and software report.",
                    "msinfo32.exe",
                ),
            ],
        },
        Section {
            icon: icons::ARROW_COUNTER_CLOCKWISE,
            title: "System reset",
            repairs: vec![
                open(
                    "rstrui",
                    "System Restore",
                    "Returns system files and settings to an earlier restore point.",
                    "rstrui.exe",
                ),
                tool(
                    "sysprot",
                    "System Protection",
                    "Create, list and delete restore points.",
                    "systemprotection",
                ),
                open(
                    "recovery",
                    "Reset this PC",
                    "Opens Recovery settings, where Windows can be reinstalled keeping or removing your files.",
                    "ms-settings:recovery",
                ),
                restart(run(
                    "advstartup",
                    "Restart to advanced startup",
                    "Restarts now into the recovery menu (Startup Repair, Safe Mode, Command Prompt). Save your work first.",
                    false,
                )),
                open(
                    "recoverydrive",
                    "Create a recovery drive",
                    "Copies the recovery tools to a USB drive.",
                    "RecoveryDrive.exe",
                ),
            ],
        },
    ]
}

pub fn find(id: &str) -> Option<Repair> {
    sections()
        .into_iter()
        .flat_map(|s| s.repairs)
        .find(|r| r.id == id)
}

// ---------------------------------------------------------------------------------
// Registry checks

/// When a value counts as a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bad {
    /// Present with any value other than 0.
    NonZero,
    /// Present and 0.
    Zero,
    Present,
    Absent,
    /// A string equal to this (ignoring case).
    StrIs(&'static str),
}

/// What the repair leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Absent,
    Dword(u32),
    Str(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub key: String,
    pub name: String,
    pub bad: Bad,
    pub want: Want,
}

fn check(key: impl Into<String>, name: &str, bad: Bad, want: Want) -> Check {
    Check {
        key: key.into(),
        name: name.to_owned(),
        bad,
        want,
    }
}

const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_DWORD: u32 = 4;

fn number(v: &registry::RawValue) -> Option<u32> {
    match v.vtype {
        REG_DWORD => registry::decode_dword(v),
        REG_SZ | REG_EXPAND_SZ => registry::decode_string(v).trim().parse().ok(),
        _ => None,
    }
}

impl Check {
    pub fn is_bad(&self) -> bool {
        let v = registry::read_raw(&self.key, &self.name);
        match (self.bad, v) {
            (Bad::Absent, v) => v.is_none(),
            (_, None) => false,
            (Bad::Present, Some(_)) => true,
            (Bad::NonZero, Some(v)) => number(&v) != Some(0),
            (Bad::Zero, Some(v)) => number(&v) == Some(0),
            (Bad::StrIs(s), Some(v)) => registry::decode_string(&v).trim().eq_ignore_ascii_case(s),
        }
    }

    fn fix(&self) -> Result<()> {
        match self.want {
            Want::Absent => registry::delete_value(&self.key, &self.name),
            Want::Dword(d) => registry::write_raw(&self.key, &self.name, &registry::dword(d)),
            Want::Str(s) => registry::write_raw(&self.key, &self.name, &registry::string(s)),
        }
    }
}

/// Fixes the checks that are bad. Undo restores every value it changed, even when
/// a later one fails. Returns how many it fixed.
pub fn fix_checks(checks: &[Check], what: &str, out: &mut Outcome) -> Result<usize> {
    let bad: Vec<&Check> = checks.iter().filter(|c| c.is_bad()).collect();
    if bad.is_empty() {
        out.log.push("  nothing to fix".into());
        return Ok(0);
    }
    let mut snaps = Vec::new();
    let mut result = Ok(());
    for c in &bad {
        snaps.push(registry::snapshot(&c.key, &c.name));
        if let Err(e) = c.fix() {
            result = Err(e);
            break;
        }
        out.log.push(match c.want {
            Want::Absent => format!("  removed {}\\{}", c.key, c.name),
            Want::Dword(d) => format!("  set {}\\{} = {d}", c.key, c.name),
            Want::Str(s) => format!("  set {}\\{} = \"{s}\"", c.key, c.name),
        });
    }
    out.undo
        .push((what.to_owned(), UndoAction::RestoreRegValues { values: snaps }));
    result.map(|()| bad.len())
}

fn both_roots(sub: &str, name: &str) -> [Check; 2] {
    [
        check(format!(r"HKCU\{sub}"), name, Bad::NonZero, Want::Absent),
        check(format!(r"HKLM\{sub}"), name, Bad::NonZero, Want::Absent),
    ]
}

fn lock_checks(values: &[(&str, &str)]) -> Vec<Check> {
    values
        .iter()
        .flat_map(|(sub, name)| both_roots(sub, name))
        .collect()
}

/// Every value of a key, each counted as a problem (for override lists).
fn all_values(key: &str) -> Vec<Check> {
    registry::open_read(key)
        .map(|k| {
            k.enum_values()
                .flatten()
                .map(|(n, _)| check(key, &n, Bad::Present, Want::Absent))
                .collect()
        })
        .unwrap_or_default()
}

const DEFENDER: &str = r"HKLM\SOFTWARE\Policies\Microsoft\Windows Defender";
const FIREWALL: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy";
const FIREWALL_POLICY: &str = r"HKLM\SOFTWARE\Policies\Microsoft\WindowsFirewall";
/// (registry profile, netsh profile)
const PROFILES: [(&str, &str); 3] = [
    ("DomainProfile", "domainprofile"),
    ("StandardProfile", "privateprofile"),
    ("PublicProfile", "publicprofile"),
];
const SHELL_ICONS: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\Shell Icons";
const DRIVE_ICONS: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons";
const BLOCKED: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Blocked";
const CLASSIC_MENU: &str = r"HKCU\Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}";

/// The registry checks behind a repair, for repairs that are made of them.
pub fn checks(id: &str) -> Option<Vec<Check>> {
    let lm_sys = format!(r"HKLM\{SYS}");
    Some(match id {
        "unlock-all" => LOCKS.iter().flat_map(|l| lock_checks(l.3)).collect(),
        "uac" => vec![
            check(&lm_sys, "EnableLUA", Bad::Zero, Want::Dword(1)),
            check(&lm_sys, "ConsentPromptBehaviorAdmin", Bad::Zero, Want::Dword(5)),
            check(&lm_sys, "PromptOnSecureDesktop", Bad::Zero, Want::Dword(1)),
        ],
        "defender" => {
            let rtp = format!(r"{DEFENDER}\Real-Time Protection");
            let mut v = vec![
                check(DEFENDER, "DisableAntiSpyware", Bad::NonZero, Want::Absent),
                check(DEFENDER, "DisableAntiVirus", Bad::NonZero, Want::Absent),
            ];
            for n in [
                "DisableRealtimeMonitoring",
                "DisableBehaviorMonitoring",
                "DisableOnAccessProtection",
                "DisableIOAVProtection",
            ] {
                v.push(check(&rtp, n, Bad::NonZero, Want::Absent));
            }
            v
        }
        "firewall" => PROFILES
            .iter()
            .map(|(p, _)| {
                check(
                    format!(r"{FIREWALL_POLICY}\{p}"),
                    "EnableFirewall",
                    Bad::Zero,
                    Want::Absent,
                )
            })
            .collect(),
        "smartscreen" => vec![
            check(
                r"HKLM\SOFTWARE\Policies\Microsoft\Windows\System",
                "EnableSmartScreen",
                Bad::Zero,
                Want::Absent,
            ),
            check(
                r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer",
                "SmartScreenEnabled",
                Bad::StrIs("Off"),
                Want::Str("Warn"),
            ),
        ],
        "secnotify" => {
            let base = r"HKLM\SOFTWARE\Policies\Microsoft\Windows Defender Security Center";
            vec![
                check(
                    format!(r"{base}\Notifications"),
                    "DisableNotifications",
                    Bad::NonZero,
                    Want::Absent,
                ),
                check(
                    format!(r"{base}\Systray"),
                    "HideSystray",
                    Bad::NonZero,
                    Want::Absent,
                ),
            ]
        }
        "wupolicy" => {
            let wu = r"HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate";
            let mut v = vec![
                check(format!(r"{wu}\AU"), "NoAutoUpdate", Bad::NonZero, Want::Absent),
                check(wu, "DisableWindowsUpdateAccess", Bad::NonZero, Want::Absent),
            ];
            v.extend(both_roots(
                r"Software\Microsoft\Windows\CurrentVersion\Policies\WindowsUpdate",
                "DisableWindowsUpdateAccess",
            ));
            v.extend(both_roots(EXP, "NoWindowsUpdate"));
            v
        }
        "shell-icons" => {
            let mut v = all_values(SHELL_ICONS);
            for class in ["lnkfile", "piffile"] {
                v.push(check(
                    format!(r"HKLM\SOFTWARE\Classes\{class}"),
                    "IsShortcut",
                    Bad::Absent,
                    Want::Str(""),
                ));
            }
            v
        }
        "blocked-ext" => {
            let mut v = all_values(&format!(r"HKCU\{BLOCKED}"));
            v.extend(all_values(&format!(r"HKLM\{BLOCKED}")));
            v
        }
        other => {
            let (_, _, _, values) = LOCKS.iter().find(|l| l.0 == other)?;
            lock_checks(values)
        }
    })
}

// ---------------------------------------------------------------------------------
// Other detections

/// Per-user verbs that take over opening folders and drives (keys to remove).
pub fn folder_overrides() -> Vec<String> {
    let mut out = Vec::new();
    for class in ["Folder", "Directory", "Drive"] {
        for verb in ["open", "explore"] {
            let key = format!(r"HKCU\Software\Classes\{class}\shell\{verb}");
            if registry::open_read(&format!(r"{key}\command")).is_ok() {
                out.push(key);
            }
        }
    }
    out
}

pub fn drive_icon_count() -> usize {
    registry::open_read(DRIVE_ICONS)
        .map(|k| k.enum_keys().flatten().count())
        .unwrap_or(0)
}

/// Firewall profiles that are turned off: (registry name, netsh name).
pub fn firewall_off() -> Vec<(&'static str, &'static str)> {
    PROFILES
        .iter()
        .filter(|(p, _)| {
            registry::read_raw(&format!(r"{FIREWALL}\{p}"), "EnableFirewall")
                .is_some_and(|v| number(&v) == Some(0))
        })
        .copied()
        .collect()
}

pub fn hosts_path() -> PathBuf {
    PathBuf::from(paths::expand_env(r"%SystemRoot%\System32\drivers\etc\hosts"))
}

/// Lines of a hosts file that map a name (not blank, not comments).
pub fn hosts_entries(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .count()
}

pub const DEFAULT_HOSTS: &str = "# Hosts file, reset by Win Manager RS.\r\n#\r\n# Each line maps an IP address to a host name, for example:\r\n#     127.0.0.1    test-site.local\r\n#\r\n# Windows resolves localhost itself, so it needs no entry here.\r\n";

pub fn classic_menu_on() -> bool {
    registry::open_read(CLASSIC_MENU).is_ok()
}

/// What a repair's status line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Fine(String),
    Problem(String),
    Info(String),
}

fn found(n: usize) -> Health {
    match n {
        0 => Health::Fine("Nothing to fix".into()),
        1 => Health::Problem("1 problem found".into()),
        n => Health::Problem(format!("{n} problems found")),
    }
}

/// The status of a repair that can tell whether it is needed (read-only).
pub fn health(id: &str) -> Option<Health> {
    if let Some(cs) = checks(id) {
        let mut n = cs.iter().filter(|c| c.is_bad()).count();
        if id == "firewall" {
            n += firewall_off().len();
        }
        return Some(found(n));
    }
    Some(match id {
        "assoc" => found(fixer::association_overrides().len()),
        "shell" => found(fixer::shell_problems().len()),
        "assoc-folders" => found(folder_overrides().len()),
        "drive-icons" => match drive_icon_count() {
            0 => Health::Fine("No custom drive icons".into()),
            n => Health::Info(format!("{n} drives with custom icons")),
        },
        "hosts-reset" => match std::fs::read_to_string(hosts_path()) {
            Ok(t) => match hosts_entries(&t) {
                0 => Health::Fine("No entries".into()),
                n => Health::Info(format!("{n} entries")),
            },
            Err(e) => Health::Problem(format!("Cannot read the hosts file ({e})")),
        },
        "classicmenu" => {
            if classic_menu_on() {
                Health::Info("The classic menu is in use".into())
            } else {
                Health::Fine("The Windows 11 menu is in use".into())
            }
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------
// Running

/// Runs a command, logging its output; fails on a non-zero exit code.
fn run_ok(program: &str, args: &[&str], log: &mut Vec<String>) -> Result<()> {
    log.push(format!("> {program} {}", args.join(" ")));
    let out = hidden_command(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("starting {program}"))?;
    for l in String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
        .filter(|l| !l.trim().is_empty())
    {
        log.push(format!("  {}", l.trim()));
    }
    if !out.status.success() {
        bail!("{program} exited with code {}", out.status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Registers app packages again for the current user (all when `names` is empty).
fn reregister(names: &[&str], out: &mut Outcome, status: &mut dyn FnMut(&str)) -> Result<()> {
    let list = if names.is_empty() {
        "Get-AppxPackage".to_owned()
    } else {
        format!("Get-AppxPackage -Name {}", names.join(","))
    };
    let script = format!(
        "foreach ($p in @({list})) {{ \
           if ($p.IsFramework -or -not $p.InstallLocation) {{ continue }}; \
           $m = Join-Path $p.InstallLocation 'AppxManifest.xml'; \
           if (-not (Test-Path $m)) {{ continue }}; \
           try {{ Add-AppxPackage -DisableDevelopmentMode -Register $m -ErrorAction Stop; \"registered $($p.Name)\" }} \
           catch {{ \"failed: $($p.Name): $($_.Exception.Message)\" }} \
         }}"
    );
    fixer::run_long(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ],
        &mut out.log,
        status,
    )
}

fn backup_file(name: &str) -> Result<PathBuf> {
    Ok(paths::backups_dir()?.join(format!("{}-{name}", fixer::stamp())))
}

fn remove_keys(keys: &[String], what: &str, out: &mut Outcome) -> Result<()> {
    for key in keys {
        let undo = fixer::export_key(key)?;
        fixer::delete_key(key)?;
        out.log.push(format!("  removed {key}"));
        out.undo.push((format!("{what}: {key}"), undo));
    }
    Ok(())
}

/// Runs one repair. `status` receives progress text.
pub fn apply(id: &str, out: &mut Outcome, status: &mut dyn FnMut(&str)) -> Result<()> {
    let title = find(id).map_or(id, |r| r.title);
    let what = format!("Repair Center: {title}");
    if let Some(cs) = checks(id) {
        fix_checks(&cs, &what, out)?;
        match id {
            "firewall" => {
                for (_, profile) in firewall_off() {
                    run_ok(
                        "netsh",
                        &["advfirewall", "set", profile, "state", "on"],
                        &mut out.log,
                    )?;
                    out.undo.push((
                        format!("Turned the firewall on ({profile})"),
                        UndoAction::RunCommand {
                            program: "netsh".into(),
                            args: ["advfirewall", "set", profile, "state", "off"]
                                .map(String::from)
                                .to_vec(),
                        },
                    ));
                }
            }
            "shell-icons" => fixer::apply("icons", out, status)?,
            "blocked-ext" => {
                restart_explorer()?;
                out.log.push("  Explorer restarted".into());
            }
            _ => {}
        }
        return Ok(());
    }
    let log = &mut out.log;
    match id {
        "scanhealth" => fixer::run_long("dism", &["/Online", "/Cleanup-Image", "/ScanHealth"], log, status),
        "compcleanup" => fixer::run_long(
            "dism",
            &["/Online", "/Cleanup-Image", "/StartComponentCleanup"],
            log,
            status,
        ),
        "msiserver" => {
            run_ok("msiexec", &["/unregister"], log)?;
            run_ok("msiexec", &["/regserver"], log)
        }
        "search" => {
            let _ = fixer::run("net", &["stop", "wsearch", "/y"], log);
            registry::write_raw(
                r"HKLM\SOFTWARE\Microsoft\Windows Search",
                "SetupCompletedSuccessfully",
                &registry::dword(0),
            )?;
            log.push("  the index will be rebuilt".into());
            run_ok("net", &["start", "wsearch"], log)
        }
        "fontcache" => {
            let _ = fixer::run("net", &["stop", "FontCache", "/y"], log);
            fixer::delete_matching(
                r"%WINDIR%\ServiceProfiles\LocalService\AppData\Local\FontCache",
                "*FontCache*",
                log,
            );
            fixer::delete_matching(r"%WINDIR%\System32", "FNTCACHE.DAT", log);
            fixer::run("net", &["start", "FontCache"], log).map(drop)
        }
        "storereg" => reregister(&["Microsoft.WindowsStore"], out, status),
        "startreg" => reregister(
            &[
                "Microsoft.Windows.StartMenuExperienceHost",
                "Microsoft.Windows.ShellExperienceHost",
                "MicrosoftWindows.Client.CBS",
            ],
            out,
            status,
        ),
        "appsreg" => reregister(&[], out, status),
        "evtclear" => {
            for name in ["Application", "System", "Setup"] {
                let file = paths::logs_dir()?.join(format!("eventlog-{}-{name}.evtx", fixer::stamp()));
                let file = file.to_string_lossy();
                if let Err(e) = run_ok("wevtutil", &["epl", name, &file], log) {
                    log.push(format!("  failed: {name} not saved, so not cleared ({e:#})"));
                    continue;
                }
                log.push(format!("  saved to {file}"));
                run_ok("wevtutil", &["cl", name], log)?;
            }
            Ok(())
        }
        "eventlog" => {
            run_ok("sc", &["config", "eventlog", "start=", "auto"], log)?;
            // Fails harmlessly when the service is already running.
            fixer::run("net", &["start", "eventlog"], log).map(drop)
        }
        "assoc-folders" => {
            let keys = folder_overrides();
            if keys.is_empty() {
                out.log.push("  nothing to fix".into());
                return Ok(());
            }
            remove_keys(&keys, "Removed folder command override", out)?;
            restart_explorer()?;
            out.log.push("  Explorer restarted".into());
            Ok(())
        }
        "drive-icons" => {
            if drive_icon_count() == 0 {
                log.push("  nothing to fix".into());
                return Ok(());
            }
            remove_keys(&[DRIVE_ICONS.to_owned()], "Removed custom drive icons", out)?;
            restart_explorer()?;
            out.log.push("  Explorer restarted".into());
            Ok(())
        }
        "classicmenu" => {
            if !classic_menu_on() {
                log.push("  the Windows 11 menu is already in use".into());
                return Ok(());
            }
            remove_keys(&[CLASSIC_MENU.to_owned()], "Restored the Windows 11 menu", out)?;
            restart_explorer()?;
            out.log.push("  Explorer restarted".into());
            Ok(())
        }
        "firewall-reset" => {
            let file = backup_file("firewall.wfw")?;
            let file_s = file.to_string_lossy().into_owned();
            run_ok("netsh", &["advfirewall", "export", &file_s], log)?;
            run_ok("netsh", &["advfirewall", "reset"], log)?;
            out.undo.push((
                "Reset the firewall rules".into(),
                UndoAction::RunCommand {
                    program: "netsh".into(),
                    args: vec!["advfirewall".into(), "import".into(), file_s],
                },
            ));
            Ok(())
        }
        "hosts-reset" => {
            let hosts = hosts_path();
            let backup = backup_file("hosts")?;
            std::fs::copy(&hosts, &backup).with_context(|| format!("saving {}", hosts.display()))?;
            log.push(format!("  saved to {}", backup.display()));
            std::fs::write(&hosts, DEFAULT_HOSTS).with_context(|| format!("writing {}", hosts.display()))?;
            out.undo.push((
                "Reset the hosts file".into(),
                UndoAction::RestoreFile {
                    original: hosts,
                    backup,
                },
            ));
            fixer::run("ipconfig", &["/flushdns"], &mut out.log).map(drop)
        }
        "advstartup" => run_ok("shutdown", &["/r", "/o", "/t", "0"], log),
        other => fixer::apply(other, out, status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = r"HKCU\Software\WinManagerRS-test-repaircenter";

    #[test]
    fn catalog_is_consistent() {
        let sections = sections();
        assert_eq!(sections.len(), 12);
        let mut ids: Vec<&str> = sections.iter().flat_map(|s| &s.repairs).map(|r| r.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "repair ids are unique");
        for r in sections.iter().flat_map(|s| &s.repairs) {
            assert!(!r.title.is_empty() && !r.description.is_empty(), "{}", r.id);
            if let How::Open(target, _) = r.how {
                assert!(!target.is_empty(), "{}", r.id);
            }
        }
        // Every lock has its own repair, and "remove all" covers them all.
        let all = checks("unlock-all").unwrap();
        for (id, _, _, values) in LOCKS {
            assert!(find(id).is_some());
            assert_eq!(checks(id).unwrap().len(), values.len() * 2);
            assert!(checks(id).unwrap().iter().all(|c| all.contains(c)));
        }
    }

    #[test]
    fn health_reads_without_failing() {
        for r in sections().iter().flat_map(|s| &s.repairs) {
            let _ = health(r.id);
        }
        assert!(health("memdiag").is_none());
        assert!(health("lock-taskmgr").is_some());
    }

    #[test]
    fn hosts_entries_skip_comments_and_blanks() {
        assert_eq!(hosts_entries(DEFAULT_HOSTS), 0);
        assert_eq!(
            hosts_entries("# c\n\n127.0.0.1 a.test\n  0.0.0.0 b.test # x\n"),
            2
        );
    }

    #[test]
    fn checks_fix_and_undo() {
        let _ = fixer::delete_key(TEST_KEY);
        registry::write_raw(TEST_KEY, "Off", &registry::dword(1)).unwrap();
        registry::write_raw(TEST_KEY, "Zero", &registry::dword(0)).unwrap();
        registry::write_raw(TEST_KEY, "Text", &registry::string("off")).unwrap();
        registry::write_raw(TEST_KEY, "Fine", &registry::dword(0)).unwrap();
        let cs = vec![
            check(TEST_KEY, "Off", Bad::NonZero, Want::Absent),
            check(TEST_KEY, "Zero", Bad::Zero, Want::Dword(5)),
            check(TEST_KEY, "Text", Bad::StrIs("Off"), Want::Str("Warn")),
            check(TEST_KEY, "Missing", Bad::Absent, Want::Str("")),
            check(TEST_KEY, "Fine", Bad::NonZero, Want::Absent),
        ];
        assert_eq!(cs.iter().filter(|c| c.is_bad()).count(), 4);

        let mut out = Outcome::default();
        assert_eq!(fix_checks(&cs, "test", &mut out).unwrap(), 4);
        assert!(cs.iter().all(|c| !c.is_bad()));
        assert!(registry::read_raw(TEST_KEY, "Off").is_none());
        assert_eq!(
            registry::read_raw(TEST_KEY, "Zero").and_then(|v| registry::decode_dword(&v)),
            Some(5)
        );
        assert!(
            registry::read_raw(TEST_KEY, "Fine").is_some(),
            "a good value is left alone"
        );

        let [(_, UndoAction::RestoreRegValues { values })] = out.undo.as_slice() else {
            panic!("one undo step expected");
        };
        for s in values {
            registry::restore(s).unwrap();
        }
        assert_eq!(
            cs.iter().filter(|c| c.is_bad()).count(),
            4,
            "undo brings them back"
        );

        let mut again = Outcome::default();
        fixer::delete_key(TEST_KEY).unwrap();
        assert_eq!(fix_checks(&cs[..3], "test", &mut again).unwrap(), 0);
        assert!(again.undo.is_empty());
    }
}
