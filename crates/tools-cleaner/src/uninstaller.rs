//! Smart Uninstaller: list installed programs, run their uninstaller, then find and
//! remove leftovers (folders → Recycle Bin, registry keys → backed up then deleted).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Tool, egui,
    paths::expand_env,
    process::{hidden_command, run_checked, split_command_line},
    registry,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, format_bytes, show_message},
};

const UNINSTALL_KEYS: [&str; 3] = [
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
    pub install_location: Option<PathBuf>,
    pub per_user: bool,
}

fn reg_string(key: &winreg::RegKey, name: &str) -> Option<String> {
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
                install_location: reg_string(&k, "InstallLocation")
                    .map(|p| PathBuf::from(expand_env(p.trim_matches('"')))),
                per_user: base.starts_with("HKCU"),
                name,
            });
        }
    }
    programs.sort_by_key(|p| p.name.to_lowercase());
    programs.dedup_by(|a, b| a.name == b.name && a.version == b.version);
    programs
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

/// Runs the uninstaller and waits for the launched process to exit.
/// (Some uninstallers hand off to a copy of themselves and return early.)
pub fn run_uninstaller(program: &Program) -> Result<i32> {
    use std::os::windows::process::CommandExt;
    let Some(cmd) = &program.uninstall else {
        bail!("{} has no uninstall command", program.name)
    };
    let cmd = normalize_uninstall(&expand_env(cmd));
    let Some((exe, args)) = split_command_line(&cmd) else {
        bail!("invalid uninstall command: {cmd}")
    };
    let status = std::process::Command::new(&exe).raw_arg(&args).status()?;
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
}

