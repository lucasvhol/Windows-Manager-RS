//! Registry Defrag: rebuilds registry hives without their free space.
//!
//! Analyze saves a compact copy of each hive (`RegSaveKeyEx` writes only live data)
//! and compares sizes. Compact saves fresh copies and asks Windows to swap them in at
//! the next start (`RegReplaceKey`); Windows keeps each original hive as a backup.
//! Anything written to the registry between the swap request and the restart would be
//! lost, so the restart follows straight away, as in the original tool.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ, REG_LATEST_FORMAT, REG_OPTION_BACKUP_RESTORE,
    RegCloseKey, RegOpenKeyExW, RegReplaceKeyW, RegSaveKeyExW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, w};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    process::{hidden_command, run_checked},
    registry,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, frame, show_message},
};

const TOOL: &str = "regdefrag";
const HIVELIST: &str = r"HKLM\SYSTEM\CurrentControlSet\Control\hivelist";
/// Machine hives worth compacting; others (BCD, COMPONENTS, DRIVERS, ELAM…) are left alone.
const MACHINE_HIVES: [&str; 4] = ["SOFTWARE", "SYSTEM", "SAM", "SECURITY"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    Machine,
    Users,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hive {
    pub root: Root,
    /// Subkey under the root, such as SOFTWARE or a user SID.
    pub name: String,
    pub file: PathBuf,
}

impl Hive {
    pub fn label(&self) -> String {
        match self.root {
            Root::Machine => self.name.clone(),
            Root::Users if self.name == ".DEFAULT" => "DEFAULT (sign-in screen)".into(),
            Root::Users => {
                let what = if self.name.ends_with("_Classes") {
                    "file associations"
                } else {
                    "settings"
                };
                let sid = self.name.trim_end_matches("_Classes");
                let who = match sid {
                    "S-1-5-18" => "System".to_owned(),
                    "S-1-5-19" => "Local Service".to_owned(),
                    "S-1-5-20" => "Network Service".to_owned(),
                    _ => user_of(&self.file).unwrap_or_else(|| sid.to_owned()),
                };
                format!("{who}: {what}")
            }
        }
    }

    fn hkey(&self) -> HKEY {
        match self.root {
            Root::Machine => HKEY_LOCAL_MACHINE,
            Root::Users => HKEY_USERS,
        }
    }

    /// A file-name-safe name for saved copies.
    fn slug(&self) -> String {
        let prefix = if self.root == Root::Machine { "HKLM" } else { "HKU" };
        let name: String = self
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        format!("{prefix}-{name}")
    }
}

/// The account folder name for a profile hive under C:\Users\<name>\….
fn user_of(file: &Path) -> Option<String> {
    let s = file.to_string_lossy();
    let lower = s.to_ascii_lowercase();
    let i = lower.find(r"\users\")?;
    s[i + 7..].split('\\').next().map(str::to_owned)
}

/// Maps a hivelist entry to a hive, or `None` for hives we leave alone.
pub fn parse_hive(name: &str, device_path: &str, devices: &[(String, char)]) -> Option<Hive> {
    let (root, sub) = if let Some(s) = name.strip_prefix(r"\REGISTRY\MACHINE\") {
        if !MACHINE_HIVES.iter().any(|h| h.eq_ignore_ascii_case(s)) {
            return None;
        }
        (Root::Machine, s)
    } else {
        (Root::Users, name.strip_prefix(r"\REGISTRY\USER\")?)
    };
    if sub.is_empty() || sub.contains('\\') {
        return None;
    }
    let file = dos_path(device_path, devices)?;
    Some(Hive {
        root,
        name: sub.to_owned(),
        file,
    })
}

/// `\Device\HarddiskVolume3\Windows\…` → `C:\Windows\…`, using (device, letter) pairs.
pub fn dos_path(device_path: &str, devices: &[(String, char)]) -> Option<PathBuf> {
    devices.iter().find_map(|(dev, letter)| {
        let rest = device_path.strip_prefix(dev.as_str())?;
        rest.starts_with('\\')
            .then(|| PathBuf::from(format!("{letter}:{rest}")))
    })
}

fn dos_devices() -> Vec<(String, char)> {
    let mut out = Vec::new();
    for letter in 'A'..='Z' {
        let mut buf = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(&HSTRING::from(format!("{letter}:")), Some(&mut buf)) };
        if n > 0 {
            let end = buf.iter().position(|c| *c == 0).unwrap_or(n as usize);
            out.push((String::from_utf16_lossy(&buf[..end]), letter));
        }
    }
    out
}

/// The loaded hives this tool can compact.
pub fn hives() -> Vec<Hive> {
    let devices = dos_devices();
    let Ok(k) = registry::open_read(HIVELIST) else {
        return Vec::new();
    };
    let mut out: Vec<Hive> = k
        .enum_values()
        .flatten()
        .filter_map(|(name, _)| {
            let path = registry::decode_string(&registry::read_raw(HIVELIST, &name)?);
            parse_hive(&name, path.trim_end_matches('\0'), &devices)
        })
        .collect();
    out.sort_by_key(|h| (h.root == Root::Users, h.name.clone()));
    out
}

// ---------------------------------------------------------------------------------
// Privileges and hive files

fn enable_privileges() -> Result<()> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )?;
        let mut result = Ok(());
        for name in [w!("SeBackupPrivilege"), w!("SeRestorePrivilege")] {
            let mut luid = LUID::default();
            if let Err(e) = LookupPrivilegeValueW(None, name, &mut luid) {
                result = Err(e);
                break;
            }
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            if let Err(e) = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None) {
                result = Err(e);
                break;
            }
            // AdjustTokenPrivileges succeeds without the privilege; check the last error.
            let last = windows::Win32::Foundation::GetLastError();
            if last.0 != 0 {
                result = Err(windows::core::Error::from(last.to_hresult()));
                break;
            }
        }
        let _ = CloseHandle(token);
        result.context("enabling the backup and restore privileges (needs administrator rights)")
    }
}

