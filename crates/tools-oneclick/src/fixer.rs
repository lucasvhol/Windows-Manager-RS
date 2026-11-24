//! 1-Click Fixer: standard fixes for common Windows problems, run in sequence with a log.

use std::path::PathBuf;

use anyhow::{Context, Result};
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    process::{hidden_command, restart_explorer},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fix {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub admin: bool,
    pub restart: bool,
}

pub const FIXES: [Fix; 9] = [
    Fix {
        id: "dns",
        title: "Flush DNS cache",
        description: "Fixes websites that moved or fail to resolve.",
        admin: true,
        restart: false,
    },
    Fix {
        id: "explorer",
        title: "Restart File Explorer",
        description: "Fixes a frozen taskbar, Start or desktop.",
        admin: false,
        restart: false,
    },
    Fix {
        id: "icons",
        title: "Rebuild icon cache",
        description: "Fixes blank or wrong icons.",
        admin: false,
        restart: false,
    },
    Fix {
        id: "thumbs",
        title: "Rebuild thumbnail cache",
        description: "Fixes wrong or missing picture previews.",
        admin: false,
        restart: false,
    },
    Fix {
        id: "store",
        title: "Reset Microsoft Store cache",
        description: "Fixes Store apps that will not download or update.",
        admin: false,
        restart: false,
    },
    Fix {
        id: "time",
        title: "Synchronise the clock",
        description: "Fixes certificate errors caused by a wrong time.",
        admin: true,
        restart: false,
    },
    Fix {
        id: "print",
        title: "Clear stuck print jobs",
        description: "Empties the print queue and restarts the Print Spooler.",
        admin: true,
        restart: false,
    },
    Fix {
        id: "network",
        title: "Reset network stack",
        description: "Resets Winsock and TCP/IP settings. Custom network settings are lost.",
        admin: true,
        restart: true,
    },
    Fix {
        id: "update",
        title: "Reset Windows Update",
        description: "Stops update services and sets aside the update cache (kept as *.bak).",
        admin: true,
        restart: false,
    },
];

fn run(program: &str, args: &[&str], log: &mut Vec<String>) -> Result<()> {
    log.push(format!("> {program} {}", args.join(" ")));
    let out = hidden_command(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    for l in String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
    {
        if !l.trim().is_empty() {
            log.push(format!("  {}", l.trim()));
        }
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

pub fn apply(fix: &str, log: &mut Vec<String>) -> Result<()> {
    match fix {
        "dns" => run("ipconfig", &["/flushdns"], log),
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
            run("w32tm", &["/resync", "/force"], log)
        }
        "print" => {
            run("net", &["stop", "spooler"], log)?;
            delete_matching(r"%WINDIR%\System32\spool\PRINTERS", "*", log);
            run("net", &["start", "spooler"], log)
        }
        "network" => {
            run("netsh", &["winsock", "reset"], log)?;
            run("netsh", &["int", "ip", "reset"], log)
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
        other => anyhow::bail!("unknown fix {other}"),
    }
}

pub struct OneClickFixer {
    selected: Vec<bool>,
    task: Option<BackgroundTask<Vec<String>>>,
    log: Vec<String>,
    confirm: Confirm<Vec<Fix>>,
    message: Message,
}

impl Default for OneClickFixer {
    fn default() -> Self {
        Self {
            selected: FIXES.iter().map(|f| matches!(f.id, "dns" | "icons")).collect(),
            task: None,
            log: Vec::new(),
            confirm: Confirm::default(),
            message: None,
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
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                match r {
                    Ok(log) => {
                        self.log = log;
                        self.message = Some(Ok("Done. See the log below.".into()));
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
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    let mut log = Vec::new();
                    for (i, f) in fixes.iter().enumerate() {
                        p.report(i as f32 / fixes.len() as f32, f.title);
                        log.push(format!("== {} ==", f.title));
                        if let Err(e) = apply(f.id, &mut log) {
                            log.push(format!("  failed: {e:#}"));
                        }
                    }
                    Ok(log)
                }));
            }
        }

        ui.label("Select the problems to fix. Each fix is a standard, well-known repair.");
        ui.add_space(6.0);
        for (i, f) in FIXES.iter().enumerate() {
            ui.horizontal(|ui| {
                let usable = !f.admin || ctx.elevated;
                let mut off = false;
                let state = if usable { &mut self.selected[i] } else { &mut off };
                ui.add_enabled(usable, egui::Checkbox::new(state, f.title))
                    .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                ui.weak(f.description);
                if f.restart {
                    ui.weak("· restart required");
                }
            });
        }
        let chosen: Vec<Fix> = FIXES
            .iter()
            .zip(&self.selected)
            .filter(|(f, s)| **s && (!f.admin || ctx.elevated))
            .map(|(f, _)| *f)
            .collect();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !chosen.is_empty() && self.task.is_none(),
                    egui::Button::new(match chosen.len() {
                        1 => "Fix 1 problem".to_owned(),
                        n => format!("Fix {n} problems"),
                    }),
                )
                .clicked()
            {
                let names: Vec<&str> = chosen.iter().map(|f| f.title).collect();
                self.confirm
                    .ask(format!("Run: {}?", names.join(", ")), chosen.clone());
            }
            if let Some(t) = &self.task {
                ui.spinner();
                ui.label(&t.status);
            }
        });
        show_message(ui, &self.message);
        if !self.log.is_empty() {
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(self.log.join("\n")).monospace().small())
                        .selectable(true),
                );
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_ids_unique_and_known() {
        let mut ids: Vec<&str> = FIXES.iter().map(|f| f.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), FIXES.len());
        let mut log = Vec::new();
        assert!(apply("nope", &mut log).is_err());
        // "dns" itself needs elevation on current Windows builds, so it is not executed here.
    }
}
