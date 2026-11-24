//! Privacy Protector: privacy settings (tweaks) and clearing of activity traces.

use std::path::PathBuf;

use anyhow::Result;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};
use wm_core::{
    AppCtx, Category, Tool, egui, paths, registry,
    safety::UndoAction,
    tweaks::TweakPanel,
    ui::{Message, show_message},
};

const EXPLORER: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trace {
    RunHistory,
    TypedPaths,
    RecentFiles,
    JumpLists,
    Clipboard,
}

impl Trace {
    pub const ALL: [Trace; 5] = [
        Trace::RunHistory,
        Trace::TypedPaths,
        Trace::RecentFiles,
        Trace::JumpLists,
        Trace::Clipboard,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Trace::RunHistory => "Run dialog history",
            Trace::TypedPaths => "File Explorer address bar history",
            Trace::RecentFiles => "Recent files list",
            Trace::JumpLists => "Taskbar jump list history (recent and pinned items)",
            Trace::Clipboard => "Current clipboard contents",
        }
    }

    pub fn undoable(self) -> bool {
        !matches!(self, Trace::Clipboard)
    }
}

/// Registry key whose values (except the key's default) hold the history.
fn history_key(t: Trace) -> Option<String> {
    match t {
        Trace::RunHistory => Some(format!(r"{EXPLORER}\RunMRU")),
        Trace::TypedPaths => Some(format!(r"{EXPLORER}\TypedPaths")),
        _ => None,
    }
}

fn history_folder(t: Trace) -> Vec<PathBuf> {
    let recent = PathBuf::from(paths::expand_env(r"%APPDATA%\Microsoft\Windows\Recent"));
    match t {
        Trace::RecentFiles => vec![recent],
        Trace::JumpLists => vec![
            recent.join("AutomaticDestinations"),
            recent.join("CustomDestinations"),
        ],
        _ => Vec::new(),
    }
}

/// Number of entries the trace currently holds.
pub fn count(t: Trace) -> usize {
    if let Some(key) = history_key(t) {
        return registry::open_read(&key)
            .map(|k| k.enum_values().flatten().filter(|(n, _)| !n.is_empty()).count())
            .unwrap_or(0);
    }
    history_folder(t)
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|e| e.flatten())
        .filter(|e| e.path().is_file())
        .count()
}

pub fn clear(ctx: &AppCtx<'_>, t: Trace) -> Result<usize> {
    if let Some(key) = history_key(t) {
        let names: Vec<String> = registry::open_read(&key)
            .map(|k| {
                k.enum_values()
                    .flatten()
                    .map(|(n, _)| n)
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let snaps: Vec<_> = names.iter().map(|n| registry::snapshot(&key, n)).collect();
        for n in &names {
            registry::delete_value(&key, n)?;
        }
        if !snaps.is_empty() {
            ctx.journal.record(
                "privacy",
                format!("Cleared {}", t.label()),
                UndoAction::RestoreRegValues { values: snaps },
            )?;
        }
        return Ok(names.len());
    }
    if t == Trace::Clipboard {
        unsafe {
            OpenClipboard(None)?;
            let r = EmptyClipboard();
            let _ = CloseClipboard();
            r?;
        }
        ctx.journal
            .record("privacy", "Cleared the clipboard", UndoAction::None)?;
        return Ok(1);
    }
    let files: Vec<PathBuf> = history_folder(t)
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|e| e.flatten())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && !p
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("desktop.ini"))
        })
        .collect();
    let n = files.len();
    if n > 0 {
        ctx.journal
            .recycle("privacy", format!("Cleared {} ({n} items)", t.label()), files)?;
    }
    Ok(n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Settings,
    Traces,
}

pub struct PrivacyProtector {
    tab: Tab,
    panel: TweakPanel,
    selected: Vec<bool>,
    counts: Option<Vec<usize>>,
    message: Message,
}

impl Default for PrivacyProtector {
    fn default() -> Self {
        Self {
            tab: Tab::Settings,
            panel: TweakPanel::new("privacy", include_str!("../tweaks/privacy.toml")),
            selected: vec![true; Trace::ALL.len()],
            counts: None,
            message: None,
        }
    }
}

impl Tool for PrivacyProtector {
    fn id(&self) -> &'static str {
        "privacy"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Settings, "Privacy settings");
            ui.selectable_value(&mut self.tab, Tab::Traces, "Clear activity traces");
        });
        ui.separator();
        if self.tab == Tab::Settings {
            self.panel.ui(ui, ctx);
            return;
        }

        let counts = self
            .counts
            .get_or_insert_with(|| Trace::ALL.iter().map(|t| count(*t)).collect())
            .clone();
        for (i, t) in Trace::ALL.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.selected[i], t.label());
                if *t != Trace::Clipboard {
                    ui.weak(format!("{} entries", counts[i]));
                }
                if !t.undoable() {
                    ui.weak("· cannot be undone");
                }
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Clear selected").clicked() {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    let mut total = 0;
                    let mut errors = Vec::new();
                    for (i, t) in Trace::ALL.iter().enumerate().filter(|(i, _)| self.selected[*i]) {
                        match clear(ctx, *t) {
                            Ok(n) => total += n,
                            Err(e) => errors.push(format!("{}: {e:#}", Trace::ALL[i].label())),
                        }
                    }
                    if errors.is_empty() {
                        Ok(format!("Cleared {total} entries. Registry history and files can be restored from the Change Journal."))
                    } else {
                        Err(errors.join("; "))
                    }
                });
                self.counts = None;
            }
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.counts = None;
            }
        });
        show_message(ui, &self.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tweak_definitions_parse() {
        for text in [
            include_str!("../tweaks/privacy.toml"),
            include_str!("../tweaks/lockdown.toml"),
        ] {
            let tweaks = wm_core::tweaks::parse(text).unwrap();
            assert!(!tweaks.is_empty());
            for t in tweaks {
                assert!(t.settings.iter().all(|s| s.on != s.off), "{}", t.id);
                let _ = t.state();
            }
        }
    }

    #[test]
    fn counting_does_not_fail() {
        for t in Trace::ALL {
            let _ = count(t);
        }
        assert!(history_key(Trace::RunHistory).unwrap().ends_with("RunMRU"));
    }
}
