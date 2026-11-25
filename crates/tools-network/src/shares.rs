//! System Network: the folders this PC shares. Listing works for every account
//! (NetShareEnum level 1, paths from LanmanServer's share list); sharing a folder or
//! stopping a share uses `net share`, which needs administrator rights, and is journaled.

use std::path::Path;

use anyhow::{Result, bail};
use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
use windows::Win32::Storage::FileSystem::{NetShareEnum, SHARE_INFO_1};
use wm_core::{
    AppCtx, egui,
    process::{hidden_command, run_checked},
    registry,
    safety::UndoAction,
    tweaks::ExtraSetting,
    ui::{Confirm, Message, show_message},
};

const SHARES_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\LanmanServer\Shares";
const STYPE_SPECIAL: u32 = 0x8000_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub name: String,
    pub kind: &'static str,
    pub remark: String,
    /// Hidden administrative share (C$, ADMIN$, IPC$).
    pub special: bool,
    pub path: Option<String>,
}

fn kind_text(t: u32) -> &'static str {
    match t & 0xFF {
        0 => "Folder",
        1 => "Printer",
        2 => "Device",
        3 => "IPC",
        _ => "Other",
    }
}

/// The folder a user-made share points to (from its "Path=" line).
fn share_path(name: &str) -> Option<String> {
    let v = registry::read_raw(SHARES_KEY, name)?;
    // REG_MULTI_SZ of "Key=Value" lines.
    let units: Vec<u16> = v
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    units
        .split(|c| *c == 0)
        .map(String::from_utf16_lossy)
        .find_map(|l| l.strip_prefix("Path=").map(str::to_owned))
}

pub fn list() -> Result<Vec<Share>> {
    unsafe {
        let mut buf: *mut u8 = std::ptr::null_mut();
        let (mut read, mut total) = (0u32, 0u32);
        let r = NetShareEnum(None, 1, &mut buf, u32::MAX, &mut read, &mut total, None);
        if r != 0 {
            bail!("listing shares failed (error {r})");
        }
        let rows = std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize);
        let mut out: Vec<Share> = rows
            .iter()
            .map(|s| {
                let name = s.shi1_netname.to_string().unwrap_or_default();
                Share {
                    path: share_path(&name),
                    kind: kind_text(s.shi1_type.0),
                    remark: s.shi1_remark.to_string().unwrap_or_default(),
                    special: s.shi1_type.0 & STYPE_SPECIAL != 0 || name.ends_with('$'),
                    name,
                }
            })
            .collect();
        NetApiBufferFree(Some(buf as *const _));
        out.sort_by_key(|s| (s.special, s.name.to_lowercase()));
        Ok(out)
    }
}

pub fn valid_share_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && !name
            .chars()
            .any(|c| r#"\/[]:|<>+=;,?*""#.contains(c) || c.is_control())
}

pub fn share_folder(ctx: &AppCtx<'_>, name: &str, folder: &Path, read_only: bool) -> Result<()> {
    if !valid_share_name(name) {
        bail!("A share name can't be empty or contain \\ / [ ] : | < > + = ; , ? * \"");
    }
    if !folder.is_dir() {
        bail!("{} isn't a folder", folder.display());
    }
    let grant = format!("Everyone,{}", if read_only { "READ" } else { "CHANGE" });
    run_checked(
        hidden_command("net")
            .arg("share")
            .arg(format!("{name}={}", folder.display()))
            .arg(format!("/GRANT:{grant}")),
    )?;
    ctx.journal.record(
        "systemnetwork",
        format!("Shared {} as \\\\{}\\{name}", folder.display(), hostname()),
        UndoAction::RunCommand {
            program: "net".into(),
            args: vec!["share".into(), name.into(), "/delete".into(), "/y".into()],
        },
    )?;
    Ok(())
}

pub fn stop_sharing(ctx: &AppCtx<'_>, share: &Share) -> Result<()> {
    run_checked(hidden_command("net").args(["share", &share.name, "/delete", "/y"]))?;
    let undo = match &share.path {
        Some(p) => UndoAction::RunCommand {
            program: "net".into(),
            args: vec!["share".into(), format!("{}={p}", share.name)],
        },
        None => UndoAction::None,
    };
    ctx.journal
        .record("systemnetwork", format!("Stopped sharing {}", share.name), undo)?;
    Ok(())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
}

pub struct SharesSetting {
    shares: Option<Result<Vec<Share>, String>>,
    show_special: bool,
    new_name: String,
    new_folder: Option<std::path::PathBuf>,
    read_only: bool,
    confirm: Confirm<Share>,
    message: Message,
}

