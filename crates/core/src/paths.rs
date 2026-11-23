//! Per-user directories (config, data, backups, logs).

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "win-manager-rs").context("could not determine user profile directories")
}

fn ensure(dir: PathBuf) -> Result<PathBuf> {
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `%APPDATA%\win-manager-rs\config`
pub fn config_dir() -> Result<PathBuf> {
    ensure(project_dirs()?.config_dir().to_path_buf())
}

/// `%APPDATA%\win-manager-rs\data`
pub fn data_dir() -> Result<PathBuf> {
    ensure(project_dirs()?.data_dir().to_path_buf())
}

/// Registry exports and other undo material.
pub fn backups_dir() -> Result<PathBuf> {
    ensure(data_dir()?.join("backups"))
}

pub fn logs_dir() -> Result<PathBuf> {
    ensure(data_dir()?.join("logs"))
}
