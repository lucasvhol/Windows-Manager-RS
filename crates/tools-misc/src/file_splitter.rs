//! File Splitter: split a large file into numbered parts plus a manifest with BLAKE3
//! checksums, and join them back with full verification.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Tool, egui,
    safety::UndoAction,
    task::{BackgroundTask, Progress},
    ui::{Message, format_bytes, show_message},
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
/// and every file created.
pub fn split(
    source: &Path,
    out_dir: &Path,
    part_size: u64,
    report: Reporter<'_>,
) -> Result<(PathBuf, Vec<PathBuf>)> {
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
    let mut created = Vec::new();
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
    Ok((manifest_path, created))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Split,
    Join,
}

pub struct FileSplitter {
    mode: Mode,
    source: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    part_mb: f64,
    task: Option<BackgroundTask<(String, Vec<PathBuf>)>>,
    message: Message,
}

impl Default for FileSplitter {
    fn default() -> Self {
        Self {
            mode: Mode::Split,
            source: None,
            out_dir: None,
            part_mb: 100.0,
            task: None,
            message: None,
        }
    }
}

fn reporter<T>(p: &Progress<T>) -> impl Fn(f32, String) + '_ {
    move |f, s| p.report(f, s)
}

impl Tool for FileSplitter {
    fn id(&self) -> &'static str {
        "splitter"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                self.message = Some(r.map(|(desc, created)| {
                    let _ = ctx.journal.record(
                        "splitter",
                        desc.clone(),
                        UndoAction::DeleteCreatedFiles { paths: created },
                    );
                    desc
                }));
            }
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, Mode::Split, "Split a file");
            ui.selectable_value(&mut self.mode, Mode::Join, "Join parts");
        });
        ui.separator();
        let splitting = self.mode == Mode::Split;
        ui.horizontal(|ui| {
            let label = if splitting {
                "File to split:"
            } else {
                "Manifest (.wmsplit):"
            };
            ui.label(label);
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
                    self.out_dir = p.parent().map(Path::to_path_buf);
                    self.source = Some(p);
                }
            }
        });
        if splitting
            && let Some(size) = self
                .source
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
        {
            ui.horizontal(|ui| {
                ui.label("Part size:");
                ui.add(
                    egui::DragValue::new(&mut self.part_mb)
                        .range(1.0..=100_000.0)
                        .speed(1.0)
                        .suffix(" MB"),
                );
                for (label, mb) in [
                    ("FAT32 max", 4095.0),
                    ("1 GB", 1024.0),
                    ("100 MB", 100.0),
                    ("25 MB e-mail", 25.0),
                ] {
                    if ui.small_button(label).clicked() {
                        self.part_mb = mb;
                    }
                }
                let parts = size.div_ceil((self.part_mb * 1024.0 * 1024.0) as u64).max(1);
                ui.weak(format!("{} → {parts} parts", format_bytes(size)));
            });
        }
        ui.horizontal(|ui| {
            ui.label("Output folder:");
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
                    let part = (self.part_mb * 1024.0 * 1024.0) as u64;
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
                            ))
                        } else {
                            let dest = join(&src, &out, Some(&r))?;
                            Ok((format!("Joined and verified {}", dest.display()), vec![dest]))
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
}
