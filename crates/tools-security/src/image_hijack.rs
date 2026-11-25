//! Image Hijack: programs that Windows starts something else in place of, through Image
//! File Execution Options ("Debugger") or Silent Process Exit ("MonitorProcess"). Some
//! tools use this on purpose (Process Explorer replacing Task Manager, Win Manager's own
//! program blocks); viruses use it to stop security tools from starting. Listing works
//! for every account; removing an entry needs administrator rights and can be undone.

use std::path::Path;

use anyhow::Result;
use wm_core::{
    AppCtx, Category, Tool, egui, registry,
    safety::UndoAction,
    ui::{Confirm, Message, show_message},
};

const IFEO: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";
const SPE: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\SilentProcessExit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hijack {
    /// The program that gets redirected (`taskmgr.exe`).
    pub program: String,
    /// What runs instead (or as well).
    pub command: String,
    pub key: String,
    pub value: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Win Manager's own "block this program".
    Blocked,
    /// A known tool that does this on purpose.
    Known,
    /// The program it points to doesn't exist (left over, or broken).
    Missing,
    Unknown,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Blocked => "Blocked by Win Manager (Process Manager)",
            Verdict::Known => "A known tool",
            Verdict::Missing => "Points to a program that doesn't exist",
            Verdict::Unknown => "Check this one",
        }
    }
}

/// The program part of a command line (quoted or not).
pub fn command_exe(command: &str) -> String {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        rest.split('"').next().unwrap_or_default().to_owned()
    } else {
        c.split_whitespace().next().unwrap_or_default().to_owned()
    }
}

/// Programs that redirect others on purpose: Process Explorer / Process Hacker /
/// System Informer replacing Task Manager, Visual Studio's debuggers.
const KNOWN: [&str; 7] = [
    "procexp.exe",
    "procexp64.exe",
    "processhacker.exe",
    "systeminformer.exe",
    "vsjitdebugger.exe",
    "windbg.exe",
    "devenv.exe",
];

pub fn verdict(h: &Hijack) -> Verdict {
    let exe = command_exe(&h.command);
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    if exe.eq_ignore_ascii_case(&format!(r"{windir}\System32\systray.exe")) && h.value == "Debugger" {
        return Verdict::Blocked;
    }
    let name = Path::new(&exe)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let expanded = wm_core::paths::expand_env(&exe);
    let exists = Path::new(&expanded).is_file()
        || (!expanded.contains('\\') && Path::new(&format!(r"{windir}\System32\{expanded}")).is_file());
    if !exists {
        Verdict::Missing
    } else if KNOWN.contains(&name.as_str()) {
        Verdict::Known
    } else {
        Verdict::Unknown
    }
}

/// Every redirect in the registry. Keys a standard account can't read are skipped.
pub fn list() -> Vec<Hijack> {
    let mut out = Vec::new();
    for (root, value) in [(IFEO, "Debugger"), (SPE, "MonitorProcess")] {
        let Ok(k) = registry::open_read(root) else {
            continue;
        };
        for name in k.enum_keys().flatten() {
            let key = format!(r"{root}\{name}");
            if let Some(v) = registry::read_raw(&key, value) {
                let command = registry::decode_string(&v);
                if !command.trim().is_empty() {
                    out.push(Hijack {
                        program: name,
                        command,
                        key,
                        value,
                    });
                }
            }
        }
    }
    out.sort_by_key(|h| h.program.to_lowercase());
    out
}

pub fn remove(ctx: &AppCtx<'_>, h: &Hijack) -> Result<()> {
    let snap = registry::snapshot(&h.key, h.value);
    registry::delete_value(&h.key, h.value)?;
    ctx.journal.record(
        "imagehijack",
        format!("Removed the redirect of {} to {}", h.program, h.command),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

#[derive(Default)]
pub struct ImageHijack {
    list: Option<Vec<Hijack>>,
    confirm: Confirm<Hijack>,
    message: Message,
}

impl Tool for ImageHijack {
    fn id(&self) -> &'static str {
        "imagehijack"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(h) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                remove(ctx, &h)
                    .map(|()| {
                        format!(
                            "{} starts normally again. Undo it from the Change Journal.",
                            h.program
                        )
                    })
                    .map_err(|e| format!("{e:#}"))
            });
            self.list = None;
        }
        let list = self.list.get_or_insert_with(list).clone();
        ui.label("Programs Windows starts something else in place of. Tools such as Process Explorer do this on purpose; viruses do it to stop security tools from starting.");
        ui.horizontal(|ui| {
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.list = None;
            }
            if !ctx.elevated {
                ui.weak("Removing an entry needs the app to run as administrator.");
            }
        });
        show_message(ui, &self.message);
        ui.separator();
        if list.is_empty() {
            ui.label("No program is redirected. 👍");
            return;
        }
        let mut ask = None;
        egui::Grid::new("ifeo-grid")
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                for h in ["Program", "Runs instead", "Verdict", ""] {
                    ui.strong(h);
                }
                ui.end_row();
                for h in &list {
                    ui.label(&h.program);
                    ui.label(format!(
                        "{}{}",
                        h.command,
                        if h.value == "MonitorProcess" {
                            "  (when it exits)"
                        } else {
                            ""
                        }
                    ));
                    let v = verdict(h);
                    match v {
                        Verdict::Unknown | Verdict::Missing => {
                            ui.colored_label(ui.visuals().warn_fg_color, v.label());
                        }
                        _ => {
                            ui.weak(v.label());
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button("Open key").clicked() {
                            self.message = wm_core::registry::open_in_regedit(&h.key)
                                .err()
                                .map(|e| Err(format!("{e:#}")));
                        }
                        if ui
                            .add_enabled(ctx.elevated, egui::Button::new("Remove").small())
                            .clicked()
                        {
                            ask = Some(h.clone());
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some(h) = ask {
            self.confirm.ask(
                format!(
                    "Remove the redirect, so {} starts normally? It can be undone.",
                    h.program
                ),
                h,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(command: &str) -> Hijack {
        Hijack {
            program: "taskmgr.exe".into(),
            command: command.into(),
            key: String::new(),
            value: "Debugger",
        }
    }

    #[test]
    fn verdicts() {
        let windir = std::env::var("WINDIR").unwrap();
        assert_eq!(
            verdict(&h(&format!(r"{windir}\System32\systray.exe"))),
            Verdict::Blocked
        );
        assert_eq!(verdict(&h(r#""C:\nowhere\evil.exe" -x"#)), Verdict::Missing);
        assert_eq!(
            verdict(&h(&format!(r"{windir}\System32\notepad.exe"))),
            Verdict::Unknown
        );
        assert_eq!(
            command_exe(r#""C:\Program Files\x\procexp64.exe" /e"#),
            r"C:\Program Files\x\procexp64.exe"
        );
        assert_eq!(command_exe(r"vsjitdebugger.exe -p"), "vsjitdebugger.exe");
    }

    #[test]
    fn listing_works_without_admin() {
        // Reading needs no rights; some keys are skipped silently.
        let _ = list();
    }
}
