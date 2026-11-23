//! Save and restore a file's DACL as an SDDL string (used for exact permission undo).
//! Restoring works without elevation for objects the user may change permissions on.

use std::path::Path;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
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

/// The DACL (including its protected/inherited flag) as SDDL, e.g. `D:PAI(A;;FA;;;SY)…`.
pub fn dacl_sddl(path: &Path) -> Result<String> {
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        check(
            GetNamedSecurityInfoW(
                &HSTRING::from(path.as_os_str()),
                SE_FILE_OBJECT,
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

/// Applies a DACL saved by [`dacl_sddl`]. Inherited entries are re-derived from the parent.
pub fn set_dacl_sddl(path: &Path, sddl: &str) -> Result<()> {
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
                    &HSTRING::from(path.as_os_str()),
                    SE_FILE_OBJECT,
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
}
