//! Context Menu Manager: enable/disable classic context menu commands and shell
//! extension handlers, and switch Windows 11 to the full classic menu.
//!
//! Commands are disabled with the documented `LegacyDisable` value; handlers by
//! prefixing their CLSID with `-` (the convention used by Windows tools). Both are
//! fully reversible and journaled.

use windows::Win32::UI::Shell::SHLoadIndirectString;
use windows::core::HSTRING;
use wm_core::{
    AppCtx, Category, Tool, egui,
    process::{hidden_command, restart_explorer, run_checked},
    registry,
    safety::UndoAction,
    ui::{Message, show_message},
};

/// Shell class paths whose menus we manage, with friendly labels.
const LOCATIONS: [(&str, &str); 5] = [
    ("*", "Files"),
    ("AllFilesystemObjects", "Files and folders"),
    ("Directory", "Folders"),
    (r"Directory\Background", "Folder and desktop background"),
    ("Drive", "Drives"),
];

/// Commands that Explorer itself relies on; never offered for disabling.
const CORE_VERBS: &[&str] = &[
    "open",
    "opennewwindow",
    "opennewprocess",
    "opennewtab",
    "explore",
    "runas",
    "find",
];

const CLASSIC_MENU_KEY: &str = r"HKCU\Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Handler,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub location: &'static str,
    pub kind: Kind,
    /// Full registry key of the entry (HKCU\Software\Classes\… or HKLM\SOFTWARE\Classes\…).
    pub key: String,
    pub name: String,
    pub display: String,
    pub enabled: bool,
    /// Handler CLSID (for handlers whose default value holds it).
    pub clsid: Option<String>,
}

impl Entry {
    pub fn protected(&self) -> bool {
        self.kind == Kind::Command && CORE_VERBS.iter().any(|v| v.eq_ignore_ascii_case(&self.name))
    }

    /// Handlers whose key *name* is the CLSID (empty default) cannot be toggled by value.
    pub fn toggleable(&self) -> bool {
        !self.protected() && (self.kind == Kind::Command || self.clsid.is_some())
    }
}

/// Resolves `@dll,-123` resource strings to text.
fn resolve_indirect(s: &str) -> String {
    if !s.starts_with('@') {
        return s.to_owned();
    }
    let mut buf = [0u16; 512];
    match unsafe { SHLoadIndirectString(&HSTRING::from(s), &mut buf, None) } {
        Ok(()) => {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        }
        Err(_) => s.to_owned(),
    }
}

/// Strips menu accelerators: "&Open with Code" → "Open with Code".
pub fn strip_accelerator(s: &str) -> String {
    s.replace("&&", "\u{0}").replace('&', "").replace('\u{0}', "&")
}

fn read_string(key: &str, name: &str) -> Option<String> {
    registry::read_raw(key, name)
        .map(|v| registry::decode_string(&v))
        .filter(|s| !s.trim().is_empty())
}

fn clsid_name(clsid: &str) -> Option<String> {
    let clean = clsid.trim_start_matches('-');
    read_string(&format!(r"HKCR\CLSID\{clean}"), "")
}

pub fn is_clsid(s: &str) -> bool {
    let s = s.trim_start_matches('-');
    s.len() == 38 && s.starts_with('{') && s.ends_with('}')
}

pub fn enumerate() -> Vec<Entry> {
    let mut out = Vec::new();
    for (class, label) in LOCATIONS {
        for root in [r"HKCU\Software\Classes", r"HKLM\SOFTWARE\Classes"] {
            let shell = format!(r"{root}\{class}\shell");
            if let Ok(k) = registry::open_read(&shell) {
                for verb in k.enum_keys().flatten() {
                    let key = format!(r"{shell}\{verb}");
                    let display = read_string(&key, "MUIVerb")
                        .or_else(|| read_string(&key, ""))
                        .map(|d| strip_accelerator(&resolve_indirect(&d)))
                        .unwrap_or_else(|| verb.clone());
                    out.push(Entry {
                        location: label,
                        kind: Kind::Command,
                        enabled: registry::read_raw(&key, "LegacyDisable").is_none(),
                        key,
                        name: verb,
                        display,
                        clsid: None,
                    });
                }
            }
            let handlers = format!(r"{root}\{class}\shellex\ContextMenuHandlers");
            if let Ok(k) = registry::open_read(&handlers) {
                for name in k.enum_keys().flatten() {
                    let key = format!(r"{handlers}\{name}");
                    let default = read_string(&key, "").filter(|d| is_clsid(d));
                    let clsid_for_name = default.clone().or_else(|| is_clsid(&name).then(|| name.clone()));
                    let display = clsid_for_name
                        .as_deref()
                        .and_then(clsid_name)
                        .unwrap_or_else(|| name.clone());
                    out.push(Entry {
                        location: label,
                        kind: Kind::Handler,
                        enabled: !default.as_deref().is_some_and(|d| d.starts_with('-')),
                        key,
                        name,
                        display,
                        clsid: default,
                    });
                }
            }
        }
    }
    out
}

