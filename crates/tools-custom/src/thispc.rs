//! This PC Manager: add folders, files, programs and Windows tools to This PC; rename,
//! change icons, turn items off and on; and choose which personal folders it shows.
//!
//! Every item is a per-user shell class (`HKCU\Software\Classes\CLSID\{id}`) listed
//! under `HKCU\…\Explorer\MyComputer\NameSpace`. Folders use the shell's folder
//! shortcut object; files and programs are plain items whose Open verb runs them.
//! Windows tools (Control Panel, Recycle Bin…) are their existing classes, listed.
//! Turning an item off removes it from the namespace but keeps its definition.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use egui_extras::{Column, TableBuilder};
use windows::core::GUID;
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    icons, paths,
    process::{hidden_command, restart_explorer},
    registry,
    safety::UndoAction,
    shell::shell_open,
    tweaks::TweakPanel,
    ui::{Confirm, Message, ToolButton, frame, section_nav, show_message},
};

use crate::navpane::{
    CLASSES, delete_key, expand_sz, merged_key, notify, own_key, resolve, string_of, valid_name,
    write_folder_class,
};

const TOOL: &str = "thispc";
const NAMESPACE: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\MyComputer\NameSpace";
/// Marks items this tool created; its value is the kind (`folder`, `file`, `program`).
const MARKER: &str = "WinManagerRS.ThisPC";
const TARGET: &str = "WinManagerRS.Target";
const ARGS: &str = "WinManagerRS.Args";
const FILE_ICON: &str = r"%SystemRoot%\system32\shell32.dll,0";

