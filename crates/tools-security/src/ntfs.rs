//! Minimal, read-only NTFS reader used by File Undelete.
//!
//! It parses the boot sector, MFT file records (with update-sequence fixups),
//! `$FILE_NAME` / unnamed `$DATA` attributes and data runs, and reads clusters from a
//! raw volume handle (`\\.\C:`). It never writes to the volume. All parsing is
//! bounds-checked so damaged on-disk structures yield errors, not panics.

use std::collections::HashMap;
use std::fs::File;
use std::os::windows::fs::FileExt;

use anyhow::{Context, Result, bail};

const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;
const ROOT_RECORD: u64 = 5;
const BITMAP_RECORD: u64 = 6;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    pub bytes_per_sector: u32,
    pub cluster_size: u64,
    pub mft_lcn: u64,
    pub record_size: u32,
    pub total_sectors: u64,
}

pub fn parse_boot(b: &[u8]) -> Result<BootSector> {
    if b.get(3..11) != Some(b"NTFS    ") {
        bail!("not an NTFS volume");
    }
    let bytes_per_sector = u32::from(u16_at(b, 0x0B).context("boot sector")?);
    let sectors_per_cluster = u64::from(*b.get(0x0D).context("boot sector")?);
    let cluster_size = u64::from(bytes_per_sector) * sectors_per_cluster;
    let raw = *b.get(0x40).context("boot sector")? as i8;
    let record_size = if raw > 0 {
        raw as u64 * cluster_size
    } else {
        1u64 << (-(raw as i32))
    };
    if !(512..=4096).contains(&bytes_per_sector) || cluster_size == 0 || !(256..=65536).contains(&record_size)
    {
        bail!("implausible NTFS geometry");
    }
    Ok(BootSector {
        bytes_per_sector,
        cluster_size,
        mft_lcn: u64_at(b, 0x30).context("boot sector")?,
        record_size: record_size as u32,
        total_sectors: u64_at(b, 0x28).context("boot sector")?,
    })
}

