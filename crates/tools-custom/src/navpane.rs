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
use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, SHLoadIndirectString};
use windows::core::{GUID, HSTRING};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, paths,
    process::restart_explorer,
    registry::{self, RawValue},
    safety::UndoAction,
    shell::{reveal_in_explorer, shell_open},
    tweaks::TweakPanel,
    ui::{Confirm, Message, frame, show_message},
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

/// The Edit group's fields, loaded from the selected folder.
struct Editor {
    clsid: String,
    name: String,
    target: String,
    icon: String,
}

enum Ask {
    Remove(NavItem),
    RestartExplorer,
}

pub struct NavPaneManager {
    /// The tweak options page is shown instead of the item list.
    options: bool,
    items: Option<Vec<NavItem>>,
    selected: Option<String>,
    editor: Option<Editor>,
    /// New folders go to the top of the pane (the original's "Place on Top").
    place_on_top: bool,
    panel: TweakPanel,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for NavPaneManager {
    fn default() -> Self {
        Self {
            options: false,
            items: None,
            selected: None,
            editor: None,
            place_on_top: true,
            panel: TweakPanel::new(TOOL, crate::NAVPANE_TWEAKS),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

/// Items in the order the pane shows them (shown items by order, then hidden ones).
fn pane_order(items: &[NavItem]) -> Vec<&NavItem> {
    let mut v: Vec<&NavItem> = items.iter().collect();
    v.sort_by_key(|i| (!i.shown, i.order.unwrap_or(u32::MAX)));
    v
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
        self.editor = None;
    }

    /// Adds a folder picked in a dialog, named after it, and selects it.
    fn add_folder_dialog(&mut self, ctx: &AppCtx<'_>) {
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        if self.dry(ctx) {
            return;
        }
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        let r = add_folder(ctx, &name, &dir, "").and_then(|id| {
            if self.place_on_top {
                let items = enumerate();
                let shown: Vec<NavItem> = pane_order(&items)
                    .into_iter()
                    .filter(|i| i.shown)
                    .cloned()
                    .collect();
                if let Some(from) = shown.iter().position(|i| i.clsid == id)
                    && from > 0
                {
                    reorder(
                        ctx,
                        &shown,
                        from,
                        0,
                        format!("Placed \"{name}\" on top of the navigation pane"),
                    )?;
                }
            }
            self.selected = Some(id);
            Ok(format!("Added \"{name}\"; change its title below if you like."))
        });
        self.done(r);
    }

    /// The original's Edit group: Title and Path (plus the icon) of the selected
    /// added folder.
    fn edit_bar(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>, sel: Option<&NavItem>) {
        // Load the fields when the selection changes.
        match sel {
            Some(NavItem {
                clsid,
                name,
                kind: Kind::Folder { target, icon },
                ..
            }) => {
                if self.editor.as_ref().is_none_or(|e| &e.clsid != clsid) {
                    self.editor = Some(Editor {
                        clsid: clsid.clone(),
                        name: name.clone(),
                        target: target.clone(),
                        icon: icon.clone(),
                    });
                }
            }
            _ => self.editor = None,
        }
        let mut save = false;
        frame::edit_bar(ui, |ui| {
            let enabled = self.editor.is_some();
            let mut empty = Editor {
                clsid: String::new(),
                name: String::new(),
                target: String::new(),
                icon: String::new(),
            };
            let ed = self.editor.as_mut().unwrap_or(&mut empty);
            let w = (ui.available_width() - 110.0).max(200.0);
            ui.add_enabled_ui(enabled, |ui| {
                let label = |ui: &mut egui::Ui, t: &str| {
                    ui.add_sized([40.0, 20.0], egui::Label::new(t));
                };
                ui.horizontal(|ui| {
                    label(ui, "Title");
                    ui.add(egui::TextEdit::singleline(&mut ed.name).desired_width(w));
                });
                ui.horizontal(|ui| {
                    label(ui, "Path");
                    ui.add(egui::TextEdit::singleline(&mut ed.target).desired_width(w));
                    if ui.button(icons::FOLDER_OPEN).on_hover_text("Browse").clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_folder()
                    {
                        ed.target = p.display().to_string();
                    }
                });
                ui.horizontal(|ui| {
                    label(ui, "Icon");
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.icon)
                            .hint_text("Default folder icon (or file.dll,index)")
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
            .on_disabled_hover_text("Select a folder you added to edit it");
        });
        if save
            && let Some(ed) = self.editor.take()
            && !self.dry(ctx)
        {
            let target = PathBuf::from(ed.target.trim().trim_matches('"'));
            let r = match self.items.iter().flatten().find(|i| i.clsid == ed.clsid).cloned() {
                Some(item) => edit_folder(ctx, &item, &ed.name, &target, &ed.icon)
                    .map(|()| format!("Saved \"{}\".", ed.name.trim())),
                None => Err(anyhow::anyhow!("the folder no longer exists")),
            };
            self.done(r);
        }
    }

    fn items_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.items.get_or_insert_with(enumerate).clone();
        let sel_item = self
            .selected
            .as_ref()
            .and_then(|id| items.iter().find(|i| &i.clsid == id))
            .cloned();
        let shown: Vec<NavItem> = pane_order(&items)
            .into_iter()
            .filter(|i| i.shown)
            .cloned()
            .collect();
        let sel_shown = sel_item
            .as_ref()
            .and_then(|s| shown.iter().position(|i| i.clsid == s.clsid));
        let is_folder = sel_item
            .as_ref()
            .is_some_and(|i| matches!(i.kind, Kind::Folder { .. }));
        let is_shown = sel_item.as_ref().is_some_and(|i| i.shown);

        // The original's action column.
        let mut act = None;
        frame::right(ui, 190.0, |ui| {
            ui.checkbox(&mut self.place_on_top, "Place on Top")
                .on_hover_text("New folders go to the top of the navigation pane");
            ui.add_space(4.0);
            if frame::action(ui, icons::PLUS, "Add Folder", true) {
                act = Some("add");
            }
            ui.separator();
            if frame::action(ui, icons::ARROW_UP, "Up", sel_shown.is_some_and(|i| i > 0)) {
                act = Some("up");
            }
            if frame::action(
                ui,
                icons::ARROW_DOWN,
                "Down",
                sel_shown.is_some_and(|i| i + 1 < shown.len()),
            ) {
                act = Some("down");
            }
            if frame::action(
                ui,
                icons::PUSH_PIN,
                "Move to Top",
                sel_shown.is_some_and(|i| i > 0),
            ) {
                act = Some("top");
            }
            ui.separator();
            if frame::action(ui, icons::EYE_SLASH, "Disable", sel_item.is_some() && is_shown) {
                act = Some("hide");
            }
            if frame::action(ui, icons::CHECK, "Enable", sel_item.is_some() && !is_shown) {
                act = Some("show");
            }
            if frame::action(ui, icons::X, "Remove", is_folder) {
                act = Some("remove");
            }
            ui.separator();
            if frame::action(ui, icons::DESKTOP, "This PC", true) {
                act = Some("thispc");
            }
            if frame::action(ui, icons::GEAR, "Options", true) {
                act = Some("options");
            }
            ui.separator();
            if frame::action(ui, icons::FOLDER_OPEN, "Open", sel_item.is_some()) {
                act = Some("open");
            }
            if frame::action(ui, icons::ARROWS_CLOCKWISE, "Refresh", true) {
                act = Some("refresh");
            }
            if frame::action(ui, icons::WINDOWS_LOGO, "Restart Explorer", true) {
                act = Some("explorer");
            }
        });
        match act {
            Some("add") => self.add_folder_dialog(ctx),
            Some(b @ ("up" | "down" | "top")) => {
                if let (Some(from), Some(item)) = (sel_shown, &sel_item)
                    && !self.dry(ctx)
                {
                    let to = match b {
                        "top" => 0,
                        "up" => from - 1,
                        _ => from + 1,
                    };
                    let what = format!("Moved \"{}\" in the navigation pane", item.name);
                    let r =
                        reorder(ctx, &shown, from, to, what).map(|()| format!("Moved \"{}\".", item.name));
                    self.done(r);
                }
            }
            Some(b @ ("hide" | "show")) => {
                if let Some(item) = &sel_item
                    && !self.dry(ctx)
                {
                    let shown_now = b == "show";
                    let mut r = set_shown(ctx, item, shown_now);
                    // A newly shown item goes to the end of the shown items.
                    if r.is_ok() && shown_now && item.order.is_none() {
                        let last = items.iter().filter(|i| i.shown).filter_map(|i| i.order).max();
                        let v = last.map_or(DEFAULT_ORDER, |l| l + 1);
                        r = set_values(
                            ctx,
                            format!("Placed \"{}\" in the navigation pane", item.name),
                            &[(own_key(&item.clsid), ORDER, registry::dword(v))],
                        );
                    }
                    let verb = if shown_now { "Enabled" } else { "Disabled" };
                    self.done(r.map(|()| format!("{verb} \"{}\".", item.name)));
                }
            }
            Some("remove") => {
                if let Some(item) = sel_item.clone() {
                    self.confirm.ask(
                        format!("Remove \"{}\" from the navigation pane?", item.name),
                        Ask::Remove(item),
                    );
                }
            }
            Some("thispc") => wm_core::ui::request_open_tool(ui.ctx(), "thispc"),
            Some("options") => self.options = true,
            Some("open") => {
                if let Some(item) = &sel_item {
                    let r = match &item.kind {
                        Kind::Folder { target, .. } => shell_open(&paths::expand_env(target), ""),
                        _ => shell_open(&format!("shell:::{}", item.clsid), ""),
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

        let n = items.iter().filter(|i| i.shown).count();
        frame::status(ui, &format!("{} items, {n} shown", items.len()), |_| {});
        self.edit_bar(ui, ctx, sel_item.as_ref());
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            self.tree(ui, &items);
        });
    }

    /// The original's tree: "Navigation pane" with its items in pane order; disabled
    /// items are greyed.
    fn tree(&mut self, ui: &mut egui::Ui, items: &[NavItem]) {
        let id = ui.make_persistent_id("navpane-tree");
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
            .show_header(ui, |ui| {
                ui.label(egui::RichText::new(format!("{}  Navigation pane", icons::APP_WINDOW)).strong());
            })
            .body(|ui| {
                for item in pane_order(items) {
                    let icon = match item.kind {
                        Kind::Folder { .. } => icons::FOLDER_OPEN,
                        _ => icons::FOLDERS,
                    };
                    let mut text = egui::RichText::new(format!("{icon}  {}", item.name));
                    if !item.shown {
                        text = text.weak();
                    }
                    if item.missing() {
                        text = text.color(ui.visuals().error_fg_color);
                    }
                    let sel = self.selected.as_ref() == Some(&item.clsid);
                    let mut r = ui.selectable_label(sel, text);
                    let mut tip = item.type_label().to_owned();
                    if let Kind::Folder { target, .. } = &item.kind {
                        tip = format!("{tip}: {target}");
                    }
                    if !item.shown {
                        tip.push_str(" (disabled)");
                    }
                    if item.missing() {
                        tip.push_str(" — the folder no longer exists");
                    }
                    r = r.on_hover_text(tip);
                    if r.clicked() {
                        self.selected = Some(item.clsid.clone());
                    }
                    if r.double_clicked()
                        && let Kind::Folder { target, .. } = &item.kind
                    {
                        reveal_in_explorer(Path::new(&paths::expand_env(target)));
                    }
                }
            });
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
        if self.options {
            let mut back = false;
            egui::Panel::top("navpane-back")
                .resizable(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        back = ui
                            .button(format!("{}  Navigation pane", icons::ARROW_UP))
                            .clicked();
                        ui.label("›");
                        ui.strong("Options");
                    });
                });
            if back {
                self.options = false;
            } else {
                frame::nested(ui, |ui| self.panel.ui_framed(ui, ctx));
            }
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
