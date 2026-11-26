//! Files and Folders (File Security): a list of files and folders to encrypt or decrypt
//! (through Security Lock), shred, protect or unlock; plus a viewer for an item's owner
//! and permissions with take ownership, grant yourself access, reset to inherited
//! permissions and removal of explicit entries.
//!
//! Reading uses the Win32 security API; changes go through `icacls` (locale-independent
//! switches). Before each change the ACLs are saved with `icacls /save` and the owner is
//! recorded, so the journal can restore both.

use std::path::{Path, PathBuf};

use crate::file_tools::{self, Holder, ShredReport};
use crate::security_lock::SecurityLock;
use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, LookupAccountSidW,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui,
    explorer_verb::{self, Target, Verb},
    icons, paths,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, frame, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct AceInfo {
    pub account: String,
    pub sid: String,
    pub allow: bool,
    pub mask: u32,
    pub inherited: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Security {
    pub owner: String,
    pub entries: Vec<AceInfo>,
}

/// Human-readable summary of an access mask.
pub fn mask_text(mask: u32) -> String {
    const GENERIC_ALL: u32 = 0x1000_0000;
    const FULL: u32 = 0x001F_01FF;
    const MODIFY: u32 = 0x0013_01BF;
    const READ_EXECUTE: u32 = 0x0012_00A9;
    const READ: u32 = 0x0012_0089;
    const WRITE: u32 = 0x0010_0116;
    if mask & GENERIC_ALL != 0 || mask & FULL == FULL {
        "Full control".into()
    } else if mask & MODIFY == MODIFY {
        "Modify".into()
    } else if mask & READ_EXECUTE == READ_EXECUTE {
        if mask & WRITE == WRITE {
            "Read, execute and write".into()
        } else {
            "Read and execute".into()
        }
    } else if mask & READ == READ {
        if mask & WRITE == WRITE {
            "Read and write".into()
        } else {
            "Read".into()
        }
    } else if mask & WRITE == WRITE {
        "Write".into()
    } else if mask & 0xA000_0000 != 0 {
        "Read and execute (generic)".into()
    } else {
        format!("Special (0x{mask:08X})")
    }
}

fn account_name(sid: PSID) -> String {
    unsafe {
        let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
        let (mut nlen, mut dlen) = (name.len() as u32, domain.len() as u32);
        let mut use_ = SID_NAME_USE(0);
        if LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut nlen,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut dlen,
            &mut use_,
        )
        .is_ok()
        {
            let n = String::from_utf16_lossy(&name[..nlen as usize]);
            let d = String::from_utf16_lossy(&domain[..dlen as usize]);
            return if d.is_empty() { n } else { format!("{d}\\{n}") };
        }
        let s = sid_string(sid);
        if s.is_empty() {
            "(unknown account)".into()
        } else {
            s
        }
    }
}

/// `S-1-1-0` style text for a SID (the same in every Windows language).
fn sid_string(sid: PSID) -> String {
    unsafe {
        let mut s = PWSTR::null();
        if ConvertSidToStringSidW(sid, &mut s).is_ok() {
            let text = s.to_string().unwrap_or_default();
            let _ = LocalFree(Some(HLOCAL(s.0.cast())));
            return text;
        }
        String::new()
    }
}

