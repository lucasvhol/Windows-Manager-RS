//! Helpers for running external Windows tools (reg, powershell, dism, …).

use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`Command`] that does not flash a console window.
pub fn hidden_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
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
