//! Pictures for the desktop: the lock-screen (Windows Spotlight) images Windows has
//! downloaded, and Bing's pictures of the day. Both can be saved and set as wallpaper.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SystemParametersInfoW,
};
use windows::core::HSTRING;
use wm_core::{
    AppCtx, paths, registry,
    safety::UndoAction,
    shell::{KnownFolder, known_folder},
};

/// Width and height from a JPEG's frame header, or `None` when it is not a JPEG.
pub fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((w, h));
        }
        if marker == 0xD9 || len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

#[derive(Debug, Clone)]
pub struct Picture {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

impl Picture {
    pub fn landscape(&self) -> bool {
        self.width > self.height
    }
}

/// Folders where Windows keeps lock-screen and Spotlight pictures.
fn spotlight_dirs() -> Vec<PathBuf> {
    [
        r"%LOCALAPPDATA%\Packages\Microsoft.Windows.ContentDeliveryManager_cw5n1h2txyewy\LocalState\Assets",
        r"%LOCALAPPDATA%\Packages\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\LocalCache\Microsoft\IrisService",
    ]
    .iter()
    .map(|d| PathBuf::from(paths::expand_env(d)))
    .filter(|d| d.is_dir())
    .collect()
}

/// Lock-screen pictures (large JPEGs only; icons and small assets are skipped),
/// without duplicates.
pub fn spotlight() -> Vec<Picture> {
    let mut out: Vec<Picture> = Vec::new();
    for dir in spotlight_dirs() {
        for e in walkdir::WalkDir::new(dir).max_depth(4).into_iter().flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() < 100 * 1024 {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(e.path()) else {
                continue;
            };
            let mut head = vec![0u8; 64 * 1024];
            let n = std::io::Read::read(&mut f, &mut head).unwrap_or(0);
            head.truncate(n);
            let Some((width, height)) = jpeg_size(&head) else {
                continue;
            };
            if width.max(height) < 1000 {
                continue;
            }
            let dup = out
                .iter()
                .any(|p| p.bytes == meta.len() && p.width == width && p.height == height);
            if !dup {
                out.push(Picture {
                    path: e.into_path(),
                    width,
                    height,
                    bytes: meta.len(),
                });
            }
        }
    }
    out
}

pub fn default_folder(name: &str) -> PathBuf {
    known_folder(KnownFolder::Pictures)
        .unwrap_or_else(|_| PathBuf::from(paths::expand_env(r"%USERPROFILE%\Pictures")))
        .join(name)
}

/// Copies pictures to `dest` as `.jpg`; returns how many were new.
pub fn save_all(pics: &[Picture], dest: &Path) -> Result<usize> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut n = 0;
    for p in pics {
        let stem: String = p
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().chars().take(24).collect())
            .unwrap_or_default();
        let target = dest.join(format!("{stem}.jpg"));
        if !target.exists() {
            std::fs::copy(&p.path, &target).with_context(|| format!("copying to {}", target.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Sets the desktop wallpaper now (journaled; the undo applies at the next sign-in).
pub fn set_wallpaper(ctx: &AppCtx<'_>, path: &Path) -> Result<()> {
    let snap = registry::snapshot(r"HKCU\Control Panel\Desktop", "WallPaper");
    let w = HSTRING::from(path.as_os_str());
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(w.as_ptr() as *mut _),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
    }
    .context("setting the wallpaper")?;
    ctx.journal.record(
        "visual",
        format!(
            "Wallpaper set to {} (undo applies at the next sign-in)",
            path.display()
        ),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

/// A small RGBA preview of an image file or bytes.
pub fn thumbnail(bytes: &[u8], max: u32) -> Option<([usize; 2], Vec<u8>)> {
    let img = image::load_from_memory(bytes)
        .ok()?
        .thumbnail(max, max)
        .to_rgba8();
    Some(([img.width() as usize, img.height() as usize], img.into_raw()))
}

// ---------------------------------------------------------------------------------
// Bing picture of the day

#[derive(Debug, Clone)]
pub struct BingImage {
    pub title: String,
    pub copyright: String,
    /// `YYYYMMDD`
    pub date: String,
    urlbase: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum Resolution {
    #[default]
    Uhd,
    FullHd,
    Hd,
    /// Portrait, for phones and rotated screens.
    Portrait,
}

impl Resolution {
    pub const ALL: [Resolution; 3] = [Resolution::Uhd, Resolution::FullHd, Resolution::Hd];
    pub const WITH_PORTRAIT: [Resolution; 4] = [
        Resolution::Uhd,
        Resolution::FullHd,
        Resolution::Hd,
        Resolution::Portrait,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Resolution::Uhd => "Largest available (up to 4K)",
            Resolution::FullHd => "1920 × 1080",
            Resolution::Hd => "1366 × 768",
            Resolution::Portrait => "1080 × 1920 (portrait)",
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Resolution::Uhd => "_UHD.jpg",
            Resolution::FullHd => "_1920x1080.jpg",
            Resolution::Hd => "_1366x768.jpg",
            Resolution::Portrait => "_1080x1920.jpg",
        }
    }
}

/// Bing markets with their own picture of the day (code, name).
pub const MARKETS: [(&str, &str); 10] = [
    ("", "Automatic (your region)"),
    ("en-US", "United States"),
    ("en-GB", "United Kingdom"),
    ("de-DE", "Germany"),
    ("fr-FR", "France"),
    ("ja-JP", "Japan"),
    ("zh-CN", "China"),
    ("pt-BR", "Brazil"),
    ("en-IN", "India"),
    ("en-CA", "Canada"),
];

/// The last eight pictures of a market ("" = automatic).
pub fn bing_list_for(market: &str) -> Result<Vec<BingImage>> {
    let mkt = if MARKETS.iter().any(|(c, _)| *c == market && !c.is_empty()) {
        format!("&mkt={market}")
    } else {
        String::new()
    };
    parse_bing(&wm_core::net::get_text(&format!(
        "{BING}/HPImageArchive.aspx?format=js&idx=0&n=8{mkt}"
    ))?)
}

/// Writes `text` on a dark stripe along the bottom of a JPEG (in place).
pub fn caption(path: &Path, text: &str) -> Result<()> {
    use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
    let mut img = image::open(path).context("opening the picture")?.to_rgb8();
    let (w, h) = img.dimensions();
    let size = (h as f32 / 40.0).clamp(14.0, 48.0);
    let pad = (size * 0.6) as u32;
    let band = size as u32 + pad * 2;
    for y in h.saturating_sub(band)..h {
        for x in 0..w {
            let p = img.get_pixel_mut(x, y);
            for c in p.0.iter_mut() {
                *c = (*c as f32 * 0.45) as u8;
            }
        }
    }
    let font = FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).context("loading the font")?;
    let scaled = font.as_scaled(PxScale::from(size));
    let mut x = pad as f32;
    let baseline = (h - pad) as f32 - scaled.descent().abs();
    let mut prev = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            x += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(x, baseline));
        x += scaled.h_advance(id);
        prev = Some(id);
        if x > w as f32 - pad as f32 {
            break;
        }
        if let Some(outline) = font.outline_glyph(glyph) {
            let b = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                    let p = img.get_pixel_mut(px as u32, py as u32);
                    for c in p.0.iter_mut() {
                        *c = (*c as f32 * (1.0 - cov) + 255.0 * cov) as u8;
                    }
                }
            });
        }
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92).encode_image(&img)?;
    Ok(())
}

