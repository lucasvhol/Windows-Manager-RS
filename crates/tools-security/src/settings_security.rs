//! Settings Security: restriction policies, which Settings pages and Control Panel
//! items are available, and shortcuts that open a Settings page directly.
//!
//! - Settings pages use the documented `SettingsPageVisibility` policy
//!   (`hide:a;b` or `showonly:a;b`, machine-wide, administrator required).
//! - Control Panel items use the per-user `DisallowCpl` / `RestrictCpl` policies with
//!   each item's canonical name (`System.ApplicationName`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use windows::Win32::UI::Shell::SHLoadIndirectString;
use windows::core::HSTRING;
use wm_core::{
    AppCtx, Category, Tool, egui, paths, registry,
    safety::UndoAction,
    shell::{KnownFolder, ShortcutSpec, create_shortcut, known_folder, shell_open},
    tweaks::TweakPanel,
    ui::{Message, show_message},
};

const TOOL: &str = "lockdown";
const MACHINE_POLICY: &str = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
const USER_POLICY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
const PAGE_VALUE: &str = "SettingsPageVisibility";
const CPL_NAMESPACE: &str = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Explorer\ControlPanel\NameSpace";

/// Settings pages: (`ms-settings:` name, title, section).
pub const PAGES: &[(&str, &str, &str)] = &[
    ("display", "Display", "System"),
    ("sound", "Sound", "System"),
    ("notifications", "Notifications", "System"),
    ("quiethours", "Focus", "System"),
    ("powersleep", "Power & battery", "System"),
    ("storagesense", "Storage", "System"),
    ("multitasking", "Multitasking", "System"),
    ("project", "Projecting to this PC", "System"),
    ("clipboard", "Clipboard", "System"),
    ("remotedesktop", "Remote Desktop", "System"),
    ("recovery", "Recovery", "System"),
    ("troubleshoot", "Troubleshoot", "System"),
    ("activation", "Activation", "System"),
    ("about", "About", "System"),
    ("bluetooth", "Bluetooth & devices", "Bluetooth & devices"),
    ("printers", "Printers & scanners", "Bluetooth & devices"),
    ("mousetouchpad", "Mouse", "Bluetooth & devices"),
    ("devices-touchpad", "Touchpad", "Bluetooth & devices"),
    ("pen", "Pen & Windows Ink", "Bluetooth & devices"),
    ("autoplay", "AutoPlay", "Bluetooth & devices"),
    ("usb", "USB", "Bluetooth & devices"),
    ("network-status", "Network status", "Network & internet"),
    ("network-wifi", "Wi-Fi", "Network & internet"),
    ("network-ethernet", "Ethernet", "Network & internet"),
    ("network-vpn", "VPN", "Network & internet"),
    ("network-mobilehotspot", "Mobile hotspot", "Network & internet"),
    ("network-airplanemode", "Airplane mode", "Network & internet"),
    ("network-proxy", "Proxy", "Network & internet"),
    ("network-dialup", "Dial-up", "Network & internet"),
    (
        "network-advancedsettings",
        "Advanced network settings",
        "Network & internet",
    ),
    ("personalization-background", "Background", "Personalization"),
    ("colors", "Colors", "Personalization"),
    ("themes", "Themes", "Personalization"),
    ("lockscreen", "Lock screen", "Personalization"),
    ("personalization-start", "Start", "Personalization"),
    ("taskbar", "Taskbar", "Personalization"),
    ("fonts", "Fonts", "Personalization"),
    ("appsfeatures", "Installed apps", "Apps"),
    ("defaultapps", "Default apps", "Apps"),
    ("optionalfeatures", "Optional features", "Apps"),
    ("appsforwebsites", "Apps for websites", "Apps"),
    ("videoplayback", "Video playback", "Apps"),
    ("startupapps", "Startup", "Apps"),
    ("maps", "Offline maps", "Apps"),
    ("yourinfo", "Your info", "Accounts"),
    ("emailandaccounts", "Email & accounts", "Accounts"),
    ("signinoptions", "Sign-in options", "Accounts"),
    ("otherusers", "Other users", "Accounts"),
    ("workplace", "Access work or school", "Accounts"),
    ("backup", "Windows backup", "Accounts"),
    ("dateandtime", "Date & time", "Time & language"),
    ("regionlanguage", "Language & region", "Time & language"),
    ("typing", "Typing", "Time & language"),
    ("speech", "Speech", "Time & language"),
    ("gaming-gamebar", "Game Bar", "Gaming"),
    ("gaming-gamedvr", "Captures", "Gaming"),
    ("gaming-gamemode", "Game Mode", "Gaming"),
    ("easeofaccess-display", "Text size", "Accessibility"),
    (
        "easeofaccess-mousepointer",
        "Mouse pointer and touch",
        "Accessibility",
    ),
    ("easeofaccess-magnifier", "Magnifier", "Accessibility"),
    ("easeofaccess-highcontrast", "Contrast themes", "Accessibility"),
    ("easeofaccess-narrator", "Narrator", "Accessibility"),
    ("easeofaccess-closedcaptioning", "Captions", "Accessibility"),
    ("easeofaccess-keyboard", "Keyboard", "Accessibility"),
    ("easeofaccess-mouse", "Mouse keys", "Accessibility"),
    ("windowsdefender", "Windows Security", "Privacy & security"),
    ("findmydevice", "Find my device", "Privacy & security"),
    ("developers", "For developers", "Privacy & security"),
    ("privacy-general", "General privacy", "Privacy & security"),
    ("privacy-location", "Location", "Privacy & security"),
    ("privacy-webcam", "Camera", "Privacy & security"),
    ("privacy-microphone", "Microphone", "Privacy & security"),
    (
        "privacy-speech",
        "Online speech recognition",
        "Privacy & security",
    ),
    ("privacy-feedback", "Diagnostics & feedback", "Privacy & security"),
    (
        "privacy-activityhistory",
        "Activity history",
        "Privacy & security",
    ),
    ("windowsupdate", "Windows Update", "Windows Update"),
    ("windowsupdate-history", "Update history", "Windows Update"),
    ("windowsupdate-options", "Advanced options", "Windows Update"),
    (
        "windowsupdate-restartoptions",
        "Restart options",
        "Windows Update",
    ),
    ("delivery-optimization", "Delivery Optimization", "Windows Update"),
    ("windowsinsider", "Windows Insider Program", "Windows Update"),
];

