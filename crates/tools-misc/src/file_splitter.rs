//! File Splitter: split a large file into numbered parts plus a manifest with BLAKE3
//! checksums, and join them back with full verification.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui,
    explorer_verb::{self, Target, Verb},
    icons,
    safety::UndoAction,
    task::{BackgroundTask, Progress},
    ui::{Message, format_bytes, frame, section_nav, show_message},
};

pub const MANIFEST_EXT: &str = "wmsplit";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub file_name: String,
    pub size: u64,
    pub part_size: u64,
    pub hash: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub name: String,
    pub size: u64,
    pub hash: String,
}

fn part_name(file_name: &str, index: usize) -> String {
    format!("{file_name}.{:03}", index + 1)
}

type Reporter<'a> = Option<&'a dyn Fn(f32, String)>;

/// Splits `source` into parts of `part_size` bytes inside `out_dir`; returns the manifest path
/// and every file created. If it fails part-way, the parts written so far are removed.
pub fn split(
    source: &Path,
    out_dir: &Path,
    part_size: u64,
    report: Reporter<'_>,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    let mut created = Vec::new();
    match split_into(source, out_dir, part_size, report, &mut created) {
        Ok(manifest) => Ok((manifest, created)),
        Err(e) => {
            for p in &created {
                let _ = std::fs::remove_file(p);
            }
            Err(e)
        }
    }
}

fn split_into(
    source: &Path,
    out_dir: &Path,
    part_size: u64,
    report: Reporter<'_>,
    created: &mut Vec<PathBuf>,
) -> Result<PathBuf> {
    if part_size == 0 {
        bail!("the part size must be greater than zero");
    }
    let file_name = source
        .file_name()
        .context("invalid file")?
        .to_string_lossy()
        .into_owned();
    let size = std::fs::metadata(source)?.len();
    let count = size.div_ceil(part_size).max(1) as usize;
    if count > 999 {
        bail!("that would create {count} parts; choose a larger part size (maximum 999 parts)");
    }
    let mut input = BufReader::new(File::open(source)?);
    let mut whole = blake3::Hasher::new();
    let mut parts = Vec::new();
    let mut buf = vec![0u8; 1024 * 1024];
    for i in 0..count {
        let name = part_name(&file_name, i);
        let path = out_dir.join(&name);
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        let mut out = BufWriter::new(File::create(&path)?);
        created.push(path);
        let mut part_hash = blake3::Hasher::new();
        let mut written = 0u64;
        while written < part_size {
            let want = (part_size - written).min(buf.len() as u64) as usize;
            let n = input.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            part_hash.update(&buf[..n]);
            whole.update(&buf[..n]);
            written += n as u64;
        }
        out.flush()?;
        parts.push(Part {
            name,
            size: written,
            hash: part_hash.finalize().to_hex().to_string(),
        });
        if let Some(r) = report {
            r(
                (i + 1) as f32 / count as f32,
                format!("Wrote part {} of {count}", i + 1),
            );
        }
    }
    let manifest = Manifest {
        file_name: file_name.clone(),
        size,
        part_size,
        hash: whole.finalize().to_hex().to_string(),
        parts,
    };
    let manifest_path = out_dir.join(format!("{file_name}.{MANIFEST_EXT}"));
    std::fs::write(&manifest_path, toml::to_string_pretty(&manifest)?)?;
    created.push(manifest_path.clone());
    Ok(manifest_path)
}

