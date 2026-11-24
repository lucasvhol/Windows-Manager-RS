//! Shell helpers: known folders (respecting OneDrive/folder redirection) and `.lnk` targets.

use std::path::{Path, PathBuf};

use anyhow::Result;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_PublicDesktop, IShellLinkW, KF_FLAG_DEFAULT,
    SHGetKnownFolderPath, ShellExecuteW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
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
    known_folder_by_id(id)
}

/// Path of any known folder by its id (e.g. from a `{B4BFCC3A-…}\file` reference).
pub fn known_folder_by_id(id: GUID) -> Result<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT, None)?;
        let path = PathBuf::from(p.to_string()?);
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(path)
    }
}

/// Parses `B4BFCC3A-DB2C-424C-B029-7FE99A87C641` (braces optional) without panicking.
pub fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s.trim_matches(['{', '}']).chars().filter(|c| *c != '-').collect();
    (hex.len() == 32)
        .then(|| u128::from_str_radix(&hex, 16).ok())
        .flatten()
        .map(GUID::from_u128)
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

/// True when the shortcut points at a path on a local fixed disk that no longer
/// exists. Targets on network shares, removable or optical drives (which may simply
/// be disconnected) are never reported as broken.
pub fn is_broken_shortcut(lnk: &Path) -> bool {
    matches!(shortcut_target(lnk), Ok(Some(t))
        if crate::drives::kind_of(&t) == crate::drives::DriveKind::Fixed && !t.exists())
}

/// Initialises COM (single-threaded apartment) on the current thread if needed.
/// Safe to call repeatedly; an already-initialised thread is left as it is.
pub fn ensure_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Opens a program, document, `.msc`/`.cpl` file or URI (e.g. `ms-settings:display`)
/// the way Explorer would.
pub fn shell_open(target: &str, args: &str) -> Result<()> {
    let r = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            &HSTRING::from(target),
            &HSTRING::from(args),
            windows::core::PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if r.0 as isize <= 32 {
        anyhow::bail!("could not open {target} (code {})", r.0 as isize);
    }
    Ok(())
}

/// Shows Explorer's Properties dialog for a file or folder (non-blocking).
pub fn show_properties(path: &Path) -> Result<()> {
    ensure_com();
    let ok = unsafe {
        windows::Win32::UI::Shell::SHObjectProperties(
            None,
            windows::Win32::UI::Shell::SHOP_FILEPATH,
            &HSTRING::from(path.as_os_str()),
            windows::core::PCWSTR::null(),
        )
    };
    if !ok.as_bool() {
        anyhow::bail!("could not show the properties of {}", path.display());
    }
    Ok(())
}

/// Selects `path` in a new Explorer window.
pub fn reveal_in_explorer(path: &Path) {
    use std::os::windows::process::CommandExt;
    // Explorer wants `/select,"C:\a b\c"`; std's quoting of the whole argument breaks it.
    let _ = std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

/// What a shortcut should launch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShortcutSpec {
    pub target: PathBuf,
    pub args: String,
    pub working_dir: Option<PathBuf>,
    pub description: String,
    /// `HOTKEYF_*` modifiers in the high byte, virtual-key code in the low byte; 0 = none.
    pub hotkey: u16,
    /// Window state: 1 normal, 3 maximized, 7 minimized; 0 = Windows' default.
    pub show_cmd: i32,
}

/// Builds an in-memory shell link (also used for jump list tasks).
pub fn make_link(spec: &ShortcutSpec) -> Result<IShellLinkW> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(spec.target.as_os_str()))?;
        if !spec.args.is_empty() {
            link.SetArguments(&HSTRING::from(spec.args.as_str()))?;
        }
        let dir = spec
            .working_dir
            .clone()
            .or_else(|| spec.target.parent().map(Path::to_path_buf));
        if let Some(d) = dir {
            link.SetWorkingDirectory(&HSTRING::from(d.as_os_str()))?;
        }
        if !spec.description.is_empty() {
            link.SetDescription(&HSTRING::from(spec.description.as_str()))?;
        }
        link.SetIconLocation(&HSTRING::from(spec.target.as_os_str()), 0)?;
        if spec.hotkey != 0 {
            link.SetHotkey(spec.hotkey)?;
        }
        if spec.show_cmd != 0 {
            link.SetShowCmd(windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD(
                spec.show_cmd,
            ))?;
        }
        Ok(link)
    }
}

/// Writes a `.lnk` file.
pub fn create_shortcut(lnk: &Path, spec: &ShortcutSpec) -> Result<()> {
    let link = make_link(spec)?;
    unsafe {
        link.cast::<IPersistFile>()?
            .Save(&HSTRING::from(lnk.as_os_str()), true)?
    };
    Ok(())
}

/// Writes a `.lnk` file that opens a shell item by parsing name, e.g. a Store app as
/// `shell:AppsFolder\<AppUserModelID>`. The shortcut shows the item's own icon.
pub fn create_item_shortcut(lnk: &Path, parsing_name: &str) -> Result<()> {
    use windows::Win32::UI::Shell::{Common::ITEMIDLIST, ILFree, SHParseDisplayName};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(&HSTRING::from(parsing_name), None, &mut pidl, 0, None)?;
        let result = (|| -> Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetIDList(pidl)?;
            link.cast::<IPersistFile>()?
                .Save(&HSTRING::from(lnk.as_os_str()), true)?;
            Ok(())
        })();
        ILFree(Some(pidl));
        result
    }
}

/// Reads target, arguments and hotkey from a `.lnk` file.
pub fn read_shortcut(lnk: &Path) -> Result<ShortcutSpec> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.cast::<IPersistFile>()?
            .Load(&HSTRING::from(lnk.as_os_str()), STGM_READ)?;
        let mut buf = vec![0u16; 2048];
        let text = |buf: &[u16]| {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        };
        let target = if link.GetPath(&mut buf, std::ptr::null_mut(), 4).is_ok() {
            text(&buf)
        } else {
            String::new()
        };
        buf.fill(0);
        let args = if link.GetArguments(&mut buf).is_ok() {
            text(&buf)
        } else {
            String::new()
        };
        Ok(ShortcutSpec {
            target: PathBuf::from(crate::paths::expand_env(&target)),
            args,
            working_dir: None,
            description: String::new(),
            hotkey: link.GetHotkey().unwrap_or(0),
            show_cmd: link.GetShowCmd().map(|s| s.0).unwrap_or(0),
        })
    }
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

    #[test]
    fn create_and_read_shortcut_with_hotkey() {
        let dir = tempfile::tempdir().unwrap();
        let lnk = dir.path().join("np.lnk");
        // Ctrl+Alt+N: modifiers (HOTKEYF_CONTROL 2 | HOTKEYF_ALT 4) << 8 | 'N'.
        let spec = ShortcutSpec {
            target: PathBuf::from(r"C:\Windows\notepad.exe"),
            args: "/A test.txt".into(),
            hotkey: (6 << 8) | u16::from(b'N'),
            show_cmd: 3,
            ..Default::default()
        };
        create_shortcut(&lnk, &spec).unwrap();
        let back = read_shortcut(&lnk).unwrap();
        assert_eq!(back.target, spec.target);
        assert_eq!(back.args, spec.args);
        assert_eq!(back.hotkey, spec.hotkey);
        assert_eq!(back.show_cmd, 3);
    }
}