/// Windows tools that can be listed in This PC.
const UTILITIES: [(&str, &str); 12] = [
    ("{26EE0668-A00A-44D7-9371-BEB064C98683}", "Control Panel"),
    ("{ED7BA470-8E54-465E-825C-99712043E01C}", "All Tasks"),
    ("{645FF040-5081-101B-9F08-00AA002F954E}", "Recycle Bin"),
    ("{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", "Network"),
    ("{7007ACC7-3202-11D1-AAD2-00805FC1270E}", "Network Connections"),
    ("{A8A91A66-3A7D-4424-8D24-04E180695C7A}", "Devices and Printers"),
    ("{7b81be6a-ce2b-4676-a29e-eb907a5126c5}", "Programs and Features"),
    ("{D20EA4E1-3957-11d2-A40B-0C5020524153}", "Windows Tools"),
    ("{BD84B380-8CA2-1069-AB1D-08000948F534}", "Fonts"),
    ("{2559a1f3-21d7-11d4-bdaf-00c04f60b9f0}", "Run"),
    ("{59031a47-3f72-44a7-89c5-5595fe6b30ee}", "User folder"),
    ("{031E4825-7B94-4dc3-B131-E946B44C8DD5}", "Libraries"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Folder,
    File,
    Program,
    Utility,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Folder => "Folder",
            Kind::File => "File",
            Kind::Program => "Program",
            Kind::Utility => "Windows",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Kind::Folder => "folder",
            Kind::File => "file",
            Kind::Program => "program",
            Kind::Utility => "utility",
        }
    }

    fn from_tag(s: &str) -> Option<Self> {
        [Kind::Folder, Kind::File, Kind::Program]
            .into_iter()
            .find(|k| k.tag() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcItem {
    pub clsid: String,
    pub name: String,
    pub kind: Kind,
    pub target: String,
    pub args: String,
    pub icon: String,
    pub enabled: bool,
}

impl PcItem {
    fn own(&self) -> bool {
        self.kind != Kind::Utility
    }

    fn missing(&self) -> bool {
        self.own() && !Path::new(&paths::expand_env(&self.target)).exists()
    }
}

fn ns_key(clsid: &str) -> String {
    format!(r"{NAMESPACE}\{clsid}")
}

fn listed(clsid: &str) -> bool {
    registry::open_read(&ns_key(clsid)).is_ok()
}

fn utility_name(clsid: &str, fallback: &str) -> String {
    string_of(&merged_key(clsid), "LocalizedString")
        .map(|s| resolve(&s))
        .filter(|s| !s.starts_with('@'))
        .unwrap_or_else(|| fallback.to_owned())
}

/// This tool's items (in name order), then the Windows tools.
pub fn enumerate() -> Vec<PcItem> {
    let mut own = Vec::new();
    if let Ok(classes) = registry::open_read(CLASSES) {
        for id in classes.enum_keys().flatten().filter(|s| s.starts_with('{')) {
            let key = own_key(&id);
            let Some(kind) = string_of(&key, MARKER).and_then(|t| Kind::from_tag(&t)) else {
                continue;
            };
            own.push(PcItem {
                name: string_of(&key, "").unwrap_or_else(|| id.clone()),
                kind,
                target: string_of(&key, TARGET).unwrap_or_default(),
                args: string_of(&key, ARGS).unwrap_or_default(),
                icon: string_of(&format!(r"{key}\DefaultIcon"), "").unwrap_or_default(),
                enabled: listed(&id),
                clsid: id,
            });
        }
    }
    own.sort_by_key(|i| i.name.to_lowercase());
    own.extend(
        UTILITIES
            .iter()
            .filter(|(id, _)| registry::open_read(&merged_key(id)).is_ok())
            .map(|(id, name)| PcItem {
                clsid: (*id).to_owned(),
                name: utility_name(id, name),
                kind: Kind::Utility,
                target: String::new(),
                args: String::new(),
                icon: String::new(),
                enabled: listed(id),
            }),
    );
    own
}

/// What an item's Open verb runs.
fn command_for(kind: Kind, target: &str, args: &str) -> String {
    match kind {
        Kind::Program => format!("\"{target}\" {}", args.trim()).trim_end().to_owned(),
        // Explorer opens a file with its default program.
        _ => format!(r#""%SystemRoot%\explorer.exe" "{target}""#),
    }
}

fn default_icon(kind: Kind, target: &str) -> String {
    match kind {
        Kind::Program => format!("{target},0"),
        _ => FILE_ICON.to_owned(),
    }
}

fn check_target(kind: Kind, target: &Path) -> Result<()> {
    match kind {
        Kind::Folder if !target.is_dir() => bail!("{} is not a folder", target.display()),
        Kind::File | Kind::Program if !target.is_file() => bail!("{} is not a file", target.display()),
        Kind::Utility => bail!("Windows tools are already listed; turn them on instead"),
        _ => Ok(()),
    }
}

/// Writes an item's class (everything except the marker and the namespace entry).
fn write_class(key: &str, kind: Kind, name: &str, target: &Path, args: &str, icon: &str) -> Result<()> {
    let t = target.display().to_string();
    registry::write_raw(key, "", &registry::string(name))?;
    registry::write_raw(key, TARGET, &registry::string(&t))?;
    registry::write_raw(key, ARGS, &registry::string(args.trim()))?;
    registry::write_raw(key, "InfoTip", &registry::string(&t))?;
    if kind == Kind::Folder {
        return write_folder_class(key, target, icon);
    }
    let icon = if icon.trim().is_empty() {
        default_icon(kind, &t)
    } else {
        icon.trim().to_owned()
    };
    registry::write_raw(&format!(r"{key}\DefaultIcon"), "", &expand_sz(&icon))?;
    let server = format!(r"{key}\InProcServer32");
    registry::write_raw(&server, "", &expand_sz(r"%SystemRoot%\system32\shell32.dll"))?;
    registry::write_raw(&server, "ThreadingModel", &registry::string("Apartment"))?;
    registry::write_raw(
        &format!(r"{key}\Shell\Open\Command"),
        "",
        &expand_sz(&command_for(kind, &t, args)),
    )?;
    registry::write_raw(&format!(r"{key}\ShellFolder"), "Attributes", &registry::dword(0))?;
    Ok(())
}

/// Adds a folder, file or program to This PC. Returns its CLSID.
pub fn add(
    ctx: &AppCtx<'_>,
    kind: Kind,
    name: &str,
    target: &Path,
    args: &str,
    icon: &str,
) -> Result<String> {
    let name = valid_name(name)?;
    check_target(kind, target)?;
    let clsid = format!("{{{:?}}}", GUID::new()?);
    let key = own_key(&clsid);
    let write = || -> Result<()> {
        write_class(&key, kind, &name, target, args, icon)?;
        registry::write_raw(&key, MARKER, &registry::string(kind.tag()))?;
        registry::write_raw(&ns_key(&clsid), "", &registry::string(&name))
    };
    if let Err(e) = write() {
        let _ = delete_key(&ns_key(&clsid));
        let _ = delete_key(&key);
        return Err(e);
    }
    ctx.journal.record(
        TOOL,
        format!("Added \"{name}\" to This PC"),
        UndoAction::Sequence {
            steps: vec![
                UndoAction::DeleteRegKey { key: ns_key(&clsid) },
                UndoAction::DeleteRegKey { key },
            ],
        },
    )?;
    notify();
    Ok(clsid)
}

/// Changes an item's name, target, arguments or icon (its kind stays).
pub fn edit(
    ctx: &AppCtx<'_>,
    item: &PcItem,
    name: &str,
    target: &Path,
    args: &str,
    icon: &str,
) -> Result<()> {
    if !item.own() {
        bail!("Windows tools cannot be edited");
    }
    let name = valid_name(name)?;
    check_target(item.kind, target)?;
    let key = own_key(&item.clsid);
    let undo = ctx.journal.backup_reg_key(&key)?;
    let ns_undo = if item.enabled {
        Some(ctx.journal.backup_reg_key(&ns_key(&item.clsid))?)
    } else {
        None
    };
    // Rewrite from scratch so nothing of the old kind's layout is left over.
    delete_key(&key)?;
    write_class(&key, item.kind, &name, target, args, icon)?;
    registry::write_raw(&key, MARKER, &registry::string(item.kind.tag()))?;
    if item.enabled {
        registry::write_raw(&ns_key(&item.clsid), "", &registry::string(&name))?;
    }
    let mut steps = vec![UndoAction::DeleteRegKey { key }, undo];
    steps.extend(ns_undo);
    ctx.journal.record(
        TOOL,
        format!("Edited \"{name}\" in This PC"),
        UndoAction::Sequence { steps },
    )?;
    notify();
    Ok(())
}

/// Lists or unlists an item in This PC.
pub fn set_enabled(ctx: &AppCtx<'_>, item: &PcItem, on: bool) -> Result<()> {
    let ns = ns_key(&item.clsid);
    let undo = if on {
        registry::write_raw(&ns, "", &registry::string(&item.name))?;
        UndoAction::DeleteRegKey { key: ns }
    } else {
        let undo = ctx.journal.backup_reg_key(&ns)?;
        delete_key(&ns)?;
        undo
    };
    let verb = if on { "Turned on" } else { "Turned off" };
    ctx.journal
        .record(TOOL, format!("{verb} \"{}\" in This PC", item.name), undo)?;
    notify();
    Ok(())
}

/// Deletes an item this tool added.
pub fn remove(ctx: &AppCtx<'_>, item: &PcItem) -> Result<()> {
    if !item.own() {
        bail!("Windows tools cannot be removed; turn them off instead");
    }
    let key = own_key(&item.clsid);
    let ns = ns_key(&item.clsid);
    let steps = vec![
        ctx.journal.backup_reg_key(&key)?,
        ctx.journal.backup_reg_key(&ns)?,
    ];
    delete_key(&ns)?;
    delete_key(&key)?;
    ctx.journal.record(
        TOOL,
        format!("Removed \"{}\" from This PC", item.name),
        UndoAction::Sequence { steps },
    )?;
    notify();
    Ok(())
}

/// Opens Windows' Desktop Icon Settings dialog.
pub fn desktop_icon_settings() -> Result<()> {
    hidden_command("rundll32.exe")
        .args(["shell32.dll,Control_RunDLL", "desk.cpl,,0"])
        .spawn()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Items,
    Folders,
}

struct Editor {
    clsid: Option<String>,
    kind: Kind,
    name: String,
    target: String,
    args: String,
    icon: String,
}

enum Ask {
    Remove(PcItem),
    RestartExplorer,
}

pub struct ThisPcManager {
    tab: Tab,
    items: Option<Vec<PcItem>>,
    selected: Option<String>,
    editor: Option<Editor>,
    panel: TweakPanel,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for ThisPcManager {
    fn default() -> Self {
        Self {
            tab: Tab::Items,
            items: None,
            selected: None,
            editor: None,
            panel: TweakPanel::new(TOOL, crate::THISPC_TWEAKS),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl ThisPcManager {
    fn dry(&mut self, ctx: &AppCtx<'_>) -> bool {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
        }
        ctx.config.dry_run
    }

    fn done(&mut self, r: Result<String>) {
        self.message = Some(
            r.map(|m| format!("{m} Open This PC again to see the change."))
                .map_err(|e| format!("{e:#}")),
        );
        self.items = None;
    }

    fn editor_window(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ed) = &mut self.editor else { return };
        let mut result = None;
        let title = if ed.clsid.is_some() {
            "Edit item"
        } else {
            "Add to This PC"
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                egui::Grid::new("thispc-edit").num_columns(2).show(ui, |ui| {
                    ui.label("Type");
                    ui.add_enabled_ui(ed.clsid.is_none(), |ui| {
                        ui.horizontal(|ui| {
                            for k in [Kind::Folder, Kind::File, Kind::Program] {
                                ui.selectable_value(&mut ed.kind, k, k.label());
                            }
                        });
                    });
                    ui.end_row();
                    ui.label(ed.kind.label());
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut ed.target).desired_width(260.0));
                        if ui.button("Browse…").clicked() {
                            let picked = match ed.kind {
                                Kind::Folder => rfd::FileDialog::new().pick_folder(),
                                Kind::Program => rfd::FileDialog::new()
                                    .add_filter("Programs", &["exe", "bat", "cmd", "com", "msc"])
                                    .pick_file(),
                                _ => rfd::FileDialog::new().pick_file(),
                            };
                            if let Some(p) = picked {
                                ed.target = p.display().to_string();
                                if ed.name.is_empty() {
                                    ed.name = p
                                        .file_stem()
                                        .or(p.file_name())
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| ed.target.clone());
                                }
                            }
                        }
                    });
                    ui.end_row();
                    if ed.kind == Kind::Program {
                        ui.label("Arguments");
                        ui.add(egui::TextEdit::singleline(&mut ed.args).desired_width(260.0));
                        ui.end_row();
                    }
                    ui.label("Name");
                    ui.text_edit_singleline(&mut ed.name);
                    ui.end_row();
                    ui.label("Icon");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.icon)
                                .hint_text("Default icon")
                                .desired_width(220.0),
                        );
                        if ui.button("Browse…").clicked()
                            && let Some(p) = rfd::FileDialog::new()
                                .add_filter("Icons", &["ico", "dll", "exe"])
                                .pick_file()
                        {
                            let is_ico = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ico"));
                            ed.icon = if is_ico {
                                p.display().to_string()
                            } else {
                                format!("{},0", p.display())
                            };
                        }
                    });
                    ui.end_row();
                });
                ui.weak("Icons from a DLL or EXE take an index after a comma, e.g. imageres.dll,-3.");
                ui.horizontal(|ui| {
                    let ok = !ed.name.trim().is_empty() && !ed.target.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
                        result = Some(true);
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        result = Some(false);
                    }
                });
            });
        match result {
            Some(true) => {
                let ed = self.editor.take().expect("open");
                if self.dry(ctx) {
                    return;
                }
                let target = PathBuf::from(ed.target.trim().trim_matches('"'));
                let r = match &ed.clsid {
                    None => add(ctx, ed.kind, &ed.name, &target, &ed.args, &ed.icon).map(|id| {
                        self.selected = Some(id);
                        format!("Added \"{}\".", ed.name.trim())
                    }),
                    Some(id) => match self.items.iter().flatten().find(|i| &i.clsid == id).cloned() {
                        Some(item) => edit(ctx, &item, &ed.name, &target, &ed.args, &ed.icon)
                            .map(|()| format!("Saved \"{}\".", ed.name.trim())),
                        None => Err(anyhow::anyhow!("the item no longer exists")),
                    },
                };
                self.done(r);
            }
            Some(false) => self.editor = None,
            None => {}
        }
    }

    fn items_tab(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.items.get_or_insert_with(enumerate).clone();
        let sel = self
            .selected
            .as_ref()
            .and_then(|id| items.iter().find(|i| &i.clsid == id))
            .cloned();
        let own = sel.as_ref().is_some_and(PcItem::own);
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add")),
                Some(ToolButton::new(icons::PENCIL_SIMPLE, "Edit").enabled(own)),
                Some(ToolButton::new(icons::TRASH, "Remove").enabled(own)),
                None,
                Some(
                    ToolButton::new(
                        icons::EYE_SLASH,
                        if sel.as_ref().is_some_and(|i| i.enabled) {
                            "Turn off"
                        } else {
                            "Turn on"
                        },
                    )
                    .enabled(sel.is_some()),
                ),
                Some(ToolButton::new(icons::FOLDER_OPEN, "Open").enabled(sel.is_some())),
                None,
                Some(ToolButton::new(icons::DESKTOP, "Desktop icons")),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
                Some(ToolButton::new(icons::WINDOWS_LOGO, "Restart Explorer")),
            ],
            |_| {},
        ) {
            Some(0) => {
                self.editor = Some(Editor {
                    clsid: None,
                    kind: Kind::Folder,
                    name: String::new(),
                    target: String::new(),
                    args: String::new(),
                    icon: String::new(),
                })
            }
            Some(1) => {
                if let Some(i) = sel {
                    self.editor = Some(Editor {
                        clsid: Some(i.clsid),
                        kind: i.kind,
                        name: i.name,
                        target: i.target,
                        args: i.args,
                        icon: i.icon,
                    });
                }
            }
            Some(2) => {
                if let Some(i) = sel {
                    self.confirm
                        .ask(format!("Remove \"{}\" from This PC?", i.name), Ask::Remove(i));
                }
            }
            Some(4) => {
                if let Some(i) = sel
                    && !self.dry(ctx)
                {
                    let on = !i.enabled;
                    let verb = if on { "Turned on" } else { "Turned off" };
                    let r = set_enabled(ctx, &i, on).map(|()| format!("{verb} \"{}\".", i.name));
                    self.done(r);
                }
            }
            Some(5) => {
                if let Some(i) = sel {
                    let r = match i.kind {
                        Kind::Utility => shell_open(&format!("shell:::{}", i.clsid), ""),
                        Kind::Program => shell_open(&paths::expand_env(&i.target), &i.args),
                        _ => shell_open(&paths::expand_env(&i.target), ""),
                    };
                    if let Err(e) = r {
                        self.message = Some(Err(format!("{e:#}")));
                    }
                }
            }
            Some(7) => {
                if let Err(e) = desktop_icon_settings() {
                    self.message = Some(Err(format!("{e:#}")));
                }
            }
            Some(8) => self.items = None,
            Some(9) => self.confirm.ask(
                "Restart File Explorer now? Open Explorer windows close.",
                Ask::RestartExplorer,
            ),
            _ => {}
        }

        let on = items.iter().filter(|i| i.enabled).count();
        frame::status(ui, &format!("{} items, {on} in This PC", items.len()), |_| {});
        frame::central_fixed(ui, |ui| self.items_table(ui, &items));
    }

    fn items_table(&mut self, ui: &mut egui::Ui, items: &[PcItem]) {
        show_message(ui, &self.message);
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(50.0))
            .column(Column::initial(200.0).at_least(120.0))
            .column(Column::exact(80.0))
            .column(Column::remainder().at_least(120.0))
            .header(row_h, |mut h| {
                for t in ["On", "Name", "Type", "Opens"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, items.len(), |mut row| {
                    let item = &items[row.index()];
                    row.set_selected(self.selected.as_ref() == Some(&item.clsid));
                    row.col(|ui| {
                        ui.label(if item.enabled { icons::CHECK } else { "" });
                    });
                    row.col(|ui| {
                        if item.missing() {
                            ui.colored_label(ui.visuals().error_fg_color, &item.name)
                                .on_hover_text("The target no longer exists");
                        } else {
                            ui.label(&item.name);
                        }
                    });
                    row.col(|ui| {
                        ui.label(item.kind.label());
                    });
                    row.col(|ui| {
                        let opens = if item.own() {
                            format!("{} {}", item.target, item.args).trim().to_owned()
                        } else {
                            item.clsid.clone()
                        };
                        ui.label(opens);
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        self.selected = Some(item.clsid.clone());
                    }
                });
            });
    }
}

