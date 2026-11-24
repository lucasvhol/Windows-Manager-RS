//! Launch counts and last-launch times that Explorer records per user ("UserAssist"),
//! for programs and shortcuts started from the shell. Names are ROT13-encoded and may
//! start with a known-folder id such as `{B4BFCC3A-…}\Steam.lnk` (the Desktop).

use std::collections::HashMap;

use chrono::{DateTime, Local};

use crate::registry;

const ROOT: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\UserAssist";

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub runs: u32,
    pub last: Option<DateTime<Local>>,
}

impl Usage {
    /// Combines two records (e.g. several files inside one desktop folder).
    pub fn merge(self, other: Usage) -> Usage {
        Usage {
            runs: self.runs + other.runs,
            last: self.last.max(other.last),
        }
    }
}

pub fn rot13(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' => (((c as u8 - b'a') + 13) % 26 + b'a') as char,
            'A'..='Z' => (((c as u8 - b'A') + 13) % 26 + b'A') as char,
            _ => c,
        })
        .collect()
}

/// Version 5 records (Windows 7 and later) are 72 bytes: the run count at offset 4
/// and the last launch as a FILETIME at offset 60.
pub fn parse_record(bytes: &[u8]) -> Option<Usage> {
    if bytes.len() < 68 {
        return None;
    }
    let runs = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let ft = i64::from_le_bytes(bytes[60..68].try_into().ok()?);
    // FILETIME counts 100 ns since 1601; 0 means never.
    let last = (ft > 0)
        .then(|| ft / 10_000_000 - 11_644_473_600)
        .filter(|secs| *secs > 0)
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
        .map(|d| d.with_timezone(&Local));
    Some(Usage { runs, last })
}

/// `{GUID}\rest` → the known folder's path joined with `rest`.
fn resolve_known_folder(name: &str) -> Option<String> {
    let rest = name.strip_prefix('{')?;
    let (guid, tail) = rest.split_once("}\\")?;
    let id = crate::shell::parse_guid(guid)?;
    let base = crate::shell::known_folder_by_id(id).ok()?;
    Some(base.join(tail).display().to_string())
}

/// Every recorded path (lowercased, known folders resolved) with its usage.
pub fn read() -> HashMap<String, Usage> {
    let mut out: HashMap<String, Usage> = HashMap::new();
    let Ok(root) = registry::open_read(ROOT) else {
        return out;
    };
    for sub in root.enum_keys().flatten() {
        let Ok(count) = root.open_subkey(format!(r"{sub}\Count")) else {
            continue;
        };
        for (name, value) in count.enum_values().flatten() {
            let Some(usage) = parse_record(&value.bytes) else {
                continue;
            };
            let decoded = rot13(&name);
            let path = resolve_known_folder(&decoded).unwrap_or(decoded);
            let e = out.entry(path.to_lowercase()).or_default();
            *e = e.merge(usage);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rot13_round_trip() {
        assert_eq!(rot13("P:\\Hfref"), "C:\\Users");
        assert_eq!(rot13(&rot13("Steam.lnk 123")), "Steam.lnk 123");
    }

    #[test]
    fn records() {
        let mut b = vec![0u8; 72];
        b[4..8].copy_from_slice(&7u32.to_le_bytes());
        // 2026-01-01T00:00:00Z as FILETIME.
        let ft: i64 = (1_767_225_600 + 11_644_473_600) * 10_000_000;
        b[60..68].copy_from_slice(&ft.to_le_bytes());
        let u = parse_record(&b).unwrap();
        assert_eq!(u.runs, 7);
        assert_eq!(u.last.unwrap().timestamp(), 1_767_225_600);
        assert_eq!(parse_record(&[0u8; 72]).unwrap().last, None);
        assert_eq!(parse_record(&[0u8; 16]), None);
        let merged = u.merge(Usage { runs: 1, last: None });
        assert_eq!((merged.runs, merged.last), (8, u.last));
    }

    #[test]
    fn known_folder_prefix() {
        let desktop = crate::shell::known_folder(crate::shell::KnownFolder::Desktop).unwrap();
        let p = resolve_known_folder(r"{B4BFCC3A-DB2C-424C-B029-7FE99A87C641}\Steam.lnk").unwrap();
        assert_eq!(p, desktop.join("Steam.lnk").display().to_string());
        assert_eq!(resolve_known_folder(r"C:\x.exe"), None);
        assert_eq!(resolve_known_folder(r"{nope}\x"), None);
    }

    #[test]
    fn reads_this_users_records() {
        // Every signed-in user has some records; the call must not fail either way.
        let all = read();
        assert!(all.keys().all(|k| k == &k.to_lowercase()));
    }
}
