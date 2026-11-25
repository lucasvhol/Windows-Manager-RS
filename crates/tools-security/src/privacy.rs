//! Privacy Protector: privacy settings (tweaks) and clearing of activity traces.

use std::path::PathBuf;

use crate::privacy_traces::{self, Item, Section};
use anyhow::Result;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};
use wm_core::{
    AppCtx, Category, Tool, egui, paths, registry,
    safety::UndoAction,
    task::BackgroundTask,
    tool_settings,
    tweaks::TweakPanel,
    ui::{Confirm, Message, show_message},
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

const SELECTION_ID: &str = "privacy-traces";

/// Ticks the user changed, by item id; other items use their default.
type Selection = std::collections::BTreeMap<String, bool>;

pub struct PrivacyProtector {
    tab: Tab,
    panel: TweakPanel,
    items: Vec<(Item, usize)>,
    scan: Option<BackgroundTask<Vec<(Item, usize)>>>,
    selection: Selection,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for PrivacyProtector {
    fn default() -> Self {
        Self {
            tab: Tab::Settings,
            panel: TweakPanel::new("privacy", include_str!("../tweaks/privacy.toml")),
            items: Vec::new(),
            scan: None,
            selection: tool_settings::load(SELECTION_ID),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl PrivacyProtector {
    fn rescan(&mut self, ctx: &egui::Context) {
        self.scan = Some(BackgroundTask::spawn(ctx, |_| {
            Ok(privacy_traces::catalog()
                .into_iter()
                .map(|i| {
                    let n = privacy_traces::count_item(&i);
                    (i, n)
                })
                .collect())
        }));
    }

    fn selected(&self, item: &Item) -> bool {
        self.selection.get(&item.id).copied().unwrap_or(item.default_on)
    }

    fn set_selected(&mut self, id: &str, on: bool) {
        self.selection.insert(id.to_owned(), on);
        let _ = tool_settings::save(SELECTION_ID, &self.selection);
    }

    fn clear_selected(&mut self, ctx: &AppCtx<'_>) -> Result<String, String> {
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        let mut total = 0;
        let mut done = 0;
        let mut problems = Vec::new();
        for (item, _) in self.items.iter().filter(|(i, _)| self.selected(i)) {
            match privacy_traces::clear_item(ctx, item) {
                Ok(n) => {
                    total += n;
                    done += 1;
                }
                Err(e) => problems.push(format!("{}: {e:#}", item.label)),
            }
        }
        let summary = format!("Cleared {done} item(s), {total} entries.");
        if problems.is_empty() {
            Ok(format!("{summary} Undo from the Change Journal where allowed."))
        } else {
            Err(format!("{summary} Not cleared: {}", problems.join("; ")))
        }
    }

    fn traces_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if self.items.is_empty() && self.scan.is_none() {
            self.rescan(ui.ctx());
        }
        if let Some(task) = &mut self.scan {
            task.poll();
            if let Some(result) = task.take_result() {
                match result {
                    Ok(items) => self.items = items,
                    Err(e) => self.message = Some(Err(e)),
                }
                self.scan = None;
            }
        }
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            self.message = Some(self.clear_selected(ctx));
            self.rescan(ui.ctx());
        }

        let chosen: Vec<&Item> = self
            .items
            .iter()
            .map(|(i, _)| i)
            .filter(|i| self.selected(i) && (!i.admin || ctx.elevated))
            .collect();
        let lasting = chosen.iter().filter(|i| !i.undoable()).count();
        let n_chosen = chosen.len();
        ui.horizontal(|ui| {
            let label = format!("Clear selected ({n_chosen})");
            if ui
                .add_enabled(n_chosen > 0 && self.scan.is_none(), egui::Button::new(label))
                .clicked()
            {
                let mut q = format!("Clear the {n_chosen} selected item(s)?");
                if lasting > 0 {
                    q += &format!(" {lasting} of them can't be undone.");
                }
                self.confirm.ask(q, ());
            }
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() && self.scan.is_none() {
                self.rescan(ui.ctx());
            }
            if ui.button("Default selection").clicked() {
                self.selection.clear();
                let _ = tool_settings::save(SELECTION_ID, &self.selection);
            }
            if self.scan.is_some() {
                ui.spinner();
                ui.weak("Counting…");
            }
        });
        show_message(ui, &self.message);
        ui.add_space(4.0);

        let mut toggles: Vec<(String, bool)> = Vec::new();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for section in Section::ALL {
                    let rows: Vec<&(Item, usize)> =
                        self.items.iter().filter(|(i, _)| i.section == section).collect();
                    if rows.is_empty() {
                        continue;
                    }
                    let on = rows.iter().filter(|(i, _)| self.selected(i)).count();
                    egui::CollapsingHeader::new(format!(
                        "{}  ({on} of {} selected)",
                        section.label(),
                        rows.len()
                    ))
                    .id_salt(section)
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.small_button("All").clicked() {
                                toggles.extend(rows.iter().map(|(i, _)| (i.id.clone(), true)));
                            }
                            if ui.small_button("None").clicked() {
                                toggles.extend(rows.iter().map(|(i, _)| (i.id.clone(), false)));
                            }
                        });
                        for (item, n) in rows {
                            ui.horizontal(|ui| {
                                let mut sel = self.selected(item);
                                let enabled = !item.admin || ctx.elevated;
                                let resp =
                                    ui.add_enabled(enabled, egui::Checkbox::new(&mut sel, item.label.as_str()));
                                if resp.changed() {
                                    toggles.push((item.id.clone(), sel));
                                }
                                // Unelevated, admin items can't be read, so no count.
                                if item.countable() && enabled {
                                    ui.weak(match *n {
                                        0 => "empty".to_owned(),
                                        1 => "1 entry".to_owned(),
                                        n => format!("{n} entries"),
                                    });
                                }
                                let mut notes: Vec<&str> = Vec::new();
                                if !item.note.is_empty() {
                                    notes.push(item.note);
                                }
                                if item.close.is_some() {
                                    notes.push("close the browser first");
                                }
                                if !item.undoable() {
                                    notes.push("can't be undone");
                                }
                                if item.admin {
                                    notes.push("admin");
                                }
                                if !notes.is_empty() {
                                    ui.weak(format!("· {}", notes.join(" · ")));
                                }
                            });
                        }
                    });
                }
                if !self.items.is_empty() {
                    ui.add_space(6.0);
                    ui.weak("Program history lists only programs that have left some behind. Firefox isn't covered: its history and bookmarks share one file.");
                }
            });
        for (id, on) in toggles {
            self.set_selected(&id, on);
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
        match self.tab {
            Tab::Settings => self.panel.ui(ui, ctx),
            Tab::Traces => self.traces_ui(ui, ctx),
        }
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
