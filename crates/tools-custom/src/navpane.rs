//! Navigation Pane Manager: what File Explorer's navigation pane shows, in which order.
//!
//! Folders are added the documented way for cloud-storage roots: a per-user shell
//! folder (`HKCU\Software\Classes\CLSID\{id}`) whose instance is a folder shortcut
//! pointing at the target, listed under the desktop namespace and marked
//! `System.IsPinnedToNameSpaceTree`. Built-in items (Home, Gallery, Libraries,
//! Control Panel, Recycle Bin, your user folder) and other programs' items (OneDrive,
//! Dropbox…) are shown or hidden, and ordered, through the same per-user values.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, SHLoadIndirectString};
use windows::core::{GUID, HSTRING};
use wm_core::{
    AppCtx, Category, Tool,
    egui::{self, Vec2},
    icons, paths,
    process::restart_explorer,
    registry::{self, RawValue},
    safety::UndoAction,
    shell::{reveal_in_explorer, shell_open},
    tweaks::TweakPanel,
    ui::{Confirm, Message, ToolButton, show_message, status_bar, toolbar},
};

const TOOL: &str = "navpane";
pub(crate) const CLASSES: &str = r"HKCU\Software\Classes\CLSID";
const NAMESPACE: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace";
const HIDE_ON_DESKTOP: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
const PINNED: &str = "System.IsPinnedToNameSpaceTree";
const ORDER: &str = "SortOrderIndex";
/// Marks the folders this tool created.
const MARKER: &str = "WinManagerRS";
/// The shell's folder-shortcut object, which shows a file system folder as a shell folder.
const FOLDER_SHORTCUT: &str = "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}";
pub(crate) const FOLDER_ICON: &str = r"%SystemRoot%\system32\imageres.dll,-3";
/// Where new folders go: after Home (0x40) and Gallery (0x41), next to OneDrive (0x42).
const DEFAULT_ORDER: u32 = 0x42;

/// Built-in items that follow `System.IsPinnedToNameSpaceTree`.
const BUILT_IN: [(&str, &str); 6] = [
    ("{f874310e-b6b7-47dc-bc84-b9e6b38f5903}", "Home"),
    ("{e88865ea-0e1c-4e20-9aa6-edcd0212c87c}", "Gallery"),
    ("{031E4825-7B94-4dc3-B131-E946B44C8DD5}", "Libraries"),
    ("{26EE0668-A00A-44D7-9371-BEB064C98683}", "Control Panel"),
    ("{645FF040-5081-101B-9F08-00AA002F954E}", "Recycle Bin"),
    ("{59031a47-3f72-44a7-89c5-5595fe6b30ee}", "User folder"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A folder added by this tool.
    Folder { target: String, icon: String },
    /// A Windows item.
    Utility,
    /// Another program's item (cloud storage and the like).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavItem {
    /// `{…}` as written in the registry.
    pub clsid: String,
    pub name: String,
    pub kind: Kind,
    pub shown: bool,
    pub order: Option<u32>,
}

impl NavItem {
    fn type_label(&self) -> &'static str {
        match self.kind {
            Kind::Folder { .. } => "Folder",
            Kind::Utility => "Windows",
            Kind::Other => "Program",
        }
    }

    fn missing(&self) -> bool {
        matches!(&self.kind, Kind::Folder { target, .. } if !Path::new(&paths::expand_env(target)).is_dir())
    }
}

pub(crate) fn own_key(clsid: &str) -> String {
    format!(r"{CLASSES}\{clsid}")
}

pub(crate) fn merged_key(clsid: &str) -> String {
    format!(r"HKCR\CLSID\{clsid}")
}

pub(crate) fn dword_of(key: &str, name: &str) -> Option<u32> {
    registry::read_raw(key, name).and_then(|v| registry::decode_dword(&v))
}

pub(crate) fn string_of(key: &str, name: &str) -> Option<String> {
    registry::read_raw(key, name)
        .map(|v| registry::decode_string(&v))
        .filter(|s| !s.trim().is_empty())
}

pub(crate) fn expand_sz(s: &str) -> RawValue {
    RawValue {
        vtype: 2,
        ..registry::string(s)
    }
}

/// Resolves `@dll,-id` strings; anything else is returned unchanged.
pub(crate) fn resolve(s: &str) -> String {
    if !s.starts_with('@') {
        return s.to_owned();
    }
    let mut buf = [0u16; 260];
    match unsafe { SHLoadIndirectString(&HSTRING::from(s), &mut buf, None) } {
        Ok(()) => String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]),
        Err(_) => s.to_owned(),
    }
}