/// Whether listed items are hidden, or are the only ones left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Hide,
    ShowOnly,
}

/// Parses a `SettingsPageVisibility` value.
pub fn parse_visibility(value: &str) -> Option<(Mode, Vec<String>)> {
    let (mode, list) = value.trim().split_once(':')?;
    let mode = match mode.trim().to_ascii_lowercase().as_str() {
        "hide" => Mode::Hide,
        "showonly" => Mode::ShowOnly,
        _ => return None,
    };
    let pages = list
        .split(';')
        .map(|p| p.trim().trim_start_matches("ms-settings:").to_owned())
        .filter(|p| !p.is_empty())
        .collect();
    Some((mode, pages))
}

pub fn format_visibility(mode: Mode, pages: &[String]) -> String {
    let m = match mode {
        Mode::Hide => "hide",
        Mode::ShowOnly => "showonly",
    };
    format!("{m}:{}", pages.join(";"))
}

pub fn read_pages() -> Option<(Mode, Vec<String>)> {
    registry::read_raw(MACHINE_POLICY, PAGE_VALUE)
        .map(|v| registry::decode_string(&v))
        .and_then(|s| parse_visibility(&s))
}

/// Sets (or with no pages, removes) the Settings page policy.
pub fn write_pages(ctx: &AppCtx<'_>, mode: Mode, pages: &[String]) -> Result<String> {
    let snap = registry::snapshot(MACHINE_POLICY, PAGE_VALUE);
    let what = if pages.is_empty() {
        registry::delete_value(MACHINE_POLICY, PAGE_VALUE)?;
        "All Settings pages are available again".to_owned()
    } else {
        registry::write_raw(
            MACHINE_POLICY,
            PAGE_VALUE,
            &registry::string(&format_visibility(mode, pages)),
        )?;
        match mode {
            Mode::Hide => format!("Hid {} Settings pages", pages.len()),
            Mode::ShowOnly => format!("Settings shows only {} pages", pages.len()),
        }
    };
    ctx.journal.record(
        TOOL,
        what.clone(),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(what)
}

/// A Control Panel item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CplItem {
    /// Canonical name, e.g. `Microsoft.Mouse`.
    pub canonical: String,
    pub name: String,
}

