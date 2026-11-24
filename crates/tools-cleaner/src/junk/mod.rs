//! Junk File Cleaner: rule-based removal of temp files, caches, reports, dumps and
//! logs, plus code-based scans (broken shortcuts, empty folders, zero-byte files,
//! invalid Start Menu and Program Files folders, orphaned installer packages).
//!
//! Safety: rule roots must pass [`is_safe_root`], only files *inside* a root are
//! touched, links/junctions are never followed, recently modified files can be
//! skipped per rule, the user's skip list and (optionally) system files are always
//! left alone, and drive-wide scans keep out of Windows, program and app-data
//! folders. Deletion is permanent by default (that is what frees space); a "use
//! Recycle Bin" option is available.

pub(crate) mod dynamic;
mod view;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use windows::Win32::UI::Shell::{
    SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW, SHQUERYRBINFO,
    SHQueryRecycleBinW,
};
use windows::core::PCWSTR;
use wm_core::{
    paths::{expand_env, expand_pattern, wildcard_match},
    tool_settings,
};

pub use view::JunkCleaner;

pub const RULES: &str = include_str!("../../rules/junk.toml");
const TOOL: &str = "junk";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    #[default]
    Files,
    RecycleBin,
    /// Shortcuts on the desktop, in the Start Menu and Quick Launch whose target is gone.
    BrokenShortcuts,
    /// Empty Start Menu folders and broken shortcuts for all users.
    InvalidStartMenu,
    /// Program Files folders that are completely empty.
    EmptyProgramFiles,
    /// `.msi` / `.msp` files in `Windows\Installer` that no installed product uses.
    InstallerOrphans,
    /// Empty folders in user data on the chosen drives.
    EmptyFolders,
    /// Zero-byte files in user data on the chosen drives.
    ZeroByteFiles,
}

impl Kind {
    /// Scans that walk the drives picked in the window.
    pub fn uses_drives(self) -> bool {
        matches!(self, Kind::EmptyFolders | Kind::ZeroByteFiles)
    }
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

/// Options remembered between sessions; also used by 1-Click Cleaner and scheduled runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Folders (`C:\Keep`) or wildcard patterns (`*.log`, `*\Projects\*`) never cleaned.
    pub skip: Vec<String>,
    /// Leave files and folders marked as system files alone.
    pub skip_system: bool,
    /// Drive letters for the empty-folder and zero-byte scans (empty = Windows drive).
    pub drives: Vec<char>,
    pub use_recycle_bin: bool,
    /// Ticked rule ids, once the user has changed the defaults.
    pub selected: Option<Vec<String>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            skip: Vec::new(),
            skip_system: true,
            drives: Vec::new(),
            use_recycle_bin: false,
            selected: None,
        }
    }
}

impl Options {
    pub fn saved() -> Self {
        tool_settings::load(TOOL)
    }

    pub fn save(&self) -> Result<()> {
        tool_settings::save(TOOL, self)
    }

    /// True when `path` matches the skip list: a plain entry skips that file or
    /// folder and everything inside it; an entry with `*` or `?` is a wildcard.
    pub fn skips(&self, path: &Path) -> bool {
        let p = path.to_string_lossy().to_lowercase();
        self.skip.iter().any(|s| {
            let s = expand_env(s.trim()).to_lowercase();
            if s.is_empty() {
                false
            } else if s.contains(['*', '?']) {
                wildcard_match(&s, &p)
            } else {
                let s = s.trim_end_matches('\\');
                p == s || p.starts_with(&format!("{s}\\"))
            }
        })
    }

    pub fn drive_roots(&self) -> Vec<PathBuf> {
        let letters: Vec<char> = if self.drives.is_empty() {
            expand_env("%SystemDrive%").chars().take(1).collect()
        } else {
            self.drives.clone()
        };
        letters
            .into_iter()
            .map(|c| PathBuf::from(format!("{}:\\", c.to_ascii_uppercase())))
            .filter(|p| p.exists())
            .collect()
    }
}

/// Files and folders with the system attribute.
pub fn is_system(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    md.file_attributes() & FILE_ATTRIBUTE_SYSTEM != 0
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
    /// Empty folders to remove, deepest first.
    pub dirs: Vec<PathBuf>,
    /// Directories whose contents were collected (kept themselves; emptied subfolders removed).
    pub roots: Vec<PathBuf>,
    pub bytes: u64,
}

impl RuleScan {
    pub fn count(&self) -> usize {
        self.files.len() + self.dirs.len()
    }

    /// Drops one entry (and, for a folder, everything listed inside it).
    pub fn remove(&mut self, path: &Path) {
        self.files.retain(|(p, _)| !p.starts_with(path));
        self.dirs.retain(|d| !d.starts_with(path));
        self.bytes = self.files.iter().map(|(_, s)| s).sum();
    }
}

fn old_enough(modified: Option<SystemTime>, min_age: Duration) -> bool {
    min_age.is_zero()
        || modified
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age >= min_age)
}

/// Whether a found file may be offered for cleaning under the user's options.
fn allowed(path: &Path, md: &std::fs::Metadata, opts: &Options) -> bool {
    !opts.skips(path) && !(opts.skip_system && is_system(md))
}

pub fn scan_rule(rule: &Rule, opts: &Options) -> RuleScan {
    scan_rule_with(rule, opts, &|_| {})
}

