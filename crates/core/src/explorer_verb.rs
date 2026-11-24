//! Per-user Explorer context-menu entries ("Analyze with Win Manager" and similar).
//!
//! Entries live under `HKCU\Software\Classes\<target>\shell\<id>`, so no administrator
//! rights are needed, and each one runs this program with arguments plus `"%1"` (or
//! `"%V"` for a folder background).

use anyhow::{Context, Result};

use crate::registry;
use crate::safety::{ChangeJournal, UndoAction};

/// Where in Explorer the entry appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Right-click on a folder.
    Folder,
    /// Right-click on a drive.
    Drive,
    /// Right-click on empty space inside a folder.
    FolderBackground,
    /// Right-click on any file.
    AnyFile,
}

impl Target {
    fn class(self) -> &'static str {
        match self {
            Target::Folder => "Directory",
            Target::Drive => "Drive",
            Target::FolderBackground => r"Directory\Background",
            Target::AnyFile => "*",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Target::FolderBackground => "%V",
            _ => "%1",
        }
    }
}

/// One entry: `id` names the registry key (keep it unique, e.g. `WinManager.DiskAnalyzer`).
pub struct Verb<'a> {
    pub id: &'a str,
    pub label: &'a str,
    /// Arguments placed before the quoted path, e.g. `--analyze`.
    pub args: &'a str,
    pub targets: &'a [Target],
}

impl Verb<'_> {
    fn key(&self, target: Target) -> String {
        format!(r"HKCU\Software\Classes\{}\shell\{}", target.class(), self.id)
    }

    /// True when the entry is registered for every target.
    pub fn is_installed(&self) -> bool {
        self.targets
            .iter()
            .all(|t| registry::open_read(&format!(r"{}\command", self.key(*t))).is_ok())
    }

    /// Registers the entry and journals it (undo deletes the keys).
    pub fn install(&self, journal: &ChangeJournal, tool: &str) -> Result<()> {
        let exe = std::env::current_exe().context("locating the program")?;
        let mut undo = Vec::new();
        for &t in self.targets {
            let key = self.key(t);
            undo.push(journal.backup_reg_key(&key)?);
            let k = registry::open_write(&key)?;
            k.set_value("", &self.label)?;
            k.set_value("Icon", &format!("\"{}\",0", exe.display()))?;
            let cmd = registry::open_write(&format!(r"{key}\command"))?;
            cmd.set_value(
                "",
                &format!("\"{}\" {} \"{}\"", exe.display(), self.args, t.placeholder()),
            )?;
        }
        journal.record(
            tool,
            format!("Added \"{}\" to the Explorer context menu", self.label),
            UndoAction::Sequence { steps: undo },
        )?;
        Ok(())
    }

    /// Removes the entry from every target (backed up first, so it can be undone).
    pub fn uninstall(&self, journal: &ChangeJournal, tool: &str) -> Result<()> {
        let mut undo = Vec::new();
        for &t in self.targets {
            let key = self.key(t);
            if registry::open_read(&key).is_err() {
                continue;
            }
            undo.push(journal.backup_reg_key(&key)?);
            let (root, sub) = registry::split_path(&key)?;
            root.delete_subkey_all(sub)
                .with_context(|| format!("deleting {key}"))?;
        }
        journal.record(
            tool,
            format!("Removed \"{}\" from the Explorer context menu", self.label),
            UndoAction::Sequence { steps: undo },
        )?;
        Ok(())
    }
}

/// The path that followed `flag` on this process's command line, if any.
///
/// Explorer passes a drive as `"C:\"`, which the C runtime rules read as `C:"` (the
/// backslash escapes the closing quote), so a trailing quote is turned back into `\`.
pub fn arg_after(flag: &str) -> Option<std::path::PathBuf> {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| fix_drive_arg(&w[1]).into())
}

fn fix_drive_arg(arg: &str) -> String {
    match arg.strip_suffix('"') {
        Some(head) => format!("{head}\\"),
        None => arg.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn drive_argument_is_repaired() {
        assert_eq!(super::fix_drive_arg("C:\""), "C:\\");
        assert_eq!(super::fix_drive_arg(r"D:\Data"), r"D:\Data");
    }

    /// Writes a throwaway per-user entry, then removes it, and checks undo restores it.
    #[test]
    fn install_uninstall_and_undo() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = ChangeJournal::open(&dir.path().join("j.json"), dir.path()).unwrap();
        let id = format!("WinManager.Test{}", std::process::id());
        let verb = Verb {
            id: &id,
            label: "Win Manager test entry",
            args: "--test",
            targets: &[Target::Folder, Target::Drive],
        };
        assert!(!verb.is_installed());
        verb.install(&journal, "test").unwrap();
        assert!(verb.is_installed());
        let cmd = registry::open_read(&format!(r"{}\command", verb.key(Target::Drive)))
            .unwrap()
            .get_value::<String, _>("")
            .unwrap();
        assert!(cmd.ends_with(r#"--test "%1""#), "{cmd}");

        verb.uninstall(&journal, "test").unwrap();
        assert!(!verb.is_installed());
        // Undoing the removal brings the entry back; undoing the install deletes it.
        let entries = journal.entries(); // newest first
        journal.undo(entries[0].id).unwrap();
        assert!(verb.is_installed());
        journal.undo(entries[1].id).unwrap();
        assert!(!verb.is_installed());
    }
}
