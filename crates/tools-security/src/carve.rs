//! Deep scan for File Undelete: finds files by their content when the file table is
//! gone (a formatted or damaged drive). Every 512-byte sector start is checked for a
//! known file header; the file's length then comes from walking its own structure (JPEG
//! segments, PNG chunks, GIF blocks, MP4 boxes…) or from the end marker it carries
//! (ZIP, PDF). Only files stored in one piece come back whole. The volume is only read.

use std::fs::File;
use std::os::windows::fs::FileExt;

use anyhow::{Context, Result, bail};

pub const SECTOR: u64 = 512;
/// Raw volume reads must be aligned to the device sector size; 4096 covers 512 and 4K.
const ALIGN: u64 = 4096;
const BLOCK: usize = 1 << 20;

/// Something that can be read at any byte offset.
pub trait Source: Sync {
    fn len(&self) -> u64;
    /// Reads up to `buf.len()` bytes at `off`; fewer only at the end.
    fn read_at(&self, buf: &mut [u8], off: u64) -> Result<usize>;
}

impl Source for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&self, buf: &mut [u8], off: u64) -> Result<usize> {
        let start = (off as usize).min(<[u8]>::len(self));
        let n = buf.len().min(<[u8]>::len(self) - start);
        buf[..n].copy_from_slice(&self[start..start + n]);
        Ok(n)
    }
}

/// `\\.\X:` opened read-only; reads are widened to aligned blocks.
pub struct RawVolume {
    file: File,
    size: u64,
}

impl RawVolume {
    pub fn open(letter: char) -> Result<Self> {
        let letter = letter.to_ascii_uppercase();
        let path = format!(r"\\.\{letter}:");
        let file = File::open(&path)
            .with_context(|| format!("opening {path} (administrator rights are required)"))?;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        let mut total = 0u64;
        unsafe {
            windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                windows::core::PCWSTR(root.as_ptr()),
                None,
                Some(&mut total),
                None,
            )
        }
        .context("reading the drive size")?;
        Ok(Self {
            file,
            size: total / ALIGN * ALIGN,
        })
    }
}

impl Source for RawVolume {
    fn len(&self) -> u64 {
        self.size
    }
    fn read_at(&self, buf: &mut [u8], off: u64) -> Result<usize> {
        if off >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let end = (off + buf.len() as u64).min(self.size);
        let a_start = off / ALIGN * ALIGN;
        let a_end = end.div_ceil(ALIGN) * ALIGN;
        if a_start == off && a_end == end {
            self.file.seek_read(&mut buf[..(end - off) as usize], off)?;
            return Ok((end - off) as usize);
        }
        let mut tmp = vec![0u8; (a_end - a_start) as usize];
        self.file.seek_read(&mut tmp, a_start)?;
        let from = (off - a_start) as usize;
        let n = (end - off) as usize;
        buf[..n].copy_from_slice(&tmp[from..from + n]);
        Ok(n)
    }
}

/// Buffered random access over a [`Source`], for the structure walkers.
struct Reader<'a, S: Source + ?Sized> {
    src: &'a S,
    buf: Vec<u8>,
    buf_off: u64,
}

impl<'a, S: Source + ?Sized> Reader<'a, S> {
    fn new(src: &'a S) -> Self {
        Self {
            src,
            buf: Vec::new(),
            buf_off: 0,
        }
    }

    /// `n` (≤ 64 KiB) bytes at `pos`, or None past the end.
    fn get(&mut self, pos: u64, n: usize) -> Option<&[u8]> {
        let have = pos >= self.buf_off && pos + n as u64 <= self.buf_off + self.buf.len() as u64;
        if !have {
            let start = pos / SECTOR * SECTOR;
            self.buf.resize(BLOCK, 0);
            let got = self.src.read_at(&mut self.buf, start).ok()?;
            self.buf.truncate(got);
            self.buf_off = start;
            if pos + n as u64 > start + got as u64 {
                return None;
            }
        }
        let i = (pos - self.buf_off) as usize;
        Some(&self.buf[i..i + n])
    }

