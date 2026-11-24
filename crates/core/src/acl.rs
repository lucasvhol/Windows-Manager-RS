//! Save and restore a file's or registry key's DACL as an SDDL string (used for exact
//! permission undo), and reset a key's permissions to what it inherits.
//! These work without elevation for objects the user may change permissions on.

use std::path::Path;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SE_OBJECT_TYPE, SE_REGISTRY_KEY, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows::core::{BOOL, HSTRING, PWSTR};

fn check(err: WIN32_ERROR, what: &str) -> Result<()> {
    if err != WIN32_ERROR(0) {
        bail!(
            "{what}: {}",
            windows::core::Error::from(err.to_hresult()).message()
        );
    }
    Ok(())
}

/// `HKCU\Software\X` → `CURRENT_USER\Software\X`, the form the security API expects.
fn registry_object(key: &str) -> Result<HSTRING> {
    let (root, sub) = key.split_once('\\').unwrap_or((key, ""));
    let root = match root.to_ascii_uppercase().as_str() {
        "HKCU" | "HKEY_CURRENT_USER" => "CURRENT_USER",
        "HKLM" | "HKEY_LOCAL_MACHINE" => "MACHINE",
        "HKCR" | "HKEY_CLASSES_ROOT" => "CLASSES_ROOT",
        "HKU" | "HKEY_USERS" => "USERS",
        other => bail!("unsupported registry root `{other}`"),
    };
    Ok(HSTRING::from(if sub.is_empty() {
        root.to_owned()
    } else {
        format!(r"{root}\{sub}")
    }))
}

fn read_sddl(name: &HSTRING, kind: SE_OBJECT_TYPE) -> Result<String> {
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        check(
            GetNamedSecurityInfoW(
                name,
                kind,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                &mut sd,
            ),
            "reading permissions",
        )?;
        let mut text = PWSTR::null();
        let r = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            None,
        );
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        r?;
        let s = text.to_string().unwrap_or_default();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        Ok(s)
    }
}

fn write_sddl(name: &HSTRING, kind: SE_OBJECT_TYPE, sddl: &str) -> Result<()> {
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
        let result = (|| -> Result<()> {
            let (mut present, mut defaulted) = (BOOL(0), BOOL(0));
            let mut dacl: *mut ACL = std::ptr::null_mut();
            GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)?;
            let (mut control, mut rev) = (0u16, 0u32);
            GetSecurityDescriptorControl(sd, &mut control, &mut rev)?;
            let protection = if control & SE_DACL_PROTECTED.0 != 0 {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
            check(
                SetNamedSecurityInfoW(
                    name,
                    kind,
                    DACL_SECURITY_INFORMATION | protection,
                    None,
                    None,
                    Some(dacl),
                    None,
                ),
                "restoring permissions",
            )
        })();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        result
    }
}

/// The DACL (including its protected/inherited flag) as SDDL, e.g. `D:PAI(A;;FA;;;SY)…`.
pub fn dacl_sddl(path: &Path) -> Result<String> {
    read_sddl(&HSTRING::from(path.as_os_str()), SE_FILE_OBJECT)
}

/// Applies a DACL saved by [`dacl_sddl`]. Inherited entries are re-derived from the parent.
pub fn set_dacl_sddl(path: &Path, sddl: &str) -> Result<()> {
    write_sddl(&HSTRING::from(path.as_os_str()), SE_FILE_OBJECT, sddl)
}

/// A registry key's DACL as SDDL (`HKCU\…` style path).
pub fn reg_dacl_sddl(key: &str) -> Result<String> {
    read_sddl(&registry_object(key)?, SE_REGISTRY_KEY)
}

/// Applies a registry key DACL saved by [`reg_dacl_sddl`].
pub fn set_reg_dacl_sddl(key: &str, sddl: &str) -> Result<()> {
    write_sddl(&registry_object(key)?, SE_REGISTRY_KEY, sddl)
}

/// Drops a registry key's own permission entries so it only inherits from its parent
/// (like `icacls /reset` for files). Needs permission to change the key's DACL.
pub fn reset_reg_dacl(key: &str) -> Result<()> {
    unsafe {
        // An empty ACL, unprotected: only the inherited entries remain.
        let mut buf = [0u32; 4];
        let acl = buf.as_mut_ptr().cast::<ACL>();
        InitializeAcl(acl, std::mem::size_of_val(&buf) as u32, ACL_REVISION)?;
        check(
            SetNamedSecurityInfoW(
                &registry_object(key)?,
                SE_REGISTRY_KEY,
                DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl),
                None,
            ),
            "resetting permissions",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_change_restore() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.txt");
        std::fs::write(&f, "x").unwrap();
        let before = dacl_sddl(&f).unwrap();
        assert!(before.starts_with("D:"), "{before}");
        // Protect the DACL with only SYSTEM + Administrators + Everyone read, then restore.
        set_dacl_sddl(&f, "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;WD)").unwrap();
        assert_ne!(dacl_sddl(&f).unwrap(), before);
        set_dacl_sddl(&f, &before).unwrap();
        assert_eq!(dacl_sddl(&f).unwrap(), before);
    }

    #[test]
    fn registry_key_lock_reset_restore() {
        let key = format!(r"HKCU\Software\WM-acl-test-{}", std::process::id());
        crate::registry::write_raw(&key, "v", &crate::registry::dword(1)).unwrap();
        let before = reg_dacl_sddl(&key).unwrap();
        // Lock it: protected, only SYSTEM. The owner (this user) may still change the DACL.
        set_reg_dacl_sddl(&key, "D:P(A;;KA;;;SY)").unwrap();
        assert!(
            crate::registry::open_read(&key).is_err(),
            "locked key must be unreadable"
        );
        reset_reg_dacl(&key).unwrap();
        assert!(
            crate::registry::open_read(&key).is_ok(),
            "reset key inherits read access"
        );
        set_reg_dacl_sddl(&key, &before).unwrap();
        // Same entries; Windows may add the auto-inherited flag (`D:AI`).
        let aces = |s: &str| s[s.find('(').unwrap_or(s.len())..].to_owned();
        assert_eq!(aces(&reg_dacl_sddl(&key).unwrap()), aces(&before));
        let (root, sub) = crate::registry::split_path(&key).unwrap();
        root.delete_subkey_all(sub).unwrap();
    }
}
