//! Icon glyphs from the Phosphor icon font (MIT; https://phosphoricons.com).
//! The font file is vendored in `assets/Phosphor.ttf` (see `assets/Phosphor-LICENSE-MIT.txt`);
//! codepoints taken from the egui-phosphor crate (MIT OR Apache-2.0).

/// Font family name the glyphs are registered under.
pub const FAMILY: &str = "phosphor";
/// The regular-weight Phosphor font.
pub const FONT: &[u8] = include_bytes!("../assets/Phosphor.ttf");

pub const GEAR: &str = "\u{E270}";
pub const INFO: &str = "\u{E2CE}";
pub const AIRPLANE_TILT: &str = "\u{E5D6}";
pub const TRASH: &str = "\u{E4A6}";
pub const USER: &str = "\u{E4C2}";
pub const SHIELD: &str = "\u{E40A}";
pub const GLOBE: &str = "\u{E288}";
pub const WRENCH: &str = "\u{E5D4}";
pub const HOUSE: &str = "\u{E2C2}";
pub const CPU: &str = "\u{E610}";
pub const LIST_CHECKS: &str = "\u{EADC}";
pub const CHART_LINE: &str = "\u{E154}";
pub const TREE_STRUCTURE: &str = "\u{E67C}";
pub const DESKTOP: &str = "\u{E560}";
pub const BROOM: &str = "\u{EC54}";
pub const MAGIC_WAND: &str = "\u{E6B6}";
pub const FIRST_AID_KIT: &str = "\u{E570}";
pub const SKULL: &str = "\u{E916}";
pub const CLOCK_COUNTER_CLOCKWISE: &str = "\u{E1A0}";
pub const ROCKET: &str = "\u{E3FC}";
pub const RECYCLE: &str = "\u{E75A}";
pub const COPY: &str = "\u{E1CA}";
pub const HARD_DRIVES: &str = "\u{E2A0}";
pub const DOTS_NINE: &str = "\u{E1FC}";
pub const FOLDERS: &str = "\u{E260}";
pub const APP_WINDOW: &str = "\u{E5DA}";
pub const KEYBOARD: &str = "\u{E2D8}";
pub const PUSH_PIN: &str = "\u{E3E2}";
pub const LIST: &str = "\u{E2F0}";
pub const TERMINAL_WINDOW: &str = "\u{EAE8}";
pub const PAINT_BRUSH: &str = "\u{E6F0}";
pub const EYE_SLASH: &str = "\u{E224}";
pub const LOCK: &str = "\u{E2FA}";
pub const FILE_LOCK: &str = "\u{E95C}";
pub const ARROW_COUNTER_CLOCKWISE: &str = "\u{E038}";
pub const PLUGS: &str = "\u{EB56}";
pub const WIFI_HIGH: &str = "\u{E4EA}";
pub const LINK: &str = "\u{E2E2}";
pub const NETWORK: &str = "\u{EDDE}";
pub const MONITOR: &str = "\u{E32E}";
pub const CALENDAR_CHECK: &str = "\u{E712}";
pub const SCISSORS: &str = "\u{EAE0}";
pub const STACK: &str = "\u{E466}";
pub const DATABASE: &str = "\u{E1DE}";
pub const TOOLBOX: &str = "\u{ECA0}";
pub const DOWNLOAD: &str = "\u{E20A}";
pub const IMAGE: &str = "\u{E2CA}";
pub const FOLDER_OPEN: &str = "\u{E256}";
pub const WINDOWS_LOGO: &str = "\u{E692}";
pub const GAUGE: &str = "\u{E628}";
pub const LIGHTNING: &str = "\u{E2DE}";
pub const SPEEDOMETER: &str = "\u{EE74}";
pub const STOP_CIRCLE: &str = "\u{E46E}";
pub const ARROWS_CLOCKWISE: &str = "\u{E094}";
pub const QUESTION: &str = "\u{E3E8}";
pub const SIGN_OUT: &str = "\u{E42A}";
pub const MAGNIFYING_GLASS: &str = "\u{E30C}";
pub const FUNNEL: &str = "\u{E266}";
pub const SWAP: &str = "\u{E83C}";
pub const CARET_DOWN: &str = "\u{E136}";
pub const ARROW_UP: &str = "\u{E08E}";
pub const ARROW_DOWN: &str = "\u{E03E}";
pub const CLOUD_SLASH: &str = "\u{E1B6}";
pub const SHIELD_CHECK: &str = "\u{E40C}";
pub const FINGERPRINT: &str = "\u{E23E}";
pub const USB: &str = "\u{E956}";
pub const DEVICES: &str = "\u{EBA4}";
pub const THERMOMETER: &str = "\u{E5C6}";
pub const POWER: &str = "\u{E3DA}";
pub const LIST_BULLETS: &str = "\u{E2F2}";
pub const GRID_FOUR: &str = "\u{E296}";
pub const ARCHIVE: &str = "\u{E00C}";
pub const BELL: &str = "\u{E0CE}";
pub const PRINTER: &str = "\u{E3DC}";
pub const FILE_X: &str = "\u{E23C}";
pub const UPLOAD: &str = "\u{E4BE}";
pub const SLIDERS: &str = "\u{E432}";
pub const COMPASS: &str = "\u{E1C8}";
pub const BOOKMARK_SIMPLE: &str = "\u{E0EA}";
pub const KEY: &str = "\u{E2D6}";
pub const CLIPBOARD_TEXT: &str = "\u{E198}";
pub const ARROW_SQUARE_OUT: &str = "\u{E5DE}";
pub const PLAY: &str = "\u{E3D0}";
pub const STOP: &str = "\u{E46C}";
pub const PLUS: &str = "\u{E3D4}";
pub const MINUS: &str = "\u{E32A}";
pub const PENCIL_SIMPLE: &str = "\u{E3B4}";
pub const FLOPPY_DISK: &str = "\u{E248}";
pub const CHECK: &str = "\u{E182}";
pub const X: &str = "\u{E4F6}";
pub const WARNING: &str = "\u{E4E0}";
pub const CROSSHAIR: &str = "\u{E1D6}";
pub const PAUSE: &str = "\u{E39E}";
pub const TREE_VIEW: &str = "\u{EE48}";
pub const MOUSE_SIMPLE: &str = "\u{E644}";
pub const FILE_TEXT: &str = "\u{E23A}";
pub const FILE_SEARCH: &str = "\u{E238}";
pub const CLOCK: &str = "\u{E19A}";

/// Registers the icon font as a fallback of the proportional family, so glyphs can be
/// used inline in any label (e.g. `format!("{} Refresh", icons::ARROWS_CLOCKWISE)`).
pub fn install(ctx: &crate::egui::Context) {
    use crate::egui::epaint::text::{FontData, FontInsert, FontPriority, InsertFontFamily};
    ctx.add_font(FontInsert {
        name: FAMILY.to_owned(),
        data: FontData::from_static(FONT),
        families: vec![InsertFontFamily {
            family: crate::egui::FontFamily::Proportional,
            priority: FontPriority::Lowest,
        }],
    });
}
