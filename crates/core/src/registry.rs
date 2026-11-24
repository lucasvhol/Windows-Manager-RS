//! Registry helpers: `HKCU\…`-style path parsing and exact value snapshots for undo.
//!
//! A [`ValueSnapshot`] records whether a value existed and its exact type and bytes, so
//! restoring it can also *delete* a value that a change created. A `.reg` re-import
//! cannot do that.

use std::borrow::Cow;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use winreg::enums::*;
use winreg::{RegKey, RegValue};

/// Splits `HKCU\Software\Foo` into the root key and the sub-path.
pub fn split_path(path: &str) -> Result<(RegKey, &str)> {
    let (root, sub) = path.split_once('\\').unwrap_or((path, ""));
    let hkey = match root.to_ascii_uppercase().as_str() {
        "HKCU" | "HKEY_CURRENT_USER" => HKEY_CURRENT_USER,
        "HKLM" | "HKEY_LOCAL_MACHINE" => HKEY_LOCAL_MACHINE,
        "HKCR" | "HKEY_CLASSES_ROOT" => HKEY_CLASSES_ROOT,
        "HKU" | "HKEY_USERS" => HKEY_USERS,
        "HKCC" | "HKEY_CURRENT_CONFIG" => HKEY_CURRENT_CONFIG,
        other => bail!("unknown registry root `{other}`"),
    };
    Ok((RegKey::predef(hkey), sub))
}

/// Per-user policy roots: Windows lets standard accounts read them but usually not
/// write them (only administrators and Group Policy may).
const USER_POLICY_ROOTS: [&str; 2] = [
    r"software\microsoft\windows\currentversion\policies",
    r"software\policies",
];

