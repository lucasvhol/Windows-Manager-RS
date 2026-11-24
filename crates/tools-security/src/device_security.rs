//! Device Security: hide or block drives, block programs, protect USB drives against
//! autorun, remove or assign drive letters, and removable-storage policies.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use windows::{
    Win32::Storage::FileSystem::{
        DeleteVolumeMountPointW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW,
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumePathNamesForVolumeNameW,
        SetVolumeMountPointW,
    },
    core::PCWSTR,
};
use wm_core::{
    AppCtx, Category, Tool, egui, icons, registry,
    safety::{ChangeJournal, UndoAction},
    tweaks::TweakPanel,
    ui::{Confirm, Message, format_bytes, section_nav, show_message},
};

const POLICIES: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
const DISALLOW_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer\DisallowRun";
const DRIVE_REMOVABLE: u32 = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn drive_type(root: &str) -> u32 {
    let w = wide(root);
    unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) }
}

/// Letters that currently have a drive.
fn present_letters() -> u32 {
    unsafe { GetLogicalDrives() }
}

fn letter_bit(letter: char) -> u32 {
    1 << (letter as u32 - 'A' as u32)
}

/// The drive Windows is installed on (never unmounted).
fn system_letter() -> char {
    std::env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase()
}

// --- Drives: hide / block ----------------------------------------------------

fn read_mask(name: &str) -> u32 {
    registry::read_raw(POLICIES, name)
        .as_ref()
        .and_then(registry::decode_dword)
        .unwrap_or(0)
}

