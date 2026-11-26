//! File Undelete: find deleted files on an NTFS volume by reading the MFT directly and
//! copy them to a *different* drive. The source volume is only ever read.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Message, ToolButton, empty_state, format_bytes, frame, section_nav, show_message},
};

use crate::carve::{self, RawVolume};
use crate::ntfs::{Data, DeletedFile, Volume};

pub fn condition(f: &DeletedFile) -> (&'static str, bool) {
    match f.data {
        Data::NonResident { complete: false, .. } => {
            return ("Fragmented: only partly recoverable", false);
        }
        Data::Raw { .. } => return ("Found by content (whole if stored in one piece)", true),
        _ => {}
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileType {
    #[default]
    All,
    Pictures,
    Documents,
    Videos,
    Music,
    Archives,
    Programs,
}

impl FileType {
    pub const ALL: [FileType; 7] = [
        FileType::All,
        FileType::Pictures,
        FileType::Documents,
        FileType::Videos,
        FileType::Music,
        FileType::Archives,
        FileType::Programs,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileType::All => "All types",
            FileType::Pictures => "Pictures",
            FileType::Documents => "Documents",
            FileType::Videos => "Videos",
            FileType::Music => "Music",
            FileType::Archives => "Archives",
            FileType::Programs => "Programs",
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            FileType::All => &[],
            FileType::Pictures => &[
                "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "raw", "cr2", "nef",
                "dng", "svg", "ico", "psd",
            ],
            FileType::Documents => &[
                "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pdf", "txt", "rtf", "odt", "ods", "odp", "csv",
                "md", "epub", "pages", "key", "numbers",
            ],
            FileType::Videos => &[
                "mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "mpg", "mpeg", "flv", "3gp",
            ],
            FileType::Music => &["mp3", "wav", "flac", "aac", "m4a", "ogg", "wma", "opus", "aiff"],
            FileType::Archives => &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "cab"],
            FileType::Programs => &["exe", "msi", "dll", "bat", "cmd", "ps1", "jar", "appx", "msix"],
        }
    }

    pub fn matches(self, name: &str) -> bool {
        if self == FileType::All {
            return true;
        }
        let ext = Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        self.extensions().contains(&ext.as_str())
    }
}

