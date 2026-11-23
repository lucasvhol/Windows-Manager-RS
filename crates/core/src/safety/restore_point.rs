//! System Restore checkpoints. Requires administrator rights and System Protection
//! enabled on the system drive. Note: Windows silently skips creating a new point if
//! one was created in the last 24 h (`SystemRestorePointCreationFrequency`).

use anyhow::Result;

use crate::process::{hidden_command, run_checked};

/// Creates a restore point with the given description (blocking; may take a minute).
pub fn create(description: &str) -> Result<()> {
    // Single quotes in a PowerShell literal are escaped by doubling.
    let desc = description.replace('\'', "''");
    run_checked(hidden_command("powershell").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!(
            "Checkpoint-Computer -Description '{desc}' -RestorePointType MODIFY_SETTINGS -ErrorAction Stop"
        ),
    ]))?;
    tracing::info!("restore point created: {description}");
    Ok(())
}