fn display_name(clsid: &str, fallback: &str) -> String {
    let key = merged_key(clsid);
    if string_of(&own_key(clsid), MARKER).is_some() {
        return string_of(&own_key(clsid), "").unwrap_or_else(|| clsid.to_owned());
    }
    string_of(&key, "LocalizedString")
        .map(|s| resolve(&s))
        .filter(|s| !s.starts_with('@'))
        .or_else(|| (!fallback.is_empty()).then(|| fallback.to_owned()))
        .or_else(|| string_of(&key, ""))
        .unwrap_or_else(|| clsid.to_owned())
}

fn namespace_ids() -> Vec<String> {
    let mut ids = Vec::new();
    for root in ["HKCU", "HKLM"] {
        if let Ok(k) = registry::open_read(&format!(r"{root}\{NAMESPACE}")) {
            ids.extend(k.enum_keys().flatten().filter(|s| s.starts_with('{')));
        }
    }
    ids
}

fn read_item(clsid: &str, fallback: &str, utility: bool) -> NavItem {
    let key = merged_key(clsid);
    let own = own_key(clsid);
    let kind = if string_of(&own, MARKER).is_some() {
        Kind::Folder {
            target: string_of(&format!(r"{own}\Instance\InitPropertyBag"), "TargetFolderPath")
                .unwrap_or_default(),
            icon: string_of(&format!(r"{own}\DefaultIcon"), "").unwrap_or_default(),
        }
    } else if utility {
        Kind::Utility
    } else {
        Kind::Other
    };
    NavItem {
        clsid: clsid.to_owned(),
        name: display_name(clsid, fallback),
        kind,
        shown: dword_of(&key, PINNED).is_some_and(|v| v != 0),
        order: dword_of(&key, ORDER),
    }
}

/// Every item the navigation pane can show, in the pane's order (shown items first).
pub fn enumerate() -> Vec<NavItem> {
    let mut items: Vec<NavItem> = BUILT_IN
        .iter()
        .filter(|(id, _)| registry::open_read(&merged_key(id)).is_ok())
        .map(|(id, name)| read_item(id, name, true))
        .collect();
    for id in namespace_ids() {
        if items.iter().any(|i| i.clsid.eq_ignore_ascii_case(&id)) {
            continue;
        }
        // Only items that opt into the navigation pane.
        if registry::read_raw(&merged_key(&id), PINNED).is_some()
            || string_of(&own_key(&id), MARKER).is_some()
        {
            items.push(read_item(&id, "", false));
        }
    }
    sort(&mut items);
    items
}

fn sort(items: &mut [NavItem]) {
    items.sort_by_key(|i| (!i.shown, i.order.unwrap_or(u32::MAX), i.name.to_lowercase()));
}

/// Tells Explorer the namespace changed (open windows pick it up when reopened).
pub(crate) fn notify() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

fn set_values(ctx: &AppCtx<'_>, what: String, values: &[(String, &str, RawValue)]) -> Result<()> {
    let snaps: Vec<_> = values.iter().map(|(k, n, _)| registry::snapshot(k, n)).collect();
    for (k, n, v) in values {
        registry::write_raw(k, n, v)?;
    }
    ctx.journal
        .record(TOOL, what, UndoAction::RestoreRegValues { values: snaps })?;
    notify();
    Ok(())
}

pub fn set_shown(ctx: &AppCtx<'_>, item: &NavItem, shown: bool) -> Result<()> {
    let verb = if shown { "Showed" } else { "Hid" };
    set_values(
        ctx,
        format!("{verb} \"{}\" in the navigation pane", item.name),
        &[(own_key(&item.clsid), PINNED, registry::dword(u32::from(shown)))],
    )
}

/// New order values for `items` (in the wanted order), spaced so later moves fit
/// between them. Only items whose value changes are returned.
pub fn renumber(items: &[NavItem]) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut prev: Option<u32> = None;
    for (n, item) in items.iter().enumerate() {
        let keep = item.order.filter(|&o| prev.is_none_or(|p| o > p));
        let value = keep.unwrap_or_else(|| prev.map_or(0x30 + n as u32, |p| p + 1));
        if keep.is_none() {
            out.push((item.clsid.clone(), value));
        }
        prev = Some(value);
    }
    out
}

