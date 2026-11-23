//! Persistent log of every change the app makes, each with an undo action.
//!
//! Stored as JSON (one file) in the data directory; registry backups are `.reg`
//! exports placed next to it in `backups/`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::process::{hidden_command, run_checked};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoAction {
    /// Re-import a `.reg` export taken before the change.
    ImportRegFile { path: PathBuf },
    /// The key did not exist before the change; undo deletes it.
    DeleteRegKey { key: String },
    /// Put individual registry values back exactly (deleting ones that did not exist).
    RestoreRegValues {
        values: Vec<crate::registry::ValueSnapshot>,
    },
    /// Files were moved to the Recycle Bin; undo restores them to their original paths.
    RestoreFromRecycleBin { paths: Vec<PathBuf> },
    /// Files were moved `(from, to)`; undo moves each `to` back to `from`.
    MoveBack { moves: Vec<(PathBuf, PathBuf)> },
    /// Files were created by the change; undo moves them to the Recycle Bin.
    DeleteCreatedFiles { paths: Vec<PathBuf> },
    /// A file was modified; undo copies the pre-change `backup` over `original`.
    RestoreFile { original: PathBuf, backup: PathBuf },
    /// Several undo steps, executed in the given order (stops at the first failure).
    Sequence { steps: Vec<UndoAction> },
    /// Re-apply a file's permissions saved as SDDL (see [`crate::acl`]).
    RestoreAcl { path: PathBuf, sddl: String },
    /// Run a command to revert (e.g. `sc config … start= auto`).
    RunCommand { program: String, args: Vec<String> },
    /// The change cannot be reverted automatically (recorded for the audit trail only).
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub time: DateTime<Local>,
    pub tool: String,
    pub description: String,
    pub undo: UndoAction,
    pub undone: bool,
}

pub struct ChangeJournal {
    file: PathBuf,
    backups: PathBuf,
    entries: Mutex<Vec<Entry>>,
}

impl ChangeJournal {
    /// Opens the journal in the default per-user data directory.
    pub fn open_default() -> Result<Self> {
        Self::open(
            &crate::paths::data_dir()?.join("journal.json"),
            &crate::paths::backups_dir()?,
        )
    }

    pub fn open(file: &Path, backups: &Path) -> Result<Self> {
        std::fs::create_dir_all(backups)?;
        let entries = match std::fs::read_to_string(file) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing journal {}", file.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).context("reading journal"),
        };
        Ok(Self {
            file: file.to_owned(),
            backups: backups.to_owned(),
            entries: Mutex::new(entries),
        })
    }

    /// Snapshot of all entries, newest first.
    pub fn entries(&self) -> Vec<Entry> {
        let mut v = self.entries.lock().unwrap().clone();
        v.reverse();
        v
    }

    fn persist(&self, entries: &[Entry]) -> Result<()> {
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(entries)?)?;
        std::fs::rename(&tmp, &self.file).context("saving journal")
    }

    /// Records a change that has already been applied.
    pub fn record(&self, tool: &str, description: impl Into<String>, undo: UndoAction) -> Result<Entry> {
        let mut entries = self.entries.lock().unwrap();
        let entry = Entry {
            id: entries.iter().map(|e| e.id).max().map_or(1, |m| m + 1),
            time: Local::now(),
            tool: tool.to_owned(),
            description: description.into(),
            undo,
            undone: false,
        };
        entries.push(entry.clone());
        self.persist(&entries)?;
        tracing::info!(tool, id = entry.id, "journaled: {}", entry.description);
        Ok(entry)
    }

    /// Backs up a registry key (e.g. `HKCU\Software\Foo`) *before* it is modified and
    /// returns the undo action to pass to [`Self::record`].
    pub fn backup_reg_key(&self, key: &str) -> Result<UndoAction> {
        let exists = hidden_command("reg")
            .args(["query", key])
            .output()?
            .status
            .success();
        if !exists {
            return Ok(UndoAction::DeleteRegKey { key: key.to_owned() });
        }
        let stamp = Local::now().format("%Y%m%d-%H%M%S%.3f");
        let safe: String = key
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .take(80)
            .collect();
        let path = self.backups.join(format!("{stamp}-{safe}.reg"));
        run_checked(hidden_command("reg").args(["export", key]).arg(&path).arg("/y"))?;
        Ok(UndoAction::ImportRegFile { path })
    }

    /// Copies a file to the backups folder *before* it is modified; returns the undo action.
    pub fn backup_file(&self, path: &Path) -> Result<UndoAction> {
        let stamp = Local::now().format("%Y%m%d-%H%M%S%.3f");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let backup = self.backups.join(format!("{stamp}-{name}"));
        std::fs::copy(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
        Ok(UndoAction::RestoreFile {
            original: path.to_owned(),
            backup,
        })
    }

    /// Moves files/folders to the Recycle Bin and journals it.
    pub fn recycle(&self, tool: &str, description: impl Into<String>, paths: Vec<PathBuf>) -> Result<Entry> {
        trash::delete_all(&paths).context("moving to Recycle Bin")?;
        self.record(tool, description, UndoAction::RestoreFromRecycleBin { paths })
    }

    /// Reverts the change with the given id.
    pub fn undo(&self, id: u64) -> Result<()> {
        let entry = {
            let entries = self.entries.lock().unwrap();
            entries
                .iter()
                .find(|e| e.id == id)
                .cloned()
                .context("no such journal entry")?
        };
        if entry.undone {
            bail!("change #{id} was already undone");
        }
        execute_undo(&entry.undo)?;
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
            e.undone = true;
        }
        self.persist(&entries)
    }
}

