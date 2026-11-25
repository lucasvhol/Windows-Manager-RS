//! Icon replacements: desktop system icons (per user), Windows' stock shell icons
//! (for the whole PC) and file-type icons (per user). Every change is journaled.

use anyhow::{Result, bail};
use wm_core::{
    AppCtx,
    registry::{self, RawValue, ValueSnapshot},
    safety::UndoAction,
};

fn expand_sz(s: &str) -> RawValue {
    RawValue {
        vtype: 2,
        ..registry::string(s)
    }
}

fn read_string(key: &str, name: &str) -> Option<String> {
    registry::read_raw(key, name)
        .map(|v| registry::decode_string(&v))
        .filter(|s| !s.trim().is_empty())
}

/// Writes or deletes values as one journaled change.
fn change(ctx: &AppCtx<'_>, values: &[(&str, &str, Option<&str>)], what: String) -> Result<()> {
    let snaps: Vec<ValueSnapshot> = values.iter().map(|(k, n, _)| registry::snapshot(k, n)).collect();
    for (key, name, value) in values {
        match value {
            Some(v) => registry::write_raw(key, name, &expand_sz(v))?,
            None => {
                registry::delete_value(key, name)?;
                registry::prune_empty_class_override(key);
            }
        }
    }
    ctx.journal
        .record("visual", what, UndoAction::RestoreRegValues { values: snaps })?;
    wm_core::shell::refresh_icons();
    Ok(())
}

/// Two icon specs name the same icon (environment variables expanded, case and
/// spacing ignored).
pub fn same_icon(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let (path, index) = wm_core::file_icon::split_icon_location(s);
        (path.to_lowercase(), index)
    };
    norm(a) == norm(b)
}

// ---------------------------------------------------------------------------------
// Desktop system icons

pub struct SystemIcon {
    pub label: &'static str,
    clsid: &'static str,
    /// Value under DefaultIcon ("" = the key's default).
    value: &'static str,
}

pub const SYSTEM_ICONS: [SystemIcon; 6] = [
    SystemIcon {
        label: "This PC",
        clsid: "{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
        value: "",
    },
    SystemIcon {
        label: "User's files",
        clsid: "{59031a47-3f72-44a7-89c5-5595fe6b30ee}",
        value: "",
    },
    SystemIcon {
        label: "Network",
        clsid: "{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}",
        value: "",
    },
    SystemIcon {
        label: "Recycle Bin (empty)",
        clsid: "{645FF040-5081-101B-9F08-00AA002F954E}",
        value: "empty",
    },
    SystemIcon {
        label: "Recycle Bin (full)",
        clsid: "{645FF040-5081-101B-9F08-00AA002F954E}",
        value: "full",
    },
    SystemIcon {
        label: "Control Panel",
        clsid: "{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}",
        value: "",
    },
];

impl SystemIcon {
    fn override_key(&self) -> String {
        format!(
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\CLSID\{}\DefaultIcon",
            self.clsid
        )
    }

    /// The icon in use and whether it differs from Windows' own. (Desktop Icon
    /// Settings writes per-user copies of the defaults, which do not count.)
    pub fn current(&self) -> (String, bool) {
        let key = format!(r"HKCR\CLSID\{}\DefaultIcon", self.clsid);
        let default = read_string(&key, self.value).unwrap_or_default();
        match read_string(&self.override_key(), self.value) {
            Some(s) => {
                let changed = !same_icon(&s, &default);
                (s, changed)
            }
            None => (default, false),
        }
    }

    pub fn set(&self, ctx: &AppCtx<'_>, spec: &str) -> Result<()> {
        let key = self.override_key();
        let mut values = vec![(key.as_str(), self.value, Some(spec))];
        // The bin shows its default value until its state next changes.
        if self.value == "empty" {
            values.push((key.as_str(), "", Some(spec)));
        }
        change(ctx, &values, format!("Changed the {} icon", self.label))
    }

    pub fn reset(&self, ctx: &AppCtx<'_>) -> Result<()> {
        let key = self.override_key();
        let mut values = vec![(key.as_str(), self.value, None)];
        if self.value == "empty" {
            values.push((key.as_str(), "", None));
        }
        change(ctx, &values, format!("Restored the {} icon", self.label))
    }
}

// ---------------------------------------------------------------------------------
// Stock shell icons (Shell Icons key, by shell32 index)

pub const SHELL_ICONS_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\Shell Icons";

/// (index, label)
pub const SHELL_ICONS: [(u32, &str); 10] = [
    (0, "Unknown file type"),
    (3, "Folder (closed)"),
    (4, "Folder (open)"),
    (7, "Removable drive"),
    (8, "Hard drive"),
    (9, "Network drive"),
    (11, "CD / DVD drive"),
    (16, "Printer"),
    (29, "Shortcut arrow"),
    (28, "Shared item overlay"),
];

