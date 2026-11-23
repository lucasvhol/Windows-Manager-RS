//! Registry Cleaner (conservative): only flags entries that point at files which
//! *definitely* no longer exist on a drive that is present. Every fix is backed up:
//! values via exact snapshots, keys via `.reg` export.

use std::path::Path;

use anyhow::Result;
use wm_core::{
    AppCtx, Category, Tool, egui,
    paths::expand_env,
    process::{exe_from_command, hidden_command, run_checked},
    registry::{self, ValueSnapshot},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    AppPath,
    Uninstall,
    SharedDll,
    MuiCache,
    StartupRun,
    CompatStore,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::AppPath => "Application paths to missing programs",
            Kind::Uninstall => "Uninstall entries for removed programs",
            Kind::SharedDll => "Shared DLL references to missing files",
            Kind::MuiCache => "Cached names of deleted programs",
            Kind::StartupRun => "Startup entries for missing programs",
            Kind::CompatStore => "Compatibility records of deleted programs",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub kind: Kind,
    pub key: String,
    /// `None` = the whole key is the issue; `Some(name)` = a single value.
    pub value: Option<String>,
    pub detail: String,
}

/// True only for absolute, local paths whose drive exists but whose file does not.
/// Relative names, network paths and unplugged drives are never reported.
pub fn definitely_missing(raw: &str) -> bool {
    let expanded = expand_env(raw.trim().trim_matches('"'));
    if expanded.contains('%') {
        return false;
    }
    let p = Path::new(&expanded);
    let bytes = expanded.as_bytes();
    let local_abs =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if !local_abs {
        return false;
    }
    let drive = format!("{}:\\", bytes[0] as char);
    Path::new(&drive).exists() && !p.exists()
}

fn values(key: &str) -> Vec<(String, String)> {
    let Ok(k) = registry::open_read(key) else {
        return Vec::new();
    };
    k.enum_values()
        .flatten()
        .map(|(name, v)| {
            let raw = registry::RawValue {
                vtype: v.vtype.clone() as u32,
                bytes: v.bytes.into_owned(),
            };
            (name, registry::decode_string(&raw))
        })
        .collect()
}

fn subkeys(key: &str) -> Vec<String> {
    registry::open_read(key)
        .map(|k| k.enum_keys().flatten().collect())
        .unwrap_or_default()
}

pub fn scan() -> Vec<Issue> {
    let mut issues = Vec::new();

    for root in [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths",
    ] {
        for sub in subkeys(root) {
            let key = format!(r"{root}\{sub}");
            if let Some((_, path)) = values(&key).into_iter().find(|(n, _)| n.is_empty())
                && definitely_missing(&path)
            {
                issues.push(Issue {
                    kind: Kind::AppPath,
                    key,
                    value: None,
                    detail: path,
                });
            }
        }
    }

    for root in [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall",
    ] {
        for sub in subkeys(root) {
            let key = format!(r"{root}\{sub}");
            let vals = values(&key);
            let get = |n: &str| vals.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
            let Some(cmd) = get("UninstallString") else {
                continue;
            };
            let Some(exe) = exe_from_command(&cmd) else {
                continue;
            };
            let lower = exe.to_lowercase();
            if lower.contains("msiexec") || lower.contains("rundll32") {
                continue; // system tools; validity depends on the MSI database instead
            }
            let location_gone =
                get("InstallLocation").is_none_or(|l| l.trim().is_empty() || definitely_missing(&l));
            if definitely_missing(&exe) && location_gone {
                let name = get("DisplayName").unwrap_or(sub.clone());
                issues.push(Issue {
                    kind: Kind::Uninstall,
                    key,
                    value: None,
                    detail: format!("{name} — {exe}"),
                });
            }
        }
    }

    let shared = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\SharedDLLs";
    for (name, _) in values(shared) {
        if definitely_missing(&name) {
            issues.push(Issue {
                kind: Kind::SharedDll,
                key: shared.into(),
                detail: name.clone(),
                value: Some(name),
            });
        }
    }

    let mui = r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\Shell\MuiCache";
    for (name, _) in values(mui) {
        // Value names look like "C:\path\app.exe.FriendlyAppName".
        let lower = name.to_lowercase();
        if let Some(i) = lower.find(".exe.")
            && definitely_missing(&name[..i + 4])
        {
            issues.push(Issue {
                kind: Kind::MuiCache,
                key: mui.into(),
                // Windows stores several values per program (name, company); show which one.
                detail: format!("{} ({})", &name[..i + 4], &name[i + 5..]),
                value: Some(name),
            });
        }
    }

    for key in [
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
    ] {
        for (name, cmd) in values(key) {
            if !name.is_empty()
                && let Some(exe) = exe_from_command(&cmd)
                && definitely_missing(&exe)
            {
                issues.push(Issue {
                    kind: Kind::StartupRun,
                    key: key.into(),
                    value: Some(name),
                    detail: exe,
                });
            }
        }
    }

    let compat =
        r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Compatibility Assistant\Store";
    for (name, _) in values(compat) {
        if definitely_missing(&name) {
            issues.push(Issue {
                kind: Kind::CompatStore,
                key: compat.into(),
                detail: name.clone(),
                value: Some(name),
            });
        }
    }

    issues.sort_by(|a, b| (a.kind, &a.detail).cmp(&(b.kind, &b.detail)));
    issues
}

