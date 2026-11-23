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
