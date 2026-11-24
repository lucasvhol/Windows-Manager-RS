//! Options a tool remembers between sessions, one JSON file per tool in
//! `config\tools\<id>.json`. Missing or unreadable files give the defaults, so a
//! newer or older settings file never stops a tool from opening.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

fn dir() -> Result<PathBuf> {
    Ok(crate::paths::config_dir()?.join("tools"))
}

fn file_in(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn load_in<T: DeserializeOwned + Default>(dir: &Path, id: &str) -> T {
    std::fs::read_to_string(file_in(dir, id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_in<T: Serialize>(dir: &Path, id: &str, value: &T) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = file_in(dir, id);
    std::fs::write(&path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("saving {}", path.display()))
}

pub fn load<T: DeserializeOwned + Default>(id: &str) -> T {
    dir().map(|d| load_in(&d, id)).unwrap_or_default()
}

pub fn save<T: Serialize>(id: &str, value: &T) -> Result<()> {
    save_in(&dir()?, id, value)
}

/// Forgets a tool's saved options (it starts with its defaults next time).
pub fn reset(id: &str) -> Result<()> {
    match std::fs::remove_file(file_in(&dir()?, id)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Ids of tools that have saved options.
pub fn saved() -> Vec<String> {
    let Ok(d) = dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(d) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "json" {
                return None;
            }
            Some(p.file_stem()?.to_string_lossy().into_owned())
        })
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Opts {
        days: u32,
        name: String,
    }

    #[test]
    fn round_trip_and_tolerant_loading() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load_in::<Opts>(d.path(), "x"), Opts::default());
        let o = Opts {
            days: 30,
            name: "a".into(),
        };
        save_in(d.path(), "x", &o).unwrap();
        assert_eq!(load_in::<Opts>(d.path(), "x"), o);
        // Unknown and missing fields are fine; garbage falls back to defaults.
        std::fs::write(file_in(d.path(), "x"), r#"{"days":5,"extra":true}"#).unwrap();
        assert_eq!(load_in::<Opts>(d.path(), "x").days, 5);
        std::fs::write(file_in(d.path(), "x"), "not json").unwrap();
        assert_eq!(load_in::<Opts>(d.path(), "x"), Opts::default());
    }
}
