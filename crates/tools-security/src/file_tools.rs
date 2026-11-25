//! Files and Folders helpers: shredding, and finding and closing the programs that
//! hold files open (Windows Restart Manager).

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmForceShutdown, RmGetList, RmRegisterResources,
    RmShutdown, RmStartSession,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use wm_core::paths;

// ---------------------------------------------------------------------------------
// Shredding

/// Places that must never be shredded: drive roots, Windows, Program Files and the
/// user profile folder itself.
pub fn protected(path: &Path) -> bool {
    let Ok(full) = std::path::absolute(path) else {
        return true;
    };
    if full.parent().is_none() {
        return true;
    }
    let norm = |p: &str| paths::expand_env(p).trim_end_matches('\\').to_ascii_lowercase();
    let target = norm(&full.to_string_lossy());
    // These folders and everything inside them.
    let trees = ["%WINDIR%", "%ProgramFiles%", "%ProgramFiles(x86)%"];
    // Only the folder itself (files inside are the user's own).
    let exact = ["%ProgramData%", "%USERPROFILE%", r"%SystemDrive%\Users"];
    trees.iter().any(|g| {
        let g = norm(g);
        target == g || target.starts_with(&format!("{g}\\"))
    }) || exact.iter().any(|g| target == norm(g))
}

/// The files a shred of `targets` removes (folders are walked).
pub fn shred_files(targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for t in targets {
        if t.is_dir() {
            out.extend(
                WalkDir::new(t)
                    .follow_links(false)
                    .into_iter()
                    .flatten()
                    .filter(|e| e.file_type().is_file())
                    .map(|e| e.into_path()),
            );
        } else if t.is_file() {
            out.push(t.clone());
        }
    }
    out
}

fn random_name(dir: &Path) -> PathBuf {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    let name: String = b.iter().map(|x| format!("{x:02x}")).collect();
    dir.join(name)
}

/// Overwrites a file with random data, renames it to a random name and deletes it.
pub fn shred_file(path: &Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
    }
    let len = std::fs::metadata(path)?.len();
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .context("opening for overwrite (is it in use? try Unlock)")?;
        f.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; 1 << 20];
        let mut left = len;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            getrandom::fill(&mut buf[..n]).map_err(|e| anyhow::anyhow!("random data: {e}"))?;
            f.write_all(&buf[..n])?;
            left -= n as u64;
        }
        f.sync_all()?;
        f.set_len(0)?;
        f.sync_all()?;
    }
    let dir = path.parent().context("no parent folder")?;
    let renamed = random_name(dir);
    std::fs::rename(path, &renamed).context("renaming")?;
    std::fs::remove_file(&renamed).context("deleting")?;
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct ShredReport {
    pub shredded: usize,
    pub failed: Vec<String>,
}

/// Shreds every file, then removes the emptied folders.
pub fn shred(targets: &[PathBuf], progress: &dyn Fn(f32, &str)) -> ShredReport {
    let mut rep = ShredReport::default();
    for t in targets {
        if protected(t) {
            rep.failed
                .push(format!("{}: a protected system folder", t.display()));
        }
    }
    let targets: Vec<PathBuf> = targets.iter().filter(|t| !protected(t)).cloned().collect();
    let files = shred_files(&targets);
    let n = files.len().max(1) as f32;
    for (i, f) in files.iter().enumerate() {
        progress(i as f32 / n, &f.display().to_string());
        match shred_file(f) {
            Ok(()) => rep.shredded += 1,
            Err(e) => rep.failed.push(format!("{}: {e:#}", f.display())),
        }
    }
    for t in targets.iter().filter(|t| t.is_dir()) {
        for d in WalkDir::new(t)
            .contents_first(true)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_dir())
        {
            let _ = std::fs::remove_dir(d.path());
        }
    }
    rep
}

// ---------------------------------------------------------------------------------
// Unlocking

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
    pub service: String,
}

/// Restart Manager handles at most a few thousand files per session.
const MAX_FILES: usize = 2000;

struct Session(u32);

