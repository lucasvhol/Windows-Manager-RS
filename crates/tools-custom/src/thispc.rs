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
use windows::core::GUID;
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, paths,
    process::{hidden_command, restart_explorer},
    registry,
    safety::UndoAction,
    shell::shell_open,
    tweaks::TweakPanel,
    ui::{Confirm, Message, frame, show_message},
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

/// The Edit group's fields, loaded from the selected item.
struct Editor {
    clsid: String,
    name: String,
    target: String,
    args: String,
    icon: String,
}

enum Ask {
    Remove(PcItem),
    RemoveAll(Vec<PcItem>),
    RestartExplorer,
}

pub struct ThisPcManager {
    /// The personal-folders tweaks are shown instead of the item list.
    folders_page: bool,
    items: Option<Vec<PcItem>>,
    selected: Option<String>,
    editor: Option<Editor>,
    /// The "Parameter" box under Add File (arguments for a program).
    parameter: String,
    /// The Windows tools list under Add Utility is open.
    show_utilities: bool,
    panel: TweakPanel,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for ThisPcManager {
    fn default() -> Self {
        Self {
            folders_page: false,
            items: None,
            selected: None,
            editor: None,
            parameter: String::new(),
            show_utilities: false,
            panel: TweakPanel::new(TOOL, crate::THISPC_TWEAKS),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

fn kind_icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Folder => icons::FOLDER_OPEN,
        Kind::File => icons::FILE_TEXT,
        Kind::Program => icons::APP_WINDOW,
        Kind::Utility => icons::WRENCH,
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
        self.editor = None;
    }

    /// Adds a picked file (a program when it is an .exe and friends) or folder.
    fn add_picked(&mut self, ctx: &AppCtx<'_>, folder: bool) {
        let picked = if folder {
            rfd::FileDialog::new().pick_folder()
        } else {
            rfd::FileDialog::new().pick_file()
        };
        let Some(p) = picked else { return };
        if self.dry(ctx) {
            return;
        }
        let kind = if folder {
            Kind::Folder
        } else if p.extension().is_some_and(|e| {
            ["exe", "bat", "cmd", "com", "msc"]
                .iter()
                .any(|x| e.eq_ignore_ascii_case(x))
        }) {
            Kind::Program
        } else {
            Kind::File
        };
        let name = p
            .file_stem()
            .or(p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string());
        let args = if kind == Kind::Program {
            self.parameter.clone()
        } else {
            String::new()
        };
        let r = add(ctx, kind, &name, &p, &args, "").map(|id| {
            self.selected = Some(id);
            format!("Added \"{name}\"; change its title below if you like.")
        });
        self.done(r);
    }

    /// The original's Edit group: Title, Parameter and Path (plus the icon).
    fn edit_bar(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>, sel: Option<&PcItem>) {
        match sel {
            Some(i) if i.own() => {
                if self.editor.as_ref().is_none_or(|e| e.clsid != i.clsid) {
                    self.editor = Some(Editor {
                        clsid: i.clsid.clone(),
                        name: i.name.clone(),
                        target: i.target.clone(),
                        args: i.args.clone(),
                        icon: i.icon.clone(),
                    });
                }
            }
            _ => self.editor = None,
        }
        let is_program = sel.is_some_and(|i| i.kind == Kind::Program);
        let is_folder = sel.is_some_and(|i| i.kind == Kind::Folder);
        let mut save = false;
        frame::edit_bar(ui, |ui| {
            let enabled = self.editor.is_some();
            let mut empty = Editor {
                clsid: String::new(),
                name: String::new(),
                target: String::new(),
                args: String::new(),
                icon: String::new(),
            };
            let ed = self.editor.as_mut().unwrap_or(&mut empty);
            let w = (ui.available_width() - 110.0).max(200.0);
            ui.add_enabled_ui(enabled, |ui| {
                let label = |ui: &mut egui::Ui, t: &str| {
                    ui.add_sized([64.0, 20.0], egui::Label::new(t));
                };
                ui.horizontal(|ui| {
                    label(ui, "Title");
                    ui.add(egui::TextEdit::singleline(&mut ed.name).desired_width(w));
                });
                ui.horizontal(|ui| {
                    label(ui, "Path");
                    ui.add(egui::TextEdit::singleline(&mut ed.target).desired_width(w));
                    if ui.button(icons::FOLDER_OPEN).on_hover_text("Browse").clicked() {
                        let picked = if is_folder {
                            rfd::FileDialog::new().pick_folder()
                        } else {
                            rfd::FileDialog::new().pick_file()
                        };
                        if let Some(p) = picked {
                            ed.target = p.display().to_string();
                        }
                    }
                });
                ui.add_enabled_ui(is_program, |ui| {
                    ui.horizontal(|ui| {
                        label(ui, "Parameter");
                        ui.add(egui::TextEdit::singleline(&mut ed.args).desired_width(w));
                    });
                });
                ui.horizontal(|ui| {
                    label(ui, "Icon");
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.icon)
                            .hint_text("Default icon (or file.dll,index)")
                            .desired_width(w),
                    );
                    if ui.button(icons::IMAGE).on_hover_text("Browse").clicked()
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
                let ok = !ed.name.trim().is_empty() && !ed.target.trim().is_empty();
                save = ui.add_enabled(ok, egui::Button::new("Save")).clicked();
            })
            .response
            .on_disabled_hover_text("Select an item you added to edit it");
        });
        if save
            && let Some(ed) = self.editor.take()
            && !self.dry(ctx)
        {
            let target = PathBuf::from(ed.target.trim().trim_matches('"'));
            let r = match self.items.iter().flatten().find(|i| i.clsid == ed.clsid).cloned() {
                Some(item) => edit(ctx, &item, &ed.name, &target, &ed.args, &ed.icon)
                    .map(|()| format!("Saved \"{}\".", ed.name.trim())),
                None => Err(anyhow::anyhow!("the item no longer exists")),
            };
            self.done(r);
        }
    }

    fn items_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.items.get_or_insert_with(enumerate).clone();
        let sel = self
            .selected
            .as_ref()
            .and_then(|id| items.iter().find(|i| &i.clsid == id))
            .cloned();
        let own = sel.as_ref().is_some_and(PcItem::own);
        let own_items: Vec<PcItem> = items.iter().filter(|i| i.own()).cloned().collect();
        let enabled = sel.as_ref().is_some_and(|i| i.enabled);

        // The original's action column.
        let mut act = None;
        let mut add_utility = None;
        frame::right(ui, 200.0, |ui| {
            if frame::action(ui, icons::PLUS, "Add File", true) {
                act = Some("file");
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.parameter)
                    .hint_text("Parameter")
                    .desired_width(ui.available_width()),
            )
            .on_hover_text("Arguments for a program added with Add File");
            if frame::action(ui, icons::PLUS, "Add Folder", true) {
                act = Some("folder");
            }
            if frame::action(ui, icons::PLUS, "Add Utility", true) {
                self.show_utilities = !self.show_utilities;
            }
            if self.show_utilities {
                let off: Vec<&PcItem> = items
                    .iter()
                    .filter(|i| i.kind == Kind::Utility && !i.enabled)
                    .collect();
                ui.indent("thispc-utils", |ui| {
                    if off.is_empty() {
                        ui.weak("Every Windows tool is listed.");
                    }
                    for u in off {
                        if ui.small_button(&u.name).clicked() {
                            add_utility = Some(u.clone());
                        }
                    }
                });
            }
            if frame::action(ui, icons::GEAR, "Desktop Icon Settings", true) {
                act = Some("desktop");
            }
            ui.separator();
            if frame::action(
                ui,
                if enabled { icons::EYE_SLASH } else { icons::CHECK },
                if enabled { "Disable" } else { "Enable" },
                sel.is_some(),
            ) {
                act = Some("toggle");
            }
            if frame::action(ui, icons::X, "Remove", own) {
                act = Some("remove");
            }
            if frame::action(ui, icons::X, "Remove All", !own_items.is_empty()) {
                act = Some("removeall");
            }
            ui.separator();
            if frame::action(ui, icons::DESKTOP, "This PC", true) {
                act = Some("thispc");
            }
            if frame::action(ui, icons::FOLDERS, "Personal Folders", true) {
                act = Some("folders");
            }
            ui.separator();
            if frame::action(ui, icons::FOLDER_OPEN, "Open", sel.is_some()) {
                act = Some("open");
            }
            if frame::action(ui, icons::ARROWS_CLOCKWISE, "Refresh", true) {
                act = Some("refresh");
            }
            if frame::action(ui, icons::WINDOWS_LOGO, "Restart Explorer", true) {
                act = Some("explorer");
            }
        });
        if let Some(u) = add_utility
            && !self.dry(ctx)
        {
            let r = set_enabled(ctx, &u, true).map(|()| format!("Added \"{}\".", u.name));
            self.selected = Some(u.clsid.clone());
            self.done(r);
        }
        match act {
            Some("file") => self.add_picked(ctx, false),
            Some("folder") => self.add_picked(ctx, true),
            Some("desktop") => {
                if let Err(e) = desktop_icon_settings() {
                    self.message = Some(Err(format!("{e:#}")));
                }
            }
            Some("toggle") => {
                if let Some(i) = &sel
                    && !self.dry(ctx)
                {
                    let on = !i.enabled;
                    let verb = if on { "Enabled" } else { "Disabled" };
                    let r = set_enabled(ctx, i, on).map(|()| format!("{verb} \"{}\".", i.name));
                    self.done(r);
                }
            }
            Some("remove") => {
                if let Some(i) = sel.clone() {
                    self.confirm
                        .ask(format!("Remove \"{}\" from This PC?", i.name), Ask::Remove(i));
                }
            }
            Some("removeall") => self.confirm.ask(
                format!(
                    "Remove all {} items you added to This PC? Windows tools are only turned off by Disable.",
                    own_items.len()
                ),
                Ask::RemoveAll(own_items.clone()),
            ),
            Some("thispc") => {
                if let Err(e) = shell_open("shell:MyComputerFolder", "") {
                    self.message = Some(Err(format!("{e:#}")));
                }
            }
            Some("folders") => self.folders_page = true,
            Some("open") => {
                if let Some(i) = &sel {
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
            Some("refresh") => self.items = None,
            Some("explorer") => self.confirm.ask(
                "Restart File Explorer now? Open Explorer windows close.",
                Ask::RestartExplorer,
            ),
            _ => {}
        }

        let on = items.iter().filter(|i| i.enabled).count();
        frame::status(ui, &format!("{} items, {on} in This PC", items.len()), |_| {});
        self.edit_bar(ui, ctx, sel.as_ref());
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            self.list(ui, &items);
        });
    }

