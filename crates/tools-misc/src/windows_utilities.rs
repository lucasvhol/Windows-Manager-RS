//! Windows Utilities: one place to open Windows' own tools — system utilities,
//! management consoles, Control Panel applets and items, Settings pages, command-line
//! tools, shell folders, known folders and installed Windows apps. Each item can run as
//! administrator, get a desktop shortcut or a desktop right-click entry, or have its
//! command copied. Also: switch the Microsoft Store region, and repair apps.
//!
//! Tools that are not installed on this edition (e.g. Group Policy on Home) are hidden.
//! Pinning to the taskbar is not offered: Windows 11 does not let programs pin.

use std::path::PathBuf;

use anyhow::{Context, Result};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, paths, registry,
    safety::UndoAction,
    shell::{shell_open, shell_open_as_admin},
    task::BackgroundTask,
    ui::{Confirm, Message, frame, section, show_message},
};

/// (group, name, target, arguments, description)
pub const UTILITIES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "System",
        "System Information",
        "msinfo32.exe",
        "",
        "Detailed hardware and software inventory",
    ),
    (
        "System",
        "System Configuration",
        "msconfig.exe",
        "",
        "Boot options and diagnostic startup",
    ),
    (
        "System",
        "System Properties",
        "sysdm.cpl",
        "",
        "Computer name, performance and remote settings",
    ),
    (
        "System",
        "Environment Variables",
        "rundll32.exe",
        "sysdm.cpl,EditEnvironmentVariables",
        "Edit PATH and other variables",
    ),
    (
        "System",
        "System Restore",
        "rstrui.exe",
        "",
        "Roll back to a restore point",
    ),
    (
        "System",
        "Registry Editor",
        "regedit.exe",
        "",
        "Edit the registry directly",
    ),
    (
        "System",
        "DirectX Diagnostic",
        "dxdiag.exe",
        "",
        "Graphics and audio diagnostics",
    ),
    (
        "System",
        "Windows Version",
        "winver.exe",
        "",
        "Windows edition and build",
    ),
    (
        "System",
        "Task Manager",
        "taskmgr.exe",
        "",
        "Running apps and performance",
    ),
    (
        "System",
        "Windows Features",
        "optionalfeatures.exe",
        "",
        "Turn Windows features on or off",
    ),
    (
        "System",
        "Recovery Drive",
        "RecoveryDrive.exe",
        "",
        "Create a USB recovery drive",
    ),
    (
        "System",
        "Windows Security",
        "windowsdefender:",
        "",
        "Virus protection and firewall",
    ),
    (
        "Management",
        "Computer Management",
        "compmgmt.msc",
        "",
        "Disks, users, services and logs",
    ),
    (
        "Management",
        "Device Manager",
        "devmgmt.msc",
        "",
        "Hardware and drivers",
    ),
    (
        "Management",
        "Disk Management",
        "diskmgmt.msc",
        "",
        "Partitions and volumes",
    ),
    ("Management", "Services", "services.msc", "", "Windows services"),
    (
        "Management",
        "Task Scheduler",
        "taskschd.msc",
        "",
        "Scheduled tasks",
    ),
    (
        "Management",
        "Event Viewer",
        "eventvwr.msc",
        "",
        "System and application logs",
    ),
    (
        "Management",
        "Group Policy Editor",
        "gpedit.msc",
        "",
        "Local policies (Pro and above)",
    ),
    (
        "Management",
        "Local Security Policy",
        "secpol.msc",
        "",
        "Security settings (Pro and above)",
    ),
    (
        "Management",
        "Local Users and Groups",
        "lusrmgr.msc",
        "",
        "Accounts (Pro and above)",
    ),
    (
        "Management",
        "Shared Folders",
        "fsmgmt.msc",
        "",
        "Shares, sessions and open files",
    ),
    (
        "Management",
        "Certificates (user)",
        "certmgr.msc",
        "",
        "Your certificates",
    ),
    (
        "Management",
        "Print Management",
        "printmanagement.msc",
        "",
        "Printers, drivers and queues (Pro and above)",
    ),
    (
        "Management",
        "Component Services",
        "comexp.msc",
        "",
        "COM+ and DCOM settings",
    ),
    (
        "Management",
        "ODBC Data Sources",
        "odbcad32.exe",
        "",
        "Database connections",
    ),
    (
        "Management",
        "User Accounts",
        "netplwiz.exe",
        "",
        "Sign-in and account options",
    ),
    (
        "Performance",
        "Resource Monitor",
        "resmon.exe",
        "",
        "Live CPU, disk, network and memory use",
    ),
    (
        "Performance",
        "Performance Monitor",
        "perfmon.exe",
        "",
        "Performance counters",
    ),
    (
        "Performance",
        "Reliability Monitor",
        "perfmon.exe",
        "/rel",
        "History of crashes and problems",
    ),
    (
        "Performance",
        "Memory Diagnostic",
        "mdsched.exe",
        "",
        "Test RAM at the next restart",
    ),
    (
        "Performance",
        "Disk Cleanup",
        "cleanmgr.exe",
        "",
        "Windows' built-in disk cleaner",
    ),
    (
        "Performance",
        "Optimize Drives",
        "dfrgui.exe",
        "",
        "Defragment or trim drives",
    ),
    (
        "Network",
        "Network Connections",
        "ncpa.cpl",
        "",
        "Adapters and their settings",
    ),
    (
        "Network",
        "Windows Firewall (advanced)",
        "wf.msc",
        "",
        "Inbound and outbound rules",
    ),
    (
        "Network",
        "Windows Firewall",
        "firewall.cpl",
        "",
        "Turn the firewall on or off",
    ),
    (
        "Network",
        "Internet Options",
        "inetcpl.cpl",
        "",
        "Proxy, security zones and certificates",
    ),
    (
        "Network",
        "Remote Desktop",
        "mstsc.exe",
        "",
        "Connect to another PC",
    ),
    (
        "Network",
        "Remote Assistance",
        "msra.exe",
        "",
        "Ask for or offer help",
    ),
    (
        "Control Panel applets",
        "Control Panel",
        "control.exe",
        "",
        "Classic Control Panel",
    ),
    (
        "Control Panel applets",
        "Programs and Features",
        "appwiz.cpl",
        "",
        "Installed programs",
    ),
    (
        "Control Panel applets",
        "Power Options",
        "powercfg.cpl",
        "",
        "Power plans",
    ),
    (
        "Control Panel applets",
        "Sound",
        "mmsys.cpl",
        "",
        "Playback and recording devices",
    ),
    (
        "Control Panel applets",
        "Mouse",
        "main.cpl",
        "",
        "Buttons, pointers and wheel",
    ),
    (
        "Control Panel applets",
        "Region",
        "intl.cpl",
        "",
        "Formats and location",
    ),
    (
        "Control Panel applets",
        "Date and Time",
        "timedate.cpl",
        "",
        "Time zone and internet time",
    ),
    (
        "Control Panel applets",
        "Game Controllers",
        "joy.cpl",
        "",
        "Joysticks and gamepads",
    ),
    (
        "Control Panel applets",
        "Security and Maintenance",
        "wscui.cpl",
        "",
        "Security and maintenance messages",
    ),
    (
        "Control Panel applets",
        "Add Hardware",
        "hdwwiz.exe",
        "",
        "Install older hardware",
    ),
    (
        "Control Panel items",
        "Credential Manager",
        "control.exe",
        "/name Microsoft.CredentialManager",
        "Saved passwords for sites and networks",
    ),
    (
        "Control Panel items",
        "Fonts",
        "control.exe",
        "fonts",
        "Installed fonts",
    ),
    (
        "Control Panel items",
        "Devices and Printers",
        "control.exe",
        "/name Microsoft.DevicesAndPrinters",
        "",
    ),
    (
        "Control Panel items",
        "File Explorer Options",
        "control.exe",
        "/name Microsoft.FolderOptions",
        "",
    ),
    (
        "Control Panel items",
        "Indexing Options",
        "control.exe",
        "/name Microsoft.IndexingOptions",
        "",
    ),
    (
        "Control Panel items",
        "Troubleshooting",
        "control.exe",
        "/name Microsoft.Troubleshooting",
        "",
    ),
    (
        "Control Panel items",
        "File History",
        "control.exe",
        "/name Microsoft.FileHistory",
        "",
    ),
    (
        "Control Panel items",
        "Backup and Restore (Windows 7)",
        "control.exe",
        "/name Microsoft.BackupAndRestore",
        "",
    ),
    (
        "Control Panel items",
        "Color Management",
        "control.exe",
        "/name Microsoft.ColorManagement",
        "",
    ),
    (
        "Control Panel items",
        "Ease of Access Center",
        "control.exe",
        "/name Microsoft.EaseOfAccessCenter",
        "",
    ),
    (
        "Control Panel items",
        "Administrative Tools",
        "control.exe",
        "/name Microsoft.AdministrativeTools",
        "",
    ),
    (
        "Control Panel items",
        "AutoPlay",
        "control.exe",
        "/name Microsoft.AutoPlay",
        "",
    ),
    (
        "Control Panel items",
        "Sync Center",
        "control.exe",
        "/name Microsoft.SyncCenter",
        "",
    ),
    (
        "Control Panel items",
        "All Tasks (God Mode)",
        "explorer.exe",
        "shell:::{ED7BA470-8E54-465E-825C-99712043E01C}",
        "Every Control Panel setting in one list",
    ),
    ("Settings", "Display", "ms-settings:display", "", ""),
    ("Settings", "Windows Update", "ms-settings:windowsupdate", "", ""),
    ("Settings", "Installed apps", "ms-settings:appsfeatures", "", ""),
    ("Settings", "Default apps", "ms-settings:defaultapps", "", ""),
    ("Settings", "Startup apps", "ms-settings:startupapps", "", ""),
    ("Settings", "Storage", "ms-settings:storagesense", "", ""),
    (
        "Settings",
        "Bluetooth and devices",
        "ms-settings:bluetooth",
        "",
        "",
    ),
    ("Settings", "Network and internet", "ms-settings:network", "", ""),
    (
        "Settings",
        "Personalization",
        "ms-settings:personalization",
        "",
        "",
    ),
    ("Settings", "Accounts", "ms-settings:yourinfo", "", ""),
    ("Settings", "Sign-in options", "ms-settings:signinoptions", "", ""),
    ("Settings", "Time and language", "ms-settings:dateandtime", "", ""),
    ("Settings", "Privacy and security", "ms-settings:privacy", "", ""),
    ("Settings", "Recovery", "ms-settings:recovery", "", ""),
    ("Settings", "About", "ms-settings:about", "", ""),
    ("Command line", "Command Prompt", "cmd.exe", "", ""),
    ("Command line", "PowerShell", "powershell.exe", "", ""),
    ("Command line", "Windows Terminal", "wt.exe", "", ""),
    (
        "Command line",
        "IP configuration",
        "cmd.exe",
        "/k ipconfig /all",
        "Addresses, DNS and adapters",
    ),
    (
        "Command line",
        "Open connections",
        "cmd.exe",
        "/k netstat -abno",
        "Connections and listening ports",
    ),
    (
        "Command line",
        "System summary",
        "cmd.exe",
        "/k systeminfo",
        "Windows, hardware and updates",
    ),
    (
        "Command line",
        "Driver list",
        "cmd.exe",
        "/k driverquery /v",
        "Installed drivers",
    ),
    (
        "Command line",
        "Battery report",
        "cmd.exe",
        "/k powercfg /batteryreport",
        "Writes battery-report.html in the current folder",
    ),
    (
        "Command line",
        "Check system files",
        "cmd.exe",
        "/k sfc /scannow",
        "Run as administrator",
    ),
    ("Accessories", "Notepad", "notepad.exe", "", ""),
    ("Accessories", "Paint", "mspaint.exe", "", ""),
    ("Accessories", "Snipping Tool", "SnippingTool.exe", "", ""),
    ("Accessories", "Calculator", "calc.exe", "", ""),
    ("Accessories", "Character Map", "charmap.exe", "", ""),
    ("Accessories", "On-Screen Keyboard", "osk.exe", "", ""),
    ("Accessories", "Magnifier", "magnify.exe", "", ""),
    ("Accessories", "Steps Recorder", "psr.exe", "", ""),
    (
        "Shell folders",
        "Startup (yours)",
        "explorer.exe",
        "shell:startup",
        "Programs that start when you sign in",
    ),
    (
        "Shell folders",
        "Startup (everyone)",
        "explorer.exe",
        "shell:common startup",
        "",
    ),
    (
        "Shell folders",
        "Send To",
        "explorer.exe",
        "shell:sendto",
        "The Send to menu",
    ),
    (
        "Shell folders",
        "Start menu programs",
        "explorer.exe",
        "shell:programs",
        "",
    ),
    (
        "Shell folders",
        "Applications",
        "explorer.exe",
        "shell:AppsFolder",
        "Every installed app",
    ),
    (
        "Shell folders",
        "Recent items",
        "explorer.exe",
        "shell:recent",
        "",
    ),
    (
        "Shell folders",
        "Recycle Bin",
        "explorer.exe",
        "shell:RecycleBinFolder",
        "",
    ),
    ("Shell folders", "Fonts folder", "explorer.exe", "shell:fonts", ""),
    (
        "Shell folders",
        "Quick Launch",
        "explorer.exe",
        "shell:quick launch",
        "",
    ),
    (
        "Shell folders",
        "Network locations",
        "explorer.exe",
        "shell:NetHood",
        "",
    ),
    (
        "Known folders",
        "AppData (Roaming)",
        "explorer.exe",
        "shell:AppData",
        "",
    ),
    (
        "Known folders",
        "AppData (Local)",
        "explorer.exe",
        "shell:Local AppData",
        "",
    ),
    (
        "Known folders",
        "ProgramData",
        "explorer.exe",
        "shell:Common AppData",
        "",
    ),
    ("Known folders", "Temporary files", "explorer.exe", "%TEMP%", ""),
    (
        "Known folders",
        "Program Files",
        "explorer.exe",
        "shell:ProgramFiles",
        "",
    ),
    ("Known folders", "Windows", "explorer.exe", "shell:Windows", ""),
    ("Known folders", "System32", "explorer.exe", "shell:System", ""),
    (
        "Known folders",
        "Downloads",
        "explorer.exe",
        "shell:Downloads",
        "",
    ),
    ("Known folders", "Documents", "explorer.exe", "shell:Personal", ""),
    (
        "Known folders",
        "Pictures",
        "explorer.exe",
        "shell:My Pictures",
        "",
    ),
    (
        "Known folders",
        "User profile",
        "explorer.exe",
        "shell:Profile",
        "",
    ),
    ("Known folders", "Public", "explorer.exe", "shell:Public", ""),
];

