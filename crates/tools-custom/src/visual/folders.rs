//! Folder colorizer: gives folders a coloured (or any other) icon through their
//! `desktop.ini`, the way Explorer's own "Change Icon" does. Coloured folder icons are
//! drawn by this app and saved as `.ico` files in its data folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    FILE_FLAGS_AND_ATTRIBUTES, GetFileAttributesW, SetFileAttributesW,
};
use windows::Win32::UI::Shell::{SHCNE_UPDATEDIR, SHCNE_UPDATEITEM, SHCNF_PATHW, SHChangeNotify};
use windows::core::HSTRING;
use wm_core::{AppCtx, paths, safety::UndoAction};

/// (name, colour)
pub const PALETTE: [(&str, [u8; 3]); 12] = [
    ("Red", [220, 60, 60]),
    ("Orange", [240, 140, 40]),
    ("Yellow", [240, 200, 50]),
    ("Lime", [140, 200, 60]),
    ("Green", [50, 160, 90]),
    ("Teal", [30, 160, 160]),
    ("Blue", [50, 120, 220]),
    ("Indigo", [90, 90, 200]),
    ("Purple", [150, 80, 200]),
    ("Pink", [230, 100, 170]),
    ("Brown", [150, 100, 60]),
    ("Grey", [130, 130, 140]),
];

fn shade(c: [u8; 3], f: f32) -> [u8; 3] {
    c.map(|v| (v as f32 * f).clamp(0.0, 255.0) as u8)
}

/// Coverage of a rounded rectangle at (x, y), in unit coordinates.
fn in_round_rect(x: f32, y: f32, (x0, y0, x1, y1): (f32, f32, f32, f32), r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// Draws a folder in `color` at `size` pixels (4×4 supersampled).
pub fn render_folder(size: u32, color: [u8; 3]) -> RgbaImage {
    const SS: u32 = 4;
    let back = shade(color, 0.78);
    let mut img = RgbaImage::new(size, size);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let c = if in_round_rect(x, y, (0.06, 0.38, 0.94, 0.86), 0.06) {
                        // Front, lighter at the top.
                        Some(shade(color, 1.12 - (y - 0.38) * 0.35))
                    } else if in_round_rect(x, y, (0.06, 0.16, 0.44, 0.34), 0.05)
                        || in_round_rect(x, y, (0.06, 0.24, 0.94, 0.86), 0.06)
                    {
                        Some(back)
                    } else {
                        None
                    };
                    if let Some(c) = c {
                        r += c[0] as f32;
                        g += c[1] as f32;
                        b += c[2] as f32;
                        a += 1.0;
                    }
                }
            }
            if a > 0.0 {
                let n = (SS * SS) as f32;
                img.put_pixel(
                    px,
                    py,
                    Rgba([(r / a) as u8, (g / a) as u8, (b / a) as u8, (a / n * 255.0) as u8]),
                );
            }
        }
    }
    img
}

fn icons_dir() -> Result<PathBuf> {
    let d = paths::data_dir()?.join("folder-icons");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// The `.ico` file for a colour, created on first use.
pub fn color_icon(color: [u8; 3]) -> Result<PathBuf> {
    let path = icons_dir()?.join(format!(
        "folder-{:02x}{:02x}{:02x}.ico",
        color[0], color[1], color[2]
    ));
    if !path.exists() {
        let frames: Vec<image::codecs::ico::IcoFrame> = [16u32, 24, 32, 48, 64, 256]
            .iter()
            .map(|&s| {
                let img = render_folder(s, color);
                image::codecs::ico::IcoFrame::as_png(img.as_raw(), s, s, image::ExtendedColorType::Rgba8)
            })
            .collect::<Result<_, _>>()?;
        let file = std::fs::File::create(&path)?;
        image::codecs::ico::IcoEncoder::new(file).encode_images(&frames)?;
    }
    Ok(path)
}

// ---------------------------------------------------------------------------------
// desktop.ini

fn read_ini(path: &Path) -> Option<String> {
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
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes)).into_owned()
    })
}

/// `text` with the icon lines of `[.ShellClassInfo]` replaced by `IconResource=icon`
/// (or removed when `icon` is `None`). Other sections and lines are kept.
pub fn with_icon(text: &str, icon: Option<&str>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut found = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_section && !found {
                if let Some(i) = icon {
                    out.push(format!("IconResource={i}"));
                }
                found = true;
            }
            in_section = t.eq_ignore_ascii_case("[.ShellClassInfo]");
            out.push(line.to_owned());
            continue;
        }
        if in_section {
            let key = t.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
            if matches!(key.as_str(), "iconresource" | "iconfile" | "iconindex") {
                if !found && let Some(i) = icon {
                    out.push(format!("IconResource={i}"));
                }
                found = true;
                continue;
            }
        }
        out.push(line.to_owned());
    }
    if !found && let Some(i) = icon {
        if !in_section {
            out.push("[.ShellClassInfo]".to_owned());
        }
        out.push(format!("IconResource={i}"));
    }
    out.join("\r\n") + "\r\n"
}

/// True when only an empty `[.ShellClassInfo]` section (or nothing) is left.
fn is_empty_ini(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .all(|l| l.is_empty() || l.eq_ignore_ascii_case("[.ShellClassInfo]"))
}

/// The icon a folder's desktop.ini sets, if any.
pub fn folder_icon(dir: &Path) -> Option<String> {
    let text = read_ini(&dir.join("desktop.ini"))?;
    let mut in_section = false;
    let mut file = None;
    let mut index = 0;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t.eq_ignore_ascii_case("[.ShellClassInfo]");
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = t.split_once('=') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "iconresource" => return Some(v.trim().to_owned()),
            "iconfile" => file = Some(v.trim().to_owned()),
            "iconindex" => index = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    file.map(|f| format!("{f},{index}"))
}

