//! Hub layout: which tiles each category shows (in order), tool icons, and the
//! extra tools reachable from a tile's dropdown ("split tiles").

use wm_core::{Category, icons as i};

/// Tile order per category, as in the original's hub (left to right, row by row).
/// Tools not listed here are reachable through a tile's dropdown ([`related`]),
/// search, or the header's About menu, but get no tile of their own.
pub const TILES: &[(Category, &[&str])] = &[
    (
        Category::OneClick,
        &[
            "systemprotection",
            "killprocs",
            "fixer",
            "oneclickclean",
            "repair",
            "wizard",
        ],
    ),
    (
        Category::Information,
        &["sysinfo", "procman", "events", "devices", "hwmon"],
    ),
    (
        Category::Optimizer,
        &["speed", "boot", "startup", "services", "tasks"],
    ),
    (
        Category::Cleaner,
        &[
            "diskanalyzer",
            "winsxs",
            "uninstaller",
            "apps",
            "desktop",
            "junk",
            "duplicates",
            "regclean",
            "regdefrag",
        ],
    ),
    (
        Category::Customization,
        &[
            "customsystem",
            "explorertweaks",
            "visual",
            "jumplist",
            "hotkeys",
            "runshortcuts",
            "contextmenu",
            "startmenu",
        ],
    ),
    (
        Category::Security,
        &[
            "systemsecurity",
            "othersecurity",
            "lockdown",
            "filesecurity",
            "privacy",
            "securitylock",
            "devicesecurity",
            "undelete",
        ],
    ),
    (
        Category::Network,
        &[
            "systemnetwork",
            "hosts",
            "ipswitcher",
            "wifi",
            "nettools",
            "netmonitor",
        ],
    ),
    (
        Category::Misc,
        &["mytask", "utilities", "splitter", "supercopy", "regtools"],
    ),
];

/// Shown as a link under the Misc. Utilities tiles, like the original.
pub const MISC_LINK: &str = "downloadwin";

/// Tools offered in a tile's dropdown: the original's split tiles (System Protection,
/// File Explorer Tweaker), plus our tools that have no tile of their own.
pub fn related(id: &str) -> &'static [&'static str] {
    match id {
        "systemprotection" => &["winfeatures"],
        "customsystem" => &["appsettings"],
        "explorertweaks" => &["navpane", "thispc", "winx"],
        "visual" => &["bing"],
        "systemsecurity" => &["imagehijack"],
        "filesecurity" => &["unlocker"],
        "securitylock" => &["applocker"],
        "utilities" => &["movefolders", "compat"],
        _ => &[],
    }
}

pub fn category_icon(c: Category) -> &'static str {
    match c {
        Category::Home => i::HOUSE,
        Category::OneClick => i::LIGHTNING,
        Category::Information => i::INFO,
        Category::Optimizer => i::AIRPLANE_TILT,
        Category::Cleaner => i::TRASH,
        Category::Customization => i::PAINT_BRUSH,
        Category::Security => i::SHIELD,
        Category::Network => i::GLOBE,
        Category::Misc => i::WRENCH,
    }
}

pub fn tool_icon(id: &str) -> &'static str {
    match id {
        "journal" => i::CLOCK_COUNTER_CLOCKWISE,
        "settings" => i::GEAR,
        "sysinfo" => i::CPU,
        "procman" => i::LIST_CHECKS,
        "events" => i::FILE_TEXT,
        "devices" => i::DEVICES,
        "hwmon" => i::THERMOMETER,
        "speed" => i::SPEEDOMETER,
        "boot" => i::POWER,
        "startup" => i::ROCKET,
        "services" => i::GEAR,
        "tasks" => i::CALENDAR_CHECK,
        "diskanalyzer" => i::HARD_DRIVES,
        "winsxs" => i::STACK,
        "uninstaller" => i::TRASH,
        "apps" => i::GRID_FOUR,
        "desktop" => i::DESKTOP,
        "junk" => i::BROOM,
        "duplicates" => i::COPY,
        "regclean" => i::DATABASE,
        "regdefrag" => i::STACK,
        "killprocs" => i::LIGHTNING,
        "downloadwin" => i::WINDOWS_LOGO,
        "bing" => i::GLOBE,
        "movefolders" => i::FOLDER_OPEN,
        "compat" => i::SLIDERS,
        "unlocker" => i::LOCK,
        "imagehijack" => i::SHIELD,
        "winfeatures" => i::GRID_FOUR,
        "applocker" => i::SHIELD_CHECK,
        "explorertweaks" => i::FOLDER_OPEN,
        "navpane" => i::TREE_VIEW,
        "thispc" => i::MONITOR,
        "customsystem" => i::SLIDERS,
        "appsettings" => i::GRID_FOUR,
        "systemnetwork" => i::NETWORK,
        "visual" => i::PAINT_BRUSH,
        "jumplist" => i::LIST_BULLETS,
        "hotkeys" => i::KEYBOARD,
        "runshortcuts" => i::TERMINAL_WINDOW,
        "contextmenu" => i::MOUSE_SIMPLE,
        "startmenu" => i::LIST,
        "winx" => i::WINDOWS_LOGO,
        "systemsecurity" => i::SHIELD_CHECK,
        "othersecurity" => i::FINGERPRINT,
        "devicesecurity" => i::USB,
        "systemprotection" => i::CLOCK_COUNTER_CLOCKWISE,
        "lockdown" => i::SLIDERS,
        "filesecurity" => i::FILE_LOCK,
        "privacy" => i::EYE_SLASH,
        "securitylock" => i::LOCK,
        "undelete" => i::ARROW_COUNTER_CLOCKWISE,
        "hosts" => i::LINK,
        "ipswitcher" => i::NETWORK,
        "wifi" => i::WIFI_HIGH,
        "nettools" => i::TOOLBOX,
        "netmonitor" => i::CHART_LINE,
        "mytask" => i::CLOCK,
        "utilities" => i::DOTS_NINE,
        "splitter" => i::SCISSORS,
        "supercopy" => i::FOLDERS,
        "regtools" => i::KEY,
        "oneclickclean" => i::RECYCLE,
        "fixer" => i::FIRST_AID_KIT,
        "repair" => i::WRENCH,
        "wizard" => i::MAGIC_WAND,
        _ => i::APP_WINDOW,
    }
}