/// Verifies and joins the parts listed in a manifest into `out_dir`; returns the output path.
pub fn join(manifest_path: &Path, out_dir: &Path, report: Reporter<'_>) -> Result<PathBuf> {
    let manifest: Manifest =
        toml::from_str(&std::fs::read_to_string(manifest_path)?).context("reading the manifest")?;
    if manifest.file_name.contains(['\\', '/', ':']) {
        bail!("invalid file name in manifest");
    }
    let dir = manifest_path.parent().context("invalid manifest path")?;
    let dest = out_dir.join(&manifest.file_name);
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    let tmp = out_dir.join(format!("{}.partial", manifest.file_name));
    let result = (|| -> Result<()> {
        let mut out = BufWriter::new(File::create(&tmp)?);
        let mut whole = blake3::Hasher::new();
        let mut buf = vec![0u8; 1024 * 1024];
        for (i, part) in manifest.parts.iter().enumerate() {
            if part.name.contains(['\\', '/', ':']) {
                bail!("invalid part name in manifest");
            }
            let mut input = BufReader::new(
                File::open(dir.join(&part.name)).with_context(|| format!("part {} is missing", part.name))?,
            );
            let mut h = blake3::Hasher::new();
            let mut total = 0u64;
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                whole.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                total += n as u64;
            }
            if total != part.size || h.finalize().to_hex().as_str() != part.hash {
                bail!("part {} is damaged or incomplete", part.name);
            }
            if let Some(r) = report {
                r(
                    (i + 1) as f32 / manifest.parts.len() as f32,
                    format!("Joined part {} of {}", i + 1, manifest.parts.len()),
                );
            }
        }
        out.flush()?;
        if whole.finalize().to_hex().as_str() != manifest.hash {
            bail!("the joined file does not match the original checksum");
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// The parts and manifest a join used, for deleting them afterwards.
pub fn split_files(manifest_path: &Path) -> Result<Vec<PathBuf>> {
    let manifest: Manifest =
        toml::from_str(&std::fs::read_to_string(manifest_path)?).context("reading the manifest")?;
    let dir = manifest_path.parent().context("invalid manifest path")?;
    let mut files: Vec<PathBuf> = manifest
        .parts
        .iter()
        .filter(|p| !p.name.contains(['\\', '/', ':']))
        .map(|p| dir.join(&p.name))
        .filter(|p| p.exists())
        .collect();
    files.push(manifest_path.to_owned());
    Ok(files)
}

/// The part size, in bytes, that splits `size` into `count` parts.
pub fn size_for_count(size: u64, count: u64) -> u64 {
    size.div_ceil(count.max(1)).max(1)
}

const VERB: Verb<'static> = Verb {
    id: "WinManager.FileSplitter",
    label: "Split or join with Win Manager",
    args: "--open splitter --file",
    targets: &[Target::AnyFile],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Split,
    Join,
}

/// What a finished job did: description, files created, files to recycle if asked.
type Outcome = (String, Vec<PathBuf>, Vec<PathBuf>);

pub struct FileSplitter {
    mode: Mode,
    source: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    part_mb: f64,
    /// Split into a number of parts instead of by size.
    by_count: bool,
    count: u32,
    delete_source: bool,
    task: Option<BackgroundTask<Outcome>>,
    message: Message,
    handled_args: bool,
    menu_installed: Option<bool>,
}

impl Default for FileSplitter {
    fn default() -> Self {
        Self {
            mode: Mode::Split,
            source: None,
            out_dir: None,
            part_mb: 100.0,
            by_count: false,
            count: 4,
            delete_source: false,
            task: None,
            message: None,
            handled_args: false,
            menu_installed: None,
        }
    }
}

fn reporter<T>(p: &Progress<T>) -> impl Fn(f32, String) + '_ {
    move |f, s| p.report(f, s)
}

fn is_manifest(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(MANIFEST_EXT))
}

fn step(ui: &mut egui::Ui, n: u32, title: &str) {
    ui.add_space(6.0);
    ui.strong(format!("{n}. {title}"));
}

impl FileSplitter {
    fn choose(&mut self, p: PathBuf) {
        self.mode = if is_manifest(&p) { Mode::Join } else { Mode::Split };
        self.out_dir = p.parent().map(Path::to_path_buf);
        self.source = Some(p);
    }

    fn part_size(&self, size: u64) -> u64 {
        if self.by_count {
            size_for_count(size, self.count as u64)
        } else {
            (self.part_mb * 1024.0 * 1024.0) as u64
        }
    }

    fn finish(&mut self, ctx: &AppCtx<'_>, r: Result<Outcome, String>) {
        self.message = Some(r.map(|(desc, created, recycle)| {
            let _ = ctx.journal.record(
                "splitter",
                desc.clone(),
                UndoAction::DeleteCreatedFiles { paths: created },
            );
            if recycle.is_empty() {
                return desc;
            }
            let n = recycle.len();
            match ctx.journal.recycle(
                "splitter",
                format!("Moved {n} source file(s) to the Recycle Bin after a split or join"),
                recycle,
            ) {
                Ok(_) => format!("{desc}. The source went to the Recycle Bin."),
                Err(e) => format!("{desc}. The source couldn't be deleted: {e:#}"),
            }
        }));
        if self.message.as_ref().is_some_and(|m| m.is_ok()) {
            self.source = None;
        }
    }
}