fn resolve(s: &str) -> Option<String> {
    let s = s.split('#').next().unwrap_or(s);
    if !s.starts_with('@') {
        return Some(s.to_owned()).filter(|s| !s.is_empty());
    }
    let mut buf = [0u16; 260];
    unsafe { SHLoadIndirectString(&HSTRING::from(s), &mut buf, None) }.ok()?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n])).filter(|s| !s.is_empty())
}

fn string_of(key: &str, name: &str) -> Option<String> {
    registry::read_raw(key, name).map(|v| registry::decode_string(&v))
}

/// Control Panel items with a canonical name, by display name.
pub fn cpl_items() -> Vec<CplItem> {
    let Ok(ns) = registry::open_read(CPL_NAMESPACE) else {
        return Vec::new();
    };
    let mut items: Vec<CplItem> = ns
        .enum_keys()
        .flatten()
        .filter(|id| id.starts_with('{'))
        .filter_map(|id| {
            let key = format!(r"HKCR\CLSID\{id}");
            let canonical = string_of(&key, "System.ApplicationName").filter(|s| !s.is_empty())?;
            let name = string_of(&key, "LocalizedString")
                .and_then(|s| resolve(&s))
                .or_else(|| string_of(&key, "").filter(|s| !s.is_empty()))
                .unwrap_or_else(|| canonical.clone());
            Some(CplItem { canonical, name })
        })
        .collect();
    items.sort_by_key(|i| i.canonical.to_lowercase());
    items.dedup_by(|a, b| a.canonical.eq_ignore_ascii_case(&b.canonical));
    items.sort_by_key(|i| i.name.to_lowercase());
    items
}

fn policy_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Hide => "DisallowCpl",
        Mode::ShowOnly => "RestrictCpl",
    }
}

fn read_list(key: &str) -> Vec<String> {
    let Ok(k) = registry::open_read(key) else {
        return Vec::new();
    };
    let mut values: Vec<(u32, String)> = k
        .enum_values()
        .flatten()
        .map(|(name, v)| {
            let text = registry::decode_string(&registry::RawValue {
                vtype: v.vtype as u32,
                bytes: v.bytes.into_owned(),
            });
            (name.parse().unwrap_or(u32::MAX), text)
        })
        .filter(|(_, s)| !s.is_empty())
        .collect();
    values.sort_by_key(|(n, _)| *n);
    values.into_iter().map(|(_, s)| s).collect()
}

/// The active Control Panel policy, if any (`DisallowCpl` wins if both are set).
pub fn read_cpl() -> Option<(Mode, Vec<String>)> {
    [Mode::Hide, Mode::ShowOnly].into_iter().find_map(|m| {
        let on = registry::read_raw(USER_POLICY, policy_name(m))
            .and_then(|v| registry::decode_dword(&v))
            .is_some_and(|v| v != 0);
        on.then(|| (m, read_list(&format!(r"{USER_POLICY}\{}", policy_name(m)))))
    })
}