/// Moves the shown item at `from` to `to` among the shown items.
pub fn reorder(ctx: &AppCtx<'_>, shown: &[NavItem], from: usize, to: usize, what: String) -> Result<()> {
    let mut items = shown.to_vec();
    let moved = items.remove(from);
    items.insert(to, NavItem { order: None, ..moved });
    // An item placed first goes before everything else.
    if to == 0
        && let Some(next) = items.get(1).and_then(|i| i.order)
    {
        items[0].order = Some(next.saturating_sub(1));
        if next == 0 {
            items[0].order = None;
            for i in &mut items[1..] {
                i.order = None;
            }
        }
    }
    let changes = renumber(&items);
    if changes.is_empty() {
        return Ok(());
    }
    let values: Vec<_> = changes
        .into_iter()
        .map(|(id, v)| (own_key(&id), ORDER, registry::dword(v)))
        .collect();
    set_values(ctx, what, &values)
}

pub(crate) fn valid_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        bail!("Enter a name");
    }
    Ok(n.to_owned())
}

/// Writes the parts of a per-user shell folder class that show `target` (a folder
/// shortcut object), with `icon` or the default folder icon. The name is set by
/// the caller.
pub(crate) fn write_folder_class(key: &str, target: &Path, icon: &str) -> Result<()> {
    let icon = if icon.trim().is_empty() {
        FOLDER_ICON
    } else {
        icon.trim()
    };
    registry::write_raw(&format!(r"{key}\DefaultIcon"), "", &expand_sz(icon))?;
    let server = format!(r"{key}\InProcServer32");
    registry::write_raw(&server, "", &expand_sz(r"%SystemRoot%\system32\shell32.dll"))?;
    registry::write_raw(&server, "ThreadingModel", &registry::string("Both"))?;
    registry::write_raw(
        &format!(r"{key}\Instance"),
        "CLSID",
        &registry::string(FOLDER_SHORTCUT),
    )?;
    let bag = format!(r"{key}\Instance\InitPropertyBag");
    // FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_READONLY, as the documented sample.
    registry::write_raw(&bag, "Attributes", &registry::dword(0x11))?;
    registry::write_raw(
        &bag,
        "TargetFolderPath",
        &expand_sz(&target.display().to_string()),
    )?;
    let folder = format!(r"{key}\ShellFolder");
    registry::write_raw(&folder, "FolderValueFlags", &registry::dword(0x28))?;
    registry::write_raw(&folder, "Attributes", &registry::dword(0xF080_004D))?;
    Ok(())
}

/// Adds a folder to the navigation pane. Returns its CLSID.
pub fn add_folder(ctx: &AppCtx<'_>, name: &str, target: &Path, icon: &str) -> Result<String> {
    let name = valid_name(name)?;
    if !target.is_dir() {
        bail!("{} is not a folder", target.display());
    }
    let clsid = format!("{{{:?}}}", GUID::new()?);
    let key = own_key(&clsid);
    let ns = format!(r"HKCU\{NAMESPACE}\{clsid}");
    let undo_hide = registry::snapshot(HIDE_ON_DESKTOP, &clsid);
    let undo = UndoAction::Sequence {
        steps: vec![
            UndoAction::DeleteRegKey { key: ns.clone() },
            UndoAction::DeleteRegKey { key: key.clone() },
            UndoAction::RestoreRegValues {
                values: vec![undo_hide],
            },
        ],
    };
    let write = || -> Result<()> {
        registry::write_raw(&key, "", &registry::string(&name))?;
        registry::write_raw(&key, MARKER, &registry::dword(1))?;
        registry::write_raw(&key, PINNED, &registry::dword(1))?;
        registry::write_raw(&key, ORDER, &registry::dword(DEFAULT_ORDER))?;
        write_folder_class(&key, target, icon)?;
        registry::write_raw(&ns, "", &registry::string(&name))?;
        // Keep it off the desktop itself.
        registry::write_raw(HIDE_ON_DESKTOP, &clsid, &registry::dword(1))?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = remove_keys(&clsid);
        return Err(e);
    }
    ctx.journal
        .record(TOOL, format!("Added \"{name}\" to the navigation pane"), undo)?;
    notify();
    Ok(clsid)
}

pub(crate) fn delete_key(path: &str) -> Result<()> {
    let (root, sub) = registry::split_path(path)?;
    match root.delete_subkey_all(sub) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting {path}")),
    }
}

fn remove_keys(clsid: &str) -> Result<()> {
    delete_key(&format!(r"HKCU\{NAMESPACE}\{clsid}"))?;
    delete_key(&own_key(clsid))?;
    registry::delete_value(HIDE_ON_DESKTOP, clsid)
}

