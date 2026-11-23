//! Per-user directories (config, data, backups, logs).

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "win-manager-rs").context("could not determine user profile directories")
}

fn ensure(dir: PathBuf) -> Result<PathBuf> {
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `%APPDATA%\win-manager-rs\config`
pub fn config_dir() -> Result<PathBuf> {
    ensure(project_dirs()?.config_dir().to_path_buf())
}

/// `%APPDATA%\win-manager-rs\data`
pub fn data_dir() -> Result<PathBuf> {
    ensure(project_dirs()?.data_dir().to_path_buf())
}

/// Registry exports and other undo material.
pub fn backups_dir() -> Result<PathBuf> {
    ensure(data_dir()?.join("backups"))
}

pub fn logs_dir() -> Result<PathBuf> {
    ensure(data_dir()?.join("logs"))
}

/// Expands `%VAR%` references using the process environment (unknown variables are kept).
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Case-insensitive wildcard match supporting `*` (any run) and `?` (one char).
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Expands env vars, then wildcard segments (`*`, `?`) against the file system.
/// `C:\Users\x\AppData\Local\Google\Chrome\User Data\*\Cache` → every existing match.
pub fn expand_pattern(pattern: &str) -> Vec<PathBuf> {
    let expanded = expand_env(pattern);
    let mut parts = expanded.split('\\').filter(|s| !s.is_empty());
    let Some(first) = parts.next() else {
        return Vec::new();
    };
    // Keep the drive root ("C:" → "C:\").
    let mut current = vec![PathBuf::from(if first.ends_with(':') {
        format!("{first}\\")
    } else {
        first.to_owned()
    })];
    for part in parts {
        let mut next = Vec::new();
        for base in &current {
            if part.contains(['*', '?']) {
                if let Ok(entries) = std::fs::read_dir(base) {
                    for e in entries.flatten() {
                        if wildcard_match(part, &e.file_name().to_string_lossy()) {
                            next.push(e.path());
                        }
                    }
                }
            } else {
                let p = base.join(part);
                if p.exists() {
                    next.push(p);
                }
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    current.retain(|p| p.exists());
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_expansion() {
        let windir = std::env::var("WINDIR").unwrap();
        assert_eq!(expand_env(r"%WINDIR%\Temp"), format!(r"{windir}\Temp"));
        assert_eq!(expand_env("%NO_SUCH_VAR_X%"), "%NO_SUCH_VAR_X%");
        assert_eq!(expand_env("100%"), "100%");
        assert_eq!(expand_env("a %% b"), "a %% b");
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("thumbcache_*.db", "thumbcache_256.db"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("Profile ?", "profile 2"));
        assert!(!wildcard_match("*.db", "x.dbx"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn pattern_expansion() {
        let dir = tempfile::tempdir().unwrap();
        for p in ["one/Cache", "two/Cache", "three/Other"] {
            std::fs::create_dir_all(dir.path().join(p)).unwrap();
        }
        let pattern = format!(r"{}\*\Cache", dir.path().display());
        let mut found = expand_pattern(&pattern);
        found.sort();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.ends_with("Cache")));
        assert!(expand_pattern(&format!(r"{}\missing\*", dir.path().display())).is_empty());
    }
}
