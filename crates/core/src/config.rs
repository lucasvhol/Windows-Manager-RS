//! User settings persisted as TOML.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// BCP-47 language tag, e.g. `en-US`.
    pub language: String,
    pub theme: Theme,
    /// When true, tools only preview what they would change.
    pub dry_run: bool,
    /// Create a system restore point before bulk / system-wide operations.
    pub create_restore_points: bool,
    /// Id of the tool shown on startup.
    pub last_tool: Option<String>,
    /// Most recently opened tools, newest first (shown on the Home page).
    pub recent_tools: Vec<String>,
    /// Interface zoom (1.0 = 100 %).
    pub ui_scale: f32,
    /// Argon2id hash of the optional program password (see `password` module).
    pub password_hash: Option<String>,
    /// Hide the main window in the notification area (the rule monitor's icon) when minimised.
    pub minimize_to_tray: bool,
    /// Start elevated (UAC prompt at launch), like the original Windows Manager.
    pub run_as_admin: bool,
}

impl Config {
    /// Records that a tool was opened (keeps the 8 most recent, newest first).
    pub fn touch_recent(&mut self, id: &str) {
        self.recent_tools.retain(|t| t != id);
        self.recent_tools.insert(0, id.to_owned());
        self.recent_tools.truncate(8);
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: "en-US".into(),
            theme: Theme::System,
            dry_run: false,
            create_restore_points: true,
            last_tool: None,
            recent_tools: Vec::new(),
            ui_scale: 1.0,
            password_hash: None,
            minimize_to_tray: false,
            run_as_admin: true,
        }
    }
}

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::paths::config_dir()?.join("config.toml"))
    }

    /// Loads the config, falling back to defaults when the file is missing or invalid.
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("invalid config at {}: {e}; using defaults", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            dry_run: true,
            theme: Theme::Dark,
            last_tool: Some("journal".into()),
            ..Config::default()
        };
        cfg.save(&path).unwrap();
        assert_eq!(Config::load_or_default(&path), cfg);
    }

    #[test]
    fn missing_or_partial_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load_or_default(&path), Config::default());

        std::fs::write(&path, "dry_run = true\n").unwrap();
        let cfg = Config::load_or_default(&path);
        assert!(cfg.dry_run);
        assert_eq!(cfg.language, "en-US");
    }
}