/// Where a recovered file goes: straight into `dest`, or into the folders it was in
/// (rebuilt under `dest`; unknown parts become "Unknown folder").
pub fn output_path(dest: &Path, file: &DeletedFile, keep_folders: bool) -> PathBuf {
    let clean = |s: &str| -> String {
        let c: String = s
            .chars()
            .map(|c| {
                if r#"\/:*?"<>|"#.contains(c) || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let c = c.trim().trim_end_matches('.').to_owned();
        if c.is_empty() || c == "." || c == ".." {
            "_".into()
        } else {
            c
        }
    };
    let mut dir = dest.to_path_buf();
    if keep_folders {
        for part in file.path.split('\\').filter(|p| !p.is_empty()) {
            dir.push(if part == "?" {
                "Unknown folder".to_owned()
            } else {
                clean(part)
            });
        }
    }
    unique(&dir, &clean(&file.name))
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

/// Drives with their file system ("NTFS", "exFAT"…; empty when Windows can't read it,
/// as on a damaged drive). The MFT scan needs NTFS; the deep scan reads any drive.
fn drives() -> Vec<(char, String)> {
    ('C'..='Z')
        .filter(|c| Path::new(&format!("{c}:\\")).exists())
        .map(|c| {
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
            let name = if ok {
                String::from_utf16_lossy(&fs).trim_end_matches('\0').to_owned()
            } else {
                String::new()
            };
            (c, name)
        })
        .collect()
}

/// Turns deep-scan results into list rows ("Found 0001.jpg"); their ids can't clash
/// with MFT record numbers.
pub fn carved_rows(found: &[carve::Found]) -> Vec<DeletedFile> {
    found
        .iter()
        .enumerate()
        .map(|(i, f)| DeletedFile {
            record: (1 << 63) | f.offset,
            name: format!("Found {:04}.{}", i + 1, f.kind.ext()),
            path: String::new(),
            size: f.len,
            modified: String::new(),
            data: Data::Raw {
                offset: f.offset,
                len: f.len,
            },
            overwritten: 0.0,
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
    file_type: FileType,
    keep_folders: bool,
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
            file_type: FileType::All,
            keep_folders: true,
            marked: Default::default(),
            recover: None,
            message: None,
        }
    }
}

impl FileUndelete {
    fn start_scan(&mut self, ui: &egui::Ui, deep: bool) {
        let drive = self.drive;
        self.message = None;
        self.scan = Some(if deep {
            BackgroundTask::spawn(ui.ctx(), move |p| {
                let v = RawVolume::open(drive)?;
                let found = carve::scan(&v, |f, n| {
                    p.report(
                        f,
                        format!("Reading the drive… {:.0}%, {n} files found", f * 100.0),
                    );
                    !p.is_cancelled()
                });
                Ok(carved_rows(&found))
            })
        } else {
            BackgroundTask::spawn(ui.ctx(), move |p| {
                let v = Volume::open(drive)?;
                p.report(0.0, format!("Reading {} MFT records…", v.record_count()));
                Ok(v.scan(|f| {
                    p.report(f, format!("Scanning… {:.0}%", f * 100.0));
                    !p.is_cancelled()
                }))
            })
        });
    }

    fn start_recover(&mut self, ui: &egui::Ui) {
        let Some((source, files)) = &self.found else {
            return;
        };
        let source = *source;
        let Some(dest) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        if let Err(e) = check_destination(source, &dest) {
            self.message = Some(Err(format!("{e:#}")));
            return;
        }
        let chosen: Vec<DeletedFile> = files
            .iter()
            .filter(|x| self.marked.contains(&x.record))
            .cloned()
            .collect();
        let keep = self.keep_folders;
        self.recover = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
            let carved = chosen.iter().all(|f| matches!(f.data, Data::Raw { .. }));
            let (v, raw) = if carved {
                (None, Some(RawVolume::open(source)?))
            } else {
                (Some(Volume::open(source)?), None)
            };
            let (mut ok, mut created, mut errors) = (0, Vec::new(), Vec::new());
            for (i, file) in chosen.iter().enumerate() {
                p.report(i as f32 / chosen.len() as f32, file.name.clone());
                let out = output_path(&dest, file, keep);
                if let Some(parent) = out.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let r = File::create(&out).context("creating output").and_then(|fh| {
                    let mut w = BufWriter::new(fh);
                    match (&file.data, &v, &raw) {
                        (Data::Raw { offset, len }, _, Some(raw)) => carve::copy(raw, *offset, *len, &mut w),
                        (data, Some(v), _) => v.copy_data(data, &mut w),
                        _ => bail!("mixed scan results"),
                    }
                    .and_then(|()| std::io::Write::flush(&mut w).map_err(Into::into))
                });
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

    fn poll(&mut self, ctx: &mut AppCtx<'_>) {
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

    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.poll(ctx);
        let drives = if ctx.elevated { drives() } else { Vec::new() };
        let labels: Vec<String> = drives
            .iter()
            .map(|(d, fs)| {
                if fs.is_empty() {
                    format!("{d}:  (unreadable)")
                } else {
                    format!("{d}:  {fs}")
                }
            })
            .collect();
        let nav: Vec<(&str, &str)> = labels.iter().map(|l| (icons::HARD_DRIVES, l.as_str())).collect();
        let mut sel = drives.iter().position(|(d, _)| *d == self.drive).unwrap_or(0);
        frame::left(ui, 170.0, |ui| {
            ui.weak("Drive");
            ui.add_space(4.0);
            section_nav(ui, &nav, &mut sel)
        });
        if let Some((d, _)) = drives.get(sel) {
            self.drive = *d;
        }
        let is_ntfs = drives.iter().any(|(d, fs)| *d == self.drive && fs == "NTFS");
        let busy = self.scan.is_some() || self.recover.is_some();
        let n = self.marked.len();
        let recover_label = format!("Recover {n}…");
        let ok = ctx.elevated && !busy;
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Scan").enabled(ok && is_ntfs)),
                Some(ToolButton::new(icons::FILE_SEARCH, "Deep scan").enabled(ok)),
                Some(ToolButton::new(icons::STOP, "Cancel").enabled(self.scan.is_some())),
                None,
                Some(ToolButton::new(icons::DOWNLOAD, &recover_label).enabled(ok && n > 0)),
            ],
            |_| {},
        ) {
            Some(0) => self.start_scan(ui, false),
            Some(1) => self.start_scan(ui, true),
            Some(2) => {
                if let Some(t) = &self.scan {
                    t.cancel();
                }
            }
            Some(4) => self.start_recover(ui),
            _ => {}
        }
        frame::options(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  Filter by name or folder", icons::MAGNIFYING_GLASS))
                    .desired_width(200.0),
            );
            egui::ComboBox::from_id_salt("undelete-type")
                .selected_text(self.file_type.label())
                .show_ui(ui, |ui| {
                    for t in FileType::ALL {
                        ui.selectable_value(&mut self.file_type, t, t.label());
                    }
                });
            ui.checkbox(&mut self.good_only, "Only files in good condition");
            ui.checkbox(&mut self.keep_folders, "Recover with folders")
                .on_hover_text("Rebuilds the folders the files were in, inside the destination.");
        });

        let f = self.filter.to_lowercase();
        let visible: Vec<usize> = self
            .found
            .as_ref()
            .map(|(_, files)| {
                files
                    .iter()
                    .enumerate()
                    .filter(|(_, x)| !self.good_only || condition(x).1)
                    .filter(|(_, x)| self.file_type.matches(&x.name))
                    .filter(|(_, x)| {
                        f.is_empty()
                            || x.name.to_lowercase().contains(&f)
                            || x.path.to_lowercase().contains(&f)
                    })
                    .map(|(i, _)| i)
                    .collect()
            })
            .unwrap_or_default();
        let status = match &self.found {
            Some((d, files)) => format!(
                "{} of {} deleted files on {d}:, {n} marked",
                visible.len(),
                files.len()
            ),
            None => "Stop using the drive as soon as possible: every new write can overwrite deleted data."
                .to_owned(),
        };
        let progress = self
            .scan
            .as_ref()
            .map(|t| (t.progress, t.status.clone()))
            .or_else(|| self.recover.as_ref().map(|t| (t.progress, t.status.clone())));
        frame::status(ui, &status, |ui| {
            if let Some((p, text)) = progress {
                ui.add(egui::ProgressBar::new(p).desired_width(260.0).text(text));
            }
        });
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            if !ctx.elevated {
                empty_state(
                    ui,
                    icons::SHIELD,
                    "Administrator rights needed",
                    "Reading a drive directly requires administrator rights. Restart the app as administrator.",
                );
                return;
            }
            let Some((_, files)) = &self.found else {
                empty_state(
                    ui,
                    icons::ARROW_COUNTER_CLOCKWISE,
                    "Pick a drive and scan it",
                    "Scan reads the NTFS file table and finds recently deleted files. Deep scan finds the files \
                     of a formatted or damaged drive by their content (names and folders are lost). \
                     Files are recovered to another drive.",
                );
                return;
            };
            let marked = &mut self.marked;
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
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
                    body.rows(22.0, visible.len(), |mut row| {
                        let x = &files[visible[row.index()]];
                        row.col(|ui| {
                            let mut on = marked.contains(&x.record);
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    marked.insert(x.record);
                                } else {
                                    marked.remove(&x.record);
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
    fn types_and_output_paths() {
        assert!(FileType::Pictures.matches("Holiday.JPG"));
        assert!(!FileType::Pictures.matches("notes.txt"));
        assert!(FileType::Documents.matches("notes.txt"));
        assert!(FileType::All.matches("anything"));
        assert!(!FileType::Music.matches("no-extension"));

        let dir = tempfile::tempdir().unwrap();
        let f = |path: &str, name: &str| DeletedFile {
            record: 1,
            name: name.into(),
            path: path.into(),
            size: 1,
            modified: String::new(),
            data: crate::ntfs::Data::Resident(vec![1]),
            overwritten: 0.0,
        };
        let flat = output_path(dir.path(), &f(r"\Users\Ana\Docs", "a.txt"), false);
        assert_eq!(flat, dir.path().join("a.txt"));
        let kept = output_path(dir.path(), &f(r"\Users\Ana\Docs", "a.txt"), true);
        assert_eq!(
            kept,
            dir.path().join("Users").join("Ana").join("Docs").join("a.txt")
        );
        let odd = output_path(dir.path(), &f(r"\?\..\x:y", "b?.txt"), true);
        assert_eq!(
            odd,
            dir.path()
                .join("Unknown folder")
                .join("_")
                .join("x_y")
                .join("b_.txt"),
            "names can't escape the destination"
        );
        std::fs::write(&flat, b"x").unwrap();
        assert_eq!(
            output_path(dir.path(), &f(r"\x", "a.txt"), false),
            dir.path().join("a (2).txt"),
            "existing files are never overwritten"
        );
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
        assert!(drives().contains(&('C', "NTFS".to_owned())));
    }

    #[test]
    fn carved_rows_recover_flat_and_never_clash() {
        let rows = carved_rows(&[carve::Found {
            offset: 4096,
            len: 10,
            kind: carve::Kind::Png,
        }]);
        assert_eq!(rows[0].name, "Found 0001.png");
        assert!(rows[0].record >= 1 << 63);
        assert!(condition(&rows[0]).1);
        assert!(FileType::Pictures.matches(&rows[0].name));
        let dest = Path::new(r"D:\Out");
        assert_eq!(output_path(dest, &rows[0], true), dest.join("Found 0001.png"));
    }
}