/// True for keys that need administrator rights to write: everything outside HKCU, and
/// the per-user policy keys when this account may not write them.
pub fn needs_admin(path: &str) -> bool {
    let (root, sub) = path.split_once('\\').unwrap_or((path, ""));
    if !matches!(root.to_ascii_uppercase().as_str(), "HKCU" | "HKEY_CURRENT_USER") {
        return true;
    }
    let sub = sub.to_ascii_lowercase();
    let is_policy = USER_POLICY_ROOTS
        .iter()
        .any(|r| sub == *r || sub.starts_with(&format!(r"{r}\")));
    is_policy && !writable(path)
}

/// Whether `path` can be written (its values set, or it created) by this process: checks
/// the key itself, or its nearest existing parent.
pub fn writable(path: &str) -> bool {
    let Ok((root, sub)) = split_path(path) else {
        return false;
    };
    let mut sub = sub;
    loop {
        match root.open_subkey_with_flags(sub, KEY_SET_VALUE | KEY_CREATE_SUB_KEY) {
            Ok(_) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match sub.rsplit_once('\\') {
                Some((parent, _)) => sub = parent,
                None if !sub.is_empty() => sub = "",
                None => return false,
            },
            Err(_) => return false,
        }
    }
}

pub fn open_read(path: &str) -> Result<RegKey> {
    let (root, sub) = split_path(path)?;
    root.open_subkey_with_flags(sub, KEY_READ)
        .with_context(|| format!("opening {path}"))
}

/// Opens (creating if needed) a key for writing.
pub fn open_write(path: &str) -> Result<RegKey> {
    let (root, sub) = split_path(path)?;
    let (key, _) = root
        .create_subkey_with_flags(sub, KEY_READ | KEY_WRITE)
        .with_context(|| format!("opening {path} for writing"))?;
    Ok(key)
}

fn type_to_u32(t: &RegType) -> u32 {
    t.clone() as isize as u32
}

fn type_from_u32(v: u32) -> RegType {
    [
        REG_NONE,
        REG_SZ,
        REG_EXPAND_SZ,
        REG_BINARY,
        REG_DWORD,
        REG_DWORD_BIG_ENDIAN,
        REG_LINK,
        REG_MULTI_SZ,
        REG_RESOURCE_LIST,
        REG_FULL_RESOURCE_DESCRIPTOR,
        REG_RESOURCE_REQUIREMENTS_LIST,
        REG_QWORD,
    ]
    .into_iter()
    .find(|t| type_to_u32(t) == v)
    .unwrap_or(REG_BINARY)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawValue {
    pub vtype: u32,
    pub bytes: Vec<u8>,
}

impl RawValue {
    pub fn to_reg_value(&self) -> RegValue<'_> {
        RegValue {
            bytes: Cow::Borrowed(&self.bytes),
            vtype: type_from_u32(self.vtype),
        }
    }
}

/// State of one registry value before a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueSnapshot {
    pub key: String,
    pub name: String,
    /// `None` when the value (or its key) did not exist.
    pub previous: Option<RawValue>,
}

pub fn read_raw(key: &str, name: &str) -> Option<RawValue> {
    let k = open_read(key).ok()?;
    let v = k.get_raw_value(name).ok()?;
    Some(RawValue {
        vtype: type_to_u32(&v.vtype),
        bytes: v.bytes.into_owned(),
    })
}

pub fn snapshot(key: &str, name: &str) -> ValueSnapshot {
    ValueSnapshot {
        key: key.to_owned(),
        name: name.to_owned(),
        previous: read_raw(key, name),
    }
}

pub fn write_raw(key: &str, name: &str, value: &RawValue) -> Result<()> {
    open_write(key)?
        .set_raw_value(name, &value.to_reg_value())
        .with_context(|| format!("writing {key}\\{name}"))
}

/// Deletes a value; succeeds if it is already absent.
pub fn delete_value(key: &str, name: &str) -> Result<()> {
    let Ok(k) = split_path(key).and_then(|(root, sub)| {
        root.open_subkey_with_flags(sub, KEY_READ | KEY_WRITE)
            .map_err(anyhow::Error::from)
    }) else {
        return Ok(()); // key missing → value missing
    };
    match k.delete_value(name) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting {key}\\{name}")),
    }
}

/// Puts a value back exactly as it was in the snapshot.
pub fn restore(s: &ValueSnapshot) -> Result<()> {
    match &s.previous {
        Some(v) => write_raw(&s.key, &s.name, v),
        None => {
            delete_value(&s.key, &s.name)?;
            prune_empty_class_override(&s.key);
            Ok(())
        }
    }
}

/// A per-user `HKCU\Software\Classes\…` key shadows the machine-wide key of the same
/// name in HKCR. If deleting a value left such a key completely empty, remove it so the
/// system default shows through again.
pub fn prune_empty_class_override(key: &str) {
    let lower = key.to_ascii_lowercase();
    if !(lower.starts_with(r"hkcu\software\classes\")
        || lower.starts_with(r"hkey_current_user\software\classes\"))
    {
        return;
    }
    let Ok(k) = open_read(key) else { return };
    let empty = k.query_info().is_ok_and(|i| i.sub_keys == 0 && i.values == 0);
    if empty && let Ok((root, sub)) = split_path(key) {
        let _ = root.delete_subkey(sub);
    }
}

/// REG_DWORD helper.
pub fn dword(v: u32) -> RawValue {
    RawValue {
        vtype: type_to_u32(&REG_DWORD),
        bytes: v.to_le_bytes().to_vec(),
    }
}

/// REG_SZ helper (UTF-16LE with terminating NUL).
pub fn string(s: &str) -> RawValue {
    let mut bytes: Vec<u8> = s
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    bytes.shrink_to_fit();
    RawValue {
        vtype: type_to_u32(&REG_SZ),
        bytes,
    }
}

/// REG_BINARY helper.
pub fn binary(b: &[u8]) -> RawValue {
    RawValue {
        vtype: type_to_u32(&REG_BINARY),
        bytes: b.to_vec(),
    }
}

/// Decodes REG_SZ / REG_EXPAND_SZ bytes to a string.
pub fn decode_string(v: &RawValue) -> String {
    let wide: Vec<u16> = v
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// Decodes a REG_DWORD.
pub fn decode_dword(v: &RawValue) -> Option<u32> {
    Some(u32::from_le_bytes(v.bytes.get(..4)?.try_into().ok()?))
}

/// `HKCU\Software` → `Computer\HKEY_CURRENT_USER\Software` (Registry Editor's address format).
pub fn regedit_path(key: &str) -> String {
    let (root, rest) = key.split_once('\\').unwrap_or((key, ""));
    let upper = root.to_ascii_uppercase();
    let long = match upper.as_str() {
        "HKCU" => "HKEY_CURRENT_USER",
        "HKLM" => "HKEY_LOCAL_MACHINE",
        "HKCR" => "HKEY_CLASSES_ROOT",
        "HKU" => "HKEY_USERS",
        "HKCC" => "HKEY_CURRENT_CONFIG",
        other => other,
    };
    if rest.is_empty() {
        format!(r"Computer\{long}")
    } else {
        format!(r"Computer\{long}\{rest}")
    }
}

/// Opens Registry Editor at `key` (sets the key it remembers, then starts it).
pub fn open_in_regedit(key: &str) -> Result<()> {
    write_raw(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Applets\Regedit",
        "LastKey",
        &string(&regedit_path(key)),
    )?;
    crate::shell::shell_open("regedit.exe", "-m")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(tag: &str) -> String {
        format!(r"HKCU\Software\win-manager-rs-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn path_parsing() {
        assert!(split_path(r"HKCU\Software").is_ok());
        assert!(split_path(r"HKEY_LOCAL_MACHINE\SOFTWARE").is_ok());
        assert!(split_path(r"HKXX\Foo").is_err());
        assert!(!needs_admin(r"HKCU\Software"));
        assert!(needs_admin(r"HKLM\SOFTWARE"));
        // A missing key falls back to its nearest existing parent (HKCU\Software).
        assert!(writable(r"HKCU\Software\WM-does-not-exist\child"));
        let policy = r"HKCU\Software\Policies\WM-test";
        assert_eq!(needs_admin(policy), !writable(policy));
        assert!(!needs_admin(r"HKCU\Software\PoliciesNot"));
    }

    #[test]
    fn encoders() {
        assert_eq!(decode_string(&string("abc")), "abc");
        assert_eq!(decode_dword(&dword(0x8000_0001)), Some(0x8000_0001));
        assert_eq!(type_from_u32(type_to_u32(&REG_QWORD)), REG_QWORD);
    }

    #[test]
    fn snapshot_restore_existing_and_missing_values() {
        let key = test_key("snap");
        write_raw(&key, "Existing", &string("before")).unwrap();

        let existing = snapshot(&key, "Existing");
        let missing = snapshot(&key, "Created");
        assert!(missing.previous.is_none());

        write_raw(&key, "Existing", &dword(7)).unwrap();
        write_raw(&key, "Created", &dword(1)).unwrap();

        restore(&existing).unwrap();
        restore(&missing).unwrap();
        assert_eq!(read_raw(&key, "Existing"), Some(string("before")));
        assert_eq!(read_raw(&key, "Created"), None);

        let (root, sub) = split_path(&key).unwrap();
        root.delete_subkey_all(sub).unwrap();
        // Deleting from a missing key is a no-op.
        delete_value(&key, "Existing").unwrap();
    }

    #[test]
    fn empty_class_override_is_pruned_but_others_kept() {
        let class_key = format!(r"HKCU\Software\Classes\CLSID\{{wm-test-{}}}", std::process::id());
        let snap = snapshot(&class_key, "System.IsPinnedToNameSpaceTree");
        write_raw(&class_key, "System.IsPinnedToNameSpaceTree", &dword(0)).unwrap();
        restore(&snap).unwrap();
        assert!(open_read(&class_key).is_err(), "empty per-user class key removed");

        // Keys outside HKCU\Software\Classes are never pruned.
        let other = test_key("noprune");
        write_raw(&other, "x", &dword(1)).unwrap();
        delete_value(&other, "x").unwrap();
        prune_empty_class_override(&other);
        assert!(open_read(&other).is_ok());
        let (root, sub) = split_path(&other).unwrap();
        root.delete_subkey_all(sub).unwrap();
    }
}