/// Saves a compact copy of `hive` to `dest` (which must not exist).
fn save_hive(hive: &Hive, dest: &Path) -> Result<()> {
    let _ = std::fs::remove_file(dest);
    unsafe {
        let mut key = HKEY::default();
        RegOpenKeyExW(
            hive.hkey(),
            &HSTRING::from(hive.name.as_str()),
            Some(REG_OPTION_BACKUP_RESTORE.0),
            KEY_READ,
            &mut key,
        )
        .ok()
        .with_context(|| format!("opening {}", hive.name))?;
        let r = RegSaveKeyExW(key, &HSTRING::from(dest.as_os_str()), None, REG_LATEST_FORMAT);
        let _ = RegCloseKey(key);
        r.ok().with_context(|| format!("saving {}", hive.name))?;
    }
    Ok(())
}

fn replace_hive(hive: &Hive, new: &Path, old: &Path) -> Result<()> {
    unsafe {
        RegReplaceKeyW(
            hive.hkey(),
            &HSTRING::from(hive.name.as_str()),
            &HSTRING::from(new.as_os_str()),
            &HSTRING::from(old.as_os_str()),
        )
        .ok()
        .with_context(|| format!("scheduling the swap of {}", hive.name))
    }
}

fn file_size(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok().map(|m| m.len())
}

fn same_drive(a: &Path, b: &Path) -> bool {
    let drive = |p: &Path| p.to_string_lossy().chars().next().map(|c| c.to_ascii_uppercase());
    drive(a).is_some() && drive(a) == drive(b)
}

/// Where compacted hives and the backups Windows keeps go.
fn work_root() -> PathBuf {
    PathBuf::from(wm_core::paths::expand_env(r"%ProgramData%\WinManager\RegDefrag"))
}

// ---------------------------------------------------------------------------------
// Analyze and compact

#[derive(Debug, Clone)]
pub struct Row {
    pub hive: Hive,
    pub current: Option<u64>,
    pub compacted: Option<u64>,
    pub error: Option<String>,
}

impl Row {
    pub fn saving(&self) -> u64 {
        match (self.current, self.compacted) {
            (Some(c), Some(n)) => c.saturating_sub(n),
            _ => 0,
        }
    }

    /// Percentage of the hive that compacting removes.
    pub fn rate(&self) -> f32 {
        match self.current {
            Some(c) if c > 0 => self.saving() as f32 * 100.0 / c as f32,
            _ => 0.0,
        }
    }
}

