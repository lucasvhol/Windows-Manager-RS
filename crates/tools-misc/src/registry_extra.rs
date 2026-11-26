//! Registry Tools helpers: key path normalising (jumper), value editing with undo,
//! key permissions (read from SDDL, grant yourself full control), desktop shortcuts to a
//! key, and .reg import with a backup of every key the file touches.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use wm_core::{AppCtx, acl, registry, safety::UndoAction};

const TOOL: &str = "regtools";

/// Turns what people paste into `HKCU\…` form: `Computer\HKEY_CURRENT_USER\…`,
/// `HKEY_LOCAL_MACHINE/…`, `"HKLM\…\"`, `[HKCU\…]` (from a .reg file).
pub fn normalize_key(input: &str) -> Result<String> {
    let mut s = input.trim().trim_matches(['"', '[', ']']).replace('/', "\\");
    if let Some(rest) = s
        .strip_prefix("Computer\\")
        .or_else(|| s.strip_prefix("computer\\"))
        .or_else(|| s.strip_prefix("Computer: "))
    {
        s = rest.to_owned();
    }
    let s = s.trim_matches('\\');
    let (root, rest) = s.split_once('\\').unwrap_or((s, ""));
    let short = match root.to_ascii_uppercase().as_str() {
        "HKCU" | "HKEY_CURRENT_USER" => "HKCU",
        "HKLM" | "HKEY_LOCAL_MACHINE" => "HKLM",
        "HKCR" | "HKEY_CLASSES_ROOT" => "HKCR",
        "HKU" | "HKEY_USERS" => "HKU",
        "HKCC" | "HKEY_CURRENT_CONFIG" => "HKCC",
        "" => bail!("Enter a registry key."),
        other => bail!("\"{other}\" isn't a registry root (HKCU, HKLM, HKCR, HKU or HKCC)."),
    };
    let rest: Vec<&str> = rest.split('\\').filter(|p| !p.is_empty()).collect();
    Ok(if rest.is_empty() {
        short.to_owned()
    } else {
        format!(r"{short}\{}", rest.join("\\"))
    })
}

// ---------------------------------------------------------------------------------
// Search filters

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypeFilter {
    #[default]
    Any,
    String,
    Dword,
    Qword,
    Binary,
    Multi,
}

impl TypeFilter {
    pub const ALL: [TypeFilter; 6] = [
        TypeFilter::Any,
        TypeFilter::String,
        TypeFilter::Dword,
        TypeFilter::Qword,
        TypeFilter::Binary,
        TypeFilter::Multi,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TypeFilter::Any => "Any type",
            TypeFilter::String => "Strings",
            TypeFilter::Dword => "DWORD",
            TypeFilter::Qword => "QWORD",
            TypeFilter::Binary => "Binary",
            TypeFilter::Multi => "Multi-string",
        }
    }

    /// Whether a value of registry type `vtype` passes.
    pub fn accepts(self, vtype: u32) -> bool {
        match self {
            TypeFilter::Any => true,
            TypeFilter::String => vtype == 1 || vtype == 2,
            TypeFilter::Dword => vtype == 4,
            TypeFilter::Qword => vtype == 11,
            TypeFilter::Binary => vtype == 3,
            TypeFilter::Multi => vtype == 7,
        }
    }
}

/// How text is compared in a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Matching {
    pub whole: bool,
    pub case_sensitive: bool,
}

impl Matching {
    pub fn matches(self, text: &str, needle: &str) -> bool {
        match (self.whole, self.case_sensitive) {
            (true, true) => text == needle,
            (true, false) => {
                text.eq_ignore_ascii_case(needle) || text.to_lowercase() == needle.to_lowercase()
            }
            (false, true) => text.contains(needle),
            (false, false) => text.to_lowercase().contains(&needle.to_lowercase()),
        }
    }
}