    /// What This PC shows: items added here (greyed when disabled), then the Windows
    /// tools that are listed. Other Windows tools come in through Add Utility.
    fn list(&mut self, ui: &mut egui::Ui, items: &[PcItem]) {
        let shown: Vec<&PcItem> = items.iter().filter(|i| i.own() || i.enabled).collect();
        if shown.is_empty() {
            wm_core::ui::empty_state(
                ui,
                icons::DESKTOP,
                "Nothing added to This PC",
                "Use Add File, Add Folder or Add Utility on the right.",
            );
            return;
        }
        for item in shown {
            let mut text = egui::RichText::new(format!("{}  {}", kind_icon(item.kind), item.name));
            if !item.enabled {
                text = text.weak();
            }
            if item.missing() {
                text = text.color(ui.visuals().error_fg_color);
            }
            let sel = self.selected.as_ref() == Some(&item.clsid);
            let mut tip = item.kind.label().to_owned();
            if item.own() {
                tip = format!("{tip}: {} {}", item.target, item.args).trim().to_owned();
            }
            if !item.enabled {
                tip.push_str(" (not in This PC)");
            }
            if item.missing() {
                tip.push_str(" — the target no longer exists");
            }
            let r = ui.selectable_label(sel, text).on_hover_text(tip);
            if r.clicked() {
                self.selected = Some(item.clsid.clone());
            }
        }
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
                Ask::RemoveAll(list) => {
                    if !self.dry(ctx) {
                        let r = list
                            .iter()
                            .try_for_each(|i| remove(ctx, i))
                            .map(|()| format!("Removed {} items.", list.len()));
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
        if self.folders_page {
            let mut back = false;
            egui::Panel::top("thispc-back")
                .resizable(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        back = ui.button(format!("{}  This PC items", icons::ARROW_UP)).clicked();
                        ui.label("›");
                        ui.strong("Personal folders");
                    });
                });
            if back {
                self.folders_page = false;
                return;
            }
            frame::nested(ui, |ui| {
                frame::options(ui, |ui| {
                    ui.weak("Which personal folders This PC shows. Applies to all users (administrator required).");
                });
                self.panel.ui_framed(ui, ctx);
            });
            return;
        }
        self.items_page(ui, ctx);
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