impl Tool for FileSplitter {
    fn id(&self) -> &'static str {
        "splitter"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if !self.handled_args {
            self.handled_args = true;
            if let Some(p) = explorer_verb::arg_after("--file").filter(|p| p.is_file()) {
                self.choose(p);
            }
        }
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                self.finish(ctx, r);
            }
        }
        let mut sel = usize::from(self.mode == Mode::Join);
        frame::left(ui, 170.0, |ui| {
            section_nav(
                ui,
                &[(icons::SCISSORS, "Split a file"), (icons::STACK, "Join parts")],
                &mut sel,
            )
        });
        self.mode = if sel == 1 { Mode::Join } else { Mode::Split };
        frame::options(ui, |ui| {
            let installed = *self.menu_installed.get_or_insert_with(|| VERB.is_installed());
            let mut on = installed;
            if ui
                .checkbox(&mut on, "In Explorer's right-click menu")
                .on_hover_text(
                    "Adds \"Split or join with Win Manager\" for files; on a .wmsplit file it joins.",
                )
                .changed()
            {
                let r = if on {
                    VERB.install(ctx.journal, "splitter")
                } else {
                    VERB.uninstall(ctx.journal, "splitter")
                };
                self.message = Some(
                    r.map(|()| "Explorer menu updated.".into())
                        .map_err(|e| format!("{e:#}")),
                );
                self.menu_installed = None;
            }
        });
        frame::status(
            ui,
            if self.mode == Mode::Split {
                "Each part is checked with a BLAKE3 hash; the .wmsplit file lists them."
            } else {
                "Every part is verified before the file is rebuilt."
            },
            |_| {},
        );
        frame::central(ui, |ui| {
            let splitting = self.mode == Mode::Split;
            if self.source.as_ref().is_some_and(|p| is_manifest(p) == splitting) {
                self.source = None;
            }

            step(
                ui,
                1,
                if splitting {
                    "Choose the file to split"
                } else {
                    "Choose the .wmsplit file next to the parts"
                },
            );
            ui.horizontal(|ui| {
                ui.label(
                    self.source
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into()),
                );
                if ui.button("Choose…").clicked() {
                    let dialog = rfd::FileDialog::new();
                    let dialog = if splitting {
                        dialog
                    } else {
                        dialog.add_filter("Split manifest", &[MANIFEST_EXT])
                    };
                    if let Some(p) = dialog.pick_file() {
                        self.choose(p);
                    }
                }
            });

            let size = self
                .source
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.len());
            let mut n = 2;
            if splitting {
                step(ui, n, "Choose the part size");
                n += 1;
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.by_count, false, "Size of each part");
                    ui.add_enabled(
                        !self.by_count,
                        egui::DragValue::new(&mut self.part_mb)
                            .range(1.0..=100_000.0)
                            .speed(1.0)
                            .suffix(" MB"),
                    );
                    for (label, mb) in [
                        ("FAT32 max", 4095.0),
                        ("DVD", 4480.0),
                        ("1 GB", 1024.0),
                        ("100 MB", 100.0),
                        ("25 MB e-mail", 25.0),
                    ] {
                        if ui.small_button(label).clicked() {
                            self.part_mb = mb;
                            self.by_count = false;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.by_count, true, "Number of parts");
                    ui.add_enabled(
                        self.by_count,
                        egui::DragValue::new(&mut self.count).range(2..=999),
                    );
                });
                if let Some(size) = size {
                    let part = self.part_size(size);
                    let parts = size.div_ceil(part).max(1);
                    ui.weak(format!(
                        "{} → {parts} parts of up to {}",
                        format_bytes(size),
                        format_bytes(part)
                    ));
                }
            }

            step(ui, n, "Choose where to save");
            n += 1;
            ui.horizontal(|ui| {
                ui.label(
                    self.out_dir
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into()),
                );
                if ui.button("Change…").clicked()
                    && let Some(d) = rfd::FileDialog::new().pick_folder()
                {
                    self.out_dir = Some(d);
                }
            });
            ui.checkbox(
                &mut self.delete_source,
                if splitting {
                    "Move the original file to the Recycle Bin afterwards"
                } else {
                    "Move the parts to the Recycle Bin after a verified join"
                },
            );

            step(ui, n, if splitting { "Split" } else { "Verify and join" });
            let ready = self.source.is_some() && self.out_dir.is_some() && self.task.is_none();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(if splitting { "Split" } else { "Verify and join" }),
                    )
                    .clicked()
                {
                    if ctx.config.dry_run {
                        self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    } else if let (Some(src), Some(out)) = (self.source.clone(), self.out_dir.clone()) {
                        let part = self.part_size(size.unwrap_or(0));
                        let delete = self.delete_source;
                        self.message = None;
                        self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                            let r = reporter(p);
                            if splitting {
                                let (m, created) = split(&src, &out, part, Some(&r))?;
                                Ok((
                                    format!(
                                        "Split into {} parts; keep {} with them",
                                        created.len() - 1,
                                        m.display()
                                    ),
                                    created,
                                    if delete { vec![src] } else { Vec::new() },
                                ))
                            } else {
                                let used = if delete { split_files(&src)? } else { Vec::new() };
                                let dest = join(&src, &out, Some(&r))?;
                                Ok((
                                    format!("Joined and verified {}", dest.display()),
                                    vec![dest],
                                    used,
                                ))
                            }
                        }));
                    }
                }
                if let Some(t) = &self.task {
                    ui.add(
                        egui::ProgressBar::new(t.progress)
                            .desired_width(220.0)
                            .text(t.status.clone()),
                    );
                }
            });
            show_message(ui, &self.message);
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_join_roundtrip_with_verification() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data.bin");
        let data: Vec<u8> = (0..2_500_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let parts_dir = dir.path().join("parts");
        std::fs::create_dir_all(&parts_dir).unwrap();

        let (manifest, created) = split(&src, &parts_dir, 1_000_000, None).unwrap();
        assert_eq!(created.len(), 4, "3 parts + manifest");
        assert!(parts_dir.join("data.bin.003").exists());

        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let joined = join(&manifest, &out, None).unwrap();
        assert_eq!(std::fs::read(joined).unwrap(), data);

        // A corrupted part is detected and no output is left behind.
        let out2 = dir.path().join("out2");
        std::fs::create_dir_all(&out2).unwrap();
        let mut p2 = std::fs::read(parts_dir.join("data.bin.002")).unwrap();
        p2[10] ^= 0xFF;
        std::fs::write(parts_dir.join("data.bin.002"), p2).unwrap();
        assert!(join(&manifest, &out2, None).is_err());
        assert!(
            std::fs::read_dir(&out2).unwrap().next().is_none(),
            "no partial output"
        );
    }

    #[test]
    fn exact_multiple_and_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("x");
        std::fs::write(&src, vec![1u8; 2000]).unwrap();
        let (_, created) = split(&src, dir.path(), 1000, None).unwrap();
        assert_eq!(created.len(), 3);
        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        let out = dir.path().join("o");
        std::fs::create_dir_all(&out).unwrap();
        let (m, _) = split(&empty, &out, 1000, None).unwrap();
        let j = dir.path().join("j");
        std::fs::create_dir_all(&j).unwrap();
        assert_eq!(std::fs::read(join(&m, &j, None).unwrap()).unwrap(), b"");
        assert!(split(&src, dir.path(), 0, None).is_err());
    }

    #[test]
    fn counts_helpers_and_cleanup_on_failure() {
        assert_eq!(size_for_count(10, 3), 4);
        assert_eq!(size_for_count(9, 3), 3);
        assert_eq!(size_for_count(0, 3), 1);
        assert_eq!(size_for_count(5, 0), 5);

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.bin");
        std::fs::write(&src, vec![1u8; 10]).unwrap();
        // The third part's name is taken, so the split fails after writing two parts.
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("big.bin.003"), b"x").unwrap();
        assert!(split(&src, &out, 4, None).is_err());
        let left: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["big.bin.003"], "partial parts are removed");

        let out2 = dir.path().join("out2");
        std::fs::create_dir(&out2).unwrap();
        let (m, created) = split(&src, &out2, 4, None).unwrap();
        let mut files = split_files(&m).unwrap();
        files.sort();
        let mut expected = created.clone();
        expected.sort();
        assert_eq!(
            files, expected,
            "a join's source files are the parts plus the manifest"
        );
    }
}
