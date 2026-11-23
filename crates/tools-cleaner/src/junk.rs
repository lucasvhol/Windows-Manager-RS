//! Junk File Cleaner: rule-based removal of temp files, caches, reports and dumps.
//!
//! Safety: rule roots must pass [`is_safe_root`], only files *inside* a root are
//! touched, links/junctions are never followed, and recently modified files can be
//! skipped per rule. Deletion is permanent by default (that is what frees space);
//! a "use Recycle Bin" option is available.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;
use walkdir::WalkDir;
use windows::Win32::UI::Shell::{
    SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW, SHQUERYRBINFO,
    SHQueryRecycleBinW,
};
use windows::core::PCWSTR;
use wm_core::{
    AppCtx, Category, Tool, egui,
    paths::{expand_env, expand_pattern},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, format_bytes, show_message},
};

pub const RULES: &str = include_str!("../rules/junk.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    #[default]
    Files,
    RecycleBin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Rule {
    pub id: String,
    pub title: String,
    pub group: String,
    pub description: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub min_age_hours: u64,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub kind: Kind,
}

#[derive(Deserialize)]
struct RuleFile {
    rule: Vec<Rule>,
}

pub fn parse_rules(text: &str) -> Result<Vec<Rule>> {
    Ok(toml::from_str::<RuleFile>(text)
        .context("parsing junk rules")?
        .rule)
}

/// Rejects roots whose contents must never be bulk-deleted (drive roots, Windows,
/// Program Files, the profile folder itself, …).
pub fn is_safe_root(path: &Path) -> bool {
    // Prefix + root + at least two folders: rejects "C:\" and "C:\Anything".
    let folders = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    if !path.is_absolute() || folders < 2 {
        return false;
    }
    let norm = |p: &str| p.trim_end_matches('\\').to_lowercase();
    let p = norm(&path.to_string_lossy());
    let forbidden = [
        "%USERPROFILE%",
        "%LOCALAPPDATA%",
        "%APPDATA%",
        "%PROGRAMDATA%",
        "%WINDIR%",
        "%WINDIR%\\System32",
        "%WINDIR%\\SysWOW64",
        "%PROGRAMFILES%",
        "%PROGRAMFILES(X86)%",
        "%PUBLIC%",
    ];
    !forbidden.iter().any(|f| norm(&expand_env(f)) == p)
}

#[derive(Debug, Clone, Default)]
pub struct RuleScan {
    pub files: Vec<(PathBuf, u64)>,
    /// Directories whose contents were collected (kept themselves; emptied subfolders removed).
    pub roots: Vec<PathBuf>,
    pub bytes: u64,
}

fn old_enough(modified: Option<SystemTime>, min_age: Duration) -> bool {
    min_age.is_zero()
        || modified
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age >= min_age)
}

pub fn scan_rule(rule: &Rule) -> RuleScan {
    let mut scan = RuleScan::default();
    if rule.kind == Kind::RecycleBin {
        scan.bytes = recycle_bin_size().unwrap_or(0);
        return scan;
    }
    let min_age = Duration::from_secs(rule.min_age_hours * 3600);
    for pattern in &rule.paths {
        for root in expand_pattern(pattern) {
            let Ok(md) = std::fs::symlink_metadata(&root) else {
                continue;
            };
            if md.file_type().is_symlink() {
                continue;
            }
            if md.is_file() {
                // Individual files (e.g. MEMORY.DMP, thumbcache_*.db) only need a safe parent.
                if root.parent().is_some_and(|p| p.components().count() >= 2)
                    && old_enough(md.modified().ok(), min_age)
                {
                    scan.bytes += md.len();
                    scan.files.push((root, md.len()));
                }
                continue;
            }
            if !is_safe_root(&root) {
                tracing::warn!("skipping unsafe junk root {}", root.display());
                continue;
            }
            for entry in WalkDir::new(&root)
                .follow_links(false)
                .min_depth(1)
                .into_iter()
                .flatten()
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let Ok(md) = entry.metadata() else { continue };
                if old_enough(md.modified().ok(), min_age) {
                    scan.bytes += md.len();
                    scan.files.push((entry.into_path(), md.len()));
                }
            }
            scan.roots.push(root);
        }
    }
    scan
}

pub fn recycle_bin_size() -> Option<u64> {
    let mut info = SHQUERYRBINFO {
        cbSize: size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info).ok()? };
    Some(info.i64Size.max(0) as u64)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CleanReport {
    pub freed: u64,
    pub removed: u64,
    pub failed: u64,
}

/// Removes empty directories below (not including) `root`, deepest first.
fn prune_empty_dirs(root: &Path) {
    let mut dirs: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = std::fs::remove_dir(d); // fails (harmlessly) when not empty
    }
}

