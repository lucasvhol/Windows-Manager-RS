//! Security Lock: password-encrypt files and folders (AES-256-GCM, Argon2id key).
//!
//! Each file becomes `name.ext.wmlock`. The encrypted copy is written to a temporary
//! file, verified by decrypting it, and only then renamed into place and (optionally)
//! the original deleted. There is no password recovery.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, frame, section, show_message},
};
use zeroize::Zeroize;

use crate::crypto::{self, KdfParams};

pub const EXT: &str = "wmlock";

fn is_locked(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXT))
}

/// Files a job applies to: a file itself, or every matching file under a folder.
pub fn collect(targets: &[PathBuf], encrypting: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for t in targets {
        let files: Vec<PathBuf> = if t.is_dir() {
            WalkDir::new(t)
                .follow_links(false)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect()
        } else {
            vec![t.clone()]
        };
        out.extend(files.into_iter().filter(|f| is_locked(f) != encrypting));
    }
    out.sort();
    out.dedup();
    out
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// `report.pdf.wmlock` → `report.pdf`, or `report (decrypted).pdf` if that exists.
pub fn decrypted_name(locked: &Path) -> PathBuf {
    let plain = locked.with_extension("");
    if !plain.exists() {
        return plain;
    }
    let stem = plain
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = plain
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| {
            let tag = if n == 1 {
                " (decrypted)".to_owned()
            } else {
                format!(" (decrypted {n})")
            };
            plain.with_file_name(format!("{stem}{tag}{ext}"))
        })
        .find(|c| !c.exists())
        .expect("unbounded")
}