/// Value change that toggles an entry: (value name, new data or None to delete).
pub fn toggle_change(entry: &Entry, enable: bool) -> Option<(String, Option<registry::RawValue>)> {
    match entry.kind {
        Kind::Command => Some(("LegacyDisable".into(), (!enable).then(|| registry::string("")))),
        Kind::Handler => {
            let clsid = entry.clsid.as_deref()?.trim_start_matches('-').to_owned();
            let value = if enable { clsid } else { format!("-{clsid}") };
            Some((String::new(), Some(registry::string(&value))))
        }
    }
}

pub fn classic_menu_enabled() -> bool {
    registry::read_raw(&format!(r"{CLASSIC_MENU_KEY}\InprocServer32"), "").is_some()
}

#[derive(Default)]
pub struct ContextMenuManager {
    entries: Option<Vec<Entry>>,
    filter: String,
    restart_pending: bool,
    message: Message,
}

impl ContextMenuManager {
    fn toggle(&mut self, ctx: &AppCtx<'_>, entry: &Entry, enable: bool) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let Some((name, data)) = toggle_change(entry, enable) else {
            return;
        };
        let snap = registry::snapshot(&entry.key, &name);
        let r = match &data {
            Some(v) => registry::write_raw(&entry.key, &name, v),
            None => registry::delete_value(&entry.key, &name),
        };
        self.message = Some(
            r.and_then(|()| {
                ctx.journal.record(
                    "contextmenu",
                    format!(
                        "{} context menu item \"{}\" ({})",
                        if enable { "Enabled" } else { "Disabled" },
                        entry.display,
                        entry.location
                    ),
                    UndoAction::RestoreRegValues { values: vec![snap] },
                )
            })
            .map(|_| {
                format!(
                    "{} \"{}\".",
                    if enable { "Enabled" } else { "Disabled" },
                    entry.display
                )
            })
            .map_err(|e| format!("{e:#}")),
        );
        self.entries = None;
    }

    fn set_classic_menu(&mut self, ctx: &AppCtx<'_>, on: bool) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let r = (|| -> anyhow::Result<()> {
            let undo = ctx.journal.backup_reg_key(CLASSIC_MENU_KEY)?;
            if on {
                registry::write_raw(
                    &format!(r"{CLASSIC_MENU_KEY}\InprocServer32"),
                    "",
                    &registry::string(""),
                )?;
            } else {
                run_checked(hidden_command("reg").args(["delete", CLASSIC_MENU_KEY, "/f"]))?;
            }
            ctx.journal.record(
                "contextmenu",
                if on {
                    "Enabled the classic context menu"
                } else {
                    "Restored the Windows 11 context menu"
                },
                undo,
            )?;
            Ok(())
        })();
        self.restart_pending = r.is_ok();
        self.message = Some(
            r.map(|()| "Saved. Restart Explorer to see the change.".into())
                .map_err(|e| format!("{e:#}")),
        );
    }
}

impl Tool for ContextMenuManager {
    fn id(&self) -> &'static str {
        "contextmenu"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let entries = self.entries.get_or_insert_with(enumerate).clone();

