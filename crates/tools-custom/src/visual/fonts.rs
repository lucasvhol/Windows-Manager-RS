//! Fonts and sizes of classic window parts (title bars, menus, message boxes, status
//! bars, icon labels), the text size setting, and replacing the system font.
//!
//! Window parts are changed through Windows' own system parameters, which apply at
//! once and are saved in `HKCU\Control Panel\Desktop\WindowMetrics`. Modern apps draw
//! their own title bars and ignore most of these.

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Graphics::Gdi::{
    DEFAULT_CHARSET, EnumFontFamiliesExW, GetDC, LOGFONTW, ReleaseDC, TEXTMETRICW,
};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    NONCLIENTMETRICSW, SPI_GETICONTITLELOGFONT, SPI_GETNONCLIENTMETRICS, SPI_SETICONTITLELOGFONT,
    SPI_SETNONCLIENTMETRICS, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW,
};
use wm_core::{
    AppCtx,
    registry::{self, ValueSnapshot},
    safety::UndoAction,
};

pub const METRICS_KEY: &str = r"HKCU\Control Panel\Desktop\WindowMetrics";
const METRIC_VALUES: [&str; 17] = [
    "BorderWidth",
    "PaddedBorderWidth",
    "ScrollWidth",
    "ScrollHeight",
    "CaptionWidth",
    "CaptionHeight",
    "SmCaptionWidth",
    "SmCaptionHeight",
    "MenuWidth",
    "MenuHeight",
    "CaptionFont",
    "SmCaptionFont",
    "MenuFont",
    "StatusFont",
    "MessageFont",
    "IconFont",
    "AppliedDPI",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    Caption,
    SmallCaption,
    Menu,
    Message,
    Status,
    Icon,
}

impl Element {
    pub const ALL: [Element; 6] = [
        Element::Caption,
        Element::SmallCaption,
        Element::Menu,
        Element::Message,
        Element::Status,
        Element::Icon,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Element::Caption => "Title bars",
            Element::SmallCaption => "Tool window title bars",
            Element::Menu => "Menus",
            Element::Message => "Message boxes",
            Element::Status => "Status bars and tooltips",
            Element::Icon => "Icon labels (desktop and Explorer)",
        }
    }
}

/// The adjustable sizes, in pixels at the system DPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Border,
    PaddedBorder,
    Scroll,
    Caption,
    Menu,
}

impl Size {
    pub const ALL: [Size; 5] = [
        Size::Caption,
        Size::Menu,
        Size::Scroll,
        Size::Border,
        Size::PaddedBorder,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Size::Border => "Window border",
            Size::PaddedBorder => "Border padding",
            Size::Scroll => "Scroll bars",
            Size::Caption => "Title bar buttons",
            Size::Menu => "Menu bar height",
        }
    }

    /// Windows' default at 96 DPI and the range offered.
    fn default_px(self) -> i32 {
        match self {
            Size::Border => 1,
            Size::PaddedBorder => 4,
            Size::Scroll => 17,
            Size::Caption => 22,
            Size::Menu => 19,
        }
    }

    pub fn range(self) -> std::ops::RangeInclusive<i32> {
        match self {
            Size::Border => 1..=20,
            Size::PaddedBorder => 0..=20,
            Size::Scroll => 8..=60,
            Size::Caption => 16..=60,
            Size::Menu => 16..=60,
        }
    }
}

pub fn system_dpi() -> u32 {
    unsafe { GetDpiForSystem() }.max(96)
}

#[derive(Clone, Copy)]
pub struct Metrics {
    pub ncm: NONCLIENTMETRICSW,
    pub icon: LOGFONTW,
}

impl PartialEq for Metrics {
    fn eq(&self, other: &Self) -> bool {
        // NONCLIENTMETRICSW / LOGFONTW are plain data; compare their bytes.
        bytes_of(&self.ncm) == bytes_of(&other.ncm) && bytes_of(&self.icon) == bytes_of(&other.icon)
    }
}