pub fn read_security(path: &Path) -> Result<Security> {
    unsafe {
        let mut owner = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let err = GetNamedSecurityInfoW(
            &HSTRING::from(path.as_os_str()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        );
        if err != WIN32_ERROR(0) {
            bail!(
                "could not read permissions: {}",
                windows::core::Error::from(err.to_hresult()).message()
            );
        }
        let result = {
            let mut entries = Vec::new();
            if !dacl.is_null() {
                for i in 0..u32::from((*dacl).AceCount) {
                    let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                    if GetAce(dacl, i, &mut ace).is_err() {
                        continue;
                    }
                    let header = &*(ace as *const ACE_HEADER);
                    // 0 = ACCESS_ALLOWED, 1 = ACCESS_DENIED; both share this layout.
                    if header.AceType > 1 {
                        continue;
                    }
                    let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
                    let sid = PSID(std::ptr::addr_of!(allowed.SidStart) as *mut _);
                    entries.push(AceInfo {
                        account: account_name(sid),
                        sid: sid_string(sid),
                        allow: header.AceType == 0,
                        mask: allowed.Mask,
                        inherited: header.AceFlags & 0x10 != 0,
                    });
                }
            }
            Security {
                owner: account_name(owner),
                entries,
            }
        };
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        Ok(result)
    }
}

pub fn current_user() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();
    if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    TakeOwnership,
    GrantMeFullControl,
    ResetToInherited,
    RemoveEntries {
        account: String,
        allow: bool,
    },
    /// Deny everyone (administrators too) changing the item: writing always, running
    /// programs unless `executable`, deleting if `undeletable`.
    Protect {
        executable: bool,
        undeletable: bool,
    },
    /// Removes the explicit deny entries for Everyone that Protect adds.
    Unprotect,
}

/// The Everyone group, by SID so it works in every Windows language.
const EVERYONE: &str = "*S-1-1-0";

impl Change {
    pub fn describe(&self) -> String {
        match self {
            Change::Protect {
                executable,
                undeletable,
            } => {
                let mut s = "Protect from changes".to_owned();
                if *executable {
                    s += ", programs can run";
                }
                if *undeletable {
                    s += ", undeletable";
                }
                s
            }
            Change::Unprotect => "Remove protection".into(),
            Change::TakeOwnership => "Take ownership".into(),
            Change::GrantMeFullControl => "Grant me full control".into(),
            Change::ResetToInherited => "Reset to inherited permissions".into(),
            Change::RemoveEntries { account, allow } => {
                format!(
                    "Remove explicit {} entries for {account}",
                    if *allow { "allow" } else { "deny" }
                )
            }
        }
    }

    /// `icacls` arguments after the path.
    pub fn icacls_args(&self, is_dir: bool, recursive: bool) -> Vec<String> {
        let mut a: Vec<String> = match self {
            Change::TakeOwnership => vec!["/setowner".into(), current_user()],
            Change::GrantMeFullControl => {
                let inherit = if is_dir { "(OI)(CI)" } else { "" };
                vec!["/grant".into(), format!("{}:{inherit}F", current_user())]
            }
            Change::ResetToInherited => vec!["/reset".into()],
            Change::Protect {
                executable,
                undeletable,
            } => {
                let mut rights = vec!["W"];
                if *undeletable {
                    rights.push("D");
                    if is_dir {
                        rights.push("DC");
                    }
                }
                let inherit = if is_dir { "(OI)(CI)" } else { "" };
                let mut a = vec![
                    "/deny".into(),
                    format!("{EVERYONE}:{inherit}({})", rights.join(",")),
                ];
                if !executable {
                    // On a folder, X also means "enter it", so deny running only for the
                    // files inside (inherit-only), never for the folder itself.
                    a.push(if is_dir {
                        format!("{EVERYONE}:(OI)(IO)(X)")
                    } else {
                        format!("{EVERYONE}:(X)")
                    });
                }
                // Inheritance already covers the contents; no /t.
                a.push("/c".into());
                return a;
            }
            Change::Unprotect => vec!["/remove:d".into(), EVERYONE.into()],
            Change::RemoveEntries { account, allow } => {
                vec![
                    if *allow { "/remove:g" } else { "/remove:d" }.into(),
                    account.clone(),
                ]
            }
        };
        if recursive && is_dir {
            a.push("/t".into());
        }
        a.push("/c".into());
        a
    }
}

fn icacls(path: &Path, args: &[String]) -> Result<()> {
    run_checked(hidden_command("icacls").arg(path).args(args)).map(|_| ())
}

/// Whether the folder's only explicit deny for Everyone is the "delete subfolders and
/// files" entry that Undeletable adds, so removing Everyone's deny entries there
/// undoes just that (and never a protection set on the folder itself).
fn only_dc_denied(folder: &Path) -> bool {
    const DELETE_CHILD: u32 = 0x40;
    let Ok(sec) = read_security(folder) else {
        return false;
    };
    let denies: Vec<&AceInfo> = sec
        .entries
        .iter()
        .filter(|e| !e.allow && !e.inherited && e.sid == "S-1-1-0")
        .collect();
    !denies.is_empty() && denies.iter().all(|e| e.mask == DELETE_CHILD)
}