/// Changes a folder's name, target or icon.
pub fn edit_folder(ctx: &AppCtx<'_>, item: &NavItem, name: &str, target: &Path, icon: &str) -> Result<()> {
    let name = valid_name(name)?;
    if !target.is_dir() {
        bail!("{} is not a folder", target.display());
    }
    let key = own_key(&item.clsid);
    let icon = if icon.trim().is_empty() {
        FOLDER_ICON
    } else {
        icon.trim()
    };
    set_values(
        ctx,
        format!("Edited \"{name}\" in the navigation pane"),
        &[
            (key.clone(), "", registry::string(&name)),
            (
                format!(r"HKCU\{NAMESPACE}\{}", item.clsid),
                "",
                registry::string(&name),
            ),
            (
                format!(r"{key}\Instance\InitPropertyBag"),
                "TargetFolderPath",
                expand_sz(&target.display().to_string()),
            ),
            (format!(r"{key}\DefaultIcon"), "", expand_sz(icon)),
        ],
    )
}

/// Removes a folder this tool added (undo re-imports the saved keys).
pub fn remove_folder(ctx: &AppCtx<'_>, item: &NavItem) -> Result<()> {
    if !matches!(item.kind, Kind::Folder { .. }) {
        bail!("only folders added here can be removed; hide the others instead");
    }
    let ns = format!(r"HKCU\{NAMESPACE}\{}", item.clsid);
    let key = own_key(&item.clsid);
    let undo = UndoAction::Sequence {
        steps: vec![
            ctx.journal.backup_reg_key(&key)?,
            ctx.journal.backup_reg_key(&ns)?,
            UndoAction::RestoreRegValues {
                values: vec![registry::snapshot(HIDE_ON_DESKTOP, &item.clsid)],
            },
        ],
    };
    remove_keys(&item.clsid)?;
    ctx.journal.record(
        TOOL,
        format!("Removed \"{}\" from the navigation pane", item.name),
        undo,
    )?;
    notify();
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Items,
    Options,
}

struct Editor {
    /// The folder being edited, or None for a new one.
    clsid: Option<String>,
    name: String,
    target: String,
    icon: String,
}

enum Ask {
    Remove(NavItem),
    RestartExplorer,
}