/// Like [`scan_rule`], reporting progress text for the long drive scans.
pub fn scan_rule_with(rule: &Rule, opts: &Options, report: &dyn Fn(&str)) -> RuleScan {
    if rule.kind == Kind::RecycleBin {
        return RuleScan {
            bytes: recycle_bin_size().unwrap_or(0),
            ..Default::default()
        };
    }
    let mut scan = match rule.kind {
        Kind::BrokenShortcuts => dynamic::broken_shortcuts(&dynamic::user_shortcut_places(), opts),
        Kind::InvalidStartMenu => dynamic::invalid_start_menu(opts),
        Kind::EmptyProgramFiles => dynamic::empty_program_files(opts),
        Kind::InstallerOrphans => dynamic::installer_orphans(opts),
        Kind::EmptyFolders => dynamic::empty_folders(&opts.drive_roots(), opts, report),
        Kind::ZeroByteFiles => dynamic::zero_byte_files(&opts.drive_roots(), opts, report),
        Kind::Files | Kind::RecycleBin => scan_files(rule, opts),
    };
    scan.bytes = scan.files.iter().map(|(_, s)| s).sum();
    scan
}

fn scan_files(rule: &Rule, opts: &Options) -> RuleScan {
    let mut scan = RuleScan::default();
    let min_age = Duration::from_secs(rule.min_age_hours * 3600);
    for pattern in &rule.paths {
        for root in expand_pattern(pattern) {
            let Ok(md) = std::fs::symlink_metadata(&root) else {
                continue;
            };
            if md.file_type().is_symlink() || opts.skips(&root) {
                continue;
            }
            if md.is_file() {
                // Individual files (e.g. MEMORY.DMP, thumbcache_*.db) only need a safe parent.
                if root.parent().is_some_and(|p| p.components().count() >= 2)
                    && old_enough(md.modified().ok(), min_age)
                {
                    scan.files.push((root, md.len()));
                }
                continue;
            }
            if !is_safe_root(&root) {
                tracing::warn!("skipping unsafe junk root {}", root.display());
                continue;
            }
            let walker = WalkDir::new(&root)
                .follow_links(false)
                .min_depth(1)
                .into_iter()
                .filter_entry(|e| !opts.skips(e.path()));
            for entry in walker.flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let Ok(md) = entry.metadata() else { continue };
                if allowed(entry.path(), &md, opts) && old_enough(md.modified().ok(), min_age) {
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
        // Empty folders: removed only if still empty, deepest first.
        for d in &scan.dirs {
            if std::fs::remove_dir(d).is_ok() {
                report.removed += 1;
            }
        }
        for root in &scan.roots {
            prune_empty_dirs(root);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_and_are_safe() {
        let rules = parse_rules(RULES).unwrap();
        assert!(rules.len() >= 20);
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
            // Scans that need review are never part of the default (or 1-Click) cleanup.
            if !matches!(r.kind, Kind::Files | Kind::RecycleBin | Kind::BrokenShortcuts) {
                assert!(!r.default, "{} must not be ticked by default", r.id);
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
    fn skip_list_matching() {
        let o = Options {
            skip: vec![r"C:\Keep\".into(), "*.log".into(), r"*\Projects\*".into()],
            ..Default::default()
        };
        assert!(o.skips(Path::new(r"C:\Keep")));
        assert!(o.skips(Path::new(r"C:\keep\a\b.tmp")));
        assert!(!o.skips(Path::new(r"C:\Keeper\b.tmp")), "whole folder names only");
        assert!(o.skips(Path::new(r"D:\x\setup.LOG")));
        assert!(o.skips(Path::new(r"C:\Users\me\Projects\a.tmp")));
        assert!(!o.skips(Path::new(r"C:\Users\me\a.tmp")));
    }

    fn rule(root: &Path, age: u64) -> Rule {
        Rule {
            id: "t".into(),
            title: "t".into(),
            group: "g".into(),
            description: String::new(),
            paths: vec![root.display().to_string()],
            min_age_hours: age,
            admin: false,
            default: true,
            kind: Kind::Files,
        }
    }

    #[test]
    fn scan_and_clean_respect_age_skip_list_and_keep_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::create_dir_all(root.join("keep")).unwrap();
        std::fs::write(root.join("a.tmp"), [1u8; 10]).unwrap();
        std::fs::write(root.join("sub/deeper/b.tmp"), [1u8; 20]).unwrap();
        std::fs::write(root.join("keep/c.tmp"), [1u8; 40]).unwrap();
        let opts = Options {
            skip: vec![root.join("keep").display().to_string()],
            ..Default::default()
        };

        // Brand-new files are skipped by an age filter.
        assert_eq!(scan_rule(&rule(&root, 24), &opts).files.len(), 0);

        let scan = scan_rule(&rule(&root, 0), &opts);
        assert_eq!(scan.files.len(), 2, "skip list honoured");
        assert_eq!(scan.bytes, 30);

        let report = clean(&[(rule(&root, 0), scan)], false);
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
        assert!(root.join("keep/c.tmp").exists());
    }

    #[test]
    fn real_rules_scan_without_error() {
        let opts = Options::default();
        for r in parse_rules(RULES)
            .unwrap()
            .iter()
            .filter(|r| r.kind == Kind::Files)
        {
            let s = scan_rule(r, &opts);
            assert!(s.files.iter().all(|(p, _)| p.is_absolute()), "{}", r.id);
            assert!(s.roots.iter().all(|root| is_safe_root(root)), "{}", r.id);
        }
        assert!(recycle_bin_size().is_some());
    }
}
