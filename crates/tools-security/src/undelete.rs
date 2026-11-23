//! File Undelete: find deleted files on an NTFS volume by reading the MFT directly and
//! copy them to a *different* drive. The source volume is only ever read.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Tool, egui,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Message, format_bytes, show_message},
};

use crate::ntfs::{DeletedFile, Volume};

pub fn condition(f: &DeletedFile) -> (&'static str, bool) {
    if let crate::ntfs::Data::NonResident { complete: false, .. } = f.data {
        return ("Fragmented: only partly recoverable", false);
    }
    match f.overwritten {
        x if x <= 0.0 => ("Good", true),
        x if x < 1.0 => ("Partly overwritten", false),
        _ => ("Overwritten", false),
    }
}

fn drive_letter(p: &Path) -> Option<char> {
    let s = p.to_string_lossy();
    let mut c = s.chars();
    let letter = c.next()?;
    (c.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// Recovered files must not be written to the scanned volume (that could overwrite them).
pub fn check_destination(source: char, dest: &Path) -> Result<()> {
    match drive_letter(dest) {
        Some(d) if d == source.to_ascii_uppercase() => {
            bail!("Choose a folder on another drive than {source}: to avoid overwriting deleted data.")
        }
        Some(_) => Ok(()),
        None => bail!("Choose a folder on a local drive."),
    }
}

fn unique(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .expect("unbounded")
}

fn ntfs_drives() -> Vec<char> {
    ('C'..='Z')
        .filter(|c| Path::new(&format!("{c}:\\")).exists())
        .filter(|c| {
            let mut fs = [0u16; 16];
            let root: Vec<u16> = format!("{c}:\\")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let ok = unsafe {
                windows::Win32::Storage::FileSystem::GetVolumeInformationW(
                    windows::core::PCWSTR(root.as_ptr()),
                    None,
                    None,
                    None,
                    None,
                    Some(&mut fs),
                )
                .is_ok()
            };
            ok && String::from_utf16_lossy(&fs).trim_end_matches('\0') == "NTFS"
        })
        .collect()
}

/// (files recovered, paths created, errors)
type RecoverResult = (usize, Vec<PathBuf>, Vec<String>);

pub struct FileUndelete {
    drive: char,
    scan: Option<BackgroundTask<Vec<DeletedFile>>>,
    found: Option<(char, Vec<DeletedFile>)>,
    filter: String,
    good_only: bool,
    marked: std::collections::HashSet<u64>,
    recover: Option<BackgroundTask<RecoverResult>>,
    message: Message,
}

impl Default for FileUndelete {
    fn default() -> Self {
        Self {
            drive: 'C',
            scan: None,
            found: None,
            filter: String::new(),
            good_only: true,
            marked: Default::default(),
            recover: None,
            message: None,
        }
    }
}

impl Tool for FileUndelete {
    fn id(&self) -> &'static str {
        "undelete"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Finds recently deleted files on NTFS drives. Stop using the drive as soon as possible: \
             every new write can overwrite deleted data. Files are recovered to another drive.",
        );
        if !ctx.elevated {
            ui.weak("Reading a drive directly requires administrator rights.");
            return;
        }

        if let Some(t) = &mut self.scan {
            t.poll();
            if let Some(r) = t.take_result() {
                self.scan = None;
                match r {
                    Ok(files) => {
                        self.marked.clear();
                        self.found = Some((self.drive, files));
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if let Some(t) = &mut self.recover {
            t.poll();
            if let Some(r) = t.take_result() {
                self.recover = None;
                self.message = Some(r.and_then(|(n, created, errors)| {
                    let _ = ctx.journal.record(
                        "undelete",
                        format!("Recovered {n} deleted files"),
                        UndoAction::DeleteCreatedFiles { paths: created },
                    );
                    if errors.is_empty() {
                        Ok(format!("Recovered {n} files."))
                    } else {
                        Err(format!("Recovered {n}; failed: {}", errors.join("; ")))
                    }
                }));
            }
        }

        ui.horizontal(|ui| {
            ui.label("Drive:");
            for d in ntfs_drives() {
                ui.selectable_value(&mut self.drive, d, format!("{d}:"));
            }
            let busy = self.scan.is_some() || self.recover.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Scan")).clicked() {
                let drive = self.drive;
                self.message = None;
                self.scan = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    let v = Volume::open(drive)?;
                    p.report(0.0, format!("Reading {} MFT records…", v.record_count()));
                    Ok(v.scan(|f| {
                        p.report(f, format!("Scanning… {:.0}%", f * 100.0));
                        !p.is_cancelled()
                    }))
                }));
            }
            if let Some(t) = &self.scan {
                ui.add(
                    egui::ProgressBar::new(t.progress)
                        .desired_width(200.0)
                        .text(t.status.clone()),
                );
                if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                    t.cancel();
                }
            }
        });
        show_message(ui, &self.message);

        let Some((source, files)) = &self.found else {
            return;
        };
        let source = *source;
        let f = self.filter.to_lowercase();
        let visible: Vec<&DeletedFile> = files
            .iter()
            .filter(|x| !self.good_only || condition(x).1)
            .filter(|x| {
                f.is_empty() || x.name.to_lowercase().contains(&f) || x.path.to_lowercase().contains(&f)
            })
            .collect();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter by name or folder…"));
            ui.checkbox(&mut self.good_only, "Only files in good condition");
            ui.weak(format!("{} of {} deleted files", visible.len(), files.len()));
            ui.separator();
            let n = self.marked.len();
            if ui
                .add_enabled(
                    n > 0 && self.recover.is_none(),
                    egui::Button::new(format!("Recover {n} files…")),
                )
                .clicked()
                && let Some(dest) = rfd::FileDialog::new().pick_folder()
            {
                match check_destination(source, &dest) {
                    Err(e) => self.message = Some(Err(format!("{e:#}"))),
                    Ok(()) => {
                        let chosen: Vec<DeletedFile> = files
                            .iter()
                            .filter(|x| self.marked.contains(&x.record))
                            .cloned()
                            .collect();
                        self.recover = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                            let v = Volume::open(source)?;
                            let (mut ok, mut created, mut errors) = (0, Vec::new(), Vec::new());
                            for (i, file) in chosen.iter().enumerate() {
                                p.report(i as f32 / chosen.len() as f32, file.name.clone());
                                let out = unique(&dest, &file.name);
                                let r = File::create(&out)
                                    .context("creating output")
                                    .and_then(|fh| v.copy_data(&file.data, &mut BufWriter::new(fh)));
                                match r {
                                    Ok(()) => {
                                        ok += 1;
                                        created.push(out);
                                    }
                                    Err(e) => errors.push(format!("{}: {e:#}", file.name)),
                                }
                            }
                            Ok((ok, created, errors))
                        }));
                    }
                }
            }
        });

        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(28.0))
            .column(Column::initial(220.0).clip(true))
            .column(Column::initial(280.0).clip(true))
            .column(Column::initial(80.0))
            .column(Column::initial(120.0))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in ["", "Name", "Folder", "Size", "Modified", "Condition"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let x = visible[row.index()];
                    row.col(|ui| {
                        let mut on = self.marked.contains(&x.record);
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                self.marked.insert(x.record);
                            } else {
                                self.marked.remove(&x.record);
                            }
                        }
                    });
                    row.col(|ui| {
                        ui.label(&x.name);
                    });
                    row.col(|ui| {
                        ui.weak(&x.path);
                    });
                    row.col(|ui| {
                        ui.label(format_bytes(x.size));
                    });
                    row.col(|ui| {
                        ui.weak(&x.modified);
                    });
                    row.col(|ui| {
                        let (text, good) = condition(x);
                        if good {
                            ui.label(text);
                        } else {
                            ui.colored_label(ui.visuals().warn_fg_color, text);
                        }
                    });
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::Data;

    #[test]
    fn destination_must_be_another_drive() {
        assert!(check_destination('C', Path::new(r"C:\Recovered")).is_err());
        assert!(check_destination('c', Path::new(r"c:\x")).is_err());
        assert!(check_destination('C', Path::new(r"D:\Recovered")).is_ok());
        assert!(check_destination('C', Path::new(r"\\server\share")).is_err());
    }

    #[test]
    fn conditions() {
        let mut f = DeletedFile {
            record: 1,
            name: "a".into(),
            path: "\\".into(),
            size: 1,
            modified: String::new(),
            data: Data::Resident(vec![1]),
            overwritten: 0.0,
        };
        assert_eq!(condition(&f), ("Good", true));
        f.overwritten = 0.5;
        assert!(!condition(&f).1);
        f.data = Data::NonResident {
            runs: vec![],
            size: 1,
            complete: false,
        };
        f.overwritten = 0.0;
        assert!(condition(&f).0.starts_with("Fragmented"));
    }

    #[test]
    fn finds_ntfs_system_drive() {
        assert!(ntfs_drives().contains(&'C'));
    }
}
