//! Disk speed test: sequential and 4 KiB random read/write with unbuffered I/O
//! (FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH) on a temporary file.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wm_core::task::Progress;

const NO_BUFFERING: u32 = 0x2000_0000;
const WRITE_THROUGH: u32 = 0x8000_0000;
const ALIGN: usize = 4096;
const BLOCK: usize = 1 << 20;
const SMALL: usize = 4096;

/// A heap buffer aligned for unbuffered I/O.
struct Aligned {
    ptr: *mut u8,
    layout: Layout,
}

impl Aligned {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, ALIGN).expect("layout");
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory");
        let a = Self { ptr, layout };
        // Fill with non-compressible data.
        let s = unsafe { std::slice::from_raw_parts_mut(a.ptr, len) };
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for b in s.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        a
    }

    fn slice(&mut self, len: usize) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, len.min(self.layout.size())) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BenchResult {
    pub seq_read_mbs: f64,
    pub seq_write_mbs: f64,
    pub rnd_read_iops: f64,
    pub rnd_write_iops: f64,
}

impl BenchResult {
    pub fn rnd_read_mbs(&self) -> f64 {
        self.rnd_read_iops * SMALL as f64 / 1e6
    }
    pub fn rnd_write_mbs(&self) -> f64 {
        self.rnd_write_iops * SMALL as f64 / 1e6
    }
}

/// Where the test file goes: the temp folder on the Windows drive, else the drive root.
pub fn test_file(drive: char) -> PathBuf {
    let temp = std::env::temp_dir();
    if temp
        .to_string_lossy()
        .to_ascii_uppercase()
        .starts_with(drive.to_ascii_uppercase())
    {
        temp.join("win-manager-rs-speedtest.tmp")
    } else {
        PathBuf::from(format!(r"{drive}:\win-manager-rs-speedtest.tmp"))
    }
}

fn open(path: &Path, create: bool) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .custom_flags(NO_BUFFERING | WRITE_THROUGH)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Runs the test with a file of `size_mb` MiB; each random phase lasts `random_secs`.
pub fn run(
    path: &Path,
    size_mb: u64,
    random_secs: f64,
    p: Option<&Progress<BenchResult>>,
) -> Result<BenchResult> {
    let report = |f: f32, s: &str| {
        if let Some(p) = p {
            p.report(f, s);
        }
    };
    let cancelled = || p.is_some_and(|p| p.is_cancelled());
    let total = size_mb * BLOCK as u64;
    let mut buf = Aligned::new(BLOCK);
    let mut r = BenchResult::default();
    let result = (|| -> Result<()> {
        // Sequential write.
        report(0.0, "Sequential write…");
        let mut f = open(path, true)?;
        let t = Instant::now();
        for _ in 0..size_mb {
            f.write_all(buf.slice(BLOCK))?;
            if cancelled() {
                anyhow::bail!("cancelled");
            }
        }
        r.seq_write_mbs = total as f64 / 1e6 / t.elapsed().as_secs_f64();
        drop(f);

        // Sequential read.
        report(0.25, "Sequential read…");
        let mut f = open(path, false)?;
        let t = Instant::now();
        for _ in 0..size_mb {
            f.read_exact(buf.slice(BLOCK))?;
        }
        r.seq_read_mbs = total as f64 / 1e6 / t.elapsed().as_secs_f64();

        // Random 4 KiB read and write (queue depth 1).
        let blocks = total / SMALL as u64;
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for (phase, write) in [(0.5f32, false), (0.75, true)] {
            report(
                phase,
                if write {
                    "Random 4K write…"
                } else {
                    "Random 4K read…"
                },
            );
            let t = Instant::now();
            let mut n = 0u64;
            while t.elapsed() < Duration::from_secs_f64(random_secs) {
                let off = (rng.next() % blocks) * SMALL as u64;
                f.seek(SeekFrom::Start(off))?;
                if write {
                    f.write_all(buf.slice(SMALL))?;
                } else {
                    f.read_exact(buf.slice(SMALL))?;
                }
                n += 1;
                if cancelled() {
                    anyhow::bail!("cancelled");
                }
            }
            let iops = n as f64 / t.elapsed().as_secs_f64();
            if write {
                r.rnd_write_iops = iops;
            } else {
                r.rnd_read_iops = iops;
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(path);
    result?;
    report(1.0, "Done");
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_run_produces_numbers_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench.tmp");
        let r = run(&path, 8, 0.2, None).unwrap();
        assert!(r.seq_write_mbs > 0.0 && r.seq_read_mbs > 0.0, "{r:?}");
        assert!(r.rnd_read_iops > 0.0 && r.rnd_write_iops > 0.0, "{r:?}");
        assert!(!path.exists());
    }
}