// ---------------------------------------------------------------------------------
// Value editing

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    String,
    Expand,
    Dword,
    Qword,
    Multi,
    Binary,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::String,
        Kind::Expand,
        Kind::Dword,
        Kind::Qword,
        Kind::Multi,
        Kind::Binary,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::String => "String (REG_SZ)",
            Kind::Expand => "Expandable string",
            Kind::Dword => "DWORD (32-bit)",
            Kind::Qword => "QWORD (64-bit)",
            Kind::Multi => "Multi-string",
            Kind::Binary => "Binary",
        }
    }

    pub fn from_vtype(t: u32) -> Option<Kind> {
        Some(match t {
            1 => Kind::String,
            2 => Kind::Expand,
            4 => Kind::Dword,
            11 => Kind::Qword,
            7 => Kind::Multi,
            3 => Kind::Binary,
            _ => return None,
        })
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn parse_number(s: &str) -> Result<u64> {
    let s = s.trim();
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else {
        s.parse::<u64>()
    };
    v.with_context(|| format!("\"{s}\" isn't a number (use 123 or 0x7B)"))
}

/// Encodes text typed in the editor as a registry value of `kind`. Multi-strings take
/// one string per line; binary takes hex bytes ("01 a0 ff" or "01,a0,ff").
pub fn encode(kind: Kind, text: &str) -> Result<registry::RawValue> {
    Ok(match kind {
        Kind::String => registry::RawValue {
            vtype: 1,
            bytes: utf16z(text),
        },
        Kind::Expand => registry::RawValue {
            vtype: 2,
            bytes: utf16z(text),
        },
        Kind::Dword => {
            let n = parse_number(text)?;
            let n = u32::try_from(n).context("too large for a DWORD (use a QWORD)")?;
            registry::dword(n)
        }
        Kind::Qword => registry::RawValue {
            vtype: 11,
            bytes: parse_number(text)?.to_le_bytes().to_vec(),
        },
        Kind::Multi => {
            let mut bytes: Vec<u8> = text.lines().filter(|l| !l.is_empty()).flat_map(utf16z).collect();
            bytes.extend([0, 0]);
            registry::RawValue { vtype: 7, bytes }
        }
        Kind::Binary => {
            let bytes = text
                .split([' ', ',', '\n', '\r', '\t'])
                .filter(|p| !p.is_empty())
                .map(|p| u8::from_str_radix(p, 16).with_context(|| format!("\"{p}\" isn't a hex byte")))
                .collect::<Result<Vec<u8>>>()?;
            registry::RawValue { vtype: 3, bytes }
        }
    })
}

/// The editable text of a value (the inverse of [`encode`]).
pub fn decode(v: &registry::RawValue) -> String {
    match v.vtype {
        1 | 2 => registry::decode_string(v),
        4 => registry::decode_dword(v)
            .map(|d| d.to_string())
            .unwrap_or_default(),
        11 if v.bytes.len() >= 8 => u64::from_le_bytes(v.bytes[..8].try_into().unwrap()).to_string(),
        7 => {
            let units: Vec<u16> = v
                .bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            units
                .split(|c| *c == 0)
                .filter(|s| !s.is_empty())
                .map(String::from_utf16_lossy)
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => v
            .bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// (name, kind, text) for each value of a key; the default value's name is "".
/// (name, registry type, text) of one value.
pub type ValueRow = (String, u32, String);

pub fn list_values(key: &str) -> Result<Vec<ValueRow>> {
    let k = registry::open_read(key)?;
    let mut out: Vec<ValueRow> = k
        .enum_values()
        .flatten()
        .map(|(n, v)| {
            let raw = registry::RawValue {
                vtype: v.vtype.clone() as u32,
                bytes: v.bytes.to_vec(),
            };
            let text = decode(&raw);
            (n, raw.vtype, text)
        })
        .collect();
    out.sort_by_key(|(n, _, _)| n.to_lowercase());
    Ok(out)
}

pub fn set_value(ctx: &AppCtx<'_>, key: &str, name: &str, kind: Kind, text: &str) -> Result<()> {
    let value = encode(kind, text)?;
    let snap = registry::snapshot(key, name);
    registry::write_raw(key, name, &value)?;
    ctx.journal.record(
        TOOL,
        format!(r"Set {key}\{}", if name.is_empty() { "(Default)" } else { name }),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

pub fn remove_value(ctx: &AppCtx<'_>, key: &str, name: &str) -> Result<()> {
    let snap = registry::snapshot(key, name);
    registry::delete_value(key, name)?;
    ctx.journal.record(
        TOOL,
        format!(
            r"Deleted {key}\{}",
            if name.is_empty() { "(Default)" } else { name }
        ),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// Permissions (SDDL)

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ace {
    pub allow: bool,
    pub inherited: bool,
    /// Applies to subkeys too (CI).
    pub subkeys: bool,
    pub rights: String,
    pub sid: String,
}

/// Splits the DACL part of an SDDL string into its entries.
pub fn parse_aces(sddl: &str) -> Vec<Ace> {
    let dacl = sddl.split("S:").next().unwrap_or(sddl);
    let mut out = Vec::new();
    let mut rest = dacl;
    while let Some(start) = rest.find('(') {
        let Some(end) = rest[start..].find(')') else { break };
        let body = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];
        let f: Vec<&str> = body.split(';').collect();
        if f.len() < 6 {
            continue;
        }
        out.push(Ace {
            allow: f[0] == "A",
            inherited: f[1].contains("ID"),
            subkeys: f[1].contains("CI"),
            rights: f[2].to_owned(),
            sid: f[5].to_owned(),
        });
    }
    out
}

/// "Full control", "Read"… for SDDL registry rights.
pub fn rights_text(rights: &str) -> String {
    match rights {
        "KA" | "GA" => "Full control".into(),
        "KR" | "GR" => "Read".into(),
        "KW" | "GW" => "Write".into(),
        "KX" | "GX" => "Read".into(),
        r if r.starts_with("0x") => match u32::from_str_radix(&r[2..], 16) {
            Ok(0xF003F) => "Full control".into(),
            Ok(0x20019) => "Read".into(),
            _ => format!("Special ({r})"),
        },
        r => format!("Special ({r})"),
    }
}

pub fn grant_me_full_control(ctx: &AppCtx<'_>, key: &str) -> Result<()> {
    let before = acl::reg_dacl_sddl(key)?;
    let me = acl::current_user_sid()?;
    acl::set_reg_dacl_sddl(key, &acl::with_full_control(&before, &me))?;
    ctx.journal.record(
        TOOL,
        format!("Granted myself full control of {key}"),
        UndoAction::RestoreRegAcl {
            key: key.to_owned(),
            sddl: before,
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// Shortcut

/// A desktop shortcut that opens Registry Editor at `key` (through `--regjump`).
pub fn desktop_shortcut(key: &str) -> Result<PathBuf> {
    let desktop = wm_core::shell::known_folder(wm_core::shell::KnownFolder::Desktop)?;
    let last = key.rsplit('\\').next().unwrap_or(key);
    let safe: String = last
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    let lnk = desktop.join(format!("Registry - {safe}.lnk"));
    let exe = std::env::current_exe()?;
    wm_core::shell::create_shortcut(
        &lnk,
        &wm_core::shell::ShortcutSpec {
            target: exe,
            args: format!("--regjump \"{key}\""),
            working_dir: None,
            description: format!("Open {key} in Registry Editor"),
            hotkey: 0,
            show_cmd: 7,
        },
    )?;
    Ok(lnk)
}

// ---------------------------------------------------------------------------------
// Import

/// Reads a .reg file (UTF-16 with BOM as Registry Editor writes it, or UTF-8 / ANSI).
pub fn read_reg_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes)).into_owned()
    })
}

/// The keys a .reg file changes (`[-…]` deletions included), in short form, without
/// keys already covered by a parent in the list.
pub fn reg_file_keys(text: &str) -> Result<Vec<String>> {
    if !text
        .trim_start()
        .starts_with("Windows Registry Editor Version 5.00")
        && !text.trim_start().starts_with("REGEDIT4")
    {
        bail!("this isn't a registry file (it should start with \"Windows Registry Editor Version 5.00\")");
    }
    let mut keys: Vec<String> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') && l.ends_with(']') {
            let k = normalize_key(l.trim_start_matches("[-"))?;
            keys.push(k);
        }
    }
    keys.sort_by_key(|k| k.to_lowercase());
    keys.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    let mut top: Vec<String> = Vec::new();
    for k in keys {
        let covered = top
            .iter()
            .any(|t| k.len() > t.len() && k.to_lowercase().starts_with(&format!("{}\\", t.to_lowercase())));
        if !covered {
            top.push(k);
        }
    }
    Ok(top)
}

/// Backs up every key the file touches, imports it, and journals the import so undo
/// puts the keys back as they were (and removes keys the file created).
pub fn import(ctx: &AppCtx<'_>, path: &Path) -> Result<usize> {
    let keys = reg_file_keys(&read_reg_text(path)?)?;
    if keys.is_empty() {
        bail!("the file contains no keys");
    }
    let mut steps = Vec::new();
    for k in &keys {
        steps.push(ctx.journal.backup_reg_key(k)?);
    }
    wm_core::process::run_checked(wm_core::process::hidden_command("reg").arg("import").arg(path))
        .context("importing (keys outside HKEY_CURRENT_USER need administrator rights)")?;
    ctx.journal.record(
        TOOL,
        format!("Imported {}", path.display()),
        UndoAction::Sequence { steps },
    )?;
    Ok(keys.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_normalized() {
        let ok = |s: &str| normalize_key(s).unwrap();
        assert_eq!(ok(r"Computer\HKEY_CURRENT_USER\Software\X\"), r"HKCU\Software\X");
        assert_eq!(
            ok("HKEY_LOCAL_MACHINE/SOFTWARE/Microsoft"),
            r"HKLM\SOFTWARE\Microsoft"
        );
        assert_eq!(ok(r#""hklm\SYSTEM""#), r"HKLM\SYSTEM");
        assert_eq!(ok(r"[HKCU\Software\\Y]"), r"HKCU\Software\Y");
        assert_eq!(ok("HKU"), "HKU");
        assert!(normalize_key(r"C:\Windows").is_err());
        assert!(normalize_key("").is_err());
    }

    #[test]
    fn matching_and_types() {
        let exact = Matching {
            whole: true,
            case_sensitive: false,
        };
        assert!(exact.matches("Shell", "shell"));
        assert!(!exact.matches("ShellEx", "shell"));
        let cs = Matching {
            whole: false,
            case_sensitive: true,
        };
        assert!(cs.matches("ShellEx", "Shell"));
        assert!(!cs.matches("shellex", "Shell"));
        assert!(TypeFilter::String.accepts(2) && !TypeFilter::String.accepts(4));
        assert!(TypeFilter::Any.accepts(3));
    }

    #[test]
    fn values_round_trip() {
        for (kind, text) in [
            (Kind::String, "hello"),
            (Kind::Expand, r"%SystemRoot%\x"),
            (Kind::Dword, "4294967295"),
            (Kind::Qword, "18446744073709551615"),
            (Kind::Multi, "one\ntwo"),
            (Kind::Binary, "00 a0 ff"),
        ] {
            let v = encode(kind, text).unwrap();
            assert_eq!(decode(&v), text, "{kind:?}");
            assert_eq!(Kind::from_vtype(v.vtype), Some(kind));
        }
        assert_eq!(decode(&encode(Kind::Dword, "0x10").unwrap()), "16");
        assert!(encode(Kind::Dword, "4294967296").is_err());
        assert!(encode(Kind::Binary, "zz").is_err());
    }

    #[test]
    fn sddl_parsing_and_granting() {
        let sddl = "D:PAI(D;;KW;;;S-1-5-21-9)(A;CIID;KA;;;SY)(A;CI;KR;;;BU)";
        let aces = parse_aces(sddl);
        assert_eq!(aces.len(), 3);
        assert!(!aces[0].allow && aces[1].inherited && aces[2].subkeys);
        assert_eq!(rights_text("KA"), "Full control");
        assert_eq!(rights_text("0x20019"), "Read");
        assert_eq!(
            acl::with_full_control(sddl, "S-1-5-21-7"),
            "D:PAI(D;;KW;;;S-1-5-21-9)(A;CI;KA;;;S-1-5-21-7)(A;CIID;KA;;;SY)(A;CI;KR;;;BU)"
        );
        assert_eq!(acl::with_full_control("D:AI", "S-1"), "D:AI(A;CI;KA;;;S-1)");
    }

    #[test]
    fn reg_files() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\A]\r\n\"x\"=dword:1\r\n\r\n[HKEY_CURRENT_USER\\Software\\A\\B]\r\n\r\n[-HKEY_CURRENT_USER\\Software\\C]\r\n";
        assert_eq!(
            reg_file_keys(text).unwrap(),
            [r"HKCU\Software\A", r"HKCU\Software\C"]
        );
        assert!(reg_file_keys("hello").is_err());
    }

    #[test]
    fn edit_import_and_permissions_with_undo() {
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
        let key = format!(r"HKCU\Software\WinManagerTestRegExtra{}", std::process::id());

        set_value(&ctx, &key, "Num", Kind::Dword, "0x2A").unwrap();
        assert_eq!(
            list_values(&key).unwrap(),
            [("Num".to_owned(), 4, "42".to_owned())]
        );
        remove_value(&ctx, &key, "Num").unwrap();
        assert!(list_values(&key).unwrap().is_empty());

        // Permissions: grant, then undo restores the exact DACL.
        let before = acl::reg_dacl_sddl(&key).unwrap();
        grant_me_full_control(&ctx, &key).unwrap();
        let me = acl::current_user_sid().unwrap();
        assert!(
            parse_aces(&acl::reg_dacl_sddl(&key).unwrap())
                .iter()
                // The built-in Administrator account is written as the alias "LA".
                .any(|a| (a.sid == me || (a.sid == "LA" && me.ends_with("-500"))) && !a.inherited)
        );
        let e = journal.entries().into_iter().max_by_key(|e| e.id).unwrap();
        journal.undo(e.id).unwrap();
        // Windows may add the AI (auto-inherited) flag on restore; the entries match.
        assert_eq!(
            parse_aces(&acl::reg_dacl_sddl(&key).unwrap()),
            parse_aces(&before)
        );
        assert!(
            !acl::account_name(&me).starts_with("S-1-"),
            "the current user resolves to a name"
        );

        // Import into a new subkey, then undo removes it.
        let reg = dir.path().join("t.reg");
        let sub = format!(r"{key}\Imported");
        let long = sub.replacen("HKCU", "HKEY_CURRENT_USER", 1);
        std::fs::write(
            &reg,
            format!("Windows Registry Editor Version 5.00\r\n\r\n[{long}]\r\n\"v\"=\"x\"\r\n"),
        )
        .unwrap();
        assert_eq!(import(&ctx, &reg).unwrap(), 1);
        assert!(registry::read_raw(&sub, "v").is_some());
        let e = journal.entries().into_iter().max_by_key(|e| e.id).unwrap();
        journal.undo(e.id).unwrap();
        assert!(
            registry::open_read(&sub).is_err(),
            "undo removes the imported key"
        );

        let (root, s) = registry::split_path(&key).unwrap();
        root.delete_subkey_all(s).unwrap();
    }
}