/// Whether this account may write its Explorer policies. Windows lets standard
/// accounts read `HKCU…Policies` but not create keys there, so the first policy
/// usually needs an administrator.
pub fn user_policy_writable() -> bool {
    const KEY_SET_VALUE: u32 = 0x0002;
    const KEY_CREATE_SUB_KEY: u32 = 0x0004;
    let Ok((root, sub)) = registry::split_path(USER_POLICY) else {
        return false;
    };
    match root.open_subkey_with_flags(sub, KEY_SET_VALUE | KEY_CREATE_SUB_KEY) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => root
            .open_subkey_with_flags(
                r"SoftwareMicrosoftWindowsCurrentVersionPolicies",
                KEY_CREATE_SUB_KEY,
            )
            .is_ok(),
        Err(_) => false,
    }
}

fn delete_key(path: &str) -> Result<()> {
    let (root, sub) = registry::split_path(path)?;
    match root.delete_subkey_all(sub) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting {path}")),
    }
}

/// Replaces the Control Panel policy (no items removes both policies).
pub fn write_cpl(ctx: &AppCtx<'_>, mode: Mode, items: &[String]) -> Result<String> {
    // Undo: drop what gets written, re-import the old lists, put the switches back.
    let mut undo = Vec::new();
    let mut snaps = Vec::new();
    for m in [Mode::Hide, Mode::ShowOnly] {
        let key = format!(r"{USER_POLICY}\{}", policy_name(m));
        let backup = ctx.journal.backup_reg_key(&key)?;
        undo.push(UndoAction::DeleteRegKey { key });
        if !matches!(backup, UndoAction::DeleteRegKey { .. }) {
            undo.push(backup);
        }
        snaps.push(registry::snapshot(USER_POLICY, policy_name(m)));
    }
    undo.push(UndoAction::RestoreRegValues { values: snaps });
    for m in [Mode::Hide, Mode::ShowOnly] {
        delete_key(&format!(r"{USER_POLICY}\{}", policy_name(m)))?;
        registry::delete_value(USER_POLICY, policy_name(m))?;
    }
    if !items.is_empty() {
        let list = format!(r"{USER_POLICY}\{}", policy_name(mode));
        for (n, item) in items.iter().enumerate() {
            registry::write_raw(&list, &(n + 1).to_string(), &registry::string(item))?;
        }
        registry::write_raw(USER_POLICY, policy_name(mode), &registry::dword(1))?;
    }
    let what = match (items.is_empty(), mode) {
        (true, _) => "All Control Panel items are available again".to_owned(),
        (false, Mode::Hide) => format!("Hid {} Control Panel items", items.len()),
        (false, Mode::ShowOnly) => format!("Control Panel shows only {} items", items.len()),
    };
    ctx.journal
        .record(TOOL, what.clone(), UndoAction::Sequence { steps: undo })?;
    Ok(what)
}

/// Where a Settings shortcut goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Desktop,
    Start,
}

fn place_dir(place: Place) -> Result<PathBuf> {
    Ok(match place {
        Place::Desktop => known_folder(KnownFolder::Desktop)?,
        Place::Start => PathBuf::from(paths::expand_env(
            r"%APPDATA%\Microsoft\Windows\Start Menu\Programs",
        )),
    })
}

/// Creates a shortcut that opens a Settings page. Returns its path.
pub fn create_page_shortcut(ctx: &AppCtx<'_>, page: &str, title: &str, place: Place) -> Result<PathBuf> {
    let name: String = title
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '-' } else { c })
        .collect();
    let lnk = place_dir(place)?.join(format!("{name} settings.lnk"));
    if lnk.exists() {
        bail!("{} already exists", lnk.display());
    }
    shortcut_at(&lnk, page, title)?;
    ctx.journal.record(
        TOOL,
        format!("Created a shortcut to the \"{title}\" settings"),
        UndoAction::DeleteCreatedFiles {
            paths: vec![lnk.clone()],
        },
    )?;
    Ok(lnk)
}