/// Validates and undoes the update-sequence protection of a FILE record in place.
pub fn apply_fixup(rec: &mut [u8], sector_size: usize) -> Result<()> {
    if rec.get(0..4) != Some(b"FILE") {
        bail!("bad record signature");
    }
    let usa_off = usize::from(u16_at(rec, 4).context("record header")?);
    let usa_count = usize::from(u16_at(rec, 6).context("record header")?);
    if usa_count == 0 || usa_off + usa_count * 2 > rec.len() || (usa_count - 1) * sector_size > rec.len() {
        bail!("bad update sequence array");
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let pos = i * sector_size - 2;
        if rec[pos..pos + 2] != usn {
            bail!("torn record (update sequence mismatch)");
        }
        rec[pos] = rec[usa_off + i * 2];
        rec[pos + 1] = rec[usa_off + i * 2 + 1];
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// `None` for sparse (unallocated, reads as zeros) runs.
    pub lcn: Option<u64>,
    pub len: u64,
}

/// Decodes an NTFS mapping-pairs array.
pub fn decode_runs(data: &[u8]) -> Result<Vec<Run>> {
    let mut runs = Vec::new();
    let mut pos = 0;
    let mut lcn: i64 = 0;
    while let Some(&header) = data.get(pos) {
        if header == 0 {
            break;
        }
        let len_size = usize::from(header & 0x0F);
        let off_size = usize::from(header >> 4);
        if len_size == 0 || len_size > 8 || off_size > 8 {
            bail!("invalid data run header");
        }
        let len_bytes = data
            .get(pos + 1..pos + 1 + len_size)
            .context("truncated data run")?;
        let mut len = 0u64;
        for (i, b) in len_bytes.iter().enumerate() {
            len |= u64::from(*b) << (8 * i);
        }
        let off_bytes = data
            .get(pos + 1 + len_size..pos + 1 + len_size + off_size)
            .context("truncated data run")?;
        if off_size == 0 {
            runs.push(Run { lcn: None, len });
        } else {
            let mut off: i64 = 0;
            for (i, b) in off_bytes.iter().enumerate() {
                off |= i64::from(*b) << (8 * i);
            }
            // Sign-extend.
            let shift = 64 - 8 * off_size as u32;
            off = (off << shift) >> shift;
            lcn = lcn.checked_add(off).context("data run overflow")?;
            if lcn < 0 {
                bail!("negative cluster number");
            }
            runs.push(Run {
                lcn: Some(lcn as u64),
                len,
            });
        }
        pos += 1 + len_size + off_size;
    }
    Ok(runs)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Resident(Vec<u8>),
    NonResident {
        runs: Vec<Run>,
        size: u64,
        complete: bool,
    },
    /// A byte range of the volume, for files found by content (deep scan).
    Raw {
        offset: u64,
        len: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileName {
    pub parent: u64,
    pub name: String,
    pub namespace: u8,
    pub modified: u64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub in_use: bool,
    pub is_dir: bool,
    /// Non-zero for extension records (belonging to another base record).
    pub base_record: u64,
    pub name: Option<FileName>,
    pub data: Option<Data>,
}

fn parse_file_name(v: &[u8]) -> Option<FileName> {
    let len = usize::from(*v.get(0x40)?);
    let raw = v.get(0x42..0x42 + len * 2)?;
    let wide: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(FileName {
        parent: u64_at(v, 0)? & 0x0000_FFFF_FFFF_FFFF,
        modified: u64_at(v, 0x10)?,
        size: u64_at(v, 0x30)?,
        namespace: *v.get(0x41)?,
        name: String::from_utf16_lossy(&wide),
    })
}

/// Parses a (fixed-up) FILE record.
pub fn parse_record(rec: &[u8]) -> Result<FileRecord> {
    if rec.get(0..4) != Some(b"FILE") {
        bail!("bad record signature");
    }
    let flags = u16_at(rec, 0x16).context("record header")?;
    let mut out = FileRecord {
        in_use: flags & 0x01 != 0,
        is_dir: flags & 0x02 != 0,
        base_record: u64_at(rec, 0x20).context("record header")? & 0x0000_FFFF_FFFF_FFFF,
        name: None,
        data: None,
    };
    let mut pos = usize::from(u16_at(rec, 0x14).context("record header")?);
    for _ in 0..64 {
        let kind = u32_at(rec, pos).context("attribute header")?;
        if kind == ATTR_END {
            break;
        }
        let len = u32_at(rec, pos + 4).context("attribute header")? as usize;
        if len < 0x18 || pos + len > rec.len() {
            bail!("bad attribute length");
        }
        let a = &rec[pos..pos + len];
        let non_resident = a[8] != 0;
        let name_len = a[9];
        match (kind, non_resident) {
            (ATTR_FILE_NAME, false) => {
                let vlen = u32_at(a, 0x10).context("attr")? as usize;
                let voff = usize::from(u16_at(a, 0x14).context("attr")?);
                if let Some(fname) = a.get(voff..voff + vlen).and_then(parse_file_name) {
                    // Prefer the long (Win32 / POSIX) name over the 8.3 DOS name.
                    let better = out
                        .name
                        .as_ref()
                        .is_none_or(|n| n.namespace == 2 && fname.namespace != 2);
                    if better {
                        out.name = Some(fname);
                    }
                }
            }
            (ATTR_DATA, false) if name_len == 0 => {
                let vlen = u32_at(a, 0x10).context("attr")? as usize;
                let voff = usize::from(u16_at(a, 0x14).context("attr")?);
                out.data = Some(Data::Resident(
                    a.get(voff..voff + vlen).context("resident data")?.to_vec(),
                ));
            }
            (ATTR_DATA, true) if name_len == 0 && u64_at(a, 0x10) == Some(0) => {
                let last_vcn = u64_at(a, 0x18).context("attr")?;
                let runs_off = usize::from(u16_at(a, 0x20).context("attr")?);
                let allocated = u64_at(a, 0x28).context("attr")?;
                let size = u64_at(a, 0x30).context("attr")?;
                let runs = decode_runs(a.get(runs_off..).context("runs")?)?;
                let covered: u64 = runs.iter().map(|r| r.len).sum();
                // If the attribute continues in extension records we only have the first part.
                let complete = covered == last_vcn + 1 && allocated > 0;
                out.data = Some(Data::NonResident { runs, size, complete });
            }
            _ => {}
        }
        pos += len;
    }
    Ok(out)
}

/// FILETIME (100 ns since 1601) → local date/time text.
pub fn filetime_to_string(ft: u64) -> String {
    let secs = (ft / 10_000_000) as i64 - 11_644_473_600;
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeletedFile {
    pub record: u64,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub modified: String,
    pub data: Data,
    /// Fraction (0–1) of the file's clusters that are now used by other files.
    pub overwritten: f32,
}

pub struct Volume {
    file: File,
    pub boot: BootSector,
    mft_runs: Vec<Run>,
    bitmap: Vec<u8>,
}

impl Volume {
    /// Opens `\\.\X:` read-only (administrator rights required).
    pub fn open(letter: char) -> Result<Self> {
        let path = format!(r"\\.\{}:", letter.to_ascii_uppercase());
        let file = File::open(&path)
            .with_context(|| format!("opening {path} (administrator rights are required)"))?;
        let mut sector = vec![0u8; 4096];
        file.seek_read(&mut sector, 0)
            .context("reading the boot sector")?;
        let boot = parse_boot(&sector)?;
        let mut v = Volume {
            file,
            boot,
            mft_runs: Vec::new(),
            bitmap: Vec::new(),
        };
        let mft0 = v.read_raw_record(boot.mft_lcn * boot.cluster_size)?;
        match parse_record(&mft0)?.data {
            Some(Data::NonResident { runs, .. }) => v.mft_runs = runs,
            _ => bail!("could not locate the MFT"),
        }
        let bitmap_rec = parse_record(&v.read_record(BITMAP_RECORD)?)?;
        v.bitmap = match bitmap_rec.data {
            Some(d) => v.read_data(&d)?,
            None => Vec::new(),
        };
        Ok(v)
    }

    fn read_raw_record(&self, offset: u64) -> Result<Vec<u8>> {
        let mut rec = vec![0u8; self.boot.record_size as usize];
        self.file.seek_read(&mut rec, offset)?;
        apply_fixup(&mut rec, self.boot.bytes_per_sector as usize)?;
        Ok(rec)
    }

    pub fn record_count(&self) -> u64 {
        let clusters: u64 = self.mft_runs.iter().map(|r| r.len).sum();
        clusters * self.boot.cluster_size / u64::from(self.boot.record_size)
    }

    /// Byte offset on the volume of MFT record `index`.
    fn record_offset(&self, index: u64) -> Option<u64> {
        let byte = index * u64::from(self.boot.record_size);
        let mut vcn_start = 0u64;
        for r in &self.mft_runs {
            let run_bytes = r.len * self.boot.cluster_size;
            if byte < vcn_start + run_bytes {
                return Some(r.lcn? * self.boot.cluster_size + (byte - vcn_start));
            }
            vcn_start += run_bytes;
        }
        None
    }

    pub fn read_record(&self, index: u64) -> Result<Vec<u8>> {
        let off = self.record_offset(index).context("record outside the MFT")?;
        self.read_raw_record(off)
    }

    pub fn cluster_in_use(&self, lcn: u64) -> bool {
        self.bitmap
            .get((lcn / 8) as usize)
            .is_some_and(|b| b & (1 << (lcn % 8)) != 0)
    }

    /// Fraction of a deleted file's clusters that are allocated again.
    pub fn overwritten_fraction(&self, data: &Data) -> f32 {
        let Data::NonResident { runs, .. } = data else {
            return 0.0;
        };
        let (mut total, mut used) = (0u64, 0u64);
        for r in runs {
            if let Some(lcn) = r.lcn {
                for c in lcn..lcn + r.len {
                    total += 1;
                    used += u64::from(self.cluster_in_use(c));
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            used as f32 / total as f32
        }
    }

    /// Reads a data attribute's bytes (sparse runs read as zeros).
    pub fn read_data(&self, data: &Data) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.copy_data(data, &mut out)?;
        Ok(out)
    }

    pub fn copy_data(&self, data: &Data, out: &mut impl std::io::Write) -> Result<()> {
        match data {
            Data::Resident(bytes) => out.write_all(bytes)?,
            Data::Raw { .. } => bail!("files found by content are copied with the deep-scan reader"),
            Data::NonResident { runs, size, .. } => {
                let cs = self.boot.cluster_size;
                let mut remaining = *size;
                let mut buf = vec![0u8; (cs * 256) as usize];
                for r in runs {
                    let mut done = 0u64;
                    while done < r.len && remaining > 0 {
                        let n = (r.len - done).min(256);
                        let bytes = (n * cs) as usize;
                        match r.lcn {
                            Some(lcn) => self.file.seek_read(&mut buf[..bytes], (lcn + done) * cs)?,
                            None => {
                                buf[..bytes].fill(0);
                                bytes
                            }
                        };
                        let take = (bytes as u64).min(remaining) as usize;
                        out.write_all(&buf[..take])?;
                        remaining -= take as u64;
                        done += n;
                    }
                }
            }
        }
        Ok(())
    }

    /// Walks the whole MFT collecting deleted files and directory names (for paths).
    pub fn scan(&self, mut progress: impl FnMut(f32) -> bool) -> Vec<DeletedFile> {
        let count = self.record_count();
        let mut dirs: HashMap<u64, (String, u64)> = HashMap::new();
        let mut candidates = Vec::new();
        for i in 0..count {
            if i % 4096 == 0 && !progress(i as f32 / count as f32) {
                break; // cancelled
            }
            let Ok(raw) = self.read_record(i) else { continue };
            let Ok(rec) = parse_record(&raw) else { continue };
            if rec.base_record != 0 {
                continue;
            }
            let Some(name) = rec.name else { continue };
            if rec.is_dir {
                dirs.insert(i, (name.name, name.parent));
            } else if !rec.in_use
                && let Some(data) = rec.data
            {
                candidates.push((i, name, data));
            }
        }
        candidates
            .into_iter()
            .filter(|(_, n, _)| !n.name.starts_with('$'))
            .map(|(record, name, data)| {
                let size = match &data {
                    Data::Resident(b) => b.len() as u64,
                    Data::NonResident { size, .. } => *size,
                    Data::Raw { len, .. } => *len,
                };
                DeletedFile {
                    record,
                    path: build_path(&dirs, name.parent),
                    size,
                    modified: filetime_to_string(name.modified),
                    overwritten: self.overwritten_fraction(&data),
                    name: name.name,
                    data,
                }
            })
            .filter(|f| f.size > 0)
            .collect()
    }
}

/// Rebuilds a folder path from parent references (`?` marks an unknown ancestor).
pub fn build_path(dirs: &HashMap<u64, (String, u64)>, mut parent: u64) -> String {
    let mut parts = Vec::new();
    for _ in 0..64 {
        if parent == ROOT_RECORD {
            break;
        }
        match dirs.get(&parent) {
            Some((name, p)) => {
                parts.push(name.clone());
                parent = *p;
            }
            None => {
                parts.push("?".into());
                break;
            }
        }
    }
    parts.reverse();
    format!("\\{}", parts.join("\\"))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn data_runs() {
        assert_eq!(
            decode_runs(&[0x21, 0x18, 0x34, 0x56, 0x00]).unwrap(),
            vec![Run {
                lcn: Some(0x5634),
                len: 0x18
            }]
        );
        // Second run has a negative relative offset (-16); third is sparse.
        let runs = decode_runs(&[0x31, 0x04, 0x00, 0x10, 0x00, 0x11, 0x02, 0xF0, 0x01, 0x05, 0x00]).unwrap();
        assert_eq!(
            runs,
            vec![
                Run {
                    lcn: Some(0x1000),
                    len: 4
                },
                Run {
                    lcn: Some(0x1000 - 16),
                    len: 2
                },
                Run { lcn: None, len: 5 }
            ]
        );
        assert!(decode_runs(&[0x21, 0x18]).is_err(), "truncated");
        assert!(
            decode_runs(&[0x11, 0x01, 0x80]).is_err(),
            "negative absolute cluster"
        );
    }

    #[test]
    fn boot_sector() {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 8;
        b[0x28..0x30].copy_from_slice(&1_000_000u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&786_432u64.to_le_bytes());
        b[0x40] = 0xF6; // -10 → 1024-byte records
        let boot = parse_boot(&b).unwrap();
        assert_eq!(
            (boot.cluster_size, boot.record_size, boot.mft_lcn),
            (4096, 1024, 786_432)
        );
        b[3] = b'X';
        assert!(parse_boot(&b).is_err());
    }

    /// Builds a 1024-byte FILE record with fixups, a $FILE_NAME and a resident $DATA.
    pub fn synthetic_record(in_use: bool, name: &str, parent: u64, content: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // USA offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 sectors
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // first attribute
        r[0x16..0x18].copy_from_slice(&u16::from(in_use).to_le_bytes());
        let mut pos = 0x38;
        // $FILE_NAME
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let vlen = 0x42 + wide.len();
        let alen = (0x18 + vlen).div_ceil(8) * 8;
        r[pos..pos + 4].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes());
        r[pos + 4..pos + 8].copy_from_slice(&(alen as u32).to_le_bytes());
        r[pos + 0x10..pos + 0x14].copy_from_slice(&(vlen as u32).to_le_bytes());
        r[pos + 0x14..pos + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        let v = pos + 0x18;
        r[v..v + 8].copy_from_slice(&parent.to_le_bytes());
        r[v + 0x30..v + 0x38].copy_from_slice(&(content.len() as u64).to_le_bytes());
        r[v + 0x40] = (wide.len() / 2) as u8;
        r[v + 0x41] = 1; // Win32 namespace
        r[v + 0x42..v + 0x42 + wide.len()].copy_from_slice(&wide);
        pos += alen;
        // resident $DATA
        let alen = (0x18 + content.len()).div_ceil(8) * 8;
        r[pos..pos + 4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        r[pos + 4..pos + 8].copy_from_slice(&(alen as u32).to_le_bytes());
        r[pos + 0x10..pos + 0x14].copy_from_slice(&(content.len() as u32).to_le_bytes());
        r[pos + 0x14..pos + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        r[pos + 0x18..pos + 0x18 + content.len()].copy_from_slice(content);
        pos += alen;
        r[pos..pos + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        // Update sequence: USN 0x0001, save the real sector tails, stamp the USN.
        r[0x30..0x32].copy_from_slice(&[1, 0]);
        for i in 1..3usize {
            let tail = i * 512 - 2;
            let (a, b) = (r[tail], r[tail + 1]);
            r[0x30 + i * 2] = a;
            r[0x30 + i * 2 + 1] = b;
            r[tail] = 1;
            r[tail + 1] = 0;
        }
        r
    }

    #[test]
    fn fixup_and_record_parsing() {
        let mut rec = synthetic_record(false, "report.docx", 1234, b"hello world");
        apply_fixup(&mut rec, 512).unwrap();
        let parsed = parse_record(&rec).unwrap();
        assert!(!parsed.in_use && !parsed.is_dir);
        let name = parsed.name.unwrap();
        assert_eq!((name.name.as_str(), name.parent), ("report.docx", 1234));
        assert_eq!(parsed.data, Some(Data::Resident(b"hello world".to_vec())));

        let mut torn = synthetic_record(true, "x", 5, b"");
        torn[1022] = 9; // corrupt a sector tail
        assert!(apply_fixup(&mut torn, 512).is_err());
        assert!(parse_record(&[0u8; 1024]).is_err());
    }

    #[test]
    fn paths() {
        let dirs: HashMap<u64, (String, u64)> =
            [(40, ("Users".to_owned(), 5)), (41, ("Lucas".to_owned(), 40))]
                .into_iter()
                .collect();
        assert_eq!(build_path(&dirs, 41), r"\Users\Lucas");
        assert_eq!(build_path(&dirs, 5), r"\");
        assert_eq!(build_path(&dirs, 999), r"\?");
    }

    #[test]
    fn real_volume_scan_when_elevated() {
        if !wm_core::elevation::is_elevated() {
            eprintln!("skipped: needs administrator rights to open \\\\.\\C:");
            return;
        }
        let v = Volume::open('C').unwrap();
        assert!(v.record_count() > 1000);
        let root = parse_record(&v.read_record(ROOT_RECORD).unwrap()).unwrap();
        assert!(root.is_dir && root.in_use);
    }
}
