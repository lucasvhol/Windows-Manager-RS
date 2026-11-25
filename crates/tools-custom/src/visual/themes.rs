//! Theme changer: the installed `.theme` files, with their wallpaper, and switching
//! between them (Windows applies a theme when its file is opened).

use std::path::{Path, PathBuf};

use anyhow::Result;
use wm_core::{AppCtx, paths, registry, safety::UndoAction};

#[derive(Debug, Clone)]
pub struct Theme {
    pub path: PathBuf,
    pub name: String,
    pub wallpaper: Option<PathBuf>,
}

fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(if bytes.starts_with(&[0xFF, 0xFE]) {
        let wide: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&wide)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// `(section, key)` → value from an INI text (case-insensitive).
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t.trim_matches(['[', ']']).eq_ignore_ascii_case(section);
            continue;
        }
        if in_section
            && let Some((k, v)) = t.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

pub fn parse(path: &Path, text: &str) -> Theme {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = ini_value(text, "Theme", "DisplayName")
        .map(|n| crate::navpane::resolve(&n))
        .filter(|n| !n.is_empty() && !n.starts_with('@'))
        .unwrap_or(stem);
    let wallpaper = ini_value(text, r"Control Panel\Desktop", "Wallpaper")
        .filter(|w| !w.is_empty())
        .map(|w| PathBuf::from(paths::expand_env(&w)))
        .filter(|p| p.is_file());
    Theme {
        path: path.to_owned(),
        name,
        wallpaper,
    }
}

/// Installed themes: Windows' own, then the account's (saved and downloaded).
pub fn list() -> Vec<Theme> {
    let mut out = Vec::new();
    for dir in [
        r"%SystemRoot%\Resources\Themes",
        r"%LOCALAPPDATA%\Microsoft\Windows\Themes",
    ] {
        let root = PathBuf::from(paths::expand_env(dir));
        for entry in walkdir::WalkDir::new(&root).max_depth(2).into_iter().flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("theme"))
                && let Some(text) = read_text(p)
            {
                out.push(parse(p, &text));
            }
        }
    }
    out.sort_by_key(|t| t.name.to_lowercase());
    out.dedup_by(|a, b| a.path == b.path);
    out
}

const THEMES_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes";

pub fn current() -> Option<PathBuf> {
    registry::read_raw(THEMES_KEY, "CurrentTheme")
        .map(|v| registry::decode_string(&v))
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(paths::expand_env(&s)))
}

/// Applies a theme. Windows opens the Personalization page while it does so.
pub fn apply(ctx: &AppCtx<'_>, theme: &Theme) -> Result<()> {
    let previous = current();
    wm_core::shell::shell_open(&theme.path.to_string_lossy(), "")?;
    let undo = match previous {
        Some(p) => UndoAction::RunCommand {
            program: "cmd".into(),
            args: vec![
                "/c".into(),
                "start".into(),
                String::new(),
                p.to_string_lossy().into_owned(),
            ],
        },
        None => UndoAction::None,
    };
    ctx.journal
        .record("visual", format!("Applied the theme {}", theme.name), undo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_files_parse() {
        let text = "[Theme]\r\nDisplayName=Sunrise\r\n\r\n[Control Panel\\Desktop]\r\nWallpaper=%SystemRoot%\\no-such.jpg\r\n";
        let t = parse(Path::new(r"C:\t\sunrise.theme"), text);
        assert_eq!(t.name, "Sunrise");
        assert!(t.wallpaper.is_none(), "a missing wallpaper file is ignored");
        let unnamed = parse(Path::new(r"C:\t\custom.theme"), "[Theme]\r\n");
        assert_eq!(unnamed.name, "custom");
        let installed = list();
        assert!(!installed.is_empty(), "Windows ships themes");
        assert!(installed.iter().all(|t| !t.name.is_empty()));
    }
}