/// Where a program lives (System32, Windows, or on PATH).
pub fn resolve(target: &str) -> Option<PathBuf> {
    let system32 = PathBuf::from(paths::expand_env(r"%WINDIR%\System32"));
    let windir = PathBuf::from(paths::expand_env("%WINDIR%"));
    let apps = PathBuf::from(paths::expand_env(r"%LOCALAPPDATA%\Microsoft\WindowsApps"));
    [system32, windir, apps]
        .into_iter()
        .map(|d| d.join(target))
        .chain(
            std::env::var_os("PATH")
                .map(|p| {
                    std::env::split_paths(&p)
                        .map(|d| d.join(target))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
        .find(|p| p.is_file())
}

/// Whether a target is present on this edition of Windows.
pub fn available(target: &str) -> bool {
    // URIs (ms-settings:, windowsdefender:) and installed apps are always offered.
    target.contains(':') || resolve(target).is_some()
}

/// The program and arguments that open a target from a shortcut or a menu entry.
pub fn launcher(target: &str, args: &str) -> (PathBuf, String) {
    let sys = |exe: &str| resolve(exe).unwrap_or_else(|| PathBuf::from(exe));
    let lower = target.to_ascii_lowercase();
    if target.contains(':') {
        (sys("explorer.exe"), target.to_owned())
    } else if lower.ends_with(".msc") {
        (sys("mmc.exe"), format!("{target} {args}").trim().to_owned())
    } else if lower.ends_with(".cpl") {
        (sys("control.exe"), format!("{target} {args}").trim().to_owned())
    } else {
        (sys(target), args.to_owned())
    }
}

pub fn command_line(target: &str, args: &str) -> String {
    let (exe, args) = launcher(target, args);
    format!("\"{}\" {args}", exe.display()).trim().to_owned()
}

fn slug(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).collect()
}

fn menu_key(name: &str) -> String {
    format!(
        r"HKCU\Software\Classes\DesktopBackground\Shell\WinManager.{}",
        slug(name)
    )
}

pub fn desktop_shortcut(name: &str, target: &str, args: &str) -> Result<PathBuf> {
    let desktop = wm_core::shell::known_folder(wm_core::shell::KnownFolder::Desktop)?;
    let lnk = desktop.join(format!(
        "{}.lnk",
        name.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "-")
    ));
    let (exe, args) = launcher(target, args);
    wm_core::shell::create_shortcut(
        &lnk,
        &wm_core::shell::ShortcutSpec {
            target: exe,
            args,
            working_dir: None,
            description: name.to_owned(),
            hotkey: 0,
            show_cmd: 0,
        },
    )?;
    Ok(lnk)
}

pub fn in_desktop_menu(name: &str) -> bool {
    registry::open_read(&format!(r"{}\command", menu_key(name))).is_ok()
}

/// Adds or removes an entry for the item in the desktop's right-click menu (per user).
pub fn set_desktop_menu(ctx: &AppCtx<'_>, name: &str, target: &str, args: &str, on: bool) -> Result<()> {
    let key = menu_key(name);
    let undo = ctx.journal.backup_reg_key(&key)?;
    if on {
        let (exe, _) = launcher(target, args);
        let k = registry::open_write(&key)?;
        k.set_value("MUIVerb", &name.to_owned())?;
        k.set_value("Icon", &format!("\"{}\",0", exe.display()))?;
        registry::open_write(&format!(r"{key}\command"))?.set_value("", &command_line(target, args))?;
    } else {
        let (root, sub) = registry::split_path(&key)?;
        root.delete_subkey_all(sub).context("removing the menu entry")?;
    }
    ctx.journal.record(
        "utilities",
        format!(
            "{} \"{name}\" {} the desktop right-click menu",
            if on { "Added" } else { "Removed" },
            if on { "to" } else { "from" }
        ),
        undo,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// Windows apps

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub name: String,
    /// `Package_Family!App` for Store apps.
    pub id: String,
}

impl App {
    pub fn family(&self) -> Option<&str> {
        self.id.split_once('!').map(|(f, _)| f)
    }
}

/// Installed Store (packaged) apps from the Start menu's list.
pub fn store_apps() -> Result<Vec<App>> {
    let v = wm_core::process::powershell_json("Get-StartApps | Select-Object Name, AppID | ConvertTo-Json")?;
    let mut apps: Vec<App> = wm_core::process::json_list(v)
        .into_iter()
        .filter_map(|a| {
            Some(App {
                name: a.get("Name")?.as_str()?.to_owned(),
                id: a.get("AppID")?.as_str()?.to_owned(),
            })
        })
        .filter(|a| a.id.contains('!'))
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.dedup_by(|a, b| a.id == b.id);
    Ok(apps)
}

/// Resets a Store app (clears its data, as in Settings > Apps > Advanced options).
pub fn reset_app(family: &str) -> Result<()> {
    let name = family.split('_').next().unwrap_or(family);
    if !name.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '-') {
        anyhow::bail!("unexpected package name");
    }
    wm_core::process::powershell_json(&format!(
        "Get-AppxPackage -Name '{name}' | Reset-AppxPackage; @() | ConvertTo-Json"
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// Store region

const GEO: &str = r"HKCU\Control Panel\International\Geo";
pub const US_GEO_ID: &str = "244";

pub fn store_region() -> (String, String) {
    let read = |n: &str| {
        registry::read_raw(GEO, n)
            .map(|v| registry::decode_string(&v))
            .unwrap_or_default()
    };
    (read("Nation"), read("Name"))
}

/// Sets the Windows location (which the Store uses for its catalog) to the United States.
pub fn set_store_region_us(ctx: &AppCtx<'_>) -> Result<()> {
    let snaps = vec![registry::snapshot(GEO, "Nation"), registry::snapshot(GEO, "Name")];
    registry::write_raw(GEO, "Nation", &registry::string(US_GEO_ID))?;
    registry::write_raw(GEO, "Name", &registry::string("US"))?;
    ctx.journal.record(
        "utilities",
        "Set the Windows location (Store region) to the United States",
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// UI

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Grid,
    List,
}

enum Ask {
    ResetApp(App),
    StoreUs,
}

pub struct WindowsUtilities {
    filter: String,
    view: View,
    sorted: bool,
    apps: Option<Result<Vec<App>, String>>,
    apps_task: Option<BackgroundTask<Vec<App>>>,
    confirm: Confirm<usize>,
    pending: Vec<Ask>,
    message: Message,
}

impl Default for WindowsUtilities {
    fn default() -> Self {
        Self {
            filter: String::new(),
            view: View::Grid,
            sorted: false,
            apps: None,
            apps_task: None,
            confirm: Confirm::default(),
            pending: Vec::new(),
            message: None,
        }
    }
}

impl WindowsUtilities {
    fn ask(&mut self, q: String, a: Ask) {
        self.pending.push(a);
        self.confirm.ask(q, self.pending.len() - 1);
    }

    /// The right-click menu shared by every item.
    fn item_menu(&mut self, resp: &egui::Response, ctx: &AppCtx<'_>, name: &str, target: &str, args: &str) {
        resp.context_menu(|ui| {
            if ui.button("Open").clicked() {
                self.message = shell_open(target, args).err().map(|e| Err(format!("{e:#}")));
                ui.close();
            }
            if ui.button("Run as administrator").clicked() {
                let (exe, a) = launcher(target, args);
                self.message = shell_open_as_admin(&exe.display().to_string(), &a)
                    .err()
                    .map(|e| Err(format!("{e:#}")));
                ui.close();
            }
            if ui.button("Create shortcut on desktop").clicked() {
                self.message = Some(
                    desktop_shortcut(name, target, args)
                        .map(|p| format!("Created {}.", p.display()))
                        .map_err(|e| format!("{e:#}")),
                );
                ui.close();
            }
            let on = in_desktop_menu(name);
            let label = if on {
                "Remove from desktop right-click menu"
            } else {
                "Add to desktop right-click menu"
            };
            if ui.button(label).clicked() {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    set_desktop_menu(ctx, name, target, args, !on)
                        .map(|()| "Desktop menu updated.".into())
                        .map_err(|e| format!("{e:#}"))
                });
                ui.close();
            }
            if ui.button("Copy command").clicked() {
                ui.ctx().copy_text(command_line(target, args));
                self.message = Some(Ok("Command copied.".into()));
                ui.close();
            }
        });
    }

    fn item(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &AppCtx<'_>,
        name: &str,
        target: &str,
        args: &str,
        desc: &str,
    ) {
        let resp = match self.view {
            View::Grid => ui.add(egui::Button::new(name).min_size(egui::vec2(190.0, 28.0))),
            View::List => {
                let r = ui.add(egui::Button::new(name).frame(false));
                if !desc.is_empty() {
                    ui.weak(desc);
                }
                r
            }
        };
        let resp = if desc.is_empty() || self.view == View::List {
            resp
        } else {
            resp.on_hover_text(desc)
        };
        if resp.clicked() {
            self.message = shell_open(target, args).err().map(|e| Err(format!("{e:#}")));
        }
        self.item_menu(&resp, ctx, name, target, args);
    }

    fn extras_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.strong("Microsoft Store and apps");
        let (nation, name) = store_region();
        ui.horizontal(|ui| {
            ui.label(format!(
                "Store region: {}",
                if name.is_empty() { "(not set)".to_owned() } else { format!("{name} ({nation})") }
            ));
            if ui
                .add_enabled(nation != US_GEO_ID, egui::Button::new("Switch to United States"))
                .on_hover_text("Changes Windows' location setting, which the Store uses. Undo it from the Change Journal.")
                .clicked()
            {
                self.ask(
                    "Set the Windows location to the United States? The Store shows the US catalog after it restarts.".into(),
                    Ask::StoreUs,
                );
            }
            if ui.button("Reset Store cache").on_hover_text("Runs wsreset, which fixes most Store download problems.").clicked() {
                self.message = Some(
                    shell_open("wsreset.exe", "")
                        .map(|()| "Store cache reset started; the Store opens when it's done.".into())
                        .map_err(|e| format!("{e:#}")),
                );
            }
        });
    }

    fn apps_ui(&mut self, ui: &mut egui::Ui, f: &str) {
        if let Some(t) = &mut self.apps_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.apps = Some(r);
                self.apps_task = None;
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.strong("Windows apps");
            if self.apps.is_none() && self.apps_task.is_none() && ui.small_button("Show").clicked() {
                self.apps_task = Some(BackgroundTask::spawn(ui.ctx(), |_| store_apps()));
            }
            if self.apps_task.is_some() {
                ui.spinner();
            }
        });
        let list = match &self.apps {
            Some(Ok(a)) => a.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        let mut reset = None;
        ui.horizontal_wrapped(|ui| {
            for app in list
                .iter()
                .filter(|a| f.is_empty() || a.name.to_lowercase().contains(f))
            {
                let target = format!(r"shell:AppsFolder\{}", app.id);
                let resp = ui.add(egui::Button::new(&app.name).min_size(egui::vec2(190.0, 28.0)));
                if resp.clicked() {
                    self.message = shell_open("explorer.exe", &target)
                        .err()
                        .map(|e| Err(format!("{e:#}")));
                }
                resp.context_menu(|ui| {
                    if ui.button("Create shortcut on desktop").clicked() {
                        self.message = Some(
                            desktop_shortcut(&app.name, "explorer.exe", &target)
                                .map(|p| format!("Created {}.", p.display()))
                                .map_err(|e| format!("{e:#}")),
                        );
                        ui.close();
                    }
                    if ui.button("Repair (reset app data)…").clicked() {
                        reset = Some(app.clone());
                        ui.close();
                    }
                });
            }
        });
        if let Some(a) = reset {
            self.ask(
                format!("Reset {}? Its settings and sign-in are cleared, as with Settings > Apps > Reset. This can't be undone.", a.name),
                Ask::ResetApp(a),
            );
        }
    }
}

impl Tool for WindowsUtilities {
    fn id(&self) -> &'static str {
        "utilities"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(i) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if let Some(a) = self.pending.drain(..).nth(i) {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    match a {
                        Ask::StoreUs => set_store_region_us(ctx)
                            .map(|()| "Store region set to the United States. Restart the Store.".into()),
                        Ask::ResetApp(app) => app
                            .family()
                            .context("not a Store app")
                            .and_then(reset_app)
                            .and_then(|()| {
                                ctx.journal.record(
                                    "utilities",
                                    format!("Reset the app {}", app.name),
                                    UndoAction::None,
                                )?;
                                Ok(format!("{} was reset.", app.name))
                            }),
                    }
                    .map_err(|e| format!("{e:#}"))
                });
            }
        } else if !self.confirm.is_open() {
            self.pending.clear();
        }
        let filter_hint = format!("{}  {}", icons::MAGNIFYING_GLASS, ctx.i18n.tr("common-filter"));
        frame::options(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(filter_hint)
                    .desired_width(220.0),
            );
            ui.separator();
            ui.selectable_value(&mut self.view, View::Grid, format!("{}  Tiles", icons::GRID_FOUR));
            ui.selectable_value(
                &mut self.view,
                View::List,
                format!("{}  List", icons::LIST_BULLETS),
            );
            ui.checkbox(&mut self.sorted, "Sort A–Z");
        });
        let f = self.filter.to_lowercase();
        let shown = UTILITIES
            .iter()
            .filter(|(_, name, target, _, desc)| {
                available(target)
                    && (f.is_empty() || name.to_lowercase().contains(&f) || desc.to_lowercase().contains(&f))
            })
            .count();
        frame::status(ui, &format!("{shown} Windows tools"), |ui| {
            ui.weak("Right-click an item for more.");
        });
        let mut groups: Vec<&str> = Vec::new();
        for (g, ..) in UTILITIES {
            if !groups.contains(g) {
                groups.push(g);
            }
        }
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            for group in groups {
                let mut items: Vec<_> = UTILITIES
                    .iter()
                    .filter(|(g, name, target, _, desc)| {
                        *g == group
                            && available(target)
                            && (f.is_empty()
                                || name.to_lowercase().contains(&f)
                                || desc.to_lowercase().contains(&f))
                    })
                    .collect();
                if items.is_empty() {
                    continue;
                }
                if self.sorted {
                    items.sort_by_key(|i| i.1.to_lowercase());
                }
                section(
                    ui,
                    group,
                    |_| {},
                    |ui| match self.view {
                        View::Grid => {
                            ui.horizontal_wrapped(|ui| {
                                for (_, name, target, args, desc) in items {
                                    self.item(ui, ctx, name, target, args, desc);
                                }
                            });
                        }
                        View::List => {
                            for (_, name, target, args, desc) in items {
                                ui.horizontal(|ui| self.item(ui, ctx, name, target, args, desc));
                            }
                        }
                    },
                );
                ui.add_space(8.0);
            }
            self.apps_ui(ui, &f);
            self.extras_ui(ui);
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_utilities_are_present() {
        for t in [
            "powershell.exe",
            "services.msc",
            "devmgmt.msc",
            "regedit.exe",
            "control.exe",
            "ncpa.cpl",
            "ms-settings:display",
        ] {
            assert!(available(t), "{t} should exist");
        }
        assert!(!available("definitely-not-a-tool.msc"));
        let mut names: Vec<_> = UTILITIES.iter().map(|u| u.1).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), UTILITIES.len(), "duplicate names");
    }

    #[test]
    fn launchers() {
        let (exe, a) = launcher("services.msc", "");
        assert!(exe.ends_with("mmc.exe") && a == "services.msc");
        let (exe, a) = launcher("ncpa.cpl", "");
        assert!(exe.ends_with("control.exe") && a == "ncpa.cpl");
        let (exe, a) = launcher("ms-settings:display", "");
        assert!(exe.ends_with("explorer.exe") && a == "ms-settings:display");
        assert!(command_line("cmd.exe", "/k ipconfig").ends_with(r#"cmd.exe" /k ipconfig"#));
        assert_eq!(slug("All Tasks (God Mode)"), "AllTasksGodMode");
    }

    #[test]
    fn desktop_menu_round_trip() {
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
        let name = format!("WM Test Item {}", std::process::id());
        set_desktop_menu(&ctx, &name, "notepad.exe", "", true).unwrap();
        assert!(in_desktop_menu(&name));
        let cmd = registry::read_raw(&format!(r"{}\command", menu_key(&name)), "").unwrap();
        assert!(registry::decode_string(&cmd).contains("notepad.exe"));
        set_desktop_menu(&ctx, &name, "notepad.exe", "", false).unwrap();
        assert!(!in_desktop_menu(&name));
        // Undoing the removal puts the entry back; undoing the add removes it again.
        let mut entries = journal.entries();
        entries.sort_by_key(|e| e.id);
        journal.undo(entries[1].id).unwrap();
        assert!(in_desktop_menu(&name));
        journal.undo(entries[0].id).unwrap();
        assert!(!in_desktop_menu(&name));
    }

    #[test]
    fn store_apps_and_region_read() {
        let apps = store_apps().unwrap();
        assert!(
            !apps.is_empty(),
            "Windows always has packaged apps (Settings, Calculator…)"
        );
        assert!(apps.iter().all(|a| a.family().is_some()));
        let _ = store_region();
    }
}