/// The replacement for a stock icon, if any.
pub fn shell_icon(index: u32) -> Option<String> {
    read_string(SHELL_ICONS_KEY, &index.to_string())
}

pub fn shell_icon_default(index: u32) -> String {
    format!(r"%SystemRoot%\System32\shell32.dll,{index}")
}

pub fn set_shell_icon(ctx: &AppCtx<'_>, index: u32, label: &str, spec: Option<&str>) -> Result<()> {
    let name = index.to_string();
    let what = match spec {
        Some(_) => format!("Replaced the stock icon: {label}"),
        None => format!("Restored the stock icon: {label}"),
    };
    change(ctx, &[(SHELL_ICONS_KEY, &name, spec)], what)
}

// ---------------------------------------------------------------------------------
// File-type icons

/// Every registered extension (`.txt`, …), sorted.
pub fn extensions() -> Vec<String> {
    let Ok(root) = registry::open_read("HKCR") else {
        return Vec::new();
    };
    let mut v: Vec<String> = root
        .enum_keys()
        .flatten()
        .filter(|k| k.starts_with('.') && k.len() > 1)
        .map(|k| k.to_ascii_lowercase())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The program identifier that decides an extension's icon: the user's choice of
/// default app, else the extension's own registration.
pub fn prog_id(ext: &str) -> Option<String> {
    let exts = format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}");
    // Windows 11 24H2+ keeps the choice in UserChoiceLatest; older builds in UserChoice.
    read_string(&format!(r"{exts}\UserChoiceLatest\ProgId"), "ProgId")
        .or_else(|| read_string(&format!(r"{exts}\UserChoice"), "ProgId"))
        .filter(|p| registry::open_read(&format!(r"HKCR\{p}")).is_ok())
        .or_else(|| read_string(&format!(r"HKCR\{ext}"), ""))
}

/// The key whose DefaultIcon applies to `ext`.
fn icon_class(ext: &str) -> String {
    prog_id(ext)
        .filter(|p| registry::open_read(&format!(r"HKCR\{p}\DefaultIcon")).is_ok())
        .unwrap_or_else(|| ext.to_owned())
}

/// The icon spec in use for an extension ("" when Windows shows a generic icon).
pub fn file_type_icon(ext: &str) -> String {
    read_string(&format!(r"HKCR\{}\DefaultIcon", icon_class(ext)), "").unwrap_or_default()
}

/// Files of this type show their own icon (programs, icon files, shortcuts).
pub fn per_file_icon(spec: &str) -> bool {
    spec.contains("%1")
}

pub fn set_file_type_icon(ctx: &AppCtx<'_>, ext: &str, spec: Option<&str>) -> Result<String> {
    let class = icon_class(ext);
    if spec.is_some() && per_file_icon(&file_type_icon(ext)) {
        bail!("{ext} files show their own icons, so their type has none to replace");
    }
    let key = format!(r"HKCU\Software\Classes\{class}\DefaultIcon");
    let what = match spec {
        Some(_) => format!("Changed the icon of {ext} files"),
        None => format!("Restored the icon of {ext} files"),
    };
    change(ctx, &[(&key, "", spec)], what)?;
    if spec.is_none() {
        registry::prune_empty_class_override(&format!(r"HKCU\Software\Classes\{class}"));
    }
    Ok(class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_icons_works_without_changes() {
        let root = std::env::var("SystemRoot").unwrap();
        assert!(same_icon(
            r"%SystemRoot%\system32\imageres.dll,-109",
            &format!(r"{root}\System32\imageres.dll, -109")
        ));
        assert_eq!(prog_id(".reg").as_deref(), Some("regfile"));
        assert!(!same_icon("a.dll,1", "a.dll,2"));
        for s in &SYSTEM_ICONS {
            let (spec, _) = s.current();
            assert!(!spec.is_empty(), "{}", s.label);
        }
        let exts = extensions();
        assert!(exts.iter().any(|e| e == ".txt"));
        // .txt may have no registered icon (its default app supplies one); .reg always does.
        assert!(!file_type_icon(".reg").is_empty());
        assert!(per_file_icon(&file_type_icon(".exe")) || file_type_icon(".exe").is_empty());
        for (i, _) in SHELL_ICONS {
            let _ = shell_icon(i);
        }
    }

    #[test]
    fn file_type_icon_round_trip_on_a_test_type() {
        let ext = ".wmrstesticon";
        let key = format!(r"HKCU\Software\Classes\{ext}");
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
        let spec = r"%SystemRoot%\System32\imageres.dll,-102";
        assert_eq!(set_file_type_icon(&ctx, ext, Some(spec)).unwrap(), ext);
        assert!(same_icon(&file_type_icon(ext), spec));
        assert_eq!(journal.entries().len(), 1);
        set_file_type_icon(&ctx, ext, None).unwrap();
        assert!(
            registry::open_read(&key).is_err(),
            "restoring removes the per-user key"
        );
    }
}
