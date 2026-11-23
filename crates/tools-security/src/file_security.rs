//! File Security: view a file or folder's owner and permissions, take ownership,
//! grant yourself access, reset to inherited permissions, or remove explicit entries.
//!
//! Reading uses the Win32 security API; changes go through `icacls` (locale-independent
//! switches). Before each change the ACLs are saved with `icacls /save` and the owner is
//! recorded, so the journal can restore both.

use std::path::{Path, PathBuf};

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
    AppCtx, Category, Tool, egui, paths,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct AceInfo {
    pub account: String,
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
        let mut s = PWSTR::null();
        if ConvertSidToStringSidW(sid, &mut s).is_ok() {
            let text = s.to_string().unwrap_or_default();
            let _ = LocalFree(Some(HLOCAL(s.0.cast())));
            return text;
        }
        "(unknown account)".into()
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
    RemoveEntries { account: String, allow: bool },
}

impl Change {
    pub fn describe(&self) -> String {
        match self {
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

#[derive(Default)]
pub struct FileSecurity {
    path: String,
    security: Option<Result<Security, String>>,
    recursive: bool,
    confirm: Confirm<Change>,
    message: Message,
}

impl FileSecurity {
    fn load(&mut self) {
        self.security = Some(read_security(Path::new(self.path.trim())).map_err(|e| format!("{e:#}")));
    }
}

impl Tool for FileSecurity {
    fn id(&self) -> &'static str {
        "filesecurity"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(change) = self.confirm.show(ui.ctx(), ctx.i18n) {
            let path = PathBuf::from(self.path.trim());
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                paths::backups_dir()
                    .and_then(|b| apply(&path, &change, self.recursive, &b))
                    .and_then(|undo| {
                        ctx.journal.record(
                            "filesecurity",
                            format!("{} on {}", change.describe(), path.display()),
                            undo,
                        )
                    })
                    .map(|_| format!("{} done.", change.describe()))
                    .map_err(|e| format!("{e:#}"))
            });
            self.load();
        }

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.path)
                    .hint_text(r"C:\path\to\file or folder")
                    .desired_width(420.0),
            );
            if ui.button("File…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_file()
            {
                self.path = p.display().to_string();
                self.load();
            }
            if ui.button("Folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.path = p.display().to_string();
                self.load();
            }
            if ui.button("Show permissions").clicked() {
                self.load();
            }
        });
        show_message(ui, &self.message);

        let sec = match &self.security {
            Some(Ok(s)) => s.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        ui.add_space(6.0);
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
                c,
            );
        }

        ui.add_space(8.0);
        let is_dir = Path::new(self.path.trim()).is_dir();
        ui.add_enabled_ui(ctx.elevated, |ui| {
            if is_dir {
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
                        self.confirm
                            .ask(format!("{} for {}?{warn}", c.describe(), self.path.trim()), c);
                    }
                }
            });
        });
        if !ctx.elevated {
            ui.weak(ctx.i18n.tr("shell-needs-admin"));
        }
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

    impl AceInfo {
        fn mask_full(&self) -> bool {
            mask_text(self.mask) == "Full control"
        }
    }
}
