//! URL Manager's browser lists: sites a browser refuses to open (URLBlocklist), and
//! sites that may or may not keep cookies (CookiesBlockedForUrls / CookiesAllowedForUrls).
//! They are the browsers' own enterprise policies, written for every account on the PC
//! (HKLM, administrator). Each list is a key of values "1".."n"; saving replaces the
//! list and is journaled, so undo puts the old values back exactly.

use anyhow::{Result, bail};
use wm_core::{
    AppCtx, egui, registry,
    safety::UndoAction,
    ui::{Message, show_message},
};

/// The machine policy root that the browsers read.
pub const ROOT: &str = r"HKLM\SOFTWARE\Policies";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Edge,
    Chrome,
    Brave,
}

impl Browser {
    pub const ALL: [Browser; 3] = [Browser::Edge, Browser::Chrome, Browser::Brave];

    pub fn name(self) -> &'static str {
        match self {
            Browser::Edge => "Microsoft Edge",
            Browser::Chrome => "Google Chrome",
            Browser::Brave => "Brave",
        }
    }

    /// The browser's policy key under a policy root.
    pub fn key(self, root: &str) -> String {
        let sub = match self {
            Browser::Edge => r"Microsoft\Edge",
            Browser::Chrome => r"Google\Chrome",
            Browser::Brave => r"BraveSoftware\Brave",
        };
        format!(r"{root}\{sub}")
    }

    /// The page that shows the policies the browser has loaded (with a Reload button).
    pub fn policy_page(self) -> &'static str {
        match self {
            Browser::Edge => "edge://policy",
            Browser::Chrome => "chrome://policy",
            Browser::Brave => "brave://policy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    BlockedSites,
    BlockedCookies,
    AllowedCookies,
}

impl List {
    pub const ALL: [List; 3] = [List::BlockedSites, List::BlockedCookies, List::AllowedCookies];

    pub fn policy(self) -> &'static str {
        match self {
            List::BlockedSites => "URLBlocklist",
            List::BlockedCookies => "CookiesBlockedForUrls",
            List::AllowedCookies => "CookiesAllowedForUrls",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            List::BlockedSites => "Blocked sites",
            List::BlockedCookies => "Sites that may not keep cookies",
            List::AllowedCookies => "Sites whose cookies are always allowed",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            List::BlockedSites => {
                "The browser shows an error page instead. \"example.com\" also covers its pages and \
                 subdomains; \"*\" blocks everything (then nothing opens)."
            }
            List::BlockedCookies => {
                "Sites on this list can't store cookies, so they won't remember sign-ins or track you."
            }
            List::AllowedCookies => {
                "Sites on this list always keep cookies, even when cookies are blocked or cleared by \
                 default, so you stay signed in."
            }
        }
    }
}

fn list_key(root: &str, browser: Browser, list: List) -> String {
    format!(r"{}\{}", browser.key(root), list.policy())
}

/// The entries of one list, in order.
pub fn read(root: &str, browser: Browser, list: List) -> Vec<String> {
    let key = list_key(root, browser, list);
    let Ok(k) = registry::open_read(&key) else {
        return Vec::new();
    };
    let mut items: Vec<(u32, String)> = k
        .enum_values()
        .flatten()
        .filter_map(|(name, _)| {
            let n = name.parse::<u32>().ok()?;
            let v = registry::read_raw(&key, &name)?;
            Some((n, registry::decode_string(&v)))
        })
        .collect();
    items.sort();
    items.into_iter().map(|(_, v)| v).collect()
}

/// Accepts the browsers' URL filter format: a host, a URL, a scheme like `https://*`,
/// or `*`. Spaces and control characters are refused.
pub fn valid_pattern(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty() && p.len() <= 2048 && !p.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Entries stay as typed; only surrounding whitespace is trimmed, and empty entries and
/// duplicates (ignoring case) are dropped.
pub fn normalise(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in items {
        let t = i.trim();
        if !t.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(t)) {
            out.push(t.to_owned());
        }
    }
    out
}