    fn byte(&mut self, pos: u64) -> Option<u8> {
        self.get(pos, 1).map(|b| b[0])
    }
    fn u16be(&mut self, pos: u64) -> Option<u16> {
        self.get(pos, 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u16le(&mut self, pos: u64) -> Option<u16> {
        self.get(pos, 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32be(&mut self, pos: u64) -> Option<u32> {
        self.get(pos, 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u32le(&mut self, pos: u64) -> Option<u32> {
        self.get(pos, 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64be(&mut self, pos: u64) -> Option<u64> {
        self.get(pos, 8)
            .map(|b| u64::from_be_bytes(b.try_into().expect("8 bytes")))
    }

    /// First position ≥ `from` and < `limit` where `pat` starts.
    fn find(&mut self, pat: &[u8], from: u64, limit: u64) -> Option<u64> {
        let mut pos = from;
        let step = 32 * 1024;
        while pos < limit {
            let want = (step + pat.len()).min((limit - pos) as usize + pat.len() - 1);
            let avail = (self.src.len().saturating_sub(pos) as usize).min(want);
            if avail < pat.len() {
                return None;
            }
            let chunk = self.get(pos, avail)?;
            if let Some(i) = chunk.windows(pat.len()).position(|w| w == pat) {
                let at = pos + i as u64;
                return (at < limit).then_some(at);
            }
            pos += (avail - pat.len() + 1) as u64;
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Gif,
    Bmp,
    Pdf,
    /// ZIP and the formats built on it (docx, xlsx, pptx, odt, epub…).
    Zip,
    Mp4,
    Wav,
    Avi,
}

impl Kind {
    pub fn ext(self) -> &'static str {
        match self {
            Kind::Jpeg => "jpg",
            Kind::Png => "png",
            Kind::Gif => "gif",
            Kind::Bmp => "bmp",
            Kind::Pdf => "pdf",
            Kind::Zip => "zip",
            Kind::Mp4 => "mp4",
            Kind::Wav => "wav",
            Kind::Avi => "avi",
        }
    }

    fn max_len(self) -> u64 {
        const MB: u64 = 1 << 20;
        match self {
            Kind::Jpeg | Kind::Png | Kind::Gif => 128 * MB,
            Kind::Bmp => 512 * MB,
            Kind::Pdf | Kind::Zip => 2048 * MB,
            Kind::Mp4 | Kind::Wav | Kind::Avi => 8192 * MB,
        }
    }
}

/// The kind of file whose header starts `head` (at least 16 bytes), if any.
pub fn detect(head: &[u8]) -> Option<Kind> {
    if head.len() < 16 {
        return None;
    }
    Some(match head {
        [0xFF, 0xD8, 0xFF, m, ..] if *m >= 0xC0 => Kind::Jpeg,
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Kind::Png,
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Kind::Gif,
        [b'%', b'P', b'D', b'F', b'-', ..] => Kind::Pdf,
        [b'P', b'K', 3, 4, ..] => Kind::Zip,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => Kind::Wav,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'A', b'V', b'I', b' ', ..] => Kind::Avi,
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => Kind::Mp4,
        [b'B', b'M', _, _, _, _, 0, 0, 0, 0, _, _, _, _, h, 0, ..]
            if matches!(*h, 12 | 40 | 56 | 108 | 124) =>
        {
            Kind::Bmp
        }
        _ => return None,
    })
}

fn jpeg_len<S: Source + ?Sized>(r: &mut Reader<S>, start: u64, max: u64) -> Option<u64> {
    let mut pos = start + 2;
    loop {
        if pos - start > max || r.byte(pos)? != 0xFF {
            return None;
        }
        while r.byte(pos + 1)? == 0xFF {
            pos += 1; // fill bytes
        }
        let m = r.byte(pos + 1)?;
        match m {
            0xD9 => return Some(pos + 2 - start),
            0x01 | 0xD0..=0xD7 => pos += 2,
            0xD8 | 0x00 => return None,
            _ => {
                let len = u64::from(r.u16be(pos + 2)?);
                if len < 2 {
                    return None;
                }
                pos += 2 + len;
                if m == 0xDA {
                    // Entropy-coded data runs to the next marker that isn't a stuffed
                    // 0xFF00 or a restart marker.
                    loop {
                        let ff = r.find(&[0xFF], pos, start + max)?;
                        let next = r.byte(ff + 1)?;
                        if next == 0x00 || (0xD0..=0xD7).contains(&next) || next == 0xFF {
                            pos = ff + 1;
                            continue;
                        }
                        pos = ff;
                        break;
                    }
                }
            }
        }
    }
}

fn png_len<S: Source + ?Sized>(r: &mut Reader<S>, start: u64, max: u64) -> Option<u64> {
    let mut pos = start + 8;
    loop {
        let len = u64::from(r.u32be(pos)?);
        let ty = r.get(pos + 4, 4)?.to_vec();
        if !ty.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        pos += 12 + len;
        if pos - start > max {
            return None;
        }
        if ty == b"IEND" {
            return Some(pos - start);
        }
    }
}

fn gif_len<S: Source + ?Sized>(r: &mut Reader<S>, start: u64, max: u64) -> Option<u64> {
    let skip_sub_blocks = |r: &mut Reader<S>, mut pos: u64| -> Option<u64> {
        loop {
            let n = u64::from(r.byte(pos)?);
            pos += 1 + n;
            if n == 0 || pos - start > max {
                return (n == 0).then_some(pos);
            }
        }
    };
    let flags = r.byte(start + 10)?;
    let mut pos = start + 13;
    if flags & 0x80 != 0 {
        pos += 3 * (1u64 << ((flags & 7) + 1));
    }
    loop {
        if pos - start > max {
            return None;
        }
        match r.byte(pos)? {
            0x3B => return Some(pos + 1 - start),
            0x21 => pos = skip_sub_blocks(r, pos + 2)?,
            0x2C => {
                let f = r.byte(pos + 9)?;
                pos += 10;
                if f & 0x80 != 0 {
                    pos += 3 * (1u64 << ((f & 7) + 1));
                }
                pos = skip_sub_blocks(r, pos + 1)?; // after the LZW minimum code size
            }
            _ => return None,
        }
    }
}

fn mp4_len<S: Source + ?Sized>(r: &mut Reader<S>, start: u64, max: u64) -> Option<u64> {
    const TOP: [&[u8; 4]; 16] = [
        b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof", b"mfra",
        b"styp", b"sidx", b"udta", b"pnot", b"junk",
    ];
    let mut pos = start;
    let mut seen_media = false;
    while let Some(size32) = r.u32be(pos) {
        let Some(ty) = r.get(pos + 4, 4).map(<[u8]>::to_vec) else {
            break;
        };
        if !TOP.iter().any(|t| t.as_slice() == ty.as_slice()) {
            break;
        }
        let size = match size32 {
            1 => r.u64be(pos + 8)?,
            0 => return None, // "to the end of the file": unknown here
            n => u64::from(n),
        };
        if size < 8 {
            return None;
        }
        seen_media |= ty == b"mdat" || ty == b"moov";
        pos += size;
        if pos - start > max {
            return None;
        }
    }
    (seen_media && pos > start).then_some(pos - start)
}

/// The length of the file of `kind` starting at `start`, if its structure checks out.
pub fn file_len<S: Source + ?Sized>(src: &S, kind: Kind, start: u64) -> Option<u64> {
    let mut r = Reader::new(src);
    let max = kind.max_len().min(src.len().saturating_sub(start));
    let len = match kind {
        Kind::Jpeg => jpeg_len(&mut r, start, max),
        Kind::Png => png_len(&mut r, start, max),
        Kind::Gif => gif_len(&mut r, start, max),
        Kind::Mp4 => mp4_len(&mut r, start, max),
        Kind::Bmp => Some(u64::from(r.u32le(start + 2)?)).filter(|&n| n >= 26),
        Kind::Wav | Kind::Avi => Some(u64::from(r.u32le(start + 4)?) + 8),
        Kind::Zip => {
            let eocd = r.find(b"PK\x05\x06", start + 4, start + max)?;
            Some(eocd + 22 + u64::from(r.u16le(eocd + 20)?) - start)
        }
        Kind::Pdf => {
            let eof = r.find(b"%%EOF", start + 5, start + max)?;
            let mut end = eof + 5;
            while matches!(r.byte(end), Some(b'\r' | b'\n')) && end - eof < 7 {
                end += 1;
            }
            Some(end - start)
        }
    }?;
    (len > 0 && len <= max).then_some(len)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub offset: u64,
    pub len: u64,
    pub kind: Kind,
}

/// Scans the whole source. `progress` gets 0–1 and returns false to stop.
pub fn scan<S: Source + ?Sized>(src: &S, mut progress: impl FnMut(f32, usize) -> bool) -> Vec<Found> {
    let total = src.len();
    let mut found = Vec::new();
    let mut buf = vec![0u8; 4 << 20];
    let mut chunk_off = 0u64;
    // Files already found are skipped over, so a JPEG inside a ZIP isn't listed twice.
    let mut skip_to = 0u64;
    while chunk_off < total {
        if !progress(chunk_off as f32 / total as f32, found.len()) {
            break;
        }
        let Ok(n) = src.read_at(&mut buf, chunk_off) else {
            chunk_off += buf.len() as u64;
            continue;
        };
        if n == 0 {
            break;
        }
        let mut i = 0usize;
        while i + 16 <= n {
            let off = chunk_off + i as u64;
            if off >= skip_to
                && let Some(kind) = detect(&buf[i..i + 16])
                && let Some(len) = file_len(src, kind, off)
            {
                found.push(Found {
                    offset: off,
                    len,
                    kind,
                });
                skip_to = (off + len).div_ceil(SECTOR) * SECTOR;
            }
            i += SECTOR as usize;
        }
        chunk_off += n as u64;
    }
    progress(1.0, found.len());
    found
}

/// Copies `len` bytes at `offset` of the source to `out`.
pub fn copy<S: Source + ?Sized>(src: &S, offset: u64, len: u64, out: &mut impl std::io::Write) -> Result<()> {
    let mut buf = vec![0u8; BLOCK];
    let mut done = 0u64;
    while done < len {
        let want = ((len - done) as usize).min(BLOCK);
        let n = src.read_at(&mut buf[..want], offset + done)?;
        if n == 0 {
            bail!("the drive ended early");
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small but valid JPEG: SOI, APP0, a quantisation table, SOF0, Huffman table,
    /// SOS with stuffed and restart bytes in its data, EOI.
    pub fn jpeg() -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend([0xFF, 0xE0, 0x00, 0x10]);
        j.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        j.extend([0xFF, 0xDB, 0x00, 0x04, 0x00, 0x01]);
        j.extend([0xFF, 0xC0, 0x00, 0x0B, 8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
        j.extend([0xFF, 0xC4, 0x00, 0x04, 0x00, 0x00]);
        j.extend([0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0x3F, 0]);
        j.extend([0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xFF, 0x00]);
        j.extend([0xFF, 0xD9]);
        j
    }

    fn png() -> Vec<u8> {
        let mut p = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        for (ty, data) in [
            (b"IHDR", vec![0u8; 13]),
            (b"IDAT", vec![7u8; 30]),
            (b"IEND", vec![]),
        ] {
            p.extend((data.len() as u32).to_be_bytes());
            p.extend(ty);
            p.extend(&data);
            p.extend([0, 0, 0, 0]);
        }
        p
    }

    fn gif() -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend([1, 0, 1, 0, 0x80, 0, 0]); // 1x1, global table of 2 colours
        g.extend([0u8; 6]);
        g.extend([0x21, 0xF9, 4, 0, 0, 0, 0, 0]); // graphic control extension
        g.extend([0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]); // image descriptor
        g.extend([2, 2, 0x44, 0x01, 0]); // LZW size, one sub-block, end
        g.push(0x3B);
        g
    }

    fn zip() -> Vec<u8> {
        let mut z = b"PK\x03\x04".to_vec();
        z.extend([0u8; 40]);
        z.extend(b"PK\x01\x02");
        z.extend([0u8; 20]);
        z.extend(b"PK\x05\x06");
        z.extend([0u8; 16]);
        z.extend(3u16.to_le_bytes());
        z.extend(b"abc");
        z
    }

    fn mp4() -> Vec<u8> {
        let mut m = Vec::new();
        for (ty, n) in [(b"ftyp", 16u32), (b"moov", 40), (b"mdat", 100)] {
            m.extend(n.to_be_bytes());
            m.extend(ty);
            m.extend(vec![0x11u8; n as usize - 8]);
        }
        m
    }

    fn pdf() -> Vec<u8> {
        b"%PDF-1.7\n1 0 obj << >> endobj\ntrailer << >>\n%%EOF\r\n".to_vec()
    }

    fn bmp() -> Vec<u8> {
        let mut b = b"BM".to_vec();
        b.extend(70u32.to_le_bytes());
        b.extend([0u8; 4]);
        b.extend(54u32.to_le_bytes());
        b.extend(40u32.to_le_bytes());
        b.resize(70, 0x22);
        b
    }

    fn wav() -> Vec<u8> {
        let mut w = b"RIFF".to_vec();
        w.extend(36u32.to_le_bytes());
        w.extend(b"WAVEfmt ");
        w.resize(44, 1);
        w
    }

    pub fn samples() -> Vec<(Kind, Vec<u8>)> {
        vec![
            (Kind::Jpeg, jpeg()),
            (Kind::Png, png()),
            (Kind::Gif, gif()),
            (Kind::Zip, zip()),
            (Kind::Mp4, mp4()),
            (Kind::Pdf, pdf()),
            (Kind::Bmp, bmp()),
            (Kind::Wav, wav()),
        ]
    }

    #[test]
    fn each_format_is_found_with_its_exact_length() {
        // Files at sector starts, separated by random-looking filler.
        let mut disk = vec![0xA5u8; 3 * 512];
        let mut expected = Vec::new();
        for (kind, bytes) in samples() {
            let off = disk.len() as u64;
            disk.extend(&bytes);
            disk.resize((disk.len() / 512 + 2) * 512, 0x5A);
            expected.push(Found {
                offset: off,
                len: bytes.len() as u64,
                kind,
            });
        }
        let found = scan(disk.as_slice(), |_, _| true);
        assert_eq!(found, expected);
        let mut out = Vec::new();
        copy(disk.as_slice(), found[0].offset, found[0].len, &mut out).unwrap();
        assert_eq!(out, jpeg());
    }

    /// Live: deep-scans the drive in WM_CARVE_DRIVE (a formatted test volume) and checks
    /// every file in WM_CARVE_SRC whose name is listed comes back byte for byte.
    #[test]
    #[ignore]
    fn live_formatted_volume() {
        let drive = std::env::var("WM_CARVE_DRIVE").unwrap().chars().next().unwrap();
        let src = std::path::PathBuf::from(std::env::var("WM_CARVE_SRC").unwrap());
        let v = RawVolume::open(drive).unwrap();
        let found = scan(&v, |_, _| true);
        let recovered: Vec<Vec<u8>> = found
            .iter()
            .map(|f| {
                let mut out = Vec::new();
                copy(&v, f.offset, f.len, &mut out).unwrap();
                out
            })
            .collect();
        for f in &found {
            eprintln!("found {:?} at {:#x}, {} bytes", f.kind, f.offset, f.len);
        }
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(&src).unwrap() {
            let p = entry.unwrap().path();
            let original = std::fs::read(&p).unwrap();
            let hit = recovered.contains(&original);
            eprintln!("{}: {}", p.display(), if hit { "recovered" } else { "NOT found" });
            if !hit {
                missing.push(p);
            }
        }
        let wanted: Vec<String> = std::env::var("WM_CARVE_MUST")
            .unwrap_or_default()
            .split(';')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        for w in wanted {
            assert!(
                !missing
                    .iter()
                    .any(|m| m.file_name().unwrap().to_string_lossy() == w),
                "{w} was not recovered"
            );
        }
    }

    #[test]
    fn broken_files_are_not_reported() {
        let mut j = jpeg();
        j.truncate(j.len() - 2); // no end marker
        j.resize(4096, 0);
        assert_eq!(scan(j.as_slice(), |_, _| true), vec![]);
        let mut p = png();
        p[8 + 4] = b'#'; // chunk type that isn't letters
        p.resize(1024, 0);
        assert_eq!(scan(p.as_slice(), |_, _| true), vec![]);
        assert_eq!(detect(&[0u8; 16]), None);
    }
}