fn bytes_of<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(v).cast(), size_of::<T>()) }
}

fn face(lf: &LOGFONTW) -> String {
    let end = lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(32);
    String::from_utf16_lossy(&lf.lfFaceName[..end])
}

fn set_face(lf: &mut LOGFONTW, name: &str) {
    lf.lfFaceName = [0; 32];
    for (dst, src) in lf.lfFaceName.iter_mut().zip(name.encode_utf16().take(31)) {
        *dst = src;
    }
}

impl Metrics {
    pub fn read() -> Result<Self> {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let mut icon = LOGFONTW::default();
        unsafe {
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(std::ptr::from_mut(&mut ncm).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .context("reading window metrics")?;
            SystemParametersInfoW(
                SPI_GETICONTITLELOGFONT,
                size_of::<LOGFONTW>() as u32,
                Some(std::ptr::from_mut(&mut icon).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .context("reading the icon font")?;
        }
        Ok(Self { ncm, icon })
    }

    fn font(&self, e: Element) -> &LOGFONTW {
        match e {
            Element::Caption => &self.ncm.lfCaptionFont,
            Element::SmallCaption => &self.ncm.lfSmCaptionFont,
            Element::Menu => &self.ncm.lfMenuFont,
            Element::Message => &self.ncm.lfMessageFont,
            Element::Status => &self.ncm.lfStatusFont,
            Element::Icon => &self.icon,
        }
    }

    fn font_mut(&mut self, e: Element) -> &mut LOGFONTW {
        match e {
            Element::Caption => &mut self.ncm.lfCaptionFont,
            Element::SmallCaption => &mut self.ncm.lfSmCaptionFont,
            Element::Menu => &mut self.ncm.lfMenuFont,
            Element::Message => &mut self.ncm.lfMessageFont,
            Element::Status => &mut self.ncm.lfStatusFont,
            Element::Icon => &mut self.icon,
        }
    }

    pub fn face(&self, e: Element) -> String {
        face(self.font(e))
    }

    pub fn set_face(&mut self, e: Element, name: &str) {
        set_face(self.font_mut(e), name);
    }

    pub fn points(&self, e: Element, dpi: u32) -> i32 {
        let h = self.font(e).lfHeight.abs();
        ((h as f32 * 72.0 / dpi as f32).round() as i32).max(1)
    }

    pub fn set_points(&mut self, e: Element, pt: i32, dpi: u32) {
        self.font_mut(e).lfHeight = -((pt as f32 * dpi as f32 / 72.0).round() as i32);
        self.font_mut(e).lfWidth = 0;
    }

    pub fn bold(&self, e: Element) -> bool {
        self.font(e).lfWeight >= 600
    }

    pub fn set_bold(&mut self, e: Element, on: bool) {
        self.font_mut(e).lfWeight = if on { 700 } else { 400 };
    }

    pub fn size(&self, s: Size) -> i32 {
        match s {
            Size::Border => self.ncm.iBorderWidth,
            Size::PaddedBorder => self.ncm.iPaddedBorderWidth,
            Size::Scroll => self.ncm.iScrollWidth,
            Size::Caption => self.ncm.iCaptionHeight,
            Size::Menu => self.ncm.iMenuHeight,
        }
    }

    pub fn set_size(&mut self, s: Size, px: i32) {
        let n = &mut self.ncm;
        match s {
            Size::Border => n.iBorderWidth = px,
            Size::PaddedBorder => n.iPaddedBorderWidth = px,
            Size::Scroll => {
                n.iScrollWidth = px;
                n.iScrollHeight = px;
            }
            Size::Caption => {
                n.iCaptionHeight = px;
                n.iCaptionWidth = px;
                n.iSmCaptionHeight = px;
                n.iSmCaptionWidth = px;
            }
            Size::Menu => {
                n.iMenuHeight = px;
                n.iMenuWidth = px;
            }
        }
    }

    /// Windows 11's defaults at `dpi`, keeping the structure sizes of `self`.
    pub fn windows_defaults(&self, dpi: u32) -> Self {
        let mut m = *self;
        for e in Element::ALL {
            m.set_face(e, "Segoe UI");
            m.set_points(e, 9, dpi);
            m.set_bold(e, false);
            let f = m.font_mut(e);
            f.lfItalic = 0;
            f.lfUnderline = 0;
            f.lfStrikeOut = 0;
        }
        for s in Size::ALL {
            m.set_size(s, (s.default_px() * dpi as i32 + 48) / 96);
        }
        m
    }

    /// Applies the metrics now; Windows also saves them for the next sign-in.
    pub fn apply(&self) -> Result<()> {
        let flags = SPIF_UPDATEINIFILE | SPIF_SENDCHANGE;
        let mut ncm = self.ncm;
        let mut icon = self.icon;
        unsafe {
            SystemParametersInfoW(
                SPI_SETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(std::ptr::from_mut(&mut ncm).cast()),
                flags,
            )
            .context("applying window metrics")?;
            SystemParametersInfoW(
                SPI_SETICONTITLELOGFONT,
                size_of::<LOGFONTW>() as u32,
                Some(std::ptr::from_mut(&mut icon).cast()),
                flags,
            )
            .context("applying the icon font")?;
        }
        Ok(())
    }
}

/// Applies `m`, journaling the saved values so the change can be undone (the undo
/// takes effect at the next sign-in).
pub fn apply_journaled(ctx: &AppCtx<'_>, m: &Metrics, what: &str) -> Result<()> {
    let snaps: Vec<ValueSnapshot> = METRIC_VALUES
        .iter()
        .map(|n| registry::snapshot(METRICS_KEY, n))
        .collect();
    m.apply()?;
    ctx.journal.record(
        "visual",
        format!("{what} (undo applies at the next sign-in)"),
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

/// Installed font families, sorted, without the vertical (`@`) variants.
pub fn families() -> Vec<String> {
    unsafe extern "system" fn add(lf: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, out: LPARAM) -> i32 {
        let names = unsafe { &mut *(out.0 as *mut Vec<String>) };
        let name = face(unsafe { &*lf });
        if !name.starts_with('@') && !name.is_empty() {
            names.push(name);
        }
        1
    }
    let mut names: Vec<String> = Vec::new();
    unsafe {
        let dc = GetDC(None);
        let lf = LOGFONTW {
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        EnumFontFamiliesExW(
            dc,
            &lf,
            Some(add),
            LPARAM(std::ptr::from_mut(&mut names) as isize),
            0,
        );
        ReleaseDC(None, dc);
    }
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

// ---------------------------------------------------------------------------------
// Text size (Settings > Accessibility > Text size)

const ACCESSIBILITY: &str = r"HKCU\Software\Microsoft\Accessibility";

pub fn text_scale() -> u32 {
    registry::read_raw(ACCESSIBILITY, "TextScaleFactor")
        .and_then(|v| registry::decode_dword(&v))
        .filter(|v| (100..=225).contains(v))
        .unwrap_or(100)
}

pub fn set_text_scale(ctx: &AppCtx<'_>, percent: u32) -> Result<()> {
    if !(100..=225).contains(&percent) {
        bail!("text size must be between 100 % and 225 %");
    }
    let snap = registry::snapshot(ACCESSIBILITY, "TextScaleFactor");
    if percent == 100 {
        registry::delete_value(ACCESSIBILITY, "TextScaleFactor")?;
    } else {
        registry::write_raw(ACCESSIBILITY, "TextScaleFactor", &registry::dword(percent))?;
    }
    ctx.journal.record(
        "visual",
        format!("Text size set to {percent} %"),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// Replacing the system font (Segoe UI) everywhere classic UI uses it.

const FONTS: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts";
const SUBSTITUTES: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\FontSubstitutes";
/// Segoe UI's registered faces and files.
const SEGOE: [(&str, &str); 7] = [
    ("Segoe UI (TrueType)", "segoeui.ttf"),
    ("Segoe UI Black (TrueType)", "seguibl.ttf"),
    ("Segoe UI Bold (TrueType)", "segoeuib.ttf"),
    ("Segoe UI Bold Italic (TrueType)", "segoeuiz.ttf"),
    ("Segoe UI Italic (TrueType)", "segoeuii.ttf"),
    ("Segoe UI Light (TrueType)", "segoeuil.ttf"),
    ("Segoe UI Semibold (TrueType)", "seguisb.ttf"),
];

/// The font that replaces Segoe UI, if any.
pub fn system_font_substitute() -> Option<String> {
    registry::read_raw(SUBSTITUTES, "Segoe UI")
        .map(|v| registry::decode_string(&v))
        .filter(|s| !s.trim().is_empty())
}

fn snapshots() -> Vec<ValueSnapshot> {
    SEGOE
        .iter()
        .map(|(n, _)| registry::snapshot(FONTS, n))
        .chain(std::iter::once(registry::snapshot(SUBSTITUTES, "Segoe UI")))
        .collect()
}

/// Makes Windows use `family` wherever it would use Segoe UI (restart needed).
pub fn replace_system_font(ctx: &AppCtx<'_>, family: &str) -> Result<()> {
    if family.trim().is_empty() || family.eq_ignore_ascii_case("Segoe UI") {
        return restore_system_font(ctx);
    }
    let snaps = snapshots();
    for (name, _) in SEGOE {
        if registry::read_raw(FONTS, name).is_some() {
            registry::write_raw(FONTS, name, &registry::string(""))?;
        }
    }
    registry::write_raw(SUBSTITUTES, "Segoe UI", &registry::string(family))?;
    ctx.journal.record(
        "visual",
        format!("System font replaced by {family} (restart needed)"),
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

pub fn restore_system_font(ctx: &AppCtx<'_>) -> Result<()> {
    let snaps = snapshots();
    let fonts_dir = wm_core::paths::expand_env(r"%SystemRoot%\Fonts");
    for (name, file) in SEGOE {
        if std::path::Path::new(&fonts_dir).join(file).exists() {
            registry::write_raw(FONTS, name, &registry::string(file))?;
        }
    }
    registry::delete_value(SUBSTITUTES, "Segoe UI")?;
    ctx.journal.record(
        "visual",
        "System font set back to Segoe UI (restart needed)",
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_read_and_convert_without_applying() {
        let m = Metrics::read().unwrap();
        let dpi = system_dpi();
        for e in Element::ALL {
            assert!(!m.face(e).is_empty(), "{e:?}");
            assert!((4..=72).contains(&m.points(e, dpi)), "{e:?} {}", m.points(e, dpi));
        }
        let mut copy = m;
        copy.set_points(Element::Menu, 12, dpi);
        assert_eq!(copy.points(Element::Menu, dpi), 12);
        copy.set_face(Element::Menu, "Consolas");
        assert_eq!(copy.face(Element::Menu), "Consolas");
        copy.set_size(Size::Scroll, 20);
        assert_eq!((copy.ncm.iScrollWidth, copy.ncm.iScrollHeight), (20, 20));
        assert!(copy != m);

        let d = m.windows_defaults(96);
        assert_eq!(d.face(Element::Caption), "Segoe UI");
        assert_eq!(d.points(Element::Icon, 96), 9);
        assert_eq!(d.size(Size::Caption), 22);
        assert_eq!(m.windows_defaults(144).size(Size::Scroll), 26);
    }

    #[test]
    fn font_families_include_segoe_ui() {
        let f = families();
        assert!(f.iter().any(|n| n == "Segoe UI"), "{} families", f.len());
        assert!(f.iter().all(|n| !n.starts_with('@')));
    }
}