pub struct NavPaneManager {
    tab: Tab,
    items: Option<Vec<NavItem>>,
    selected: Option<String>,
    editor: Option<Editor>,
    panel: TweakPanel,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for NavPaneManager {
    fn default() -> Self {
        Self {
            tab: Tab::Items,
            items: None,
            selected: None,
            editor: None,
            panel: TweakPanel::new(TOOL, crate::NAVPANE_TWEAKS),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl NavPaneManager {
    fn dry(&mut self, ctx: &AppCtx<'_>) -> bool {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
        }
        ctx.config.dry_run
    }

    fn done(&mut self, r: Result<String>) {
        self.message = Some(
            r.map(|m| format!("{m} Reopen File Explorer windows (or restart Explorer) to see it."))
                .map_err(|e| format!("{e:#}")),
        );
        self.items = None;
    }

    fn editor_window(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ed) = &mut self.editor else { return };
        let mut result = None;
        let title = if ed.clsid.is_some() {
            "Edit folder"
        } else {
            "Add folder"
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                egui::Grid::new("navpane-edit").num_columns(2).show(ui, |ui| {
                    ui.label("Folder");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut ed.target).desired_width(260.0));
                        if ui.button("Browse…").clicked()
                            && let Some(p) = rfd::FileDialog::new().pick_folder()
                        {
                            ed.target = p.display().to_string();
                            if ed.name.is_empty() {
                                ed.name = p
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| ed.target.clone());
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Name");
                    ui.text_edit_singleline(&mut ed.name);
                    ui.end_row();
                    ui.label("Icon");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.icon)
                                .hint_text("Default folder icon")
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
                    None => add_folder(ctx, &ed.name, &target, &ed.icon).map(|id| {
                        self.selected = Some(id);
                        format!("Added \"{}\".", ed.name.trim())
                    }),
                    Some(id) => {
                        let item = self.items.iter().flatten().find(|i| &i.clsid == id).cloned();
                        match item {
                            Some(item) => edit_folder(ctx, &item, &ed.name, &target, &ed.icon)
                                .map(|()| format!("Saved \"{}\".", ed.name.trim())),
                            None => Err(anyhow::anyhow!("the folder no longer exists")),
                        }
                    }
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
            .and_then(|id| items.iter().position(|i| &i.clsid == id));
        let shown: Vec<NavItem> = items.iter().filter(|i| i.shown).cloned().collect();
        let sel_item = sel.map(|i| items[i].clone());
        let sel_shown = sel_item
            .as_ref()
            .and_then(|s| shown.iter().position(|i| i.clsid == s.clsid));
        let is_folder = sel_item
            .as_ref()
            .is_some_and(|i| matches!(i.kind, Kind::Folder { .. }));

        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add folder")),
                Some(ToolButton::new(icons::PENCIL_SIMPLE, "Edit").enabled(is_folder)),
                Some(ToolButton::new(icons::TRASH, "Remove").enabled(is_folder)),
                None,
                Some(
                    ToolButton::new(
                        icons::EYE_SLASH,
                        if sel_item.as_ref().is_some_and(|i| i.shown) {
                            "Hide"
                        } else {
                            "Show"
                        },
                    )
                    .enabled(sel_item.is_some()),
                ),
                Some(ToolButton::new(icons::PUSH_PIN, "To top").enabled(sel_shown.is_some_and(|i| i > 0))),
                Some(ToolButton::new(icons::ARROW_UP, "Up").enabled(sel_shown.is_some_and(|i| i > 0))),
                Some(
                    ToolButton::new(icons::ARROW_DOWN, "Down")
                        .enabled(sel_shown.is_some_and(|i| i + 1 < shown.len())),
                ),
                None,
                Some(ToolButton::new(icons::FOLDER_OPEN, "Open").enabled(sel_item.is_some())),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
                Some(ToolButton::new(icons::WINDOWS_LOGO, "Restart Explorer")),
            ],
        ) {
            Some(0) => {
                self.editor = Some(Editor {
                    clsid: None,
                    name: String::new(),
                    target: String::new(),
                    icon: String::new(),
                })
            }
            Some(1) => {
                if let Some(NavItem {
                    clsid,
                    name,
                    kind: Kind::Folder { target, icon },
                    ..
                }) = sel_item
                {
                    self.editor = Some(Editor {
                        clsid: Some(clsid),
                        name,
                        target,
                        icon,
                    });
                }
            }
            Some(2) => {
                if let Some(item) = sel_item {
                    self.confirm.ask(
                        format!("Remove \"{}\" from the navigation pane?", item.name),
                        Ask::Remove(item),
                    );
                }
            }
            Some(4) => {
                if let Some(item) = sel_item
                    && !self.dry(ctx)
                {
                    let shown = !item.shown;
                    let mut r = set_shown(ctx, &item, shown);
                    // A newly shown item goes to the end of the shown items.
                    if r.is_ok() && shown && item.order.is_none() {
                        let last = items.iter().filter(|i| i.shown).filter_map(|i| i.order).max();
                        let v = last.map_or(DEFAULT_ORDER, |l| l + 1);
                        r = set_values(
                            ctx,
                            format!("Placed \"{}\" in the navigation pane", item.name),
                            &[(own_key(&item.clsid), ORDER, registry::dword(v))],
                        );
                    }
                    let verb = if shown { "Showing" } else { "Hiding" };
                    self.done(r.map(|()| format!("{verb} \"{}\".", item.name)));
                }
            }
            Some(b @ (5..=7)) => {
                if let (Some(from), Some(item)) = (sel_shown, sel_item)
                    && !self.dry(ctx)
                {
                    let to = match b {
                        5 => 0,
                        6 => from - 1,
                        _ => from + 1,
                    };
                    let what = format!("Moved \"{}\" in the navigation pane", item.name);
                    let r =
                        reorder(ctx, &shown, from, to, what).map(|()| format!("Moved \"{}\".", item.name));
                    self.done(r);
                }
            }
            Some(9) => {
                if let Some(item) = sel_item {
                    let r = match &item.kind {
                        Kind::Folder { target, .. } => shell_open(&paths::expand_env(target), ""),
                        _ => shell_open(&format!("shell:::{}", item.clsid), ""),
                    };
                    if let Err(e) = r {
                        self.message = Some(Err(format!("{e:#}")));
                    }
                }
            }
            Some(10) => self.items = None,
            Some(11) => self.confirm.ask(
                "Restart File Explorer now? Open Explorer windows close.",
                Ask::RestartExplorer,
            ),
            _ => {}
        }

        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let avail = ui.available_height() - 28.0;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .max_scroll_height(avail.max(120.0))
            .column(Column::exact(60.0))
            .column(Column::initial(200.0).at_least(120.0))
            .column(Column::exact(80.0))
            .column(Column::exact(60.0))
            .column(Column::remainder().at_least(120.0))
            .header(row_h, |mut h| {
                for t in ["Shown", "Name", "Type", "Order", "Location"] {
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
                        ui.label(if item.shown { icons::CHECK } else { "" });
                    });
                    row.col(|ui| {
                        if item.missing() {
                            ui.colored_label(ui.visuals().error_fg_color, &item.name)
                                .on_hover_text("The folder no longer exists");
                        } else {
                            ui.label(&item.name);
                        }
                    });
                    row.col(|ui| {
                        ui.label(item.type_label());
                    });
                    row.col(|ui| {
                        ui.label(item.order.map_or("—".to_owned(), |o| o.to_string()));
                    });
                    row.col(|ui| {
                        let loc = match &item.kind {
                            Kind::Folder { target, .. } => target.clone(),
                            _ => item.clsid.clone(),
                        };
                        ui.label(loc);
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        self.selected = Some(item.clsid.clone());
                    }
                    if resp.double_clicked()
                        && let Kind::Folder { target, .. } = &item.kind
                    {
                        reveal_in_explorer(Path::new(&paths::expand_env(target)));
                    }
                });
            });

