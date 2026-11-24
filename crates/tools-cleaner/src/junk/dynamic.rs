//! Junk scans that need code rather than a list of folders.
//!
//! Drive-wide scans (empty folders, zero-byte files) only look at user data: they never
//! enter Windows, program, app-data or hidden tool folders, never offer a drive's
//! top-level folders, a user profile or the folders directly inside it (Documents,
//! Music, …), and leave alone the zero-byte files that programs rely on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;
use wm_core::{
    paths::expand_env,
    process::{json_list, powershell_json},
    shell::is_broken_shortcut,
};

use super::{Options, RuleScan, allowed, is_system};

/// Folders a drive-wide scan never enters (matched by name, any depth).
pub(crate) const NEVER_ENTER: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "appdata",
    "$recycle.bin",
    "system volume information",
    "recovery",
    "$winreagent",
    "$sysreset",
    "$windows.~bt",
    "$windows.~ws",
    "config.msi",
    "msocache",
    "perflogs",
    "boot",
    "onedrivetemp",
    "node_modules",
    "site-packages",
    "__pycache__",
    "venv",
    "target",
    // Game libraries: installs keep empty folders and zero-byte files on purpose.
    "steamapps",
    "steamlibrary",
    "steam",
    "battle.net",
    "riot games",
    "epic games",
    "gog galaxy",
    "gog games",
    "ea games",
    "ubisoft",
    "ubisoft game launcher",
    "xboxgames",
];

/// Zero-byte files that programs and tools rely on.
const KEEP_EMPTY_FILES: &[&str] = &[
    "__init__.py",
    "py.typed",
    ".gitkeep",
    ".keep",
    ".nomedia",
    ".gitignore",
    ".placeholder",
    ".empty",
    "lock",
    "desktop.ini",
];
const KEEP_EMPTY_EXTENSIONS: &[&str] = &["lock", "pid", "flag", "marker", "stamp", "sentinel"];

fn name_lower(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Files that mark a source or build tree.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "cmakelists.txt",
    "cmakecache.txt",
    "cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "makefile",
    "meson.build",
];

/// A folder holding programs directly (an installed or portable app, a game) or the
/// root of a source/build tree: its empty folders and zero-byte files belong to it.
fn contains_program(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n.ends_with(".exe")
                || n.ends_with(".dll")
                || n.ends_with(".sln")
                || n.ends_with(".vcxproj")
                || n.ends_with(".csproj")
                || PROJECT_MARKERS.contains(&n.as_str())
        })
    })
}

/// Whether a drive-wide walk may descend into `dir`.
fn may_enter(dir: &Path, md: Option<&std::fs::Metadata>, opts: &Options) -> bool {
    let name = name_lower(dir);
    !NEVER_ENTER.contains(&name.as_str())
        && !name.starts_with('.')
        && !opts.skips(dir)
        && !md.is_some_and(|m| m.file_type().is_symlink() || (opts.skip_system && is_system(m)))
        && !contains_program(dir)
}

/// Folders that are never offered even when empty: drive top level, profiles and
/// the folders directly inside a profile (Documents, Music, Saved Games, …).
fn protected_dir(dir: &Path) -> bool {
    let parts: Vec<String> = dir
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    parts.len() <= 1 || (parts[0] == "users" && parts.len() <= 3)
}

/// Collects empty folders under `root` (deepest first). A folder is empty when it has
/// no files and only empty subfolders. Returns whether `root` itself is empty.
fn collect_empty(root: &Path, opts: &Options, enter: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false; // unreadable: treat as in use
    };
    let mut empty = true;
    for e in entries.flatten() {
        let path = e.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            empty = false;
            continue;
        };
        if md.is_dir() && !md.file_type().is_symlink() && enter(&path) && may_enter_basic(&path, &md, opts) {
            if !collect_empty(&path, opts, enter, out) {
                empty = false;
            }
        } else {
            empty = false;
        }
    }
    if empty && !opts.skips(root) {
        out.push(root.to_owned());
    }
    empty
}

fn may_enter_basic(dir: &Path, md: &std::fs::Metadata, opts: &Options) -> bool {
    !opts.skips(dir) && !(opts.skip_system && is_system(md))
}

pub fn empty_folders(roots: &[PathBuf], opts: &Options, report: &dyn Fn(&str)) -> RuleScan {
    let mut scan = RuleScan::default();
    for root in roots {
        report(&format!("Looking for empty folders on {}…", root.display()));
        let enter = |p: &Path| {
            let md = std::fs::symlink_metadata(p).ok();
            may_enter(p, md.as_ref(), opts)
        };
        let mut found = Vec::new();
        collect_empty(root, opts, &enter, &mut found);
        scan.dirs.extend(found.into_iter().filter(|d| !protected_dir(d)));
    }
    scan
}

