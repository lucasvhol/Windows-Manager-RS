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

/// Passed to the elevated instance so it never tries to elevate again.
pub const RELAUNCHED: &str = "--elevated-relaunch";

/// Quotes one command-line argument for `CommandLineToArgvW` parsing.
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_owned();
    }
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// Starts a new elevated instance of this executable with the same arguments (shows
/// the UAC prompt). The caller should exit the current instance on success.
pub fn relaunch_as_admin() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = HSTRING::from(exe.as_os_str());
    let mut args: Vec<String> = std::env::args().skip(1).filter(|a| a != RELAUNCHED).collect();
    args.push(RELAUNCHED.to_owned());
    let args: Vec<String> = args.iter().map(|a| quote_arg(a)).collect();
    let args = HSTRING::from(args.join(" "));
    let result = unsafe { ShellExecuteW(None, w!("runas"), &exe, &args, PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecuteW returns a value > 32 on success.
    if result.0 as isize <= 32 {
        bail!("elevation was cancelled or failed (code {})", result.0 as isize);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("--open"), "--open");
        assert_eq!(quote_arg(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(""), r#""""#);
    }
}