/// Protected locations that must never be offered as leftovers themselves.
fn is_protected_dir(p: &Path) -> bool {
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

fn remove_leftovers(ctx: &AppCtx<'_>, program: &str, items: &[Leftover]) -> Result<usize> {
    let mut folders = Vec::new();
    let mut removed = 0;
    for item in items {
        match item {
            Leftover::Folder(p) => folders.push(p.clone()),
            Leftover::RegistryKey(k) => {
                let undo = ctx.journal.backup_reg_key(k)?;
                run_checked(hidden_command("reg").args(["delete", k, "/f"]))?;
                ctx.journal.record(
                    "uninstaller",
                    format!("Removed leftover key {k} ({program})"),
                    undo,
                )?;
                removed += 1;
            }
        }
    }
    if !folders.is_empty() {
        removed += folders.len();
        ctx.journal.recycle(
            "uninstaller",
            format!("Removed {} leftover folders ({program})", folders.len()),
            folders,
        )?;
    }
    Ok(removed)
}

#[derive(Default)]
pub struct SmartUninstaller {
    programs: Option<Vec<Program>>,
    filter: String,
    selected: Option<String>,
    uninstall_task: Option<BackgroundTask<i32>>,
    leftovers: Option<(Program, Vec<Leftover>, Vec<bool>)>,
    confirm: Confirm<Program>,
    message: Message,
}

impl Tool for SmartUninstaller {
    fn id(&self) -> &'static str {
        "uninstaller"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let programs = self.programs.get_or_insert_with(enumerate).clone();
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| programs.iter().find(|p| &p.key == k))
            .cloned();

        if let Some(p) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let _ = ctx.journal.record(
                    "uninstaller",
                    format!("Started uninstaller for {}", p.name),
                    UndoAction::None,
                );
                self.leftovers = None;
                self.uninstall_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| run_uninstaller(&p)));
            }
        }
        if let Some(t) = &mut self.uninstall_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.uninstall_task = None;
                self.programs = None;
                self.message = Some(r.map(|code| {
                    format!(
                        "The uninstaller exited (code {code}). When it has fully finished, use \"Scan for leftovers\"."
                    )
                }));
            }
        }

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.programs = None;
            }
            ui.weak(format!("{} programs", programs.len()));
            ui.separator();
            let busy = self.uninstall_task.is_some();
            ui.add_enabled_ui(selected.is_some() && !busy, |ui| {
                if ui.button("Uninstall…").clicked()
                    && let Some(p) = &selected
                {
                    if p.uninstall.is_none() {
                        self.message = Some(Err(format!("{} does not provide an uninstaller.", p.name)));
                    } else {
                        self.confirm.ask(
                            format!("Run the uninstaller for {}? Follow its prompts; afterwards you can scan for leftovers.", p.name),
                            p.clone(),
                        );
                    }
                }
                if ui.button("Scan for leftovers").clicked()
                    && let Some(p) = &selected
                {
                    let found = find_leftovers(p);
                    let marks = vec![true; found.len()];
                    self.leftovers = Some((p.clone(), found, marks));
                }
            });
            if busy {
                ui.spinner();
                ui.label("Waiting for the uninstaller…");
            }
        });
        show_message(ui, &self.message);

        if let Some((program, found, marks)) = &mut self.leftovers {
            let mut close = false;
            let mut remove = None;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("Leftovers of {}", program.name));
                    if ui.small_button("✕").clicked() {
                        close = true;
                    }
                });
                if found.is_empty() {
                    ui.label("No leftovers found.");
                } else {
                    ui.weak("Folders go to the Recycle Bin; registry keys are backed up first. Untick anything you want to keep.");
                    if registry_present(&program.key) {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "The program still appears to be installed. Remove leftovers only after uninstalling.",
                        );
                    }
                    for (item, mark) in found.iter().zip(marks.iter_mut()) {
                        let locked = matches!(item, Leftover::RegistryKey(k) if registry::needs_admin(k)) && !ctx.elevated;
                        ui.add_enabled(!locked, egui::Checkbox::new(mark, item.label()));
                    }
                    let chosen: Vec<Leftover> = found
                        .iter()
                        .zip(marks.iter())
                        .filter(|(i, m)| **m && !(matches!(i, Leftover::RegistryKey(k) if registry::needs_admin(k)) && !ctx.elevated))
                        .map(|(i, _)| i.clone())
                        .collect();
                    if ui.add_enabled(!chosen.is_empty(), egui::Button::new(format!("Remove {} items", chosen.len()))).clicked() {
                        remove = Some(chosen);
                    }
                }
            });
            if let Some(items) = remove {
                let name = program.name.clone();
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    remove_leftovers(ctx, &name, &items)
                        .map(|n| format!("Removed {n} leftovers of {name}."))
                        .map_err(|e| format!("{e:#}"))
                });
                close = true;
                self.programs = None;
            }
            if close {
                self.leftovers = None;
            }
        }

        let f = self.filter.to_lowercase();
        let visible: Vec<&Program> = programs
            .iter()
            .filter(|p| {
                f.is_empty() || p.name.to_lowercase().contains(&f) || p.publisher.to_lowercase().contains(&f)
            })
            .collect();
        let mut clicked = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(300.0).clip(true))
            .column(Column::initial(180.0).clip(true))
            .column(Column::initial(110.0).clip(true))
            .column(Column::initial(80.0))
            .column(Column::initial(90.0))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in ["Name", "Publisher", "Version", "Size", "Installed", "For"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let p = visible[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(p.key.as_str()));
                    row.col(|ui| {
                        ui.label(&p.name);
                    });
                    row.col(|ui| {
                        ui.weak(&p.publisher);
                    });
                    row.col(|ui| {
                        ui.weak(&p.version);
                    });
                    row.col(|ui| {
                        ui.label(p.size.map(format_bytes).unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.weak(&p.installed);
                    });
                    row.col(|ui| {
                        ui.weak(if p.per_user { "You" } else { "All users" });
                    });
                    if row.response().clicked() {
                        clicked = Some(p.key.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
    }
}

fn registry_present(key: &str) -> bool {
    registry::open_read(key).is_ok()
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
}