        let n = items.iter().filter(|i| i.shown).count();
        status_bar(ui, &format!("{} items, {n} shown", items.len()), |_| {});
    }
}

impl Tool for NavPaneManager {
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
                        let r = remove_folder(ctx, &item).map(|()| format!("Removed \"{}\".", item.name));
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
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Items, "Items");
            ui.selectable_value(&mut self.tab, Tab::Options, "Options");
        });
        ui.separator();
        show_message(ui, &self.message);
        match self.tab {
            Tab::Items => self.items_tab(ui, ctx),
            Tab::Options => self.panel.ui(ui, ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, order: Option<u32>) -> NavItem {
        NavItem {
            clsid: id.into(),
            name: id.into(),
            kind: Kind::Utility,
            shown: true,
            order,
        }
    }

    #[test]
    fn renumber_keeps_increasing_values_and_fills_gaps() {
        let items = [
            item("a", Some(0x40)),
            item("b", None),
            item("c", Some(0x41)),
            item("d", Some(0x50)),
        ];
        // b goes after a; c (0x41) is no longer above b (0x41), so it moves to 0x42.
        assert_eq!(renumber(&items), vec![("b".into(), 0x41), ("c".into(), 0x42)]);
        let fresh = [item("x", None), item("y", None)];
        assert_eq!(renumber(&fresh), vec![("x".into(), 0x30), ("y".into(), 0x31)]);
        assert!(renumber(&[item("a", Some(1)), item("b", Some(5))]).is_empty());
    }

    #[test]
    fn enumerate_lists_built_in_items() {
        let items = enumerate();
        assert!(items.iter().any(|i| i.clsid.eq_ignore_ascii_case(BUILT_IN[0].0)));
        assert!(items.iter().all(|i| !i.name.is_empty()));
        // Shown items come first.
        let first_hidden = items.iter().position(|i| !i.shown).unwrap_or(items.len());
        assert!(items[first_hidden..].iter().all(|i| !i.shown));
    }

    #[test]
    fn add_edit_and_remove_a_folder() {
        let dir = tempfile::tempdir().unwrap();
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

        let id = add_folder(&ctx, "WM test folder", dir.path(), "").unwrap();
        let found = enumerate().into_iter().find(|i| i.clsid == id).unwrap();
        assert!(found.shown);
        assert_eq!(found.name, "WM test folder");
        assert!(matches!(&found.kind, Kind::Folder { target, .. } if Path::new(target) == dir.path()));

        edit_folder(&ctx, &found, "WM renamed", dir.path(), "").unwrap();
        let found = enumerate().into_iter().find(|i| i.clsid == id).unwrap();
        assert_eq!(found.name, "WM renamed");

        set_shown(&ctx, &found, false).unwrap();
        assert!(!enumerate().into_iter().find(|i| i.clsid == id).unwrap().shown);

        remove_folder(&ctx, &found).unwrap();
        assert!(enumerate().iter().all(|i| i.clsid != id));
        assert!(registry::open_read(&own_key(&id)).is_err());
        assert!(registry::read_raw(HIDE_ON_DESKTOP, &id).is_none());
    }
}