/// Applies a change after saving the current ACLs; returns the undo action.
pub fn apply(path: &Path, change: &Change, recursive: bool, backups: &Path) -> Result<UndoAction> {
    let is_dir = path.is_dir();
    let parent = path
        .parent()
        .context("cannot change permissions of a drive root here")?;
    let before = read_security(path)?;
    let tree = recursive && is_dir;

    // Single object: exact SDDL snapshot, restorable without elevation.
    // Whole tree: icacls /save + /restore (requires administrator rights, like the change).
    let mut steps = if tree {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
        let backup: PathBuf = backups.join(format!("{stamp}-acl.txt"));
        icacls(
            path,
            &[
                "/save".into(),
                backup.display().to_string(),
                "/t".into(),
                "/c".into(),
            ],
        )
        .context("saving current permissions")?;
        vec![UndoAction::RunCommand {
            program: "icacls".into(),
            args: vec![
                parent.display().to_string(),
                "/restore".into(),
                backup.display().to_string(),
                "/c".into(),
            ],
        }]
    } else {
        vec![UndoAction::RestoreAcl {
            path: path.to_owned(),
            sddl: wm_core::acl::dacl_sddl(path)?,
        }]
    };
    icacls(path, &change.icacls_args(is_dir, recursive))?;

    // Windows also lets an item be deleted by anyone allowed to "delete subfolders and
    // files" in its folder, so Undeletable denies that on the parent too. The entry is
    // not inherited, so the folder's other items stay deletable through their own
    // permissions. Drive roots are left alone.
    let parent_args: Option<Vec<String>> = match change {
        Change::Protect {
            undeletable: true, ..
        } => Some(vec!["/deny".into(), format!("{EVERYONE}:(DC)"), "/c".into()]),
        Change::Unprotect if only_dc_denied(parent) => {
            Some(vec!["/remove:d".into(), EVERYONE.into(), "/c".into()])
        }
        _ => None,
    };
    if let Some(args) = parent_args
        && parent.parent().is_some()
    {
        steps.push(UndoAction::RestoreAcl {
            path: parent.to_owned(),
            sddl: wm_core::acl::dacl_sddl(parent)?,
        });
        icacls(parent, &args)?;
    }

    if *change == Change::TakeOwnership {
        let mut args = vec![
            path.display().to_string(),
            "/setowner".into(),
            before.owner.clone(),
        ];
        if recursive && is_dir {
            args.push("/t".into());
        }
        args.push("/c".into());
        steps.push(UndoAction::RunCommand {
            program: "icacls".into(),
            args,
        });
    }
    Ok(UndoAction::Sequence { steps })
}

const VERB: Verb<'static> = Verb {
    id: "WinManager.FilesAndFolders",
    label: "Encrypt, shred or unlock with Win Manager",
    args: "--open filesecurity --add",
    targets: &[Target::AnyFile, Target::Folder],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Encrypt,
    Shred,
    Permissions,
    Unlock,
}

#[derive(Default)]
pub struct FileSecurity {
    items: Vec<PathBuf>,
    /// The item whose permissions are shown.
    selected: Option<usize>,
    tab: Tab,
    /// The Encrypt tab is on Decrypt File.
    decrypt: bool,
    lock: SecurityLock,
    security: Option<Result<Security, String>>,
    recursive: bool,
    executable: bool,
    undeletable: bool,
    confirm: Confirm<(Change, bool)>,
    shred_confirm: Confirm<()>,
    shred_task: Option<BackgroundTask<ShredReport>>,
    holders: Option<Result<Vec<Holder>, String>>,
    unlock_confirm: Confirm<()>,
    message: Message,
    handled_args: bool,
    menu_installed: Option<bool>,
}