pub fn encrypt_file(
    path: &Path,
    password: &str,
    params: KdfParams,
    delete_original: bool,
) -> Result<PathBuf> {
    let dest = with_suffix(path, &format!(".{EXT}"));
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    let tmp = with_suffix(&dest, ".partial");
    let result: Result<()> = (|| {
        let input = BufReader::new(File::open(path).context("opening file")?);
        crypto::encrypt(input, BufWriter::new(File::create(&tmp)?), password, params)?;
        // Verify before touching the original.
        crypto::decrypt(BufReader::new(File::open(&tmp)?), std::io::sink(), password)
            .context("verification failed")?;
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    if delete_original {
        std::fs::remove_file(path).context("deleting the original")?;
    }
    Ok(dest)
}

pub fn decrypt_file(path: &Path, password: &str, delete_locked: bool) -> Result<PathBuf> {
    let dest = decrypted_name(path);
    let tmp = with_suffix(&dest, ".partial");
    let result: Result<()> = (|| {
        crypto::decrypt(
            BufReader::new(File::open(path)?),
            BufWriter::new(File::create(&tmp)?),
            password,
        )?;
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    if delete_locked {
        std::fs::remove_file(path)?;
    }
    Ok(dest)
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub done: usize,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Encrypt,
    Decrypt,
}

pub struct SecurityLock {
    mode: Mode,
    targets: Vec<PathBuf>,
    password: String,
    confirm_password: String,
    delete_source: bool,
    task: Option<(Mode, BackgroundTask<Report>)>,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for SecurityLock {
    fn default() -> Self {
        Self {
            mode: Mode::Encrypt,
            targets: Vec::new(),
            password: String::new(),
            confirm_password: String::new(),
            delete_source: true,
            task: None,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl SecurityLock {
    fn start(&mut self, ui: &egui::Ui) {
        let files = collect(&self.targets, self.mode == Mode::Encrypt);
        let mut password = std::mem::take(&mut self.password);
        self.confirm_password.zeroize();
        let (mode, delete) = (self.mode, self.delete_source);
        self.task = Some((
            mode,
            BackgroundTask::spawn(ui.ctx(), move |p| {
                let mut report = Report::default();
                let n = files.len().max(1) as f32;
                for (i, f) in files.iter().enumerate() {
                    if p.is_cancelled() {
                        break;
                    }
                    p.report(i as f32 / n, format!("{}…", f.display()));
                    let r = match mode {
                        Mode::Encrypt => encrypt_file(f, &password, KdfParams::DEFAULT, delete).map(|_| ()),
                        Mode::Decrypt => decrypt_file(f, &password, delete).map(|_| ()),
                    };
                    match r {
                        Ok(()) => report.done += 1,
                        Err(e) => report.failed.push(format!("{}: {e:#}", f.display())),
                    }
                }
                password.zeroize();
                Ok(report)
            }),
        ));
    }
}

impl Tool for SecurityLock {
    fn id(&self) -> &'static str {
        "securitylock"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.poll(ui, ctx);
        let idle = self.task.is_none();
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add files").enabled(idle)),
                Some(ToolButton::new(icons::FOLDERS, "Add folder").enabled(idle)),
                Some(ToolButton::new(icons::TRASH, "Clear list").enabled(idle && !self.targets.is_empty())),
            ],
            |_| {},
        ) {
            Some(0) => {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    self.targets.extend(files);
                }
            }
            Some(1) => {
                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                    self.targets.push(d);
                }
            }
            Some(2) => self.targets.clear(),
            _ => {}
        }
        frame::options(ui, |ui| self.mode_picker(ui));
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if idle && !dropped.is_empty() {
            self.targets.extend(dropped);
        }
        self.status(ui);
        frame::central(ui, |ui| {
            section(
                ui,
                "Files and folders",
                |_| {},
                |ui| {
                    if self.targets.is_empty() {
                        ui.weak("Add files or folders, or drop them on this window.");
                    }
                    for t in &self.targets {
                        let icon = if t.is_dir() {
                            icons::FOLDERS
                        } else {
                            icons::FILE_LOCK
                        };
                        ui.label(format!("{icon}  {}", t.display()));
                    }
                },
            );
            ui.add_space(8.0);
            self.body(ui, ctx);
        });
    }
}

impl SecurityLock {
    /// Sets Encrypt (true) or Decrypt (false).
    pub fn set_encrypt(&mut self, encrypt: bool) {
        if self.task.is_none() {
            self.mode = if encrypt { Mode::Encrypt } else { Mode::Decrypt };
        }
    }

    /// Asks to run the job (like the form's own button); returns why it can't yet.
    pub fn request(&mut self) -> Result<(), String> {
        let encrypting = self.mode == Mode::Encrypt;
        let files = collect(&self.targets, encrypting);
        if self.task.is_some() {
            return Err("A job is already running.".into());
        }
        if files.is_empty() {
            return Err(if encrypting {
                "Add the files or folders to encrypt first.".into()
            } else {
                "Add .wmlock files (or folders holding them) first.".into()
            });
        }
        if self.password.is_empty() {
            return Err("Enter the password below first.".into());
        }
        if encrypting && self.password != self.confirm_password {
            return Err("The passwords do not match.".into());
        }
        let text = if encrypting {
            format!(
                "Encrypt {} files? Without the password they cannot be recovered by anyone, including you.",
                files.len()
            )
        } else {
            format!("Decrypt {} files?", files.len())
        };
        self.confirm.ask(text, ());
        Ok(())
    }

    /// Uses a list kept elsewhere (Files and Folders); ignored while a job runs.
    pub fn set_targets(&mut self, targets: &[PathBuf]) {
        if self.task.is_none() {
            self.targets = targets.to_vec();
        }
    }

    /// Finishes a job that ended and runs one the user confirmed. Call every frame.
    pub fn poll(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        if let Some((mode, t)) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                let mode = *mode;
                self.task = None;
                self.message = Some(r.and_then(|rep| {
                    let verb = if mode == Mode::Encrypt {
                        "Encrypted"
                    } else {
                        "Decrypted"
                    };
                    let _ = ctx.journal.record(
                        "securitylock",
                        format!("{verb} {} files", rep.done),
                        UndoAction::None,
                    );
                    if rep.failed.is_empty() {
                        Ok(format!("{verb} {} files.", rep.done))
                    } else {
                        Err(format!("{verb} {}; failed: {}", rep.done, rep.failed.join("; ")))
                    }
                }));
                self.targets.clear();
            }
        }
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                self.start(ui);
            }
        }
    }

    /// Encrypt / Decrypt switch.
    pub fn mode_picker(&mut self, ui: &mut egui::Ui) {
        ui.selectable_value(&mut self.mode, Mode::Encrypt, format!("{}  Encrypt", icons::LOCK));
        ui.selectable_value(&mut self.mode, Mode::Decrypt, format!("{}  Decrypt", icons::KEY));
    }

    /// The window's status bar: what the job covers, and its progress.
    fn status(&mut self, ui: &mut egui::Ui) {
        let encrypting = self.mode == Mode::Encrypt;
        let n = collect(&self.targets, encrypting).len();
        let text = if encrypting {
            format!("{n} files to encrypt")
        } else {
            format!("{n} .wmlock files to decrypt")
        };
        frame::status(ui, &text, |ui| {
            if let Some((_, t)) = &self.task {
                if ui.button("Cancel").clicked() {
                    t.cancel();
                }
                ui.add(egui::ProgressBar::new(t.progress).desired_width(220.0));
            } else {
                ui.weak("There is no password recovery.");
            }
        });
    }

    /// Password, options and the Encrypt / Decrypt button.
    pub fn body(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let encrypting = self.mode == Mode::Encrypt;
        let files = collect(&self.targets, encrypting);
        show_message(ui, &self.message);
        let title = if encrypting {
            format!("Encrypt {} files", files.len())
        } else {
            format!("Decrypt {} files (.wmlock)", files.len())
        };
        section(
            ui,
            &title,
            |_| {},
            |ui| {
                egui::Grid::new("lock_pw")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.password)
                                .password(true)
                                .desired_width(240.0),
                        );
                        ui.end_row();
                        if encrypting {
                            ui.label("Repeat password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.confirm_password)
                                    .password(true)
                                    .desired_width(240.0),
                            );
                            ui.end_row();
                        }
                    });
                if encrypting && !self.password.is_empty() && self.password.chars().count() < 10 {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Use at least 10 characters; a passphrase of several words is best.",
                    );
                }
                ui.checkbox(
                    &mut self.delete_source,
                    if encrypting {
                        "Delete the originals after successful, verified encryption"
                    } else {
                        "Delete the .wmlock files after successful decryption"
                    },
                );
                if encrypting && self.delete_source {
                    ui.weak(
                        "Originals are deleted, not sent to the Recycle Bin. On SSDs and some file systems, \
                     deleted data may still be recoverable with special tools.",
                    );
                }
                ui.add_space(6.0);
                let mismatch = encrypting && self.password != self.confirm_password;
                let ready =
                    !files.is_empty() && !self.password.is_empty() && !mismatch && self.task.is_none();
                ui.horizontal(|ui| {
                let label = if encrypting {
                    format!("{}  Encrypt", icons::LOCK)
                } else {
                    format!("{}  Decrypt", icons::KEY)
                };
                if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                    let text = if encrypting {
                        format!(
                            "Encrypt {} files? Without the password they cannot be recovered by anyone, including you.",
                            files.len()
                        )
                    } else {
                        format!("Decrypt {} files?", files.len())
                    };
                    self.confirm.ask(text, ());
                }
                if mismatch && !self.confirm_password.is_empty() {
                    ui.colored_label(ui.visuals().error_fg_color, "The passwords do not match.");
                }
                if let Some((_, t)) = &self.task {
                    ui.add(egui::ProgressBar::new(t.progress).desired_width(200.0));
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        t.cancel();
                    }
                }
            });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_files_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(sub.join("b.bin"), vec![1u8; 5000]).unwrap();

        let plain = collect(&[dir.path().to_owned()], true);
        assert_eq!(plain.len(), 2);
        for f in &plain {
            encrypt_file(f, "pw", KdfParams::TEST, true).unwrap();
        }
        assert!(!dir.path().join("a.txt").exists());
        let locked = collect(&[dir.path().to_owned()], false);
        assert_eq!(locked.len(), 2);
        assert!(
            collect(&[dir.path().to_owned()], true).is_empty(),
            "locked files are not re-encrypted"
        );

        assert!(decrypt_file(&locked[0], "wrong", true).is_err());
        assert!(locked[0].exists(), "failed decryption keeps the locked file");
        assert!(
            !with_suffix(&decrypted_name(&locked[0]), ".partial").exists(),
            "no partial left behind"
        );

        for f in &locked {
            decrypt_file(f, "pw", true).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha"
        );
        assert_eq!(std::fs::read(sub.join("b.bin")).unwrap(), vec![1u8; 5000]);
    }

    #[test]
    fn decrypted_name_avoids_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("r.pdf.wmlock");
        assert_eq!(decrypted_name(&locked), dir.path().join("r.pdf"));
        std::fs::write(dir.path().join("r.pdf"), "x").unwrap();
        assert_eq!(decrypted_name(&locked), dir.path().join("r (decrypted).pdf"));
    }
}