const BING: &str = "https://www.bing.com";

pub fn parse_bing(json: &str) -> Result<Vec<BingImage>> {
    let v: serde_json::Value = serde_json::from_str(json).context("reading Bing's answer")?;
    let Some(list) = v["images"].as_array() else {
        bail!("Bing's answer has no pictures");
    };
    Ok(list
        .iter()
        .filter_map(|i| {
            let urlbase = i["urlbase"].as_str()?.to_owned();
            // Only Bing's own paths, never a full URL elsewhere.
            if !urlbase.starts_with('/') || urlbase.starts_with("//") {
                return None;
            }
            Some(BingImage {
                title: i["title"].as_str().unwrap_or_default().to_owned(),
                copyright: i["copyright"].as_str().unwrap_or_default().to_owned(),
                date: i["startdate"].as_str().unwrap_or_default().to_owned(),
                urlbase,
            })
        })
        .collect())
}

/// The last eight pictures of the day (asks bing.com; started by the user).
pub fn bing_list() -> Result<Vec<BingImage>> {
    bing_list_for("")
}

impl BingImage {
    pub fn preview(&self) -> Result<Vec<u8>> {
        wm_core::net::get_bytes(&format!("{BING}{}_400x240.jpg", self.urlbase), 2 << 20)
    }

    fn file_name(&self) -> String {
        let id: String = self
            .urlbase
            .rsplit("OHR.")
            .next()
            .unwrap_or("bing")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(60)
            .collect();
        format!("{}-{id}.jpg", self.date)
    }

    pub fn file_name_for(&self, res: Resolution) -> String {
        let name = self.file_name();
        if res == Resolution::Portrait {
            name.replace(".jpg", "-portrait.jpg")
        } else {
            name
        }
    }

    /// Downloads the picture into `dest` (kept if already there).
    pub fn save(&self, res: Resolution, dest: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dest)?;
        let target = dest.join(self.file_name_for(res));
        if !target.exists() {
            let bytes =
                wm_core::net::get_bytes(&format!("{BING}{}{}", self.urlbase, res.suffix()), 40 << 20)?;
            if jpeg_size(&bytes).is_none() {
                bail!("Bing sent something that is not a picture");
            }
            std::fs::write(&target, bytes)?;
        }
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_header_sizes() {
        // SOI, APP0 (len 4), SOF0 with height 1080 and width 1920.
        let jpg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x04, 0x38, 0x07,
            0x80, 0x03, 0x00,
        ];
        assert_eq!(jpeg_size(&jpg), Some((1920, 1080)));
        assert_eq!(jpeg_size(b"\x89PNG...."), None);
        let p = Picture {
            path: PathBuf::new(),
            width: 1080,
            height: 1920,
            bytes: 1,
        };
        assert!(!p.landscape());
    }

    #[test]
    fn bing_answers_are_checked() {
        let json = r#"{"images":[
            {"startdate":"20261125","urlbase":"/th?id=OHR.Owls_EN-US123","title":"Owls","copyright":"(c) someone"},
            {"startdate":"20261124","urlbase":"https://evil.example/x","title":"Bad"}
        ]}"#;
        let list = parse_bing(json).unwrap();
        assert_eq!(list.len(), 1, "only Bing's own paths are kept");
        assert_eq!(list[0].file_name(), "20261125-Owls_EN-US123.jpg");
        assert!(parse_bing("{}").is_err());
    }

    #[test]
    fn spotlight_scan_reads_only() {
        for p in spotlight() {
            assert!(p.width.max(p.height) >= 1000);
        }
    }
}