fn shortcut_at(lnk: &Path, page: &str, title: &str) -> Result<()> {
    create_shortcut(
        lnk,
        &ShortcutSpec {
            target: PathBuf::from(paths::expand_env(r"%WINDIR%\explorer.exe")),
            args: format!("ms-settings:{page}"),
            description: format!("Opens {title} in Settings"),
            ..Default::default()
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Restrictions,
    Pages,
    ControlPanel,
}

pub struct SettingsSecurity {
    tab: Tab,
    panel: TweakPanel,
    /// Mode and ticked pages being edited; loaded from the policy on first view.
    pages: Option<(Mode, Vec<String>)>,
    cpl: Option<(Mode, Vec<String>)>,
    cpl_items: Option<Vec<CplItem>>,
    filter: String,
    place: Place,
    message: Message,
}

impl Default for SettingsSecurity {
    fn default() -> Self {
        Self {
            tab: Tab::Restrictions,
            panel: TweakPanel::new(TOOL, include_str!("../tweaks/lockdown.toml")),
            pages: None,
            cpl: None,
            cpl_items: None,
            filter: String::new(),
            place: Place::Desktop,
            message: None,
        }
    }
}

fn mode_picker(ui: &mut egui::Ui, mode: &mut Mode, what: &str) {
    ui.horizontal(|ui| {
        ui.radio_value(mode, Mode::Hide, format!("Hide the ticked {what}"));
        ui.radio_value(mode, Mode::ShowOnly, format!("Show only the ticked {what}"));
    });
}

fn toggle(list: &mut Vec<String>, item: &str, on: bool) {
    let has = list.iter().any(|p| p.eq_ignore_ascii_case(item));
    if on && !has {
        list.push(item.to_owned());
    } else if !on && has {
        list.retain(|p| !p.eq_ignore_ascii_case(item));
    }
}

impl SettingsSecurity {
    fn dry(&mut self, ctx: &AppCtx<'_>) -> bool {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
        }
        ctx.config.dry_run
    }

    fn pages_tab(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let (mut mode, mut ticked) = self
            .pages
            .take()
            .unwrap_or_else(|| read_pages().unwrap_or((Mode::Hide, Vec::new())));
        ui.label("Choose which Settings pages people on this PC can open. Applies to all users.");
        if !ctx.elevated {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Changing this needs administrator rights (restart the app as administrator).",
            );
        }
        mode_picker(ui, &mut mode, "pages");
        let mut apply = None;
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter")
                    .desired_width(180.0),
            );
            ui.label(format!("{} ticked", ticked.len()));
            if ui.add_enabled(ctx.elevated, egui::Button::new("Apply")).clicked() {
                apply = Some(ticked.clone());
            }
            if ui
                .add_enabled(ctx.elevated, egui::Button::new("Allow all"))
                .clicked()
            {
                apply = Some(Vec::new());
            }
            ui.separator();
            ui.label("Shortcuts go on");
            ui.radio_value(&mut self.place, Place::Desktop, "the desktop");
            ui.radio_value(&mut self.place, Place::Start, "Start");
        });
        if mode == Mode::ShowOnly && ticked.is_empty() {
            ui.weak("Tick at least one page to show; with none, Apply allows every page.");
        }
        let filter = self.filter.to_lowercase();
        let rows: Vec<_> = PAGES
            .iter()
            .filter(|(id, title, section)| {
                filter.is_empty()
                    || [*id, *title, *section]
                        .iter()
                        .any(|s| s.to_lowercase().contains(&filter))
            })
            .collect();
        let mut open = None;
        let mut shortcut = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
        let table_h = ui.available_height() - 30.0;
        TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(table_h)
            .column(Column::exact(28.0))
            .column(Column::initial(220.0).at_least(140.0))
            .column(Column::initial(150.0))
            .column(Column::initial(210.0))
            .column(Column::remainder().at_least(150.0))
            .header(row_h, |mut h| {
                for t in ["", "Page", "Section", "Name", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let (id, title, section) = rows[row.index()];
                    row.col(|ui| {
                        let mut on = ticked.iter().any(|p| p.eq_ignore_ascii_case(id));
                        if ui.checkbox(&mut on, "").changed() {
                            toggle(&mut ticked, id, on);
                        }
                    });
                    row.col(|ui| {
                        ui.label(*title);
                    });
                    row.col(|ui| {
                        ui.label(*section);
                    });
                    row.col(|ui| {
                        ui.monospace(*id);
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            if ui.small_button("Open").clicked() {
                                open = Some(*id);
                            }
                            if ui.small_button("Create shortcut").clicked() {
                                shortcut = Some((*id, *title));
                            }
                        });
                    });
                });
            });
        if let Some(id) = open
            && let Err(e) = shell_open(&format!("ms-settings:{id}"), "")
        {
            self.message = Some(Err(format!("{e:#}")));
        }
        if let Some((id, title)) = shortcut
            && !self.dry(ctx)
        {
            self.message = Some(
                create_page_shortcut(ctx, id, title, self.place)
                    .map(|p| format!("Created {}.", p.display()))
                    .map_err(|e| format!("{e:#}")),
            );
        }
        if let Some(list) = apply
            && !self.dry(ctx)
        {
            self.message = Some(
                write_pages(ctx, mode, &list)
                    .map(|m| format!("{m}. Reopen Settings to see the change."))
                    .map_err(|e| format!("{e:#}")),
            );
            if list.is_empty() {
                ticked.clear();
            }
        }
        self.pages = Some((mode, ticked));
    }

    fn cpl_tab(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.cpl_items.get_or_insert_with(cpl_items).clone();
        let (mut mode, mut ticked) = self
            .cpl
            .take()
            .unwrap_or_else(|| read_cpl().unwrap_or((Mode::Hide, Vec::new())));
        ui.label("Choose which Control Panel items this account can open.");
        let writable = ctx.elevated || user_policy_writable();
        if !writable {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Windows lets only administrators create this account's policies: restart the app \
                 as administrator to change them.",
            );
        }
        mode_picker(ui, &mut mode, "items");
        let mut apply = None;
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter")
                    .desired_width(180.0),
            );
            ui.label(format!("{} ticked", ticked.len()));
            if ui.add_enabled(writable, egui::Button::new("Apply")).clicked() {
                apply = Some(ticked.clone());
            }
            if ui.add_enabled(writable, egui::Button::new("Allow all")).clicked() {
                apply = Some(Vec::new());
            }
        });
        let filter = self.filter.to_lowercase();
        let rows: Vec<_> = items
            .iter()
            .filter(|i| {
                filter.is_empty()
                    || i.name.to_lowercase().contains(&filter)
                    || i.canonical.to_lowercase().contains(&filter)
            })
            .collect();
        let mut open = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
        let table_h = ui.available_height() - 30.0;
        TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(table_h)
            .column(Column::exact(28.0))
            .column(Column::initial(240.0).at_least(140.0))
            .column(Column::initial(280.0))
            .column(Column::remainder().at_least(60.0))
            .header(row_h, |mut h| {
                for t in ["", "Item", "Canonical name", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let item = rows[row.index()];
                    row.col(|ui| {
                        let mut on = ticked.iter().any(|p| p.eq_ignore_ascii_case(&item.canonical));
                        if ui.checkbox(&mut on, "").changed() {
                            toggle(&mut ticked, &item.canonical, on);
                        }
                    });
                    row.col(|ui| {
                        ui.label(&item.name);
                    });
                    row.col(|ui| {
                        ui.monospace(&item.canonical);
                    });
                    row.col(|ui| {
                        if ui.small_button("Open").clicked() {
                            open = Some(item.canonical.clone());
                        }
                    });
                });
            });
        if let Some(c) = open
            && let Err(e) = shell_open(
                &paths::expand_env(r"%WINDIR%\System32\control.exe"),
                &format!("/name {c}"),
            )
        {
            self.message = Some(Err(format!("{e:#}")));
        }
        if let Some(list) = apply
            && !self.dry(ctx)
        {
            self.message = Some(
                write_cpl(ctx, mode, &list)
                    .map(|m| format!("{m}. Reopen Control Panel to see the change."))
                    .map_err(|e| format!("{e:#}")),
            );
            if list.is_empty() {
                ticked.clear();
            }
        }
        self.cpl = Some((mode, ticked));
    }
}

