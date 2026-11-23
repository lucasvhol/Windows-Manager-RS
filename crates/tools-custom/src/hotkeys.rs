//! Hotkey Manager: global keyboard shortcuts that launch programs.
//!
//! Uses the hotkey field of Start menu / desktop shortcuts, which Windows itself
//! listens for, so hotkeys keep working when this app is closed. New hotkeys are
//! created as shortcuts in a dedicated Start menu folder.

use std::path::PathBuf;

use anyhow::{Result, bail};
use walkdir::WalkDir;
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    safety::UndoAction,
    shell::{KnownFolder, ShortcutSpec, create_shortcut, known_folder, read_shortcut},
    task::BackgroundTask,
    ui::{Message, show_message},
};

const HOTKEYF_SHIFT: u16 = 0x01;
const HOTKEYF_CONTROL: u16 = 0x02;
const HOTKEYF_ALT: u16 = 0x04;

pub const FOLDER: &str = "Win Manager RS Hotkeys";

/// Keys offered in the editor: (label, virtual-key code).
pub fn keys() -> Vec<(String, u8)> {
    let mut k: Vec<(String, u8)> = (b'A'..=b'Z').map(|c| ((c as char).to_string(), c)).collect();
    k.extend((b'0'..=b'9').map(|c| ((c as char).to_string(), c)));
    k.extend((1..=12u8).map(|n| (format!("F{n}"), 0x6F + n)));
    k
}

pub fn encode(ctrl: bool, alt: bool, shift: bool, vk: u8) -> u16 {
    let mut m = 0;
    if ctrl {
        m |= HOTKEYF_CONTROL;
    }
    if alt {
        m |= HOTKEYF_ALT;
    }
    if shift {
        m |= HOTKEYF_SHIFT;
    }
    (m << 8) | u16::from(vk)
}

pub fn format_hotkey(hk: u16) -> String {
    if hk == 0 {
        return String::new();
    }
    let m = hk >> 8;
    let vk = (hk & 0xFF) as u8;
    let mut parts = Vec::new();
    if m & HOTKEYF_CONTROL != 0 {
        parts.push("Ctrl".to_owned());
    }
    if m & HOTKEYF_ALT != 0 {
        parts.push("Alt".to_owned());
    }
    if m & HOTKEYF_SHIFT != 0 {
        parts.push("Shift".to_owned());
    }
    parts.push(
        keys()
            .into_iter()
            .find(|(_, k)| *k == vk)
            .map(|(l, _)| l)
            .unwrap_or_else(|| format!("0x{vk:02X}")),
    );
    parts.join("+")
}

