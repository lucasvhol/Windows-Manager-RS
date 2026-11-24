//! Windows Utilities: one place to open Windows' own administrative tools.
//! Tools that are not installed on this edition (e.g. Group Policy on Home) are hidden.

use std::path::Path;

use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    shell::shell_open,
    ui::{Message, show_message},
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
        "Remote Desktop",
        "mstsc.exe",
        "",
        "Connect to another PC",
    ),
    (
        "Control Panel",
        "Control Panel",
        "control.exe",
        "",
        "Classic Control Panel",
    ),
    (
        "Control Panel",
        "Programs and Features",
        "appwiz.cpl",
        "",
        "Installed programs",
    ),
    (
        "Control Panel",
        "Power Options",
        "powercfg.cpl",
        "",
        "Power plans",
    ),
    (
        "Control Panel",
        "Sound",
        "mmsys.cpl",
        "",
        "Playback and recording devices",
    ),
    (
        "Control Panel",
        "Credential Manager",
        "control.exe",
        "/name Microsoft.CredentialManager",
        "Saved passwords for sites and networks",
    ),
    (
        "Control Panel",
        "Fonts",
        "control.exe",
        "fonts",
        "Installed fonts",
    ),
    ("Settings", "Display settings", "ms-settings:display", "", ""),
    ("Settings", "Windows Update", "ms-settings:windowsupdate", "", ""),
    ("Settings", "Apps", "ms-settings:appsfeatures", "", ""),
    ("Settings", "Storage", "ms-settings:storagesense", "", ""),
    ("Accessories", "Command Prompt", "cmd.exe", "", ""),
    ("Accessories", "PowerShell", "powershell.exe", "", ""),
    ("Accessories", "Character Map", "charmap.exe", "", ""),
    ("Accessories", "On-Screen Keyboard", "osk.exe", "", ""),
];

/// Whether a target is present on this edition of Windows.
pub fn available(target: &str) -> bool {
    if target.contains(':') {
        return true; // URI (ms-settings:)
    }
    let system32 = paths::expand_env(r"%WINDIR%\System32");
    let windir = paths::expand_env("%WINDIR%");
    // Some tools (e.g. powershell.exe) live in subfolders that are on PATH.
    let on_path =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(target).is_file()));
    Path::new(&system32).join(target).exists() || Path::new(&windir).join(target).exists() || on_path
}

#[derive(Default)]
pub struct WindowsUtilities {
    filter: String,
    message: Message,
}

impl Tool for WindowsUtilities {
    fn id(&self) -> &'static str {
        "utilities"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
        show_message(ui, &self.message);
        let f = self.filter.to_lowercase();
        let mut groups: Vec<&str> = Vec::new();
        for (g, ..) in UTILITIES {
            if !groups.contains(g) {
                groups.push(g);
            }
        }
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for group in groups {
                let items: Vec<_> = UTILITIES
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
                ui.add_space(6.0);
                ui.strong(group);
                ui.horizontal_wrapped(|ui| {
                    for (_, name, target, args, desc) in items {
                        let b = ui.add(egui::Button::new(*name).min_size(egui::vec2(190.0, 28.0)));
                        let b = if desc.is_empty() {
                            b
                        } else {
                            b.on_hover_text(*desc)
                        };
                        if b.clicked() {
                            self.message = shell_open(target, args).err().map(|e| Err(format!("{e:#}")));
                        }
                    }
                });
            }
        });
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
}