impl Tool for SettingsSecurity {
    fn id(&self) -> &'static str {
        TOOL
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Restrictions, "Restrictions");
            ui.selectable_value(&mut self.tab, Tab::Pages, "Settings pages");
            ui.selectable_value(&mut self.tab, Tab::ControlPanel, "Control Panel items");
        });
        ui.separator();
        show_message(ui, &self.message);
        match self.tab {
            Tab::Restrictions => {
                ui.label(
                    "Restrictions apply to the account running this app. To restrict another \
                     account (for example a child's), sign in as that user and run Win Manager \
                     RS there.",
                );
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "This app can always switch these off again, even when Control Panel, Task \
                     Manager or the Registry Editor are blocked.",
                );
                ui.add_space(8.0);
                self.panel.ui(ui, ctx);
            }
            Tab::Pages => self.pages_tab(ui, ctx),
            Tab::ControlPanel => self.cpl_tab(ui, ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_round_trip() {
        let v = format_visibility(Mode::Hide, &["about".into(), "display".into()]);
        assert_eq!(v, "hide:about;display");
        assert_eq!(
            parse_visibility(&v),
            Some((Mode::Hide, vec!["about".into(), "display".into()]))
        );
        assert_eq!(
            parse_visibility(" ShowOnly: ms-settings:about ; ;bluetooth"),
            Some((Mode::ShowOnly, vec!["about".into(), "bluetooth".into()]))
        );
        assert_eq!(parse_visibility("nonsense"), None);
        let mut ids: Vec<_> = PAGES.iter().map(|p| p.0).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), PAGES.len());
    }

    #[test]
    fn control_panel_items_and_policy_round_trip() {
        let items = cpl_items();
        assert!(
            items.iter().any(|i| i.canonical == "Microsoft.Mouse"),
            "{items:?}"
        );
        assert!(items.iter().all(|i| !i.name.starts_with('@')));

        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        assert_eq!(
            read_cpl(),
            None,
            "test expects no Control Panel policy on this account"
        );
        if !user_policy_writable() {
            eprintln!("skipping the policy writes: this account may not create its policies (run elevated)");
            return;
        }
        let list = vec!["Microsoft.Mouse".to_owned(), "Microsoft.Fonts".to_owned()];
        write_cpl(&ctx, Mode::Hide, &list).unwrap();
        assert_eq!(read_cpl(), Some((Mode::Hide, list.clone())));
        write_cpl(&ctx, Mode::ShowOnly, &list[..1]).unwrap();
        assert_eq!(read_cpl(), Some((Mode::ShowOnly, list[..1].to_vec())));
        write_cpl(&ctx, Mode::Hide, &[]).unwrap();
        assert_eq!(read_cpl(), None);
        assert!(registry::open_read(&format!(r"{USER_POLICY}\RestrictCpl")).is_err());
        assert!(registry::open_read(&format!(r"{USER_POLICY}\DisallowCpl")).is_err());
    }

    #[test]
    fn page_shortcut_opens_settings() {
        let dir = tempfile::tempdir().unwrap();
        let lnk = dir.path().join("About.lnk");
        shortcut_at(&lnk, "about", "About").unwrap();
        let s = wm_core::shell::read_shortcut(&lnk).unwrap();
        assert_eq!(s.args, "ms-settings:about");
        assert!(
            s.target
                .to_string_lossy()
                .to_lowercase()
                .ends_with("explorer.exe")
        );
    }
}
