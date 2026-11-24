//! Program Files folders that nothing installed refers to: no uninstall entry
//! (including hidden and system ones), service, Run entry or App Path points into
//! them, and no installed program or publisher has a matching name.
//!
//! These are candidates for review only; the user decides what goes to the Recycle Bin.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use wm_core::{paths::expand_env, registry};

use super::{UNINSTALL_KEYS, reg_string};

/// Folders that belong to Windows or are shared by many programs.
const WINDOWS_OWNED: &[&str] = &[
    "common files",
    "internet explorer",
    "modifiablewindowsapps",
    "msbuild",
    "reference assemblies",
    "uninstall information",
    "windowsapps",
    "windowspowershell",
    "powershell",
    "dotnet",
    "packages",
    "packagemanagement",
    "installshield installation information",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
    /// Holds .exe files: possibly a portable program that is still used.
    pub has_programs: bool,
}

fn alnum(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Everything installed that may point into Program Files, lowercased.
#[derive(Default)]
pub struct References {
    paths: Vec<String>,
    names: HashSet<String>,
}

impl References {
    pub fn collect() -> Self {
        let mut r = References::default();
        for base in UNINSTALL_KEYS {
            let Ok(root) = registry::open_read(base) else {
                continue;
            };
            for sub in root.enum_keys().flatten() {
                let Ok(k) = root.open_subkey(&sub) else { continue };
                for v in [
                    "InstallLocation",
                    "UninstallString",
                    "QuietUninstallString",
                    "DisplayIcon",
                    "InstallSource",
                ] {
                    if let Some(s) = reg_string(&k, v) {
                        r.paths.push(expand_env(&s).to_lowercase());
                    }
                }
                for v in ["DisplayName", "Publisher"] {
                    if let Some(s) = reg_string(&k, v) {
                        r.add_name(&s);
                    }
                }
                r.add_name(&sub);
            }
        }
        if let Ok(services) = registry::open_read(r"HKLM\SYSTEM\CurrentControlSet\Services") {
            for sub in services.enum_keys().flatten() {
                let Ok(k) = services.open_subkey(&sub) else {
                    continue;
                };
                if let Some(s) = reg_string(&k, "ImagePath") {
                    r.paths.push(expand_env(&s).to_lowercase());
                }
                if let Ok(p) = k.open_subkey("Parameters")
                    && let Some(s) = reg_string(&p, "ServiceDll")
                {
                    r.paths.push(expand_env(&s).to_lowercase());
                }
            }
        }
        for key in [
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
        ] {
            if let Ok(k) = registry::open_read(key) {
                for (name, _) in k.enum_values().flatten() {
                    if let Some(s) = reg_string(&k, &name) {
                        r.paths.push(expand_env(&s).to_lowercase());
                    }
                }
            }
        }
        if let Ok(app_paths) =
            registry::open_read(r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths")
        {
            for sub in app_paths.enum_keys().flatten() {
                if let Ok(k) = app_paths.open_subkey(&sub)
                    && let Some(s) = reg_string(&k, "")
                {
                    r.paths.push(expand_env(&s).to_lowercase());
                }
            }
        }
        r
    }

    fn add_name(&mut self, s: &str) {
        let n = alnum(s);
        if n.len() >= 4 {
            self.names.insert(n);
        }
    }

    /// True if something installed refers to `folder` by path or by name.
    pub fn refers_to(&self, folder: &Path) -> bool {
        let p = folder.to_string_lossy().to_lowercase();
        let prefix = format!("{}\\", p.trim_end_matches('\\'));
        if self
            .paths
            .iter()
            .any(|s| s.contains(&prefix) || s.trim_matches('"').trim_end_matches('\\') == p)
        {
            return true;
        }
        let name = alnum(&folder.file_name().unwrap_or_default().to_string_lossy());
        if name.len() < 4 {
            return true; // too short to judge: keep it out of the list
        }
        self.names
            .iter()
            .any(|n| n.contains(&name) || name.contains(n.as_str()))
    }
}

fn windows_owned(name: &str) -> bool {
    let lower = name.to_lowercase();
    WINDOWS_OWNED.contains(&lower.as_str()) || lower.starts_with("microsoft") || lower.starts_with("windows")
}

fn folder_size(path: &Path) -> (u64, u64, bool) {
    let (mut size, mut files, mut exe) = (0, 0, false);
    for e in walkdir::WalkDir::new(path).into_iter().flatten() {
        if e.file_type().is_file() {
            files += 1;
            size += e.metadata().map(|m| m.len()).unwrap_or(0);
            exe |= e
                .path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("exe"));
        }
    }
    (size, files, exe)
}

pub fn program_files_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["%ProgramFiles%", "%ProgramFiles(x86)%"]
        .iter()
        .map(|v| PathBuf::from(expand_env(v)))
        .filter(|p| p.is_dir())
        .collect();
    roots.dedup();
    roots
}

/// Candidate folders directly under `roots`.
pub fn find(roots: &[PathBuf], refs: &References) -> Vec<Orphan> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() || ft.is_symlink() {
                continue;
            }
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if windows_owned(&name) || refs.refers_to(&path) {
                continue;
            }
            let (size, files, has_programs) = folder_size(&path);
            out.push(Orphan {
                path,
                size,
                files,
                has_programs,
            });
        }
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.size));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_by_path_and_name() {
        let mut refs = References::default();
        refs.paths
            .push(r#""c:\program files\acme tools\bin\run.exe" /x"#.into());
        refs.add_name("Contoso Paint 3.1");
        assert!(refs.refers_to(Path::new(r"C:\Program Files\Acme Tools")));
        assert!(
            !refs.refers_to(Path::new(r"C:\Program Files\Acme")),
            "prefix only matches whole folder names"
        );
        assert!(refs.refers_to(Path::new(r"C:\Program Files\Contoso Paint")));
        assert!(!refs.refers_to(Path::new(r"C:\Program Files\Fabrikam Studio")));
        assert!(
            refs.refers_to(Path::new(r"C:\Program Files\Abc")),
            "short names are never flagged"
        );
    }

    #[test]
    fn finds_unreferenced_folders() {
        let root = tempfile::tempdir().unwrap();
        for d in [
            "Fabrikam Studio",
            "Contoso Paint",
            "Windows Defender",
            "Common Files",
        ] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }
        std::fs::write(root.path().join("Fabrikam Studio/a.bin"), [0u8; 10]).unwrap();
        let mut refs = References::default();
        refs.add_name("Contoso Paint");
        let found = find(&[root.path().to_owned()], &refs);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].path.ends_with("Fabrikam Studio"));
        assert_eq!(
            (found[0].size, found[0].files, found[0].has_programs),
            (10, 1, false)
        );
    }

    #[test]
    fn real_references_cover_installed_programs() {
        let refs = References::collect();
        assert!(!refs.paths.is_empty() && !refs.names.is_empty());
    }
}