        ui.horizontal(|ui| {
            let mut classic = classic_menu_enabled();
            if ui
                .checkbox(
                    &mut classic,
                    "Always show the full classic context menu (Windows 11)",
                )
                .changed()
            {
                self.set_classic_menu(ctx, classic);
            }
            if self.restart_pending && ui.button("Restart Explorer").clicked() {
                self.message = Some(
                    restart_explorer()
                        .map(|()| "Explorer restarted.".into())
                        .map_err(|e| format!("{e:#}")),
                );
                self.restart_pending = false;
            }
        });
        ui.weak(
            "The entries below appear in the classic menu (\"Show more options\" on Windows 11). \
             Items added by Store apps cannot be managed here.",
        );
        if !ctx.elevated {
            ui.weak(
                "Greyed-out entries are installed for all users; restart as administrator to change them.",
            );
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.entries = None;
                self.message = None;
            }
        });
        show_message(ui, &self.message);
        ui.separator();

        let f = self.filter.to_lowercase();
        let mut action = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (_, label) in LOCATIONS {
                let items: Vec<&Entry> = entries
                    .iter()
                    .filter(|e| e.location == label)
                    .filter(|e| {
                        f.is_empty()
                            || e.display.to_lowercase().contains(&f)
                            || e.name.to_lowercase().contains(&f)
                    })
                    .collect();
                if items.is_empty() {
                    continue;
                }
                egui::CollapsingHeader::new(format!("{label} ({})", items.len()))
                    .default_open(true)
                    .show(ui, |ui| {
                        for e in items {
                            ui.horizontal(|ui| {
                                let mut on = e.enabled;
                                let locked = registry::needs_admin(&e.key) && !ctx.elevated;
                                let r = ui.add_enabled(
                                    e.toggleable() && !locked,
                                    egui::Checkbox::new(&mut on, &e.display),
                                );
                                if r.changed() {
                                    action = Some((e.clone(), on));
                                }
                                let hint = if e.protected() {
                                    "Core Explorer command".to_owned()
                                } else if locked {
                                    ctx.i18n.tr("shell-needs-admin")
                                } else {
                                    e.key.clone()
                                };
                                r.on_hover_text(&e.key).on_disabled_hover_text(hint);
                                ui.weak(match e.kind {
                                    Kind::Command => "command",
                                    Kind::Handler => "extension",
                                });
                            });
                        }
                    });
            }
        });
        if let Some((e, on)) = action {
            self.toggle(ctx, &e, on);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(strip_accelerator("&Open with Code"), "Open with Code");
        assert_eq!(strip_accelerator("Save && Exit"), "Save & Exit");
        assert!(is_clsid("{09A47860-11B0-4DA5-AFA5-26D86198A780}"));
        assert!(is_clsid("-{09A47860-11B0-4DA5-AFA5-26D86198A780}"));
        assert!(!is_clsid("EPP"));
    }

    #[test]
    fn toggle_values() {
        let cmd = Entry {
            location: "Files",
            kind: Kind::Command,
            key: r"HKCU\Software\Classes\*\shell\x".into(),
            name: "x".into(),
            display: "X".into(),
            enabled: true,
            clsid: None,
        };
        assert_eq!(
            toggle_change(&cmd, false),
            Some(("LegacyDisable".into(), Some(registry::string(""))))
        );
        assert_eq!(toggle_change(&cmd, true), Some(("LegacyDisable".into(), None)));
        let h = Entry {
            kind: Kind::Handler,
            clsid: Some("{A}".into()),
            ..cmd.clone()
        };
        assert_eq!(
            toggle_change(&h, false).unwrap().1,
            Some(registry::string("-{A}"))
        );
        let disabled = Entry {
            clsid: Some("-{A}".into()),
            ..h
        };
        assert_eq!(
            toggle_change(&disabled, true).unwrap().1,
            Some(registry::string("{A}"))
        );
        assert!(
            !Entry {
                name: "open".into(),
                ..cmd
            }
            .toggleable()
        );
    }

    #[test]
    fn disable_and_restore_real_user_verb() {
        let key = format!(
            r"HKCU\Software\Classes\Directory\Background\shell\wm-test-{}",
            std::process::id()
        );
        registry::write_raw(&key, "MUIVerb", &registry::string("WM Test")).unwrap();
        let e = enumerate()
            .into_iter()
            .find(|e| e.key == key)
            .expect("test verb listed");
        assert!(e.enabled);
        assert_eq!(e.display, "WM Test");
        let (name, data) = toggle_change(&e, false).unwrap();
        registry::write_raw(&key, &name, &data.unwrap()).unwrap();
        assert!(!enumerate().into_iter().find(|x| x.key == key).unwrap().enabled);
        let (root, sub) = registry::split_path(&key).unwrap();
        root.delete_subkey_all(sub).unwrap();
    }

    #[test]
    fn lists_real_entries() {
        let entries = enumerate();
        assert!(
            entries.iter().any(|e| e.location == "Folders"),
            "expected folder menu entries"
        );
    }
}