impl Tool for ThisPcManager {
    fn id(&self) -> &'static str {
        TOOL
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            match ask {
                Ask::Remove(item) => {
                    if !self.dry(ctx) {
                        let r = remove(ctx, &item).map(|()| format!("Removed \"{}\".", item.name));
                        self.selected = None;
                        self.done(r);
                    }
                }
                Ask::RestartExplorer => {
                    if !self.dry(ctx) {
                        self.message = Some(
                            restart_explorer()
                                .map(|()| "File Explorer restarted.".to_owned())
                                .map_err(|e| format!("{e:#}")),
                        );
                    }
                }
            }
        }
        self.editor_window(ui, ctx);
        let mut sel = usize::from(self.tab == Tab::Folders);
        frame::left(ui, 190.0, |ui| {
            section_nav(
                ui,
                &[(icons::LIST, "Items"), (icons::FOLDERS, "Personal folders")],
                &mut sel,
            )
        });
        self.tab = if sel == 1 { Tab::Folders } else { Tab::Items };
        match self.tab {
            Tab::Items => self.items_tab(ui, ctx),
            Tab::Folders => frame::nested(ui, |ui| {
                frame::options(ui, |ui| {
                    ui.weak("Which personal folders This PC shows. Applies to all users (administrator required).");
                });
                self.panel.ui_framed(ui, ctx);
            }),
        }
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_and_icons() {
        assert_eq!(
            command_for(Kind::Program, r"C:\a b\x.exe", ""),
            r#""C:\a b\x.exe""#
        );
        assert_eq!(
            command_for(Kind::Program, r"C:\x.exe", " -y "),
            r#""C:\x.exe" -y"#
        );
        assert!(command_for(Kind::File, r"C:\f.txt", "").ends_with(r#"explorer.exe" "C:\f.txt""#));
        assert_eq!(default_icon(Kind::Program, r"C:\x.exe"), r"C:\x.exe,0");
        assert!(enumerate().iter().any(|i| i.kind == Kind::Utility));
    }

    #[test]
    fn add_edit_toggle_and_remove() {
        let jd = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&jd.path().join("j.json"), &jd.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        let dir = tempfile::tempdir().unwrap();
        let notepad = PathBuf::from(paths::expand_env(r"%WINDIR%\notepad.exe"));
        let folder = add(&ctx, Kind::Folder, "WM PC folder", dir.path(), "", "").unwrap();
        let prog = add(&ctx, Kind::Program, "WM PC prog", &notepad, "-x", "").unwrap();
        let find = |id: &str| enumerate().into_iter().find(|i| i.clsid == id).unwrap();

        let p = find(&prog);
        assert!(p.enabled && p.kind == Kind::Program && p.args == "-x");
        assert_eq!(
            string_of(&format!(r"{}\Shell\Open\Command", own_key(&prog)), "").unwrap(),
            format!("\"{}\" -x", notepad.display())
        );
        let f = find(&folder);
        assert!(f.enabled && f.kind == Kind::Folder && Path::new(&f.target) == dir.path());

        edit(&ctx, &p, "WM PC renamed", &notepad, "", "").unwrap();
        let p = find(&prog);
        assert_eq!(p.name, "WM PC renamed");
        assert_eq!(string_of(&ns_key(&prog), "").unwrap(), "WM PC renamed");

        set_enabled(&ctx, &p, false).unwrap();
        let p = find(&prog);
        assert!(!p.enabled && !listed(&prog));
        set_enabled(&ctx, &p, true).unwrap();
        assert!(listed(&prog));

        for id in [&folder, &prog] {
            remove(&ctx, &find(id)).unwrap();
            assert!(registry::open_read(&own_key(id)).is_err());
            assert!(!listed(id));
        }
    }
}