impl FileSecurity {
    fn selected_path(&self) -> Option<PathBuf> {
        self.selected.and_then(|i| self.items.get(i)).cloned()
    }

    fn load(&mut self) {
        self.security = self
            .selected_path()
            .map(|p| read_security(&p).map_err(|e| format!("{e:#}")));
    }

    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if !self.items.contains(&p) {
                self.items.push(p);
            }
        }
        if self.selected.is_none() && !self.items.is_empty() {
            self.selected = Some(0);
            self.load();
        }
        self.holders = None;
    }

    fn list_ui(&mut self, ui: &mut egui::Ui) {
        ui.strong("Files");
        ui.separator();

        let mut clicked = None;
        if self.items.is_empty() {
            ui.weak("Add files or folders, or drop them on this window.");
        }
        for (i, p) in self.items.iter().enumerate() {
            let icon = if p.is_dir() {
                icons::FOLDERS
            } else {
                icons::FILE_TEXT
            };
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            if ui
                .selectable_label(self.selected == Some(i), format!("{icon}  {name}"))
                .on_hover_text(p.display().to_string())
                .clicked()
            {
                clicked = Some(i);
            }
        }
        if let Some(i) = clicked {
            self.selected = Some(i);
            self.load();
        }
    }

    /// Asks to shred the files in the list.
    fn ask_shred(&mut self) {
        let files = file_tools::shred_files(&self.items).len();
        if files == 0 || self.shred_task.is_some() {
            return;
        }
        self.shred_confirm.ask(
            format!("Shred {files} files? They can't be recovered, not even from the Change Journal."),
            (),
        );
    }

    fn shred_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if let Some(t) = &mut self.shred_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.shred_task = None;
                self.message = Some(r.and_then(|rep| {
                    let _ = ctx.journal.record(
                        "filesecurity",
                        format!("Shredded {} files", rep.shredded),
                        UndoAction::None,
                    );
                    if rep.failed.is_empty() {
                        Ok(format!("Shredded {} files.", rep.shredded))
                    } else {
                        Err(format!(
                            "Shredded {}; not shredded: {}",
                            rep.shredded,
                            rep.failed.join("; ")
                        ))
                    }
                }));
                self.items.retain(|p| p.exists());
                self.selected = None;
                self.security = None;
            }
        }
        if self.shred_confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let targets = self.items.clone();
                self.shred_task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    Ok(file_tools::shred(&targets, &|f, s| p.report(f, s)))
                }));
            }
        }
        ui.label("Overwrites each file with random data, gives it a random name, then deletes it, so recovery tools can't bring it back.");
        ui.weak("On SSDs and USB drives the drive may keep old copies of the data internally; for those, encryption gives stronger protection. System folders are refused.");
        let files = file_tools::shred_files(&self.items).len();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    files > 0 && self.shred_task.is_none(),
                    egui::Button::new(format!("Shred {files} files")),
                )
                .clicked()
            {
                self.shred_confirm.ask(
                    format!(
                        "Shred {files} files? They can't be recovered, not even from the Change Journal."
                    ),
                    (),
                );
            }
            if let Some(t) = &self.shred_task {
                ui.add(
                    egui::ProgressBar::new(t.progress)
                        .desired_width(240.0)
                        .text(t.status.clone()),
                );
            }
        });
    }

    fn unlock_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if self.unlock_confirm.show(ui.ctx(), ctx.i18n).is_some() {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                file_tools::close_holders(&self.items)
                    .and_then(|()| {
                        ctx.journal.record(
                            "filesecurity",
                            format!("Closed the programs using {} item(s)", self.items.len()),
                            UndoAction::None,
                        )?;
                        Ok(())
                    })
                    .map(|()| "Asked the programs to close; any that didn't were ended.".into())
                    .map_err(|e| format!("{e:#}"))
            });
            self.holders = None;
        }
        ui.label("Finds the programs that keep these files open, so you can close them and then move, rename or delete the files.");
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.items.is_empty(), egui::Button::new("Find programs"))
                .clicked()
            {
                self.holders = Some(file_tools::holders(&self.items).map_err(|e| format!("{e:#}")));
            }
            let any = matches!(&self.holders, Some(Ok(h)) if !h.is_empty());
            if ui.add_enabled(any, egui::Button::new("Close them")).clicked() {
                self.unlock_confirm.ask(
                    "Close the programs using these files? They are asked to close first and ended if they don't; unsaved work in them is lost.",
                    (),
                );
            }
        });
        let mut end = None;
        match &self.holders {
            None => {}
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            Some(Ok(h)) if h.is_empty() => {
                ui.weak("No program has these files open.");
            }
            Some(Ok(h)) => {
                egui::Grid::new("fs-holders")
                    .striped(true)
                    .num_columns(4)
                    .show(ui, |ui| {
                        for t in ["Program", "Process ID", "Service", ""] {
                            ui.strong(t);
                        }
                        ui.end_row();
                        for x in h {
                            ui.label(&x.name);
                            ui.label(x.pid.to_string());
                            ui.weak(&x.service);
                            if x.pid != std::process::id() && ui.small_button("End process").clicked() {
                                end = Some(x.clone());
                            }
                            ui.end_row();
                        }
                    });
            }
        }
        if let Some(x) = end {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                run_checked(hidden_command("taskkill").args(["/PID", &x.pid.to_string(), "/F"]))
                    .map(|_| {
                        let _ = ctx.journal.record(
                            "filesecurity",
                            format!("Ended {} ({}) to unlock files", x.name, x.pid),
                            UndoAction::None,
                        );
                        format!("Ended {}.", x.name)
                    })
                    .map_err(|e| format!("{e:#}"))
            });
            self.holders = Some(file_tools::holders(&self.items).map_err(|e| format!("{e:#}")));
        }
    }

    fn apply_change(&mut self, ctx: &AppCtx<'_>, change: &Change, all: bool) -> Result<String, String> {
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        let targets: Vec<PathBuf> = if all {
            self.items.clone()
        } else {
            self.selected_path().into_iter().collect()
        };
        let backups = paths::backups_dir().map_err(|e| format!("{e:#}"))?;
        let mut problems = Vec::new();
        for path in &targets {
            let r = apply(path, change, self.recursive, &backups).and_then(|undo| {
                ctx.journal.record(
                    "filesecurity",
                    format!("{} on {}", change.describe(), path.display()),
                    undo,
                )
            });
            if let Err(e) = r {
                problems.push(format!("{}: {e:#}", path.display()));
            }
        }
        if problems.is_empty() {
            Ok(format!("{} done.", change.describe()))
        } else {
            Err(problems.join("; "))
        }
    }

    fn permissions_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if let Some((change, all)) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(self.apply_change(ctx, &change, all));
            self.load();
        }
        ui.strong("Protect every item in the list");
        ui.weak("Stops everyone, administrators included, from changing the items. Remove the protection to edit them again.");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.executable, "Executable")
                .on_hover_text("Programs such as EXE and BAT files can still run.");
            ui.checkbox(&mut self.undeletable, "Undeletable")
                .on_hover_text("The items can't be deleted either.");
            let any = !self.items.is_empty();
            if ui.add_enabled(any, egui::Button::new("Protect")).clicked() {
                let c = Change::Protect {
                    executable: self.executable,
                    undeletable: self.undeletable,
                };
                self.confirm.ask(
                    format!(
                        "{} for {} item(s)? The previous permissions are saved for undo.",
                        c.describe(),
                        self.items.len()
                    ),
                    (c, true),
                );
            }
            if ui
                .add_enabled(any, egui::Button::new("Remove protection"))
                .clicked()
            {
                self.confirm.ask(
                    format!("Remove the protection from {} item(s)?", self.items.len()),
                    (Change::Unprotect, true),
                );
            }
        });
        ui.separator();

        let Some(path) = self.selected_path() else {
            ui.weak("Select an item in the list to see its owner and permissions.");
            return;
        };
        ui.horizontal(|ui| {
            ui.strong(path.display().to_string());
            if ui.small_button(ctx.i18n.tr("common-refresh")).clicked() {
                self.load();
            }
        });
        let sec = match &self.security {
            Some(Ok(s)) => s.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        ui.horizontal(|ui| {
            ui.weak("Owner:");
            ui.strong(&sec.owner);
        });
        let mut remove = None;
        egui::Grid::new("acl")
            .num_columns(5)
            .striped(true)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for h in ["Account", "Type", "Access", "Source", ""] {
                    ui.strong(h);
                }
                ui.end_row();
                for e in &sec.entries {
                    ui.label(&e.account);
                    if e.allow {
                        ui.label("Allow");
                    } else {
                        ui.colored_label(ui.visuals().warn_fg_color, "Deny");
                    }
                    ui.label(mask_text(e.mask));
                    ui.weak(if e.inherited { "inherited" } else { "explicit" });
                    if !e.inherited
                        && ui
                            .add_enabled(ctx.elevated, egui::Button::new("Remove").small())
                            .clicked()
                    {
                        remove = Some(Change::RemoveEntries {
                            account: e.account.clone(),
                            allow: e.allow,
                        });
                    }
                    ui.end_row();
                }
            });
        if let Some(c) = remove {
            self.confirm.ask(
                format!("{}? The previous permissions are saved for undo.", c.describe()),
                (c, false),
            );
        }

        ui.add_space(8.0);
        ui.add_enabled_ui(ctx.elevated, |ui| {
            if path.is_dir() {
                ui.checkbox(&mut self.recursive, "Apply to all subfolders and files");
            }
            ui.horizontal(|ui| {
                for c in [
                    Change::TakeOwnership,
                    Change::GrantMeFullControl,
                    Change::ResetToInherited,
                ] {
                    if ui.button(c.describe()).clicked() {
                        let warn = if matches!(c, Change::ResetToInherited) {
                            " Explicit permissions (including deny entries) are removed."
                        } else {
                            ""
                        };
                        self.confirm.ask(
                            format!("{} for {}?{warn}", c.describe(), path.display()),
                            (c, false),
                        );
                    }
                }
            });
        });
        if !ctx.elevated {
            ui.weak(ctx.i18n.tr("shell-needs-admin"));
        }
    }
}