pub fn clean(scans: &[(Rule, RuleScan)], use_recycle_bin: bool) -> CleanReport {
    let mut report = CleanReport::default();
    for (rule, scan) in scans {
        if rule.kind == Kind::RecycleBin {
            let flags = SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND;
            if unsafe { SHEmptyRecycleBinW(None, PCWSTR::null(), flags) }.is_ok() {
                report.freed += scan.bytes;
            }
            continue;
        }
        if use_recycle_bin {
            for chunk in scan.files.chunks(500) {
                let paths: Vec<&PathBuf> = chunk.iter().map(|(p, _)| p).collect();
                match trash::delete_all(&paths) {
                    Ok(()) => {
                        report.removed += chunk.len() as u64;
                        report.freed += chunk.iter().map(|(_, s)| s).sum::<u64>();
                    }
                    // Fall back to one-by-one so one locked file does not fail the batch.
                    Err(_) => {
                        for (p, s) in chunk {
                            if trash::delete(p).is_ok() {
                                report.removed += 1;
                                report.freed += s;
                            } else {
                                report.failed += 1;
                            }
                        }
                    }
                }
            }
        } else {
            for (p, s) in &scan.files {
                if std::fs::remove_file(p).is_ok() {
                    report.removed += 1;
                    report.freed += s;
                } else {
                    report.failed += 1; // in use or access denied
                }
            }
        }
        for root in &scan.roots {
            prune_empty_dirs(root);
        }
    }
    report
}