/// Fixes issues; returns how many were fixed. Each kind is journaled separately.
pub fn fix(ctx: &AppCtx<'_>, issues: &[Issue]) -> Result<usize> {
    let mut fixed = 0;
    let mut snapshots: Vec<(Kind, ValueSnapshot)> = Vec::new();
    for issue in issues {
        match &issue.value {
            Some(name) => {
                let snap = registry::snapshot(&issue.key, name);
                registry::delete_value(&issue.key, name)?;
                snapshots.push((issue.kind, snap));
                fixed += 1;
            }
            None => {
                let undo = ctx.journal.backup_reg_key(&issue.key)?;
                run_checked(hidden_command("reg").args(["delete", &issue.key, "/f"]))?;
                ctx.journal.record(
                    "regclean",
                    format!("Removed key {} ({})", issue.key, issue.detail),
                    undo,
                )?;
                fixed += 1;
            }
        }
    }
    let mut kinds: Vec<Kind> = snapshots.iter().map(|(k, _)| *k).collect();
    kinds.dedup();
    for kind in kinds {
        let values: Vec<ValueSnapshot> = snapshots
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, s)| s.clone())
            .collect();
        ctx.journal.record(
            "regclean",
            format!(
                "Removed {} registry values: {}",
                values.len(),
                kind.label().to_lowercase()
            ),
            UndoAction::RestoreRegValues { values },
        )?;
    }
    Ok(fixed)
}

#[derive(Default)]
pub struct RegistryCleaner {
    task: Option<BackgroundTask<Vec<Issue>>>,
    issues: Option<Vec<Issue>>,
    marked: Vec<bool>,
    confirm: Confirm<()>,
    message: Message,
}

impl Tool for RegistryCleaner {
    fn id(&self) -> &'static str {
        "regclean"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Finds registry entries that point to programs and files which no longer exist. \
             It deliberately does not \"optimise\" anything else: only clearly dead references are listed.",
        );
        if self.task.is_none() && self.issues.is_none() {
            self.task = Some(BackgroundTask::spawn(ui.ctx(), |_| Ok(scan())));
        }
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                let issues = r.unwrap_or_default();
                self.marked = vec![true; issues.len()];
                self.issues = Some(issues);
            }
        }
        let issues = self.issues.clone().unwrap_or_default();
        let usable = |i: &Issue| !registry::needs_admin(&i.key) || ctx.elevated;

        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            let chosen: Vec<Issue> = issues
                .iter()
                .zip(&self.marked)
                .filter(|(i, m)| **m && usable(i))
                .map(|(i, _)| i.clone())
                .collect();
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                fix(ctx, &chosen)
                    .map(|n| format!("Fixed {n} issues; all can be undone from the Change Journal."))
                    .map_err(|e| format!("{e:#}"))
            });
            self.issues = None;
        }

        let chosen = issues
            .iter()
            .zip(&self.marked)
            .filter(|(i, m)| **m && usable(i))
            .count();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.task.is_none(), egui::Button::new("Rescan"))
                .clicked()
            {
                self.issues = None;
                self.message = None;
            }
            if ui
                .add_enabled(chosen > 0, egui::Button::new(format!("Fix {chosen} issues")))
                .clicked()
            {
                self.confirm.ask(
                    format!("Remove {chosen} dead registry entries? A backup is made first."),
                    (),
                );
            }
            if self.task.is_some() {
                ui.spinner();
            } else {
                ui.weak(format!("{} issues found", issues.len()));
            }
        });
        show_message(ui, &self.message);
        ui.separator();

        let mut current = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (i, issue) in issues.iter().enumerate() {
                if current != Some(issue.kind) {
                    current = Some(issue.kind);
                    ui.add_space(6.0);
                    ui.strong(issue.kind.label());
                }
                ui.horizontal(|ui| {
                    let r = ui.add_enabled(usable(issue), egui::Checkbox::without_text(&mut self.marked[i]));
                    if !usable(issue) {
                        r.on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                    }
                    ui.label(&issue.detail);
                })
                .response
                .on_hover_text(match &issue.value {
                    Some(v) => format!("{}\n{v}", issue.key),
                    None => issue.key.clone(),
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_detection_is_conservative() {
        assert!(
            !definitely_missing("notepad.exe"),
            "relative names are never reported"
        );
        assert!(
            !definitely_missing(r"\\server\share\x.exe"),
            "network paths are never reported"
        );
        assert!(!definitely_missing(r"C:\Windows\notepad.exe"));
        assert!(definitely_missing(r"C:\Windows\definitely-not-here-wm.exe"));
        assert!(definitely_missing(r#""%WINDIR%\nope-wm.exe""#));
        assert!(!definitely_missing("%UNSET_WM_VAR%\\x.exe"));
        // A drive letter that does not exist (unplugged USB) is never reported.
        let absent = ('D'..='Z')
            .find(|c| !Path::new(&format!("{c}:\\")).exists())
            .unwrap();
        assert!(!definitely_missing(&format!(r"{absent}:\gone\app.exe")));
    }

    #[test]
    fn finds_and_fixes_dead_run_value_with_undo() {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let name = format!("wm-regclean-test-{}", std::process::id());
        registry::write_raw(
            key,
            &name,
            &registry::string(r#""C:\Windows\no-such-wm-app.exe" /x"#),
        )
        .unwrap();
        let found: Vec<Issue> = scan()
            .into_iter()
            .filter(|i| i.value.as_deref() == Some(name.as_str()))
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, Kind::StartupRun);

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
        assert_eq!(fix(&ctx, &found).unwrap(), 1);
        assert!(registry::read_raw(key, &name).is_none());

        journal.undo(journal.entries()[0].id).unwrap();
        assert!(
            registry::read_raw(key, &name).is_some(),
            "undo restores the value"
        );
        registry::delete_value(key, &name).unwrap();
    }
}
