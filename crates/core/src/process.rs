//! Helpers for running external Windows tools (reg, powershell, dism, …).

use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`Command`] that does not flash a console window.
pub fn hidden_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    // A PSModulePath inherited from PowerShell 7 makes Windows PowerShell 5.1 load
    // incompatible modules; without it, 5.1 uses its own default module path.
    let name = std::path::Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase());
    if name.as_deref() == Some("powershell") {
        cmd.env_remove("PSModulePath");
    }
    cmd
}

/// Runs a command to completion; errors (with stderr) on non-zero exit.
pub fn run_checked(cmd: &mut Command) -> Result<Output> {
    let out = cmd.output().with_context(|| format!("spawning {cmd:?}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "{:?} failed ({}): {}",
            cmd.get_program(),
            out.status,
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        );
    }
    Ok(out)
}

/// Splits a command line into the program and the raw remainder:
/// `"C:\a b\x.exe" -min` → (`C:\a b\x.exe`, `-min`); `C:\y\y.EXE /s` → (`C:\y\y.EXE`, `/s`).
pub fn split_command_line(command: &str) -> Option<(String, String)> {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        let end = rest.find('"')?;
        let exe = &rest[..end];
        return (!exe.is_empty()).then(|| (exe.to_owned(), rest[end + 1..].trim().to_owned()));
    }
    if c.is_empty() {
        return None;
    }
    let lower = c.to_ascii_lowercase();
    let end = lower
        .find(".exe")
        .map(|i| i + 4)
        .unwrap_or_else(|| c.find(' ').unwrap_or(c.len()));
    Some((c[..end].to_owned(), c[end..].trim().to_owned()))
}

/// Just the program part of a command line.
pub fn exe_from_command(command: &str) -> Option<String> {
    split_command_line(command).map(|(exe, _)| exe)
}

fn explorer_running() -> bool {
    hidden_command("tasklist")
        .args(["/fi", "imagename eq explorer.exe", "/fo", "csv", "/nh"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("explorer.exe")
        })
}

/// Restarts the Windows shell so Explorer/taskbar settings take effect.
/// Windows usually restarts the shell by itself; we only start it if it did not.
pub fn restart_explorer() -> Result<()> {
    let _ = hidden_command("taskkill")
        .args(["/f", "/im", "explorer.exe"])
        .output();
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        if explorer_running() {
            return Ok(());
        }
    }
    Command::new("explorer.exe")
        .spawn()
        .context("starting explorer.exe")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines() {
        assert_eq!(
            split_command_line(r#""C:\Program Files\X\x.exe" -min"#),
            Some((r"C:\Program Files\X\x.exe".into(), "-min".into()))
        );
        assert_eq!(
            split_command_line(r"C:\Program Files\Y\y.EXE /background"),
            Some((r"C:\Program Files\Y\y.EXE".into(), "/background".into()))
        );
        assert_eq!(
            exe_from_command("rundll32 foo.dll,Bar").as_deref(),
            Some("rundll32")
        );
        assert_eq!(
            split_command_line(r"MsiExec.exe /I{ABC}"),
            Some(("MsiExec.exe".into(), "/I{ABC}".into()))
        );
        assert_eq!(split_command_line("  "), None);
    }
}

/// Base64 (standard alphabet, padded), used for PowerShell's `-EncodedCommand`.
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Runs a Windows PowerShell script (passed with `-EncodedCommand`, so no quoting
/// issues) and parses its output, which the script must produce with
/// `ConvertTo-Json`. Output is read as UTF-8.
pub fn powershell_json(script: &str) -> Result<serde_json::Value> {
    let full = format!(
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8\n$ProgressPreference = 'SilentlyContinue'\n{script}"
    );
    let utf16: Vec<u8> = full.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let out = run_checked(hidden_command("powershell").args([
        "-NoProfile",
        "-NonInteractive",
        "-EncodedCommand",
        &base64(&utf16),
    ]))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Ok(serde_json::Value::Array(Vec::new()));
    }
    serde_json::from_str(text).with_context(|| {
        format!(
            "reading PowerShell output: {}",
            text.chars().take(200).collect::<String>()
        )
    })
}

/// A JSON value as a list (PowerShell returns a bare object for a single result).
pub fn json_list(v: serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    }
}

#[cfg(test)]
mod ps_tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn powershell_json_roundtrip_with_quotes() {
        let v = powershell_json(
            r#"[pscustomobject]@{ A = "it's ""quoted"""; B = 3 } | ConvertTo-Json -Compress"#,
        )
        .unwrap();
        let list = json_list(v);
        assert_eq!(list[0]["A"], r#"it's "quoted""#);
        assert_eq!(list[0]["B"], 3);
    }
}