pub struct JunkCleaner {
    rules: Vec<Rule>,
    selected: Vec<bool>,
    scans: Option<Vec<RuleScan>>,
    scan_task: Option<BackgroundTask<Vec<RuleScan>>>,
    clean_task: Option<BackgroundTask<CleanReport>>,
    use_recycle_bin: bool,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for JunkCleaner {
    fn default() -> Self {
        let rules = parse_rules(RULES).expect("built-in junk rules must parse");
        Self {
            selected: rules.iter().map(|r| r.default).collect(),
            rules,
            scans: None,
            scan_task: None,
            clean_task: None,
            use_recycle_bin: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl JunkCleaner {
    fn usable(&self, i: usize, elevated: bool) -> bool {
        !self.rules[i].admin || elevated
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let rules = self.rules.clone();
        self.scans = None;
        self.scan_task = Some(BackgroundTask::spawn(ctx, move |p| {
            let n = rules.len() as f32;
            Ok(rules
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    p.report(i as f32 / n, format!("Scanning {}…", r.title));
                    scan_rule(r)
                })
                .collect())
        }));
    }
}

impl Tool for JunkCleaner {
    fn id(&self) -> &'static str {
        "junk"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.scans.is_none() && self.scan_task.is_none() && self.clean_task.is_none() {
            self.start_scan(ui.ctx());
        }
        if let Some(t) = &mut self.scan_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.scan_task = None;
                match r {
                    Ok(s) => self.scans = Some(s),
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if let Some(t) = &mut self.clean_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.clean_task = None;
                self.message = Some(r.map(|rep| {
                    let how = if self.use_recycle_bin {
                        "moved to the Recycle Bin"
                    } else {
                        "deleted"
                    };
                    let desc = format!(
                        "Junk cleanup: {} files {how}, {} freed",
                        rep.removed,
                        format_bytes(rep.freed)
                    );
                    let _ = ctx.journal.record("junk", desc.clone(), UndoAction::None);
                    if rep.failed > 0 {
                        format!("{desc}. {} files were in use and skipped.", rep.failed)
                    } else {
                        desc + "."
                    }
                }));
                self.scans = None; // rescan
            }
        }

        let busy = self.scan_task.is_some() || self.clean_task.is_some();
        let selected_bytes: u64 = self.scans.as_ref().map_or(0, |scans| {
            scans
                .iter()
                .enumerate()
                .filter(|(i, _)| self.selected[*i] && self.usable(*i, ctx.elevated))
                .map(|(_, s)| s.bytes)
                .sum()
        });

        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else if let Some(scans) = &self.scans {
                let work: Vec<(Rule, RuleScan)> = self
                    .rules
                    .iter()
                    .zip(scans)
                    .enumerate()
                    .filter(|(i, _)| self.selected[*i] && self.usable(*i, ctx.elevated))
                    .map(|(_, (r, s))| (r.clone(), s.clone()))
                    .collect();
                let recycle = self.use_recycle_bin;
                self.clean_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                    Ok(clean(&work, recycle))
                }));
            }
        }

        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Rescan")).clicked() {
                self.message = None;
                self.start_scan(ui.ctx());
            }
            let clean = ui.add_enabled(
                !busy && selected_bytes > 0,
                egui::Button::new(format!("Clean {}", format_bytes(selected_bytes))),
            );
            if clean.clicked() {
                let how = if self.use_recycle_bin {
                    "moved to the Recycle Bin"
                } else {
                    "permanently deleted (this cannot be undone)"
                };
                self.confirm.ask(
                    format!(
                        "{} of junk files will be {how}. Continue?",
                        format_bytes(selected_bytes)
                    ),
                    (),
                );
            }
            ui.checkbox(
                &mut self.use_recycle_bin,
                "Move to Recycle Bin instead of deleting",
            );
            if busy {
                ui.spinner();
                if let Some(t) = &self.scan_task {
                    ui.weak(&t.status);
                } else {
                    ui.weak("Cleaning…");
                }
            }
        });
        show_message(ui, &self.message);
        ui.add_space(6.0);

        let mut groups: Vec<String> = Vec::new();
        for r in &self.rules {
            if !groups.contains(&r.group) {
                groups.push(r.group.clone());
            }
        }
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for group in groups {
                ui.strong(&group);
                egui::Grid::new(&group)
                    .num_columns(3)
                    .striped(true)
                    .spacing([16.0, 4.0])
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        for i in (0..self.rules.len()).filter(|&i| self.rules[i].group == group) {
                            let usable = self.usable(i, ctx.elevated);
                            let rule = &self.rules[i];
                            // Locked (admin-only) rules show unticked: they are not part of the cleanup.
                            let mut locked_off = false;
                            let state = if usable {
                                &mut self.selected[i]
                            } else {
                                &mut locked_off
                            };
                            let r = ui.add_enabled(usable, egui::Checkbox::new(state, &rule.title));
                            let r = r.on_hover_text(&rule.description);
                            if !usable {
                                r.on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                            }
                            match self.scans.as_ref().map(|s| &s[i]) {
                                Some(s) if rule.kind == Kind::RecycleBin => {
                                    ui.label(format_bytes(s.bytes));
                                    ui.label("");
                                }
                                Some(s) => {
                                    ui.label(format_bytes(s.bytes));
                                    ui.weak(format!("{} files", s.files.len()));
                                }
                                None => {
                                    ui.weak("…");
                                    ui.label("");
                                }
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(10.0);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_and_are_safe() {
        let rules = parse_rules(RULES).unwrap();
        assert!(rules.len() >= 15);
        for r in &rules {
            if r.kind == Kind::Files {
                assert!(!r.paths.is_empty(), "{} has no paths", r.id);
            }
            for p in &r.paths {
                assert!(
                    p.starts_with('%'),
                    "{}: paths must be anchored to a known folder",
                    r.id
                );
            }
        }
    }

    #[test]
    fn unsafe_roots_rejected() {
        assert!(!is_safe_root(Path::new(r"C:\")));
        assert!(!is_safe_root(Path::new(r"C:\Windows")));
        assert!(!is_safe_root(Path::new(r"C:\Windows\System32")));
        assert!(!is_safe_root(Path::new(r"C:\Users\")));
        assert!(!is_safe_root(Path::new(r"relative\path\here")));
        assert!(!is_safe_root(Path::new(&expand_env("%USERPROFILE%"))));
        assert!(!is_safe_root(Path::new(&expand_env("%LOCALAPPDATA%"))));
        assert!(is_safe_root(Path::new(&expand_env(r"%LOCALAPPDATA%\CrashDumps"))));
        assert!(is_safe_root(Path::new(r"C:\Windows\Temp")));
    }

    #[test]
    fn scan_and_clean_respect_age_and_keep_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("a.tmp"), [1u8; 10]).unwrap();
        std::fs::write(root.join("sub/deeper/b.tmp"), [1u8; 20]).unwrap();

        let rule = |age| Rule {
            id: "t".into(),
            title: "t".into(),
            group: "g".into(),
            description: String::new(),
            paths: vec![root.display().to_string()],
            min_age_hours: age,
            admin: false,
            default: true,
            kind: Kind::Files,
        };
        // Brand-new files are skipped by an age filter.
        assert_eq!(scan_rule(&rule(24)).files.len(), 0);

        let scan = scan_rule(&rule(0));
        assert_eq!(scan.files.len(), 2);
        assert_eq!(scan.bytes, 30);

        let report = clean(&[(rule(0), scan)], false);
        assert_eq!(
            report,
            CleanReport {
                freed: 30,
                removed: 2,
                failed: 0
            }
        );
        assert!(root.exists(), "rule root is kept");
        assert!(!root.join("sub").exists(), "emptied subfolders are pruned");
    }

    #[test]
    fn real_rules_scan_without_error() {
        for r in parse_rules(RULES)
            .unwrap()
            .iter()
            .filter(|r| r.kind == Kind::Files)
        {
            let s = scan_rule(r);
            assert!(s.files.iter().all(|(p, _)| p.is_absolute()), "{}", r.id);
            assert!(s.roots.iter().all(|root| is_safe_root(root)), "{}", r.id);
        }
        assert!(recycle_bin_size().is_some());
    }
}