fn attributes(path: &Path) -> u32 {
    unsafe { GetFileAttributesW(&HSTRING::from(path.as_os_str())) }
}

fn set_attributes(path: &Path, attrs: u32) {
    let _ = unsafe { SetFileAttributesW(&HSTRING::from(path.as_os_str()), FILE_FLAGS_AND_ATTRIBUTES(attrs)) };
}

fn write_ini(path: &Path, text: &str) -> Result<()> {
    if path.exists() {
        set_attributes(path, FILE_ATTRIBUTE_NORMAL.0);
    }
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    set_attributes(path, FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0);
    Ok(())
}

fn notify(dir: &Path) {
    let w = HSTRING::from(dir.as_os_str());
    unsafe {
        SHChangeNotify(SHCNE_UPDATEITEM, SHCNF_PATHW, Some(w.as_ptr().cast()), None);
        SHChangeNotify(SHCNE_UPDATEDIR, SHCNF_PATHW, Some(w.as_ptr().cast()), None);
    }
}

/// Sets (or with `None`, removes) a folder's icon. Journaled: the undo puts the
/// previous desktop.ini back, or removes the one this created.
pub fn set_folder_icon(ctx: &AppCtx<'_>, dir: &Path, icon: Option<&str>) -> Result<()> {
    let ini = dir.join("desktop.ini");
    let before = read_ini(&ini);
    let undo = match &before {
        Some(_) => {
            let backup = paths::backups_dir()?.join(format!(
                "desktop-ini-{}.ini",
                chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
            ));
            std::fs::copy(&ini, &backup).with_context(|| format!("saving {}", ini.display()))?;
            UndoAction::RestoreFile {
                original: ini.clone(),
                backup,
            }
        }
        None if icon.is_none() => return Ok(()),
        None => UndoAction::DeleteCreatedFiles {
            paths: vec![ini.clone()],
        },
    };
    let text = with_icon(before.as_deref().unwrap_or(""), icon);
    if icon.is_none() && is_empty_ini(&text) {
        set_attributes(&ini, FILE_ATTRIBUTE_NORMAL.0);
        std::fs::remove_file(&ini).with_context(|| format!("removing {}", ini.display()))?;
    } else {
        write_ini(&ini, &text)?;
        // Explorer only reads desktop.ini in read-only or system folders.
        let a = attributes(dir);
        if a & (FILE_ATTRIBUTE_READONLY.0 | FILE_ATTRIBUTE_SYSTEM.0) == 0 {
            set_attributes(dir, a | FILE_ATTRIBUTE_READONLY.0);
        }
    }
    let what = match icon {
        Some(_) => format!("Changed the icon of {}", dir.display()),
        None => format!("Restored the icon of {}", dir.display()),
    };
    ctx.journal.record("visual", what, undo)?;
    notify(dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_edits_keep_other_settings() {
        let original = "[.ShellClassInfo]\r\nLocalizedResourceName=@shell32.dll,-21770\r\nIconFile=x.ico\r\nIconIndex=2\r\n[ViewState]\r\nMode=\r\n";
        let set = with_icon(original, Some(r"C:\i\red.ico,0"));
        assert!(set.contains(r"IconResource=C:\i\red.ico,0"));
        assert!(!set.contains("IconFile") && !set.contains("IconIndex"));
        assert!(set.contains("LocalizedResourceName") && set.contains("[ViewState]"));
        assert_eq!(set.matches("IconResource").count(), 1);
        let cleared = with_icon(&set, None);
        assert!(!cleared.contains("IconResource") && cleared.contains("LocalizedResourceName"));
        assert_eq!(
            with_icon("", Some("a.ico")),
            "[.ShellClassInfo]\r\nIconResource=a.ico\r\n"
        );
        assert!(is_empty_ini(&with_icon(
            "[.ShellClassInfo]\r\nIconResource=a\r\n",
            None
        )));
        // The section is found even when another section comes first.
        let other_first = with_icon("[ViewState]\r\nMode=\r\n", Some("b.ico"));
        assert!(other_first.ends_with("[.ShellClassInfo]\r\nIconResource=b.ico\r\n"));
    }

    #[test]
    fn folder_icon_round_trip_with_undo() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Projects");
        std::fs::create_dir(&folder).unwrap();
        let jdir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&jdir.path().join("j.json"), &jdir.path().join("b"))
                .unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        let icon = color_icon(PALETTE[0].1).unwrap();
        assert!(std::fs::metadata(&icon).unwrap().len() > 1000);
        let spec = format!("{},0", icon.display());
        set_folder_icon(&ctx, &folder, Some(&spec)).unwrap();
        assert_eq!(folder_icon(&folder).as_deref(), Some(spec.as_str()));
        set_folder_icon(&ctx, &folder, None).unwrap();
        assert!(folder_icon(&folder).is_none());
        assert!(
            !folder.join("desktop.ini").exists(),
            "an emptied desktop.ini is removed"
        );
        assert_eq!(journal.entries().len(), 2);
        set_attributes(&folder, FILE_ATTRIBUTE_NORMAL.0);
    }

    #[test]
    fn folder_drawing_is_coloured_and_transparent() {
        let img = render_folder(48, [200, 0, 0]);
        assert_eq!(img.get_pixel(0, 0)[3], 0, "corner is transparent");
        let mid = img.get_pixel(24, 30);
        assert_eq!(mid[3], 255);
        assert!(mid[0] > 150 && mid[1] < 30);
    }
}
