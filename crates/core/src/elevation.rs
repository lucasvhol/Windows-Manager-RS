//! Administrator-token detection and self-relaunch via UAC.

use anyhow::{Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

/// True when the current process token is elevated.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Starts a new elevated instance of this executable (shows the UAC prompt).
/// The caller should exit the current instance on success.
pub fn relaunch_as_admin() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = HSTRING::from(exe.as_os_str());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = HSTRING::from(args.join(" "));
    let result = unsafe { ShellExecuteW(None, w!("runas"), &exe, &args, PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecuteW returns a value > 32 on success.
    if result.0 as isize <= 32 {
        bail!("elevation was cancelled or failed (code {})", result.0 as isize);
    }
    Ok(())
}