impl Session {
    fn start() -> Result<Self> {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        check(
            unsafe { RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())) },
            "starting a Restart Manager session",
        )?;
        Ok(Self(handle))
    }

    fn register(&self, files: &[PathBuf]) -> Result<()> {
        let names: Vec<HSTRING> = files.iter().map(|f| HSTRING::from(f.as_os_str())).collect();
        let ptrs: Vec<PCWSTR> = names.iter().map(|h| PCWSTR(h.as_ptr())).collect();
        check(
            unsafe { RmRegisterResources(self.0, Some(&ptrs), None, None) },
            "registering the files",
        )
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = unsafe { RmEndSession(self.0) };
    }
}

fn check(e: WIN32_ERROR, what: &str) -> Result<()> {
    if e.0 != 0 {
        bail!("{what}: {}", windows::core::Error::from(e.to_hresult()).message());
    }
    Ok(())
}

fn lock_targets(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = shred_files(paths);
    files.truncate(MAX_FILES);
    files
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|c| *c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// Programs that have any of the files (or files inside the folders) open.
pub fn holders(paths: &[PathBuf]) -> Result<Vec<Holder>> {
    let files = lock_targets(paths);
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let s = Session::start()?;
    s.register(&files)?;
    let mut needed = 0u32;
    let mut count = 0u32;
    let mut reasons = 0u32;
    let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
    // ERROR_MORE_DATA (234): grow the buffer and ask again.
    for _ in 0..3 {
        count = infos.len() as u32;
        let r = unsafe {
            RmGetList(
                s.0,
                &mut needed,
                &mut count,
                (!infos.is_empty()).then_some(infos.as_mut_ptr()),
                &mut reasons,
            )
        };
        if r.0 == 234 {
            infos = vec![RM_PROCESS_INFO::default(); needed as usize + 4];
            continue;
        }
        check(r, "listing the programs using the files")?;
        break;
    }
    let mut out: Vec<Holder> = infos
        .iter()
        .take(count as usize)
        .map(|i| Holder {
            pid: i.Process.dwProcessId,
            name: wide(&i.strAppName),
            service: wide(&i.strServiceShortName),
        })
        .collect();
    out.sort_by_key(|h| h.pid);
    out.dedup_by_key(|h| h.pid);
    Ok(out)
}

/// Asks the programs holding the files to close, forcing those that don't. Windows
/// services and critical processes are left alone by Restart Manager.
pub fn close_holders(paths: &[PathBuf]) -> Result<()> {
    let files = lock_targets(paths);
    let s = Session::start()?;
    s.register(&files)?;
    check(
        unsafe { RmShutdown(s.0, RmForceShutdown.0 as u32, None) },
        "closing the programs",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_places_are_protected() {
        for p in [
            r"C:\",
            r"C:\Windows",
            r"C:\Windows\System32\drivers",
            r"C:\Program Files",
        ] {
            assert!(protected(Path::new(p)), "{p}");
        }
        let home = paths::expand_env("%USERPROFILE%");
        assert!(protected(Path::new(&home)));
        assert!(!protected(&Path::new(&home).join("Documents").join("old.txt")));
        assert!(!protected(Path::new(&paths::expand_env(
            r"%ProgramData%\SomeApp\cache.bin"
        ))));
    }

    #[test]
    fn shredding_removes_files_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("secret");
        std::fs::create_dir_all(sub.join("deeper")).unwrap();
        std::fs::write(sub.join("a.txt"), vec![7u8; 3 << 20]).unwrap();
        std::fs::write(sub.join("deeper").join("b.txt"), b"hello").unwrap();
        let ro = dir.path().join("ro.txt");
        std::fs::write(&ro, b"x").unwrap();
        let mut p = std::fs::metadata(&ro).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&ro, p).unwrap();
        let rep = shred(&[sub.clone(), ro.clone()], &|_, _| {});
        assert_eq!(rep.shredded, 3, "{:?}", rep.failed);
        assert!(rep.failed.is_empty());
        assert!(!sub.exists() && !ro.exists());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "no renamed leftovers"
        );
    }

    #[test]
    fn open_files_have_holders_and_closed_ones_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("held.txt");
        std::fs::write(&f, b"x").unwrap();
        assert!(holders(std::slice::from_ref(&f)).unwrap().is_empty());
        let _open = std::fs::File::open(&f).unwrap();
        let held = holders(std::slice::from_ref(&f)).unwrap();
        assert!(held.iter().any(|h| h.pid == std::process::id()), "{held:?}");
    }
}