/// Windows ignores shortcut hotkeys without Ctrl or Alt.
pub fn validate(hk: u16) -> Result<()> {
    let m = hk >> 8;
    if m & (HOTKEYF_CONTROL | HOTKEYF_ALT) == 0 {
        bail!("Hotkeys need Ctrl and/or Alt.");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotkeyItem {
    pub lnk: PathBuf,
    pub name: String,
    pub target: String,
    pub hotkey: u16,
    pub ours: bool,
    pub all_users: bool,
}

pub fn our_folder() -> PathBuf {
    PathBuf::from(paths::expand_env(
        r"%APPDATA%\Microsoft\Windows\Start Menu\Programs",
    ))
    .join(FOLDER)
}

/// Shortcuts that have a hotkey, from the Start menu (both scopes) and desktops.
pub fn scan() -> Vec<HotkeyItem> {
    let mut roots = vec![
        (
            PathBuf::from(paths::expand_env(r"%APPDATA%\Microsoft\Windows\Start Menu")),
            false,
        ),
        (
            PathBuf::from(paths::expand_env(r"%ProgramData%\Microsoft\Windows\Start Menu")),
            true,
        ),
    ];
    if let Ok(d) = known_folder(KnownFolder::Desktop) {
        roots.push((d, false));
    }
    if let Ok(d) = known_folder(KnownFolder::PublicDesktop) {
        roots.push((d, true));
    }
    let ours = our_folder();
    let mut items = Vec::new();
    for (root, all_users) in roots {
        for e in WalkDir::new(root).follow_links(false).into_iter().flatten() {
            let p = e.path();
            if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
                continue;
            }
            let Ok(spec) = read_shortcut(p) else { continue };
            let is_ours = p.starts_with(&ours);
            if spec.hotkey == 0 && !is_ours {
                continue;
            }
            items.push(HotkeyItem {
                name: p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                target: format!("{} {}", spec.target.display(), spec.args)
                    .trim()
                    .to_owned(),
                hotkey: spec.hotkey,
                lnk: p.to_owned(),
                ours: is_ours,
                all_users,
            });
        }
    }
    items.sort_by_key(|i| format_hotkey(i.hotkey));
    items
}

/// Changes the hotkey of an existing shortcut, preserving everything else.
fn set_hotkey(item: &HotkeyItem, hotkey: u16) -> Result<()> {
    let mut spec = read_shortcut(&item.lnk)?;
    spec.hotkey = hotkey;
    // read_shortcut does not return the working directory; keep the target's folder.
    create_shortcut(&item.lnk, &spec)
}

#[derive(Default)]
struct Draft {
    name: String,
    target: String,
    args: String,
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: usize,
}

pub struct HotkeyManager {
    scan: Option<BackgroundTask<Vec<HotkeyItem>>>,
    items: Option<Vec<HotkeyItem>>,
    draft: Draft,
    message: Message,
}

impl Default for HotkeyManager {
    fn default() -> Self {
        Self {
            scan: None,
            items: None,
            draft: Draft {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            message: None,
        }
    }
}

impl HotkeyManager {
    fn create(&mut self, ctx: &AppCtx<'_>) -> Result<String> {
        let d = &self.draft;
        let name = d.name.trim();
        if name.is_empty() || d.target.trim().is_empty() {
            bail!("Enter a name and choose a program.");
        }
        if name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            bail!("The name cannot contain \\ / : * ? \" < > |");
        }
        let hk = encode(d.ctrl, d.alt, d.shift, keys()[d.key].1);
        validate(hk)?;
        if let Some(clash) = self.items.iter().flatten().find(|i| i.hotkey == hk) {
            bail!("{} is already used by \"{}\".", format_hotkey(hk), clash.name);
        }
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        std::fs::create_dir_all(our_folder())?;
        let lnk = our_folder().join(format!("{name}.lnk"));
        if lnk.exists() {
            bail!("A hotkey named \"{name}\" already exists.");
        }
        create_shortcut(
            &lnk,
            &ShortcutSpec {
                target: PathBuf::from(d.target.trim()),
                args: d.args.trim().to_owned(),
                hotkey: hk,
                ..Default::default()
            },
        )?;
        ctx.journal.record(
            "hotkeys",
            format!("Added hotkey {} for {name}", format_hotkey(hk)),
            UndoAction::DeleteCreatedFiles { paths: vec![lnk] },
        )?;
        Ok(format!(
            "{} now opens {name}. New hotkeys may need a sign-out to start working.",
            format_hotkey(hk)
        ))
    }
}

impl Tool for HotkeyManager {
    fn id(&self) -> &'static str {
        "hotkeys"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.scan.is_none() && self.items.is_none() {
            self.scan = Some(BackgroundTask::spawn(ui.ctx(), |_| Ok(scan())));
        }
        if let Some(t) = &mut self.scan {
            t.poll();
            if let Some(r) = t.take_result() {
                self.items = Some(r.unwrap_or_default());
                self.scan = None;
            }
        }
        ui.label("Hotkeys are stored in shortcuts, so Windows handles them even when this app is closed.");
        ui.add_space(6.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong("New hotkey");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.name)
                        .hint_text("Name")
                        .desired_width(150.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.target)
                        .hint_text("Program")
                        .desired_width(260.0),
                );
                if ui.button("Browse…").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_file()
                {
                    if self.draft.name.is_empty() {
                        self.draft.name = p
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                    }
                    self.draft.target = p.display().to_string();
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.args)
                        .hint_text("Arguments")
                        .desired_width(120.0),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.draft.ctrl, "Ctrl");
                ui.checkbox(&mut self.draft.alt, "Alt");
                ui.checkbox(&mut self.draft.shift, "Shift");
                let keys = keys();
                egui::ComboBox::from_id_salt("hotkey_key")
                    .selected_text(&keys[self.draft.key].0)
                    .width(60.0)
                    .show_ui(ui, |ui| {
                        for (i, (label, _)) in keys.iter().enumerate() {
                            ui.selectable_value(&mut self.draft.key, i, label);
                        }
                    });
                let preview = encode(
                    self.draft.ctrl,
                    self.draft.alt,
                    self.draft.shift,
                    keys[self.draft.key].1,
                );
                ui.weak(format_hotkey(preview));
                if ui.button("Create").clicked() {
                    self.message = Some(self.create(ctx).map_err(|e| format!("{e:#}")));
                    self.items = None;
                }
            });
        });
        show_message(ui, &self.message);
        ui.add_space(6.0);

        let Some(items) = self.items.clone() else {
            ui.spinner();
            return;
        };
        ui.horizontal(|ui| {
            ui.strong(format!(
                "Shortcuts with hotkeys ({})",
                items.iter().filter(|i| i.hotkey != 0).count()
            ));
            if ui.small_button(ctx.i18n.tr("common-refresh")).clicked() {
                self.items = None;
            }
        });
        let mut clear = None;
        let mut delete = None;
        egui::Grid::new("hotkeys")
            .num_columns(4)
            .striped(true)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for item in &items {
                    let dup =
                        item.hotkey != 0 && items.iter().filter(|o| o.hotkey == item.hotkey).count() > 1;
                    if dup {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!("{} (conflict)", format_hotkey(item.hotkey)),
                        );
                    } else {
                        ui.strong(format_hotkey(item.hotkey));
                    }
                    ui.label(&item.name);
                    ui.weak(&item.target);
                    ui.horizontal(|ui| {
                        let usable = !item.all_users || ctx.elevated;
                        if item.ours {
                            if ui
                                .add_enabled(usable, egui::Button::new("Delete").small())
                                .clicked()
                            {
                                delete = Some(item.clone());
                            }
                        } else if item.hotkey != 0
                            && ui
                                .add_enabled(usable, egui::Button::new("Remove hotkey").small())
                                .clicked()
                        {
                            clear = Some(item.clone());
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some(item) = clear {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                ctx.journal
                    .backup_file(&item.lnk)
                    .and_then(|undo| {
                        set_hotkey(&item, 0)?;
                        ctx.journal
                            .record("hotkeys", format!("Removed hotkey from {}", item.name), undo)
                    })
                    .map(|_| format!("Removed the hotkey from {}.", item.name))
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }
        if let Some(item) = delete {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                ctx.journal
                    .recycle(
                        "hotkeys",
                        format!("Deleted hotkey {}", item.name),
                        vec![item.lnk.clone()],
                    )
                    .map(|_| format!("Deleted {}.", item.name))
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_and_formatting() {
        let n = keys().iter().position(|(l, _)| l == "N").unwrap();
        let hk = encode(true, true, false, keys()[n].1);
        assert_eq!(hk, 0x064E);
        assert_eq!(format_hotkey(hk), "Ctrl+Alt+N");
        assert_eq!(format_hotkey(encode(true, false, true, 0x70)), "Ctrl+Shift+F1");
        assert!(validate(hk).is_ok());
        assert!(validate(encode(false, false, true, b'A')).is_err());
        assert_eq!(format_hotkey(0), "");
    }

    #[test]
    fn set_and_clear_hotkey_on_shortcut() {
        let dir = tempfile::tempdir().unwrap();
        let lnk = dir.path().join("x.lnk");
        create_shortcut(
            &lnk,
            &ShortcutSpec {
                target: r"C:\Windows\notepad.exe".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let item = HotkeyItem {
            lnk: lnk.clone(),
            name: "x".into(),
            target: String::new(),
            hotkey: 0,
            ours: false,
            all_users: false,
        };
        set_hotkey(&item, 0x0641).unwrap();
        assert_eq!(read_shortcut(&lnk).unwrap().hotkey, 0x0641);
        set_hotkey(&item, 0).unwrap();
        assert_eq!(read_shortcut(&lnk).unwrap().hotkey, 0);
        assert_eq!(
            read_shortcut(&lnk).unwrap().target,
            PathBuf::from(r"C:\Windows\notepad.exe")
        );
    }
}