impl Default for SharesSetting {
    fn default() -> Self {
        Self {
            shares: None,
            show_special: false,
            new_name: String::new(),
            new_folder: None,
            read_only: true,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl ExtraSetting for SharesSetting {
    fn group(&self) -> &str {
        "Sharing"
    }

    fn title(&self) -> &str {
        "Folders this PC shares"
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if let Some(s) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                stop_sharing(ctx, &s)
                    .map(|()| format!("Stopped sharing {}.", s.name))
                    .map_err(|e| format!("{e:#}"))
            });
            self.shares = None;
        }
        let shares = self
            .shares
            .get_or_insert_with(|| list().map_err(|e| format!("{e:#}")))
            .clone();
        ui.horizontal(|ui| {
            ui.label(self.title());
            if ui.small_button(ctx.i18n.tr("common-refresh")).clicked() {
                self.shares = None;
            }
            ui.checkbox(&mut self.show_special, "Show hidden administrative shares");
        });
        ui.indent("shares", |ui| {
            match &shares {
                Err(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                Ok(list) => {
                    let visible: Vec<&Share> = list.iter().filter(|s| self.show_special || !s.special).collect();
                    if visible.is_empty() {
                        ui.weak("This PC doesn't share any folders.");
                    }
                    egui::Grid::new("shares-grid").striped(true).num_columns(4).show(ui, |ui| {
                        for s in visible {
                            ui.strong(&s.name);
                            ui.weak(s.kind);
                            ui.label(s.path.clone().unwrap_or_else(|| s.remark.clone()));
                            ui.horizontal(|ui| {
                                if s.kind == "Folder" && ui.small_button("Open").clicked() {
                                    self.message = wm_core::shell::shell_open(&format!(r"\\localhost\{}", s.name), "")
                                        .err()
                                        .map(|e| Err(format!("{e:#}")));
                                }
                                if !s.special
                                    && ui
                                        .add_enabled(ctx.elevated, egui::Button::new("Stop sharing").small())
                                        .on_disabled_hover_text("Needs the app to run as administrator.")
                                        .clicked()
                                {
                                    self.confirm.ask(
                                        format!("Stop sharing {}? People using it on the network lose access.", s.name),
                                        s.clone(),
                                    );
                                }
                            });
                            ui.end_row();
                        }
                    });
                }
            }
            ui.add_enabled_ui(ctx.elevated, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Share a folder");
                    if ui.button("Choose…").clicked()
                        && let Some(d) = rfd::FileDialog::new().pick_folder()
                    {
                        if self.new_name.is_empty() {
                            self.new_name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        }
                        self.new_folder = Some(d);
                    }
                    if let Some(d) = &self.new_folder {
                        ui.weak(d.display().to_string());
                    }
                    ui.add(egui::TextEdit::singleline(&mut self.new_name).hint_text("Share name").desired_width(120.0));
                    ui.checkbox(&mut self.read_only, "Read-only");
                    if ui.add_enabled(self.new_folder.is_some(), egui::Button::new("Share")).clicked()
                        && let Some(d) = self.new_folder.clone()
                    {
                        self.message = Some(if ctx.config.dry_run {
                            Ok(ctx.i18n.tr("common-dry-run-skip"))
                        } else {
                            share_folder(ctx, self.new_name.trim(), &d, self.read_only)
                                .map(|()| format!("Shared as \\\\{}\\{}.", hostname(), self.new_name.trim()))
                                .map_err(|e| format!("{e:#}"))
                        });
                        self.shares = None;
                    }
                });
            });
            if !ctx.elevated {
                ui.weak("Sharing a folder or stopping a share needs the app to run as administrator.");
            }
            show_message(ui, &self.message);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_admin_shares() {
        let s = list().unwrap();
        assert!(
            s.iter().any(|x| x.name.eq_ignore_ascii_case("IPC$") && x.special),
            "{s:?}"
        );
        assert!(
            s.iter()
                .all(|x| x.special || x.path.is_some() || x.kind != "Folder")
        );
    }

    #[test]
    fn share_names() {
        assert!(valid_share_name("Photos"));
        assert!(valid_share_name("My Music"));
        assert!(!valid_share_name(""));
        assert!(!valid_share_name("a/b"));
        assert!(!valid_share_name("x*"));
        assert_eq!(kind_text(0x8000_0000), "Folder");
        assert_eq!(kind_text(3), "IPC");
    }
}
