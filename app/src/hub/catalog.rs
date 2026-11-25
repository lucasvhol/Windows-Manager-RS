//! Hub layout: which tiles each category shows (in order), tool icons, and the
//! extra tools reachable from a tile's dropdown ("split tiles").

use wm_core::{Category, icons as i};

/// Tile order per category. Tools not listed here are reachable through search,
/// a related-tools dropdown or the header buttons, but get no tile of their own.
pub const TILES: &[(Category, &[&str])] = &[
    (
        Category::OneClick,
        &["fixer", "oneclickclean", "killprocs", "repair", "wizard"],
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
        ],
    ),
    (
        Category::Customization,
        &[
            "customsystem",
            "appsettings",
            "explorertweaks",
            "visual",
            "jumplist",
            "hotkeys",
            "runshortcuts",
            "contextmenu",
            "startmenu",
            "winx",
        ],
    ),
    (
        Category::Security,
        &[
            "systemsecurity",
            "othersecurity",
            "devicesecurity",
            "systemprotection",
            "lockdown",
            "filesecurity",
            "privacy",
            "securitylock",
            "undelete",
            "unlocker",
            "imagehijack",
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
        &[
            "mytask",
            "utilities",
            "splitter",
            "supercopy",
            "regtools",
            "bing",
            "movefolders",
            "compat",
            "downloadwin",
        ],
    ),
];

/// Tools offered in a tile's dropdown.
pub fn related(id: &str) -> &'static [&'static str] {
    match id {
        "explorertweaks" => &["navpane", "thispc"],
        "startup" => &["services", "tasks"],
        "fixer" => &["repair"],
        "regclean" => &["regdefrag", "regtools"],
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

/// Recommended maintenance order shown on the Home page: (step title, tools).
pub const MAINTENANCE: &[(&str, &[&str])] = &[
    ("Clean up junk and traces", &["oneclickclean"]),
    ("Fix common problems", &["fixer", "repair"]),
    ("Trim what starts with Windows", &["startup", "services", "tasks"]),
    (
        "Reclaim disk space",
        &["diskanalyzer", "duplicates", "uninstaller"],
    ),
    ("Tidy the registry and your privacy", &["regclean", "privacy"]),
    ("Make Windows yours", &["visual", "explorertweaks"]),
];
