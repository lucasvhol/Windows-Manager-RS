//! Shell helpers: known folders (respecting OneDrive/folder redirection) and `.lnk` targets.

use std::path::{Path, PathBuf};

use anyhow::Result;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_PublicDesktop, IShellLinkW, KF_FLAG_DEFAULT,
    SHGetKnownFolderPath, ShellLink,
};
use windows::core::{GUID, HSTRING, Interface};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownFolder {
    Desktop,
    PublicDesktop,
    Documents,
}

pub fn known_folder(folder: KnownFolder) -> Result<PathBuf> {
    let id: GUID = match folder {
        KnownFolder::Desktop => FOLDERID_Desktop,
        KnownFolder::PublicDesktop => FOLDERID_PublicDesktop,
        KnownFolder::Documents => FOLDERID_Documents,
    };
    unsafe {
        let p = SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT, None)?;
        let path = PathBuf::from(p.to_string()?);
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(path)
    }
}

/// Target path stored in a `.lnk` shortcut (environment variables expanded).
/// Returns `Ok(None)` for shortcuts to non-file items (e.g. Control Panel pages).
pub fn shortcut_target(lnk: &Path) -> Result<Option<PathBuf>> {
    unsafe {
        // Shell link objects are apartment-threaded; S_FALSE / mode errors are harmless here.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.cast::<IPersistFile>()?
            .Load(&HSTRING::from(lnk.as_os_str()), STGM_READ)?;
        let mut buf = [0u16; 1024];
        // Flag 4 = SLGP_RAWPATH (no resolution side effects); we expand env vars ourselves.
        link.GetPath(&mut buf, std::ptr::null_mut(), 4)?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let raw = String::from_utf16_lossy(&buf[..end]);
        Ok((!raw.trim().is_empty()).then(|| PathBuf::from(crate::paths::expand_env(&raw))))
    }
}

/// True when the shortcut points at a file-system path that no longer exists.
pub fn is_broken_shortcut(lnk: &Path) -> bool {
    matches!(shortcut_target(lnk), Ok(Some(t)) if !t.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_folders_exist() {
        assert!(known_folder(KnownFolder::Desktop).unwrap().exists());
        assert!(known_folder(KnownFolder::Documents).unwrap().exists());
    }

    #[test]
    fn shortcut_targets() {
        // Create real shortcuts with PowerShell's WScript.Shell, then read them back.
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.lnk");
        let bad = dir.path().join("bad.lnk");
        let missing = dir.path().join("does-not-exist.exe");
        let script = format!(
            "$s=New-Object -ComObject WScript.Shell; \
             $a=$s.CreateShortcut('{}'); $a.TargetPath='C:\\Windows\\notepad.exe'; $a.Save(); \
             $b=$s.CreateShortcut('{}'); $b.TargetPath='{}'; $b.Save()",
            good.display(),
            bad.display(),
            missing.display()
        );
        crate::process::run_checked(crate::process::hidden_command("powershell").args([
            "-NoProfile",
            "-Command",
            &script,
        ]))
        .unwrap();
        assert_eq!(
            shortcut_target(&good).unwrap(),
            Some(PathBuf::from(r"C:\Windows\notepad.exe"))
        );
        assert!(!is_broken_shortcut(&good));
        assert!(is_broken_shortcut(&bad));
    }
}