fn execute_undo(action: &UndoAction) -> Result<()> {
    match action {
        UndoAction::ImportRegFile { path } => {
            run_checked(hidden_command("reg").arg("import").arg(path))?;
        }
        UndoAction::DeleteRegKey { key } => {
            run_checked(hidden_command("reg").args(["delete", key, "/f"]))?;
        }
        UndoAction::RestoreRegValues { values } => {
            // Restore in reverse order so repeated values end at their earliest state.
            for v in values.iter().rev() {
                crate::registry::restore(v)?;
            }
        }
        UndoAction::RestoreFromRecycleBin { paths } => {
            let items: Vec<_> = trash::os_limited::list()?
                .into_iter()
                .filter(|item| paths.iter().any(|p| item.original_path() == *p))
                .collect();
            if items.is_empty() {
                bail!("items are no longer in the Recycle Bin");
            }
            trash::os_limited::restore_all(items).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        UndoAction::DeleteCreatedFiles { paths } => {
            let existing: Vec<&PathBuf> = paths.iter().filter(|p| p.exists()).collect();
            if !existing.is_empty() {
                trash::delete_all(existing).context("moving created files to the Recycle Bin")?;
            }
        }
        UndoAction::RestoreAcl { path, sddl } => crate::acl::set_dacl_sddl(path, sddl)?,
        UndoAction::Sequence { steps } => {
            for step in steps {
                execute_undo(step)?;
            }
        }
        UndoAction::RestoreFile { original, backup } => {
            std::fs::copy(backup, original)
                .with_context(|| format!("restoring {} from {}", original.display(), backup.display()))?;
        }
        UndoAction::MoveBack { moves } => {
            let mut failed = Vec::new();
            for (from, to) in moves {
                if from.exists() {
                    failed.push(format!("{} (destination exists)", from.display()));
                } else if let Err(e) = std::fs::rename(to, from) {
                    failed.push(format!("{} ({e})", to.display()));
                }
            }
            if !failed.is_empty() {
                bail!("could not move back: {}", failed.join("; "));
            }
        }
        UndoAction::RunCommand { program, args } => {
            run_checked(hidden_command(program).args(args))?;
        }
        UndoAction::None => bail!("this change cannot be undone automatically"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(dir: &Path) -> ChangeJournal {
        ChangeJournal::open(&dir.join("journal.json"), &dir.join("backups")).unwrap()
    }

    #[test]
    fn record_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let j = journal(dir.path());
        j.record("test", "first", UndoAction::None).unwrap();
        j.record("test", "second", UndoAction::None).unwrap();

        let reloaded = journal(dir.path());
        let entries = reloaded.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].description, "second"); // newest first
        assert_eq!(entries[0].id, 2);
    }

    #[test]
    fn undo_none_fails_and_unknown_id_fails() {
        let dir = tempfile::tempdir().unwrap();
        let j = journal(dir.path());
        let e = j.record("test", "x", UndoAction::None).unwrap();
        assert!(j.undo(e.id).is_err());
        assert!(!j.entries()[0].undone);
        assert!(j.undo(999).is_err());
    }

    #[test]
    fn registry_backup_and_undo_roundtrip() {
        // Uses a throwaway key under HKCU; safe and needs no elevation.
        let key = format!(r"HKCU\Software\win-manager-rs-test-{}", std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let j = journal(dir.path());
        let reg = |args: &[&str]| run_checked(hidden_command("reg").args(args)).unwrap();

        reg(&["add", &key, "/v", "Value", "/t", "REG_SZ", "/d", "original", "/f"]);
        let undo = j.backup_reg_key(&key).unwrap();
        assert!(matches!(undo, UndoAction::ImportRegFile { .. }));
        reg(&["add", &key, "/v", "Value", "/t", "REG_SZ", "/d", "changed", "/f"]);
        let e = j.record("test", "change value", undo).unwrap();

        j.undo(e.id).unwrap();
        let out = reg(&["query", &key, "/v", "Value"]);
        let text = String::from_utf8_lossy(&out.stdout);
        reg(&["delete", &key, "/f"]);
        assert!(text.contains("original"), "{text}");
        assert!(j.entries()[0].undone);
    }

    #[test]
    fn backup_of_missing_key_undo_deletes_it() {
        let key = format!(r"HKCU\Software\win-manager-rs-test-new-{}", std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let j = journal(dir.path());
        let undo = j.backup_reg_key(&key).unwrap();
        assert_eq!(undo, UndoAction::DeleteRegKey { key: key.clone() });

        run_checked(hidden_command("reg").args(["add", &key, "/f"])).unwrap();
        let e = j.record("test", "create key", undo).unwrap();
        j.undo(e.id).unwrap();
        assert!(
            !hidden_command("reg")
                .args(["query", &key])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}