pub fn zero_byte_files(roots: &[PathBuf], opts: &Options, report: &dyn Fn(&str)) -> RuleScan {
    let mut scan = RuleScan::default();
    for root in roots {
        report(&format!("Looking for zero-byte files on {}…", root.display()));
        let walker = WalkDir::new(root)
            .follow_links(false)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| {
                !e.file_type().is_dir() || may_enter(e.path(), e.metadata().ok().as_ref(), opts)
            });
        for e in walker.flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if md.len() != 0 || !allowed(e.path(), &md, opts) {
                continue;
            }
            let name = name_lower(e.path());
            let ext = e
                .path()
                .extension()
                .map(|x| x.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if name.starts_with('.')
                || KEEP_EMPTY_FILES.contains(&name.as_str())
                || KEEP_EMPTY_EXTENSIONS.contains(&ext.as_str())
            {
                continue;
            }
            scan.files.push((e.into_path(), 0));
        }
    }
    scan
}

/// Per-user places with shortcuts: desktop, Start Menu, Quick Launch.
pub fn user_shortcut_places() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        r"%APPDATA%\Microsoft\Windows\Start Menu",
        r"%APPDATA%\Microsoft\Internet Explorer\Quick Launch",
    ]
    .iter()
    .map(|p| PathBuf::from(expand_env(p)))
    .collect();
    if let Ok(d) = wm_core::shell::known_folder(wm_core::shell::KnownFolder::Desktop) {
        v.push(d);
    }
    v
}

pub fn broken_shortcuts(places: &[PathBuf], opts: &Options) -> RuleScan {
    let mut scan = RuleScan::default();
    for place in places {
        let walker = WalkDir::new(place)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !opts.skips(e.path()));
        for e in walker.flatten() {
            let p = e.path();
            if e.file_type().is_file()
                && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"))
                && is_broken_shortcut(p)
                && let Ok(md) = e.metadata()
                && allowed(p, &md, opts)
            {
                scan.files.push((p.to_owned(), md.len()));
            }
        }
    }
    scan
}

/// All-users Start Menu: empty folders under both Start Menus' Programs, and broken
/// shortcuts in the shared Start Menu and on the shared desktop.
pub fn invalid_start_menu(opts: &Options) -> RuleScan {
    let mut shared: Vec<PathBuf> = vec![PathBuf::from(expand_env(
        r"%ProgramData%\Microsoft\Windows\Start Menu",
    ))];
    if let Ok(d) = wm_core::shell::known_folder(wm_core::shell::KnownFolder::PublicDesktop) {
        shared.push(d);
    }
    let mut scan = broken_shortcuts(&shared, opts);
    for programs in [
        r"%APPDATA%\Microsoft\Windows\Start Menu\Programs",
        r"%ProgramData%\Microsoft\Windows\Start Menu\Programs",
    ] {
        let root = PathBuf::from(expand_env(programs));
        let mut found = Vec::new();
        collect_empty(&root, opts, &|_| true, &mut found);
        // The Programs folder itself and Windows' own folders stay.
        scan.dirs.extend(found.into_iter().filter(|d| {
            d != &root
                && !matches!(
                    name_lower(d).as_str(),
                    "startup" | "administrative tools" | "maintenance"
                )
        }));
    }
    scan
}

/// Program Files folders (below the top level) that hold nothing at all.
pub fn empty_program_files(opts: &Options) -> RuleScan {
    let mut scan = RuleScan::default();
    for root in crate::uninstaller::orphans::program_files_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !md.is_dir()
                || md.file_type().is_symlink()
                || crate::uninstaller::orphans::windows_owned(&name_lower(&path))
            {
                continue;
            }
            let mut found = Vec::new();
            if collect_empty(&path, opts, &|_| true, &mut found) {
                scan.dirs.extend(found);
            }
        }
    }
    scan
}

/// Every `LocalPackage` the Windows Installer knows (products and patches), lowercased.
/// `None` when the list cannot be read: then nothing may be treated as orphaned.
fn installer_packages() -> Option<HashSet<String>> {
    let v = powershell_json(
        "$i = New-Object -ComObject WindowsInstaller.Installer\n\
         $p = @(foreach ($c in $i.ProductsEx('', '', 7)) { try { $c.InstallProperty('LocalPackage') } catch {} })\n\
         $p += @(foreach ($c in $i.PatchesEx('', '', 7, 15)) { try { $c.PatchProperty('LocalPackage') } catch {} })\n\
         @($p | Where-Object { $_ }) | ConvertTo-Json -Compress",
    )
    .ok()?;
    let set: HashSet<String> = json_list(v)
        .iter()
        .filter_map(|s| s.as_str().map(str::to_lowercase))
        .collect();
    // A PC with installed MSI products always has packages; an empty list means the
    // query failed in a way that must not turn every package into "junk".
    (!set.is_empty()).then_some(set)
}