/// Replaces one list (values "1".."n"); returns the undo.
pub fn write(root: &str, browser: Browser, list: List, items: &[String]) -> Result<UndoAction> {
    let items = normalise(items);
    if let Some(bad) = items.iter().find(|i| !valid_pattern(i)) {
        bail!("\"{bad}\" is not a valid site address");
    }
    let key = list_key(root, browser, list);
    let existing: Vec<String> = registry::open_read(&key)
        .map(|k| {
            k.enum_values()
                .flatten()
                .map(|(n, _)| n)
                .filter(|n| n.parse::<u32>().is_ok())
                .collect()
        })
        .unwrap_or_default();
    let mut names: Vec<String> = existing.clone();
    for n in 1..=items.len() {
        let n = n.to_string();
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let snaps: Vec<_> = names.iter().map(|n| registry::snapshot(&key, n)).collect();
    for n in &existing {
        registry::delete_value(&key, n)?;
    }
    for (i, item) in items.iter().enumerate() {
        registry::write_raw(&key, &(i + 1).to_string(), &registry::string(item))?;
    }
    Ok(UndoAction::RestoreRegValues { values: snaps })
}

/// Saves a list and journals it.
pub fn save(ctx: &AppCtx<'_>, browser: Browser, list: List, items: &[String]) -> Result<String> {
    let undo = write(ROOT, browser, list, items)?;
    let n = normalise(items).len();
    ctx.journal.record(
        "hosts",
        format!("{}: {} set to {n} entries", browser.name(), list.title()),
        undo,
    )?;
    Ok(format!(
        "Saved. {} picks it up within a few minutes, or at once from {} → Reload policies.",
        browser.name(),
        browser.policy_page()
    ))
}

// ---------------------------------------------------------------------------------
// UI

pub struct BrowserLists {
    browser: Browser,
    list: List,
    /// The list being edited; loaded on first view and after switching.
    items: Option<Vec<String>>,
    dirty: bool,
    entry: String,
    message: Message,
}

impl Default for BrowserLists {
    fn default() -> Self {
        Self {
            browser: Browser::Edge,
            list: List::BlockedSites,
            items: None,
            dirty: false,
            entry: String::new(),
            message: None,
        }
    }
}

impl BrowserLists {
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.label("Lists the browsers enforce themselves, for every account on this PC. Unlike the hosts file, they can't be bypassed with a proxy or secure DNS.");
        if !ctx.elevated {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Changing these lists needs administrator rights (restart the app as administrator).",
            );
        }
        let (mut browser, mut list) = (self.browser, self.list);
        ui.horizontal(|ui| {
            for b in Browser::ALL {
                ui.selectable_value(&mut browser, b, b.name());
            }
        });
        ui.horizontal(|ui| {
            for l in List::ALL {
                ui.selectable_value(&mut list, l, l.title());
            }
        });
        if (browser, list) != (self.browser, self.list) {
            if self.dirty {
                self.message = Some(Err("Save or revert the changes to this list first.".into()));
            } else {
                (self.browser, self.list) = (browser, list);
                self.items = None;
            }
        }
        ui.weak(self.list.hint());
        show_message(ui, &self.message);
        let items = self
            .items
            .get_or_insert_with(|| read(ROOT, self.browser, self.list));
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.entry)
                    .hint_text("example.com")
                    .desired_width(260.0),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Add").clicked() || enter {
                let e = self.entry.trim().to_owned();
                if !valid_pattern(&e) {
                    self.message = Some(Err(format!("\"{e}\" is not a valid site address.")));
                } else if items.iter().any(|i| i.eq_ignore_ascii_case(&e)) {
                    self.message = Some(Err(format!("{e} is already on the list.")));
                } else {
                    items.push(e);
                    self.entry.clear();
                    self.dirty = true;
                    self.message = None;
                }
            }
        });
        let mut remove = None;
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 40.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if items.is_empty() {
                    ui.weak("The list is empty.");
                }
                for (i, item) in items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("Remove").clicked() {
                            remove = Some(i);
                        }
                        ui.label(item);
                    });
                }
            });
        if let Some(i) = remove {
            items.remove(i);
            self.dirty = true;
        }
        let items = items.clone();
        ui.separator();
        let (mut do_save, mut revert) = (false, false);
        ui.horizontal(|ui| {
            do_save = ui
                .add_enabled(self.dirty && ctx.elevated, egui::Button::new("Save"))
                .clicked();
            revert = ui.add_enabled(self.dirty, egui::Button::new("Revert")).clicked();
            if self.dirty {
                ui.weak("Unsaved changes");
            }
        });
        if do_save {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                save(ctx, self.browser, self.list, &items).map_err(|e| format!("{e:#}"))
            });
        } else if revert {
            self.message = None;
        }
        if do_save || revert {
            self.dirty = false;
            self.items = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_and_normalising() {
        assert!(valid_pattern("example.com"));
        assert!(valid_pattern("https://*"));
        assert!(valid_pattern("*"));
        assert!(!valid_pattern(""));
        assert!(!valid_pattern("bad site.com"));
        assert_eq!(
            normalise(&[" a.com ".into(), "A.com".into(), "".into(), "b.com".into()]),
            vec!["a.com".to_owned(), "b.com".to_owned()]
        );
    }

    #[test]
    fn lists_write_read_and_undo() {
        let root = format!(
            r"HKCU\Software\win-manager-rs-test-browserpolicy-{}",
            std::process::id()
        );
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let run = || -> Result<()> {
            let key = list_key(&root, Browser::Chrome, List::BlockedSites);
            registry::write_raw(&key, "1", &registry::string("old.com"))?;
            registry::write_raw(&key, "2", &registry::string("older.com"))?;
            registry::write_raw(&key, "note", &registry::string("not ours"))?;
            let undo = write(
                &root,
                Browser::Chrome,
                List::BlockedSites,
                &["new.com".into(), "https://*".into(), "third.com".into()],
            )?;
            assert_eq!(
                read(&root, Browser::Chrome, List::BlockedSites),
                vec!["new.com", "https://*", "third.com"]
            );
            assert!(
                registry::read_raw(&key, "note").is_some(),
                "other values are kept"
            );
            assert!(read(&root, Browser::Edge, List::BlockedSites).is_empty());
            let e = journal.record("t", "lists", undo)?;
            journal.undo(e.id)?;
            assert_eq!(
                read(&root, Browser::Chrome, List::BlockedSites),
                vec!["old.com", "older.com"]
            );
            assert!(
                registry::read_raw(&key, "3").is_none(),
                "undo removes added entries"
            );
            assert!(write(&root, Browser::Chrome, List::BlockedSites, &["a b".into()]).is_err());
            Ok(())
        };
        let r = run();
        let (hk, sub) = registry::split_path(&root).unwrap();
        let _ = hk.delete_subkey_all(sub);
        r.unwrap();
    }
}