pub fn analyze(progress: &wm_core::task::Progress<Vec<Row>>) -> Result<Vec<Row>> {
    enable_privileges()?;
    let dir = std::env::temp_dir().join("wm-regdefrag");
    std::fs::create_dir_all(&dir)?;
    let hives = hives();
    let mut rows = Vec::new();
    for (i, hive) in hives.into_iter().enumerate() {
        if progress.is_cancelled() {
            break;
        }
        progress.report(i as f32 / 10.0, format!("Analyzing {}…", hive.label()));
        let dest = dir.join(hive.slug());
        let current = file_size(&hive.file);
        let (compacted, error) = match save_hive(&hive, &dest) {
            Ok(()) => (file_size(&dest), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let _ = std::fs::remove_file(&dest);
        rows.push(Row {
            hive,
            current,
            compacted,
            error,
        });
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(rows)
}

/// What a compact run scheduled.
#[derive(Debug, Clone)]
pub struct Scheduled {
    pub folder: PathBuf,
    pub hives: Vec<String>,
    pub problems: Vec<String>,
}

/// Saves fresh compact copies and schedules the swap for each hive. Earlier runs'
/// backups are removed first, so only the latest originals are kept.
pub fn compact(hives: &[Hive]) -> Result<Scheduled> {
    enable_privileges()?;
    let root = work_root();
    if root.exists() {
        for e in std::fs::read_dir(&root)?.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let folder = root.join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    std::fs::create_dir_all(&folder)?;
    let mut done = Vec::new();
    let mut problems = Vec::new();
    for hive in hives {
        if !same_drive(&hive.file, &folder) {
            problems.push(format!("{}: not on the Windows drive", hive.label()));
            continue;
        }
        let new = folder.join(format!("{}.new", hive.slug()));
        let old = folder.join(format!("{}.original", hive.slug()));
        match save_hive(hive, &new).and_then(|()| replace_hive(hive, &new, &old)) {
            Ok(()) => done.push(hive.label()),
            Err(e) => {
                let _ = std::fs::remove_file(&new);
                problems.push(format!("{}: {e:#}", hive.label()));
            }
        }
    }
    if done.is_empty() {
        let _ = std::fs::remove_dir_all(&folder);
        bail!("nothing was scheduled: {}", problems.join("; "));
    }
    Ok(Scheduled {
        folder,
        hives: done,
        problems,
    })
}

/// Seconds between scheduling the swap and the restart.
pub const RESTART_DELAY: u32 = 15;

/// Journals a compact run and restarts Windows after `delay` seconds. Returns the
/// message to show; on error, the swap is scheduled but Windows did not restart.
pub fn finish_compact(ctx: &AppCtx<'_>, s: &Scheduled, delay: u32) -> Result<String, String> {
    let desc = format!(
        "Scheduled the registry to be compacted at restart: {}. Original hives are kept in {}",
        s.hives.join(", "),
        s.folder.display()
    );
    let _ = ctx.journal.record(TOOL, desc, UndoAction::None);
    let mut msg = format!(
        "{} registry hive(s) will be compacted. Windows restarts in {delay} seconds.",
        s.hives.len()
    );
    if !s.problems.is_empty() {
        msg += &format!(" Skipped: {}", s.problems.join("; "));
    }
    run_checked(hidden_command("shutdown").args([
        "/r",
        "/t",
        &delay.to_string(),
        "/c",
        "Win Manager is compacting the registry. Changes to settings before the restart would be lost.",
    ]))
    .map(|_| msg)
    .map_err(|e| {
        format!("The registry will be compacted at the next restart, but restarting failed ({e:#}). Restart Windows yourself now, without changing settings first.")
    })
}

fn backups_size() -> u64 {
    walkdir::WalkDir::new(work_root())
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

// ---------------------------------------------------------------------------------
// UI

enum Job {
    Analyze(BackgroundTask<Vec<Row>>),
    Compact(BackgroundTask<Scheduled>),
}

#[derive(Default)]
pub struct RegistryDefrag {
    rows: Vec<Row>,
    selected: Vec<bool>,
    job: Option<Job>,
    confirm: Confirm<()>,
    message: Message,
    /// Set once swaps are scheduled; nothing else may run until Windows restarts.
    scheduled: bool,
    backups: Option<u64>,
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

impl RegistryDefrag {
    fn chosen(&self) -> Vec<Hive> {
        self.rows
            .iter()
            .zip(&self.selected)
            .filter(|(r, on)| **on && r.error.is_none() && r.compacted.is_some())
            .map(|(r, _)| r.hive.clone())
            .collect()
    }

    fn poll(&mut self, ctx: &AppCtx<'_>) {
        match &mut self.job {
            Some(Job::Analyze(task)) => {
                task.poll();
                if let Some(result) = task.take_result() {
                    match result {
                        Ok(rows) => {
                            let total: u64 = rows.iter().filter_map(|r| r.current).sum();
                            let saving: u64 = rows.iter().map(Row::saving).sum();
                            self.selected =
                                rows.iter().map(|r| r.error.is_none() && r.saving() > 0).collect();
                            let rate = if total > 0 {
                                saving as f32 * 100.0 / total as f32
                            } else {
                                0.0
                            };
                            self.message = Some(Ok(if rate < 1.0 {
                                "Analysis complete. The registry is already compact; there is no need to defrag it.".into()
                            } else {
                                format!(
                                    "Analysis complete. Compacting would save {} ({rate:.1}%).",
                                    mb(saving)
                                )
                            }));
                            self.rows = rows;
                        }
                        Err(e) => self.message = Some(Err(e)),
                    }
                    self.job = None;
                }
            }
            Some(Job::Compact(task)) => {
                task.poll();
                if let Some(result) = task.take_result() {
                    self.job = None;
                    self.message = Some(match result {
                        Ok(s) => {
                            self.scheduled = true;
                            finish_compact(ctx, &s, RESTART_DELAY)
                        }
                        Err(e) => Err(e),
                    });
                }
            }
            None => {}
        }
    }
}

impl Tool for RegistryDefrag {
    fn id(&self) -> &'static str {
        TOOL
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.poll(ctx);
        if self.rows.is_empty() && self.job.is_none() {
            // Listing hives and their sizes needs no privileges; analysis fills in the rest.
            self.rows = hives()
                .into_iter()
                .map(|hive| Row {
                    current: file_size(&hive.file),
                    hive,
                    compacted: None,
                    error: None,
                })
                .collect();
            self.selected = vec![false; self.rows.len()];
        }
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let hives = self.chosen();
                self.job = Some(Job::Compact(BackgroundTask::spawn(ui.ctx(), move |p| {
                    p.report(0.5, "Saving compact copies…");
                    compact(&hives)
                })));
            }
        }
        let backups = *self.backups.get_or_insert_with(backups_size);

        let busy = self.job.is_some();
        let can = ctx.elevated && !busy && !self.scheduled;
        let chosen = self.chosen().len();
        let compact_label = format!("Compact ({chosen})");
        let backups_label = format!("Delete backups ({})", mb(backups));
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Analyze").enabled(can)),
                Some(ToolButton::new(icons::STACK, &compact_label).enabled(can && chosen > 0)),
                None,
                Some(ToolButton::new(icons::TRASH, &backups_label).enabled(backups > 0 && !busy && !self.scheduled)),
            ],
            |_| {},
        ) {
            Some(0) => {
                self.message = None;
                self.job = Some(Job::Analyze(BackgroundTask::spawn(ui.ctx(), analyze)));
            }
            Some(1) => self.confirm.ask(
                "Windows will restart 15 seconds after the registry copies are made. Save your work and close other programs first. Continue?",
                (),
            ),
            Some(3) => {
                self.message = Some(
                    std::fs::remove_dir_all(work_root())
                        .map(|()| "Deleted the old registry backups.".into())
                        .map_err(|e| format!("{e:#}")),
                );
                self.backups = None;
            }
            _ => {}
        }
        let status = match &self.job {
            Some(Job::Analyze(t)) => t.status.clone(),
            Some(Job::Compact(_)) => "Saving compact copies…".into(),
            None if !ctx.elevated => ctx.i18n.tr("shell-needs-admin"),
            None => "Rebuilds the registry files without the gaps left by deleted keys".into(),
        };
        frame::status(ui, &status, |ui| {
            if busy {
                ui.spinner();
            }
            ui.weak(format!("{} ", icons::INFO)).on_hover_text(
                "Windows also tidies the registry by itself about once a week at startup, so the saving is often small.",
            );
        });
        frame::central_fixed(ui, |ui| self.table(ui));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl RegistryDefrag {
    fn table(&mut self, ui: &mut egui::Ui) {
        show_message(ui, &self.message);
        if self.rows.is_empty() {
            return;
        }
        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .column(egui_extras::Column::auto().at_least(24.0))
            .column(egui_extras::Column::auto().at_least(220.0))
            .column(egui_extras::Column::auto().at_least(80.0))
            .column(egui_extras::Column::auto().at_least(100.0))
            .column(egui_extras::Column::auto().at_least(60.0))
            .column(egui_extras::Column::remainder())
            .header(20.0, |mut h| {
                for t in [
                    "",
                    "Registry hive",
                    "Current",
                    "After compacting",
                    "Saving",
                    "File",
                ] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|mut body| {
                for (i, row) in self.rows.iter().enumerate() {
                    body.row(20.0, |mut r| {
                        r.col(|ui| {
                            ui.add_enabled(
                                row.error.is_none() && row.compacted.is_some(),
                                egui::Checkbox::without_text(&mut self.selected[i]),
                            );
                        });
                        r.col(|ui| {
                            ui.label(row.hive.label());
                        });
                        r.col(|ui| {
                            ui.label(row.current.map(mb).unwrap_or_default());
                        });
                        r.col(|ui| match &row.error {
                            Some(e) => {
                                ui.weak("can't read").on_hover_text(e);
                            }
                            None => match row.compacted {
                                Some(n) => {
                                    ui.label(mb(n));
                                }
                                None => {
                                    ui.weak("analyze first");
                                }
                            },
                        });
                        r.col(|ui| {
                            if row.compacted.is_some() {
                                ui.label(format!("{:.1}%", row.rate()));
                            }
                        });
                        r.col(|ui| {
                            ui.weak(row.hive.file.display().to_string());
                        });
                    });
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices() -> Vec<(String, char)> {
        vec![
            (r"\Device\HarddiskVolume3".into(), 'C'),
            (r"\Device\HarddiskVolume30".into(), 'D'),
        ]
    }

    #[test]
    fn device_paths_map_to_drive_letters() {
        let d = devices();
        assert_eq!(
            dos_path(r"\Device\HarddiskVolume3\Windows\System32\config\SOFTWARE", &d),
            Some(PathBuf::from(r"C:\Windows\System32\config\SOFTWARE"))
        );
        // Volume3 must not match Volume30.
        assert_eq!(
            dos_path(r"\Device\HarddiskVolume30\x", &d),
            Some(PathBuf::from(r"D:\x"))
        );
        assert_eq!(
            dos_path(r"\Device\HarddiskVolume1\EFI\Microsoft\Boot\BCD", &d),
            None
        );
    }

    #[test]
    fn only_known_hives_are_listed() {
        let d = devices();
        let soft = parse_hive(
            r"\REGISTRY\MACHINE\SOFTWARE",
            r"\Device\HarddiskVolume3\Windows\System32\config\SOFTWARE",
            &d,
        )
        .unwrap();
        assert_eq!((soft.root, soft.name.as_str()), (Root::Machine, "SOFTWARE"));
        assert_eq!(soft.slug(), "HKLM-SOFTWARE");
        for skipped in [
            r"\REGISTRY\MACHINE\COMPONENTS",
            r"\REGISTRY\MACHINE\BCD00000000",
            r"\REGISTRY\A\{1234}",
            r"\REGISTRY\MACHINE\HARDWARE",
        ] {
            assert!(
                parse_hive(skipped, r"\Device\HarddiskVolume3\x", &d).is_none(),
                "{skipped}"
            );
        }
        // Volatile hives have no file.
        assert!(parse_hive(r"\REGISTRY\MACHINE\SYSTEM", "", &d).is_none());
        let classes = parse_hive(
            r"\REGISTRY\USER\S-1-5-21-1-2-3-1001_Classes",
            r"\Device\HarddiskVolume3\Users\Ana\AppData\Local\Microsoft\Windows\UsrClass.dat",
            &d,
        )
        .unwrap();
        assert_eq!(classes.label(), "Ana: file associations");
        assert_eq!(classes.slug(), "HKU-S-1-5-21-1-2-3-1001_Classes");
    }

    #[test]
    fn rates() {
        let row = Row {
            hive: parse_hive(
                r"\REGISTRY\MACHINE\SYSTEM",
                r"\Device\HarddiskVolume3\Windows\System32\config\SYSTEM",
                &devices(),
            )
            .unwrap(),
            current: Some(1000),
            compacted: Some(750),
            error: None,
        };
        assert_eq!(row.saving(), 250);
        assert!((row.rate() - 25.0).abs() < 0.01);
        assert!(same_drive(Path::new(r"C:\a"), Path::new(r"c:\b")));
        assert!(!same_drive(Path::new(r"C:\a"), Path::new(r"D:\b")));
    }

    #[test]
    fn the_hive_list_reads_on_this_pc() {
        let list = hives();
        assert!(
            list.iter()
                .any(|h| h.root == Root::Machine && h.name == "SOFTWARE"),
            "{list:?}"
        );
        assert!(list.iter().all(|h| h.file.is_absolute()));
    }
}