pub fn orphans_in(dir: &Path, known: &HashSet<String>, opts: &Options) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let ext = p.extension()?.to_string_lossy().to_lowercase();
            let md = e.metadata().ok()?;
            (md.is_file()
                && (ext == "msi" || ext == "msp")
                && !known.contains(&p.display().to_string().to_lowercase())
                && !opts.skips(&p))
            .then_some((p.clone(), md.len()))
        })
        .collect()
}

pub fn installer_orphans(opts: &Options) -> RuleScan {
    let Some(known) = installer_packages() else {
        tracing::warn!("Windows Installer package list unavailable; skipping orphan scan");
        return RuleScan::default();
    };
    RuleScan {
        files: orphans_in(&PathBuf::from(expand_env(r"%WINDIR%\Installer")), &known, opts),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_folder_rules() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for p in [
            "a/b/c",
            "a/keep",
            "docs/x",
            "node_modules/pkg",
            ".git/refs",
            "skipme/e",
            "App/plugins",
            "Games/Steam/lib",
        ] {
            std::fs::create_dir_all(r.join(p)).unwrap();
        }
        std::fs::write(r.join("a/keep/file.txt"), b"x").unwrap();
        std::fs::write(r.join("App/app.dll"), b"MZ").unwrap();
        let opts = Options {
            skip: vec![r.join("skipme").display().to_string()],
            ..Default::default()
        };
        let s = empty_folders(&[r.to_owned()], &opts, &|_| {});
        let names: Vec<String> = s
            .dirs
            .iter()
            .map(|p| p.strip_prefix(r).unwrap().display().to_string())
            .collect();
        assert!(names.contains(&r"a\b\c".to_owned()) && names.contains(&r"a\b".to_owned()));
        assert!(names.contains(&r"docs\x".to_owned()) && names.contains(&"docs".to_owned()));
        assert!(!names.iter().any(|n| n == "a"), "a holds a file");
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("node_modules") || n.starts_with(".git"))
        );
        assert!(!names.iter().any(|n| n.starts_with("skipme")));
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("App") || n.starts_with("Games")),
            "{names:?}"
        );
        let pos = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert!(pos(r"a\b\c") < pos(r"a\b"), "deepest first");
    }

    #[test]
    fn protected_folders() {
        assert!(protected_dir(Path::new(r"C:\Games")));
        assert!(protected_dir(Path::new(r"C:\Users\me")));
        assert!(protected_dir(Path::new(r"C:\Users\me\Music")));
        assert!(!protected_dir(Path::new(r"C:\Users\me\Music\Old")));
        assert!(!protected_dir(Path::new(r"D:\Data\Old")));
    }

    #[test]
    fn zero_byte_rules() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("pkg")).unwrap();
        std::fs::create_dir_all(r.join("AppData")).unwrap();
        std::fs::create_dir_all(r.join("Tool/data")).unwrap();
        std::fs::create_dir_all(r.join("Steam/x")).unwrap();
        std::fs::write(r.join("Tool/tool.exe"), b"MZ").unwrap();
        for f in [
            "empty.txt",
            "pkg/__init__.py",
            "app.lock",
            "AppData/x.txt",
            ".residency",
            "Tool/data/a.fp",
            "Steam/x/b.fp",
        ] {
            std::fs::write(r.join(f), b"").unwrap();
        }
        std::fs::write(r.join("full.txt"), b"data").unwrap();
        let s = zero_byte_files(&[r.to_owned()], &Options::default(), &|_| {});
        assert_eq!(s.files.len(), 1, "{:?}", s.files);
        assert!(s.files[0].0.ends_with("empty.txt"));
    }

    #[test]
    fn installer_orphans_only_unknown_packages() {
        let d = tempfile::tempdir().unwrap();
        for f in ["used.msi", "gone.msp", "notes.txt"] {
            std::fs::write(d.path().join(f), b"x").unwrap();
        }
        let known: HashSet<String> = [d.path().join("used.msi").display().to_string().to_lowercase()].into();
        let found = orphans_in(d.path(), &known, &Options::default());
        assert_eq!(found.len(), 1);
        assert!(found[0].0.ends_with("gone.msp"));
    }

    #[test]
    fn real_shortcut_and_start_menu_scans() {
        let opts = Options::default();
        let s = broken_shortcuts(&user_shortcut_places(), &opts);
        assert!(
            s.files
                .iter()
                .all(|(p, _)| p.extension().is_some_and(|e| e == "lnk"))
        );
        let sm = invalid_start_menu(&opts);
        assert!(sm.dirs.iter().all(|d| !d.ends_with("Programs")));
    }
}