/// File & Folder Unlocker: the same window, opened on its Unlock tab.
pub struct Unlocker(FileSecurity);

impl Default for Unlocker {
    fn default() -> Self {
        Self(FileSecurity {
            tab: Tab::Unlock,
            ..FileSecurity::default()
        })
    }
}

impl Tool for Unlocker {
    fn id(&self) -> &'static str {
        "unlocker"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.0.ui(ui, ctx);
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl Tool for FileSecurity {
    fn id(&self) -> &'static str {
        "filesecurity"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if !self.handled_args {
            self.handled_args = true;
            if let Some(p) = explorer_verb::arg_after("--add").filter(|p| p.exists()) {
                self.add([p]);
            }
        }
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.add(dropped);
        }

        // The original's tabs.
        let before = (self.tab, self.decrypt);
        egui::Panel::top("fs-tabs")
            .resizable(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 6)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (tab, decrypt, icon, label) in [
                        (Tab::Encrypt, false, icons::LOCK, "Encrypt File"),
                        (Tab::Encrypt, true, icons::KEY, "Decrypt File"),
                        (Tab::Shred, false, icons::SCISSORS, "Shred File"),
                        (
                            Tab::Permissions,
                            false,
                            icons::SHIELD_CHECK,
                            "File & Folder Permission",
                        ),
                        (Tab::Unlock, false, icons::FILE_LOCK, "File & Folder Unlocker"),
                    ] {
                        let on = self.tab == tab && (tab != Tab::Encrypt || self.decrypt == decrypt);
                        if ui
                            .selectable_label(on, egui::RichText::new(format!("{icon}  {label}")).size(14.0))
                            .clicked()
                        {
                            self.tab = tab;
                            self.decrypt = decrypt;
                        }
                    }
                });
            });
        if before != (self.tab, self.decrypt) {
            self.message = None;
        }
        self.lock.set_encrypt(!self.decrypt);

        // The original's action column.
        let (has_sel, has_items) = (self.selected.is_some(), !self.items.is_empty());
        let installed = *self.menu_installed.get_or_insert_with(|| VERB.is_installed());
        let action = match self.tab {
            Tab::Encrypt if self.decrypt => Some((icons::KEY, "Decrypt File")),
            Tab::Encrypt => Some((icons::LOCK, "Encrypt File")),
            Tab::Shred => Some((icons::SCISSORS, "Shred File")),
            Tab::Unlock => Some((icons::MAGNIFYING_GLASS, "Find Programs")),
            Tab::Permissions => None,
        };
        let mut act = None;
        frame::right(ui, 210.0, |ui| {
            if frame::action(ui, icons::PLUS, "Add File", true) {
                act = Some("file");
            }
            if frame::action(ui, icons::FOLDERS, "Add Folder", true) {
                act = Some("folder");
            }
            if frame::action(ui, icons::X, "Remove Selected", has_sel) {
                act = Some("remove");
            }
            if frame::action(ui, icons::X, "Remove All", has_items) {
                act = Some("clear");
            }
            if let Some((icon, label)) = action
                && frame::action(ui, icon, label, has_items)
            {
                act = Some("run");
            }
            ui.separator();
            let label = if installed {
                "Remove from Context Menu"
            } else {
                "Add to Context Menu"
            };
            if frame::action(ui, icons::LIST, label, true) {
                act = Some("menu");
            }
        });
        match act {
            Some("file") => {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    self.add(files);
                }
            }
            Some("folder") => {
                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                    self.add([d]);
                }
            }
            Some("remove") => {
                if let Some(i) = self.selected {
                    self.items.remove(i);
                    self.selected = (!self.items.is_empty()).then(|| i.min(self.items.len() - 1));
                    self.load();
                    self.holders = None;
                }
            }
            Some("clear") => {
                self.items.clear();
                self.selected = None;
                self.security = None;
                self.holders = None;
            }
            Some("run") => match self.tab {
                Tab::Encrypt => {
                    self.lock.set_targets(&self.items);
                    if let Err(e) = self.lock.request() {
                        self.message = Some(Err(e));
                    }
                }
                Tab::Shred => self.ask_shred(),
                Tab::Unlock => {
                    self.holders = Some(file_tools::holders(&self.items).map_err(|e| format!("{e:#}")));
                }
                Tab::Permissions => {}
            },
            Some("menu") => {
                let r = if installed {
                    VERB.uninstall(ctx.journal, "filesecurity")
                } else {
                    VERB.install(ctx.journal, "filesecurity")
                };
                self.message = Some(
                    r.map(|()| {
                        if installed {
                            "Removed from Explorer's right-click menu.".into()
                        } else {
                            "Right-click files or folders in Explorer and choose \"Encrypt, shred or unlock with \
                             Win Manager\" (on Windows 11 under \"Show more options\")."
                                .into()
                        }
                    })
                    .map_err(|e| format!("{e:#}")),
                );
                self.menu_installed = None;
            }
            _ => {}
        }

        let dirs = self.items.iter().filter(|p| p.is_dir()).count();
        frame::status(
            ui,
            &format!("Total files: {}, folders: {dirs}", self.items.len() - dirs),
            |ui| {
                ui.weak("You can also drop files and folders on this window.");
            },
        );
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            self.list_ui(ui);
            ui.add_space(8.0);
            ui.separator();
            match self.tab {
                Tab::Encrypt => {
                    self.lock.set_targets(&self.items);
                    self.lock.poll(ui, ctx);
                    self.lock.body(ui, ctx);
                }
                Tab::Shred => self.shred_ui(ui, ctx),
                Tab::Permissions => self.permissions_ui(ui, ctx),
                Tab::Unlock => self.unlock_ui(ui, ctx),
            }
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
    fn masks() {
        assert_eq!(mask_text(0x001F_01FF), "Full control");
        assert_eq!(mask_text(0x0013_01BF), "Modify");
        assert_eq!(mask_text(0x0012_00A9), "Read and execute");
        assert_eq!(mask_text(0x0012_0089), "Read");
        assert_eq!(mask_text(0x1000_0000), "Full control");
        assert!(mask_text(0x0000_0001).starts_with("Special"));
    }

    #[test]
    fn icacls_arguments() {
        let user = current_user();
        assert_eq!(
            Change::GrantMeFullControl.icacls_args(true, true),
            ["/grant", &format!("{user}:(OI)(CI)F"), "/t", "/c"]
        );
        assert_eq!(
            Change::GrantMeFullControl.icacls_args(false, true),
            ["/grant", &format!("{user}:F"), "/c"]
        );
        assert_eq!(
            Change::RemoveEntries {
                account: "Everyone".into(),
                allow: false
            }
            .icacls_args(false, false),
            ["/remove:d", "Everyone", "/c"]
        );
    }

    #[test]
    fn read_and_change_own_temp_file_with_undo() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.txt");
        std::fs::write(&file, "x").unwrap();
        let before = read_security(&file).unwrap();
        assert!(!before.owner.is_empty());
        assert!(before.entries.iter().any(|e| e.allow), "{before:?}");

        // Granting ourselves full control does not need elevation on our own file.
        let backups = dir.path().join("b");
        std::fs::create_dir_all(&backups).unwrap();
        let undo = apply(&file, &Change::GrantMeFullControl, false, &backups).unwrap();
        let after = read_security(&file).unwrap();
        assert!(
            after.entries.iter().any(|e| !e.inherited && e.mask_full()),
            "{after:?}"
        );

        let journal = wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &backups).unwrap();
        let e = journal.record("t", "grant", undo).unwrap();
        journal.undo(e.id).unwrap();
        let restored = read_security(&file).unwrap();
        assert_eq!(restored.entries, before.entries, "undo restores the original ACL");
    }

    #[test]
    fn protect_arguments() {
        let p = |executable, undeletable| Change::Protect {
            executable,
            undeletable,
        };
        assert_eq!(
            p(false, false).icacls_args(false, true),
            ["/deny", "*S-1-1-0:(W)", "*S-1-1-0:(X)", "/c"]
        );
        // Folders: inherited by the contents, and never deny entering the folder itself.
        assert_eq!(
            p(false, true).icacls_args(true, true),
            ["/deny", "*S-1-1-0:(OI)(CI)(W,D,DC)", "*S-1-1-0:(OI)(IO)(X)", "/c"]
        );
        assert_eq!(
            p(true, false).icacls_args(true, false),
            ["/deny", "*S-1-1-0:(OI)(CI)(W)", "/c"]
        );
        assert_eq!(
            Change::Unprotect.icacls_args(false, false),
            ["/remove:d", "*S-1-1-0", "/c"]
        );
    }

    #[test]
    fn protecting_blocks_writes_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.txt");
        std::fs::write(&file, "x").unwrap();
        let backups = dir.path().join("b");
        std::fs::create_dir_all(&backups).unwrap();
        let protect = Change::Protect {
            executable: true,
            undeletable: true,
        };
        let undo = apply(&file, &protect, false, &backups).unwrap();
        assert!(std::fs::write(&file, "changed").is_err(), "writing is denied");
        assert!(std::fs::remove_file(&file).is_err(), "deleting is denied");
        assert!(
            !only_dc_denied(&file),
            "the file's own deny entries are not DC-only"
        );
        assert!(
            only_dc_denied(dir.path()),
            "Undeletable also denies DC on the folder"
        );
        apply(&file, &Change::Unprotect, false, &backups).unwrap();
        assert!(
            !read_security(dir.path())
                .unwrap()
                .entries
                .iter()
                .any(|e| !e.allow),
            "the folder's deny entry is removed"
        );
        std::fs::write(&file, "changed").unwrap();
        // The journal's undo for Protect also restores the ACL.
        let journal = wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &backups).unwrap();
        let e = journal.record("t", "protect", undo).unwrap();
        journal.undo(e.id).unwrap();
        std::fs::remove_file(&file).unwrap();
    }

    impl AceInfo {
        fn mask_full(&self) -> bool {
            mask_text(self.mask) == "Full control"
        }
    }
}