/// Writes NoDrives / NoViewOnDrive (deleting a value when its mask is 0), journaled.
fn save_masks(hide: u32, block: u32, journal: &ChangeJournal) -> Result<()> {
    let snaps = vec![
        registry::snapshot(POLICIES, "NoDrives"),
        registry::snapshot(POLICIES, "NoViewOnDrive"),
    ];
    for (name, mask) in [("NoDrives", hide), ("NoViewOnDrive", block)] {
        if mask == 0 {
            registry::delete_value(POLICIES, name)?;
        } else {
            registry::write_raw(POLICIES, name, &registry::dword(mask))?;
        }
    }
    journal.record(
        "devicesecurity",
        "Changed hidden and blocked drives",
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

// --- Restrict programs (DisallowRun) -----------------------------------------

fn read_blocked() -> Vec<String> {
    let Ok(key) = registry::open_read(DISALLOW_KEY) else {
        return Vec::new();
    };
    let mut items: Vec<(u32, String)> = key
        .enum_values()
        .flatten()
        .filter_map(|(name, _)| {
            let n = name.parse::<u32>().ok()?;
            let v = registry::read_raw(DISALLOW_KEY, &name)?;
            Some((n, registry::decode_string(&v)))
        })
        .collect();
    items.sort();
    items.into_iter().map(|(_, v)| v).collect()
}

/// Replaces the block list (values "1".."n") and sets the DisallowRun switch, journaled.
fn save_blocked(list: &[String], journal: &ChangeJournal) -> Result<()> {
    let old_count = read_blocked().len().max(list.len());
    let mut snaps = vec![registry::snapshot(POLICIES, "DisallowRun")];
    snaps.extend((1..=old_count).map(|n| registry::snapshot(DISALLOW_KEY, &n.to_string())));
    for n in 1..=old_count {
        registry::delete_value(DISALLOW_KEY, &n.to_string())?;
    }
    for (i, name) in list.iter().enumerate() {
        registry::write_raw(DISALLOW_KEY, &(i + 1).to_string(), &registry::string(name))?;
    }
    if list.is_empty() {
        registry::delete_value(POLICIES, "DisallowRun")?;
    } else {
        registry::write_raw(POLICIES, "DisallowRun", &registry::dword(1))?;
    }
    journal.record(
        "devicesecurity",
        format!("Blocked programs list set to {} entries", list.len()),
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

/// Accepts "name.exe" only: DisallowRun matches the file name, not a path.
fn valid_program_name(name: &str) -> bool {
    let n = name.trim();
    n.to_ascii_lowercase().ends_with(".exe") && !n.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
}

// --- USB autorun protection ----------------------------------------------------

fn autorun_path(letter: char) -> PathBuf {
    PathBuf::from(format!(r"{letter}:\autorun.inf"))
}

fn protection_undo(letter: char) -> UndoAction {
    UndoAction::RunCommand {
        program: "cmd".into(),
        args: vec![
            "/c".into(),
            format!(r"attrib -r -s -h {letter}:\autorun.inf && rmdir {letter}:\autorun.inf"),
        ],
    }
}

/// Creates a hidden, read-only `autorun.inf` folder so no autorun file can be written.
fn protect_drive(letter: char, journal: &ChangeJournal) -> Result<()> {
    let p = autorun_path(letter);
    if p.is_file() {
        bail!(
            "{} already contains an autorun.inf file. Check it with an antivirus scanner and delete it first.",
            p.display()
        );
    }
    if p.is_dir() {
        return Ok(());
    }
    std::fs::create_dir(&p).with_context(|| format!("creating {}", p.display()))?;
    wm_core::process::run_checked(wm_core::process::hidden_command("attrib").args([
        "+r",
        "+s",
        "+h",
        &p.display().to_string(),
    ]))?;
    journal.record(
        "devicesecurity",
        format!("Protected {letter}: against autorun"),
        protection_undo(letter),
    )?;
    Ok(())
}

fn unprotect_drive(letter: char) -> Result<()> {
    let UndoAction::RunCommand { program, args } = protection_undo(letter) else {
        unreachable!()
    };
    wm_core::process::run_checked(wm_core::process::hidden_command(&program).args(&args))?;
    Ok(())
}

// --- Volumes and mount points ------------------------------------------------

#[derive(Debug, Clone)]
struct Volume {
    guid_path: String,
    label: String,
    file_system: String,
    size: u64,
    mounts: Vec<String>,
}

fn list_volumes() -> Vec<Volume> {
    let mut out = Vec::new();
    let mut name = [0u16; 260];
    let Ok(handle) = (unsafe { FindFirstVolumeW(&mut name) }) else {
        return out;
    };
    loop {
        let guid_path = from_wide(&name);
        let w = wide(&guid_path);
        let mut paths = [0u16; 1024];
        let mut len = 0u32;
        let _ = unsafe { GetVolumePathNamesForVolumeNameW(PCWSTR(w.as_ptr()), Some(&mut paths), &mut len) };
        let mounts: Vec<String> = paths[..(len as usize).min(paths.len())]
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        let _ = unsafe {
            GetVolumeInformationW(
                PCWSTR(w.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                Some(&mut fs),
            )
        };
        let mut total = 0u64;
        let _ = unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), None, Some(&mut total), None) };
        out.push(Volume {
            guid_path,
            label: from_wide(&label),
            file_system: from_wide(&fs),
            size: total,
            mounts,
        });
        if unsafe { FindNextVolumeW(handle, &mut name) }.is_err() {
            break;
        }
    }
    let _ = unsafe { FindVolumeClose(handle) };
    out
}

fn remove_letter(mount: &str, guid_path: &str, journal: &ChangeJournal) -> Result<()> {
    let letter = mount.chars().next().unwrap_or('?').to_ascii_uppercase();
    if letter == system_letter() {
        bail!("The Windows drive can't be unmounted.");
    }
    let w = wide(mount);
    unsafe { DeleteVolumeMountPointW(PCWSTR(w.as_ptr())) }.with_context(|| format!("removing {mount}"))?;
    journal.record(
        "devicesecurity",
        format!("Removed drive letter {mount}"),
        UndoAction::RunCommand {
            program: "mountvol".into(),
            args: vec![mount.into(), guid_path.into()],
        },
    )?;
    Ok(())
}

fn assign_letter(letter: char, guid_path: &str, journal: &ChangeJournal) -> Result<()> {
    let mount = format!(r"{letter}:\");
    let (m, v) = (wide(&mount), wide(guid_path));
    unsafe { SetVolumeMountPointW(PCWSTR(m.as_ptr()), PCWSTR(v.as_ptr())) }
        .with_context(|| format!("assigning {mount}"))?;
    journal.record(
        "devicesecurity",
        format!("Assigned drive letter {mount}"),
        UndoAction::RunCommand {
            program: "mountvol".into(),
            args: vec![mount, "/D".into()],
        },
    )?;
    Ok(())
}

// --- Tool --------------------------------------------------------------------

const SECTIONS: [(&str, &str); 5] = [
    (icons::HARD_DRIVES, "Hide or block drives"),
    (icons::STOP_CIRCLE, "Block programs"),
    (icons::USB, "Removable storage"),
    (icons::SHIELD_CHECK, "USB drive protection"),
    (icons::TREE_STRUCTURE, "Drive letters"),
];

enum Ask {
    RemoveLetter(String, String),
    SaveDrives,
}

pub struct DeviceSecurity {
    section: usize,
    hide: Option<u32>,
    block: Option<u32>,
    blocked: Option<Vec<String>>,
    new_program: String,
    volumes: Option<Vec<Volume>>,
    assign: Option<(usize, char)>,
    policies: TweakPanel,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for DeviceSecurity {
    fn default() -> Self {
        Self {
            section: 0,
            hide: None,
            block: None,
            blocked: None,
            new_program: String::new(),
            volumes: None,
            assign: None,
            policies: TweakPanel::new("devicesecurity", crate::DEVICE_SECURITY_TWEAKS),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl DeviceSecurity {
    fn drives(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let (hide, block) = (
            *self.hide.get_or_insert_with(|| read_mask("NoDrives")),
            *self.block.get_or_insert_with(|| read_mask("NoViewOnDrive")),
        );
        ui.label("Hidden drives don't appear in This PC or File Explorer. Blocked drives can't be opened from File Explorer for this account.");
        ui.weak("Programs can still reach hidden drives by path; use Removable storage policies to really block devices.");
        ui.add_space(6.0);
        let present = present_letters();
        let letters: Vec<char> = ('A'..='Z')
            .filter(|&c| present & letter_bit(c) != 0 || (hide | block) & letter_bit(c) != 0)
            .collect();
        let (mut new_hide, mut new_block) = (hide, block);
        egui::Grid::new("drive_masks")
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("");
                for c in &letters {
                    ui.strong(format!("{c}:"));
                }
                ui.end_row();
                for (label, mask) in [("Hide", &mut new_hide), ("Block access", &mut new_block)] {
                    ui.label(label);
                    for &c in &letters {
                        let mut on = *mask & letter_bit(c) != 0;
                        if ui.checkbox(&mut on, "").changed() {
                            *mask ^= letter_bit(c);
                        }
                    }
                    ui.end_row();
                }
            });
        self.hide = Some(new_hide);
        self.block = Some(new_block);
        let changed = new_hide != read_mask("NoDrives") || new_block != read_mask("NoViewOnDrive");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(changed, egui::Button::new(format!("{}  Save", icons::FLOPPY_DISK))).clicked() {
                if new_block & letter_bit(system_letter()) != 0 {
                    self.confirm.ask(
                        format!("Blocking {}: also blocks the Windows drive in File Explorer for this account. Continue?", system_letter()),
                        Ask::SaveDrives,
                    );
                } else {
                    self.save_drives(ctx);
                }
            }
            if changed && ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                self.hide = None;
                self.block = None;
            }
            if ui.button("Restart Explorer").clicked() {
                self.message = Some(
                    wm_core::process::restart_explorer().map(|()| "Explorer restarted.".into()).map_err(|e| format!("{e:#}")),
                );
            }
        });
    }

    fn save_drives(&mut self, ctx: &mut AppCtx<'_>) {
        let (hide, block) = (self.hide.unwrap_or(0), self.block.unwrap_or(0));
        self.message = Some(if ctx.config.dry_run {
            Ok(ctx.i18n.tr("common-dry-run-skip"))
        } else {
            save_masks(hide, block, ctx.journal)
                .map(|()| "Saved. Restart Explorer to apply.".into())
                .map_err(|e| format!("{e:#}"))
        });
        self.hide = None;
        self.block = None;
    }

    fn programs(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Programs on this list can't be started from File Explorer, Start or the taskbar by this account.");
        ui.weak(
            "Windows matches the file name only, and programs started by other programs are not blocked.",
        );
        ui.add_space(6.0);
        let list = self.blocked.get_or_insert_with(read_blocked);
        let mut remove = None;
        if list.is_empty() {
            ui.weak("No programs are blocked.");
        }
        for (i, name) in list.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}  {name}", icons::STOP_CIRCLE));
                if ui
                    .small_button(icons::X)
                    .on_hover_text("Remove from the list")
                    .clicked()
                {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            list.remove(i);
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_program)
                    .hint_text("program.exe")
                    .desired_width(200.0),
            );
            if ui.button("Browse…").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("Programs", &["exe"])
                    .pick_file()
                && let Some(n) = p.file_name()
            {
                self.new_program = n.to_string_lossy().into_owned();
            }
            let ok = valid_program_name(&self.new_program);
            if ui
                .add_enabled(ok, egui::Button::new(format!("{}  Add", icons::PLUS)))
                .clicked()
            {
                let n = self.new_program.trim().to_owned();
                if !list.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                    list.push(n);
                }
                self.new_program.clear();
            }
        });
        let changed = *list != read_blocked();
        ui.add_space(6.0);
        if ui
            .add_enabled(
                changed,
                egui::Button::new(format!("{}  Save", icons::FLOPPY_DISK)),
            )
            .clicked()
        {
            let list = list.clone();
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                save_blocked(&list, ctx.journal)
                    .map(|()| {
                        "Saved. Takes effect for newly started programs; restart Explorer if needed.".into()
                    })
                    .map_err(|e| format!("{e:#}"))
            });
            self.blocked = None;
        }
    }

    fn usb_protection(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Creates a protected autorun.inf folder on a USB drive, so malware can't place an autorun file there. \
             Your files are not touched.",
        );
        ui.add_space(6.0);
        let present = present_letters();
        let drives: Vec<char> = ('A'..='Z')
            .filter(|&c| present & letter_bit(c) != 0 && drive_type(&format!(r"{c}:\")) == DRIVE_REMOVABLE)
            .collect();
        if drives.is_empty() {
            ui.weak("No USB drives or memory cards are connected.");
        }
        for c in drives {
            let protected = autorun_path(c).is_dir();
            ui.horizontal(|ui| {
                ui.label(format!("{}  {c}:", icons::USB));
                ui.weak(if protected { "protected" } else { "not protected" });
                if !protected && ui.button("Protect").clicked() {
                    self.message = Some(if ctx.config.dry_run {
                        Ok(ctx.i18n.tr("common-dry-run-skip"))
                    } else {
                        protect_drive(c, ctx.journal)
                            .map(|()| format!("{c}: is protected."))
                            .map_err(|e| format!("{e:#}"))
                    });
                }
                if protected && ui.button("Remove protection").clicked() {
                    self.message = Some(
                        unprotect_drive(c)
                            .map(|()| format!("Removed the protection from {c}:."))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            });
        }
    }

    fn letters(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Remove a drive letter to hide a partition from every program, or give a partition without a letter one.");
        ui.weak(format!(
            "The Windows drive ({}:) is never changed.",
            system_letter()
        ));
        ui.add_space(6.0);
        if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
            self.volumes = None;
        }
        let volumes = self.volumes.get_or_insert_with(list_volumes).clone();
        let used = present_letters();
        let free: Vec<char> = ('D'..='Z').filter(|&c| used & letter_bit(c) == 0).collect();
        let locked = !ctx.elevated;
        egui::Grid::new("volumes").striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
            for h in ["Mounted at", "Label", "File system", "Size", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for (i, v) in volumes.iter().enumerate() {
                ui.label(if v.mounts.is_empty() { "(no letter)".to_owned() } else { v.mounts.join(", ") });
                ui.label(&v.label);
                ui.label(&v.file_system);
                ui.label(if v.size > 0 { format_bytes(v.size) } else { String::new() });
                ui.horizontal(|ui| {
                    for m in v.mounts.iter().filter(|m| m.len() == 3) {
                        let is_system = m.starts_with(system_letter());
                        if ui
                            .add_enabled(!locked && !is_system, egui::Button::new(format!("Remove {}", &m[..2])).small())
                            .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"))
                            .clicked()
                        {
                            self.confirm.ask(
                                format!("Remove the drive letter {m}? Programs and shortcuts using it stop working until a letter is assigned again."),
                                Ask::RemoveLetter(m.clone(), v.guid_path.clone()),
                            );
                        }
                    }
                    if v.mounts.is_empty() && !v.file_system.is_empty() && !free.is_empty() {
                        let sel = match self.assign {
                            Some((k, c)) if k == i => c,
                            _ => free[0],
                        };
                        let mut chosen = sel;
                        egui::ComboBox::from_id_salt(("letter", i)).width(50.0).selected_text(format!("{chosen}:")).show_ui(ui, |ui| {
                            for &c in &free {
                                ui.selectable_value(&mut chosen, c, format!("{c}:"));
                            }
                        });
                        self.assign = Some((i, chosen));
                        if ui
                            .add_enabled(!locked, egui::Button::new("Assign").small())
                            .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"))
                            .clicked()
                        {
                            self.message = Some(if ctx.config.dry_run {
                                Ok(ctx.i18n.tr("common-dry-run-skip"))
                            } else {
                                assign_letter(chosen, &v.guid_path, ctx.journal)
                                    .map(|()| format!("Assigned {chosen}:."))
                                    .map_err(|e| format!("{e:#}"))
                            });
                            self.volumes = None;
                        }
                    }
                });
                ui.end_row();
            }
        });
    }
}

impl Tool for DeviceSecurity {
    fn id(&self) -> &'static str {
        "devicesecurity"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        match self.confirm.show(ui.ctx(), ctx.i18n) {
            Some(Ask::RemoveLetter(mount, guid)) => {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    remove_letter(&mount, &guid, ctx.journal)
                        .map(|()| format!("Removed {mount}."))
                        .map_err(|e| format!("{e:#}"))
                });
                self.volumes = None;
            }
            Some(Ask::SaveDrives) => self.save_drives(ctx),
            None => {}
        }
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(210.0);
                if section_nav(ui, &SECTIONS, &mut self.section) {
                    self.message = None;
                }
            });
            ui.add_space(14.0);
            ui.vertical(|ui| {
                ui.heading(SECTIONS[self.section].1);
                ui.add_space(4.0);
                show_message(ui, &self.message);
                match self.section {
                    0 => self.drives(ui, ctx),
                    1 => self.programs(ui, ctx),
                    2 => self.policies.ui(ui, ctx),
                    3 => self.usb_protection(ui, ctx),
                    _ => self.letters(ui, ctx),
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_names() {
        assert!(valid_program_name("game.exe"));
        assert!(valid_program_name(" Game.EXE "));
        assert!(!valid_program_name(r"C:\x\game.exe"));
        assert!(!valid_program_name("game"));
        assert!(!valid_program_name("*.exe"));
    }

    #[test]
    fn letter_bits() {
        assert_eq!(letter_bit('A'), 1);
        assert_eq!(letter_bit('C'), 4);
        assert_eq!(letter_bit('Z'), 1 << 25);
    }

    #[test]
    fn lists_at_least_the_system_volume() {
        let vols = list_volumes();
        let sys = format!(r"{}:\", system_letter());
        assert!(vols.iter().any(|v| v.mounts.contains(&sys)), "{vols:?}");
    }

    #[test]
    fn policies_parse() {
        for t in wm_core::tweaks::parse(crate::DEVICE_SECURITY_TWEAKS).unwrap() {
            let _ = t.state();
        }
    }
}
