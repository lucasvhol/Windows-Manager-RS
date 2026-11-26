//! JumpList Quick Launcher: named jump lists of programs, files, folders, web
//! addresses, Start items, Windows tools and Store apps, organised in groups.
//!
//! The "Win Manager RS" list belongs to this app: pin the app to the taskbar and
//! right-click it. Every other list gets its own app identity and a launcher
//! shortcut in the Start menu carrying it; pin that launcher to get that list. Jump
//! lists keep working when this app is closed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{E_ACCESSDENIED, PROPERTYKEY, VARIANT_TRUE};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_AppUserModel_ID, PKEY_AppUserModel_IsDestListSeparator, PKEY_Title,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
use windows::Win32::System::Variant::{VT_BOOL, VT_LPWSTR};
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW, SHStrDupW, ShellLink,
};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    icons, paths, registry,
    safety::UndoAction,
    shell::{ShortcutSpec, ensure_com, make_link, read_shortcut},
    ui::{Confirm, Message, frame, show_message},
};

const TOOL: &str = "jumplist";
/// The list attached to this app itself.
pub const MAIN_LIST: &str = "Win Manager RS";
const ADVANCED: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub title: String,
    pub target: PathBuf,
    #[serde(default)]
    pub args: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// Empty = the standard "Tasks" section.
    pub name: String,
    #[serde(default)]
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct List {
    pub name: String,
    #[serde(default)]
    pub groups: Vec<Group>,
}

impl List {
    pub fn count(&self) -> usize {
        self.groups.iter().map(|g| g.items.len()).sum()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    list: Vec<List>,
    /// Format of earlier versions: one list of tasks.
    #[serde(default)]
    item: Vec<Item>,
}

fn store_path() -> Result<PathBuf> {
    Ok(paths::config_dir()?.join("jumplist.toml"))
}

pub fn load_from(path: &Path) -> Vec<List> {
    let saved = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<Saved>(&t).ok())
        .unwrap_or_default();
    let mut lists = saved.list;
    if lists.is_empty() {
        lists.push(List {
            name: MAIN_LIST.into(),
            groups: vec![Group {
                name: String::new(),
                items: saved.item,
            }],
        });
    }
    lists
}

pub fn save_to(path: &Path, lists: &[List]) -> Result<()> {
    std::fs::write(
        path,
        toml::to_string_pretty(&Saved {
            list: lists.to_vec(),
            item: Vec::new(),
        })?,
    )
    .context("saving jump lists")
}

/// App identity of a named list (the main list uses this app's own identity).
pub fn app_id(list: &str) -> Option<String> {
    if list == MAIN_LIST {
        return None;
    }
    let slug: String = list.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    Some(format!("WinManagerRS.JumpList.{slug}"))
}

pub fn launcher_dir() -> PathBuf {
    PathBuf::from(paths::expand_env(
        r"%APPDATA%\Microsoft\Windows\Start Menu\Programs\Win Manager RS Jump Lists",
    ))
}

fn launcher_path(list: &str) -> PathBuf {
    launcher_dir().join(format!("{list}.lnk"))
}

/// Programs run directly; everything else opens through Explorer (folders, files,
/// web addresses, Store apps).
pub fn spec_for(item: &Item) -> ShortcutSpec {
    let t = item.target.to_string_lossy();
    let is_program = item.target.extension().is_some_and(|e| {
        ["exe", "com", "bat", "cmd", "msc"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    }) && !t.contains("://");
    if is_program {
        ShortcutSpec {
            target: item.target.clone(),
            args: item.args.clone(),
            ..Default::default()
        }
    } else {
        let arg = if t.starts_with("shell:") {
            t.into_owned()
        } else {
            format!("\"{t}\"")
        };
        ShortcutSpec {
            target: PathBuf::from(paths::expand_env(r"%WINDIR%\explorer.exe")),
            args: arg,
            ..Default::default()
        }
    }
}

/// A CoTaskMem string PROPVARIANT (freed by PROPVARIANT's Drop).
fn string_prop(s: &str) -> Result<PROPVARIANT> {
    let text: PWSTR = unsafe { SHStrDupW(&HSTRING::from(s))? };
    Ok(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                Anonymous: PROPVARIANT_0_0_0 { pwszVal: text },
                ..Default::default()
            }),
        },
    })
}

fn set_prop(link: &IShellLinkW, key: &PROPERTYKEY, value: &str) -> Result<()> {
    let store: IPropertyStore = link.cast()?;
    unsafe {
        store.SetValue(key, &string_prop(value)?)?;
        store.Commit()?;
    }
    Ok(())
}

/// A link that Windows draws as a separator line in the Tasks section.
fn separator() -> Result<IShellLinkW> {
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    let value = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BOOL,
                Anonymous: PROPVARIANT_0_0_0 {
                    boolVal: VARIANT_TRUE,
                },
                ..Default::default()
            }),
        },
    };
    let store: IPropertyStore = link.cast()?;
    unsafe {
        store.SetValue(&PKEY_AppUserModel_IsDestListSeparator, &value)?;
        store.Commit()?;
    }
    Ok(link)
}

/// How a list ended up in the jump list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// How many items Windows shows per category (it may show fewer than were added).
    pub slots: u32,
    /// Named groups were refused (Windows allows custom categories only while
    /// "Show recently opened items" is on), so they went under Tasks, split by lines.
    pub as_tasks: bool,
}

/// Whether Windows lets jump lists show recent items, which custom categories need.
pub fn recent_items_on() -> bool {
    registry::read_raw(ADVANCED, "Start_TrackDocs")
        .and_then(|v| registry::decode_dword(&v))
        .is_none_or(|v| v != 0)
}

/// Fills a begun list. With `as_tasks`, named groups go under Tasks instead of
/// their own categories, each after a separator line.
unsafe fn fill(dest: &ICustomDestinationList, list: &List, as_tasks: bool) -> Result<()> {
    let groups = list.groups.iter().filter(|g| !g.items.is_empty());
    let tasks: IObjectCollection =
        unsafe { CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)? };
    let mut categories = Vec::new();
    for g in groups {
        let named = !g.name.trim().is_empty();
        let coll: IObjectCollection = if named && !as_tasks {
            unsafe { CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)? }
        } else {
            if named && unsafe { tasks.GetCount()? } > 0 {
                unsafe { tasks.AddObject(&separator()?)? };
            }
            tasks.clone()
        };
        for item in &g.items {
            let link = make_link(&spec_for(item))?;
            // Jump list entries take their caption from PKEY_Title.
            set_prop(&link, &PKEY_Title, &item.title)?;
            unsafe { coll.AddObject(&link)? };
        }
        if named && !as_tasks {
            categories.push((g.name.trim(), coll));
        }
    }
    // Categories first: Windows refuses them as a whole when recent items are off.
    for (name, coll) in categories {
        unsafe { dest.AppendCategory(&HSTRING::from(name), &coll.cast::<IObjectArray>()?)? };
    }
    if unsafe { tasks.GetCount()? } > 0 {
        unsafe { dest.AddUserTasks(&tasks.cast::<IObjectArray>()?)? };
    }
    Ok(())
}

/// Replaces a list's jump list (an empty list clears it).
pub fn apply(list: &List) -> Result<Applied> {
    ensure_com();
    unsafe {
        let dest: ICustomDestinationList = CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        if let Some(id) = app_id(&list.name) {
            dest.SetAppID(&HSTRING::from(id))?;
        }
        if list.count() == 0 {
            let _ = dest.DeleteList(match app_id(&list.name) {
                Some(id) => PCWSTR(HSTRING::from(id).as_ptr()),
                None => PCWSTR::null(),
            });
            return Ok(Applied {
                slots: 0,
                as_tasks: false,
            });
        }
        let mut slots = 0u32;
        let _removed: IObjectArray = dest.BeginList(&mut slots)?;
        let mut as_tasks = false;
        if let Err(e) = fill(&dest, list, false) {
            let refused = e
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|e| e.code() == E_ACCESSDENIED);
            let _ = dest.AbortList();
            if !refused {
                return Err(e);
            }
            as_tasks = true;
            let _removed: IObjectArray = dest.BeginList(&mut slots)?;
            fill(&dest, list, true)?;
        }
        dest.CommitList()?;
        Ok(Applied { slots, as_tasks })
    }
}

/// Creates the Start menu launcher a named list is pinned from. Returns its path.
pub fn create_launcher(list: &str) -> Result<PathBuf> {
    let id = app_id(list).context("the main list needs no launcher")?;
    std::fs::create_dir_all(launcher_dir())?;
    let lnk = launcher_path(list);
    let exe = std::env::current_exe()?;
    let link = make_link(&ShortcutSpec {
        target: exe,
        args: "--open jumplist".into(),
        description: format!("Jump list \"{list}\""),
        ..Default::default()
    })?;
    set_prop(&link, &PKEY_AppUserModel_ID, &id)?;
    unsafe {
        link.cast::<IPersistFile>()?
            .Save(&HSTRING::from(lnk.as_os_str()), true)?
    };
    Ok(lnk)
}

/// Windows' limit on recent items shown in jump lists (the "maximum count").
pub fn max_items() -> u32 {
    registry::read_raw(ADVANCED, "JumpListItems_Maximum")
        .and_then(|v| registry::decode_dword(&v))
        .unwrap_or(10)
}

pub fn set_max_items(ctx: &AppCtx<'_>, n: u32) -> Result<()> {
    let snap = registry::snapshot(ADVANCED, "JumpListItems_Maximum");
    registry::write_raw(ADVANCED, "JumpListItems_Maximum", &registry::dword(n))?;
    ctx.journal.record(
        TOOL,
        format!("Jump lists now show up to {n} recent items"),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

/// What a new item opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Path,
    Url,
    StartItem,
    Utility,
    App,
}

struct Editor {
    /// (list, group, item) being edited, or None for a new item.
    at: Option<(usize, usize, usize)>,
    group: usize,
    source: Source,
    title: String,
    target: String,
    args: String,
    utility: usize,
    app: Option<usize>,
}

enum Ask {
    DeleteList(usize),
    Restore(PathBuf),
}

#[derive(Default)]
pub struct JumpListLauncher {
    lists: Vec<List>,
    loaded: bool,
    list: usize,
    selected: Option<(usize, usize)>,
    /// The Edit group's Title, Path and Parameter for the selected item.
    edit: Option<(String, String, String)>,
    editor: Option<Editor>,
    new_list: String,
    new_group: String,
    apps: Vec<(String, String)>,
    apps_task: Option<wm_core::task::BackgroundTask<Vec<(String, String)>>>,
    confirm: Confirm<Ask>,
    message: Message,
}

impl JumpListLauncher {
    fn save_and_apply(&mut self, ctx: &AppCtx<'_>, what: String) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let list = self.lists[self.list].clone();
        let r = store_path()
            .and_then(|p| save_to(&p, &self.lists))
            .and_then(|()| apply(&list));
        let pin = if list.name == MAIN_LIST {
            "Pin Win Manager RS to the taskbar and right-click it to use the list.".to_owned()
        } else {
            format!(
                "Pin \"{}\" from Start (Win Manager RS Jump Lists) to use the list.",
                list.name
            )
        };
        self.message = Some(
            r.map(|Applied { slots, as_tasks }| {
                let _ = ctx.journal.record(TOOL, what.clone(), UndoAction::None);
                let more = if as_tasks {
                    " Groups appear under Tasks, separated by lines: Windows shows group headings \n                     only while \"Show recently opened items\" is on (Settings > Personalization > \n                     Start).".to_owned()
                } else if slots > 0
                    && list
                        .groups
                        .iter()
                        .any(|g| !g.name.is_empty() && g.items.len() > slots as usize)
                {
                    format!(" Windows shows at most {slots} items per group.")
                } else {
                    String::new()
                };
                format!("{what}. {pin}{more}")
            })
            .map_err(|e| format!("{e:#}")),
        );
    }

    fn editor_window(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.apps_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.apps = r.unwrap_or_default();
                self.apps_task = None;
            }
        }
        let groups: Vec<String> = self.lists[self.list]
            .groups
            .iter()
            .map(|g| {
                if g.name.is_empty() {
                    "Tasks".into()
                } else {
                    g.name.clone()
                }
            })
            .collect();
        let Some(ed) = &mut self.editor else { return };
        let mut result = None;
        let mut want_apps = false;
        egui::Window::new(if ed.at.is_some() { "Edit item" } else { "Add item" })
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                if ed.at.is_none() {
                    ui.horizontal_wrapped(|ui| {
                        for (s, l) in [
                            (Source::Path, "Program, file or folder"),
                            (Source::Url, "Web address"),
                            (Source::StartItem, "Start item"),
                            (Source::Utility, "Windows tool"),
                            (Source::App, "Store app"),
                        ] {
                            if ui.radio_value(&mut ed.source, s, l).changed() && s == Source::App {
                                want_apps = true;
                            }
                        }
                    });
                }
                egui::Grid::new("jl-edit")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        match ed.source {
                            Source::Path | Source::Url => {
                                ui.label("Opens");
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(&mut ed.target).desired_width(260.0));
                                    if ed.source == Source::Path {
                                        let mut picked = None;
                                        if ui.button("File…").clicked() {
                                            picked = rfd::FileDialog::new().pick_file();
                                        }
                                        if ui.button("Folder…").clicked() {
                                            picked = rfd::FileDialog::new().pick_folder();
                                        }
                                        if let Some(p) = picked {
                                            ed.target = p.display().to_string();
                                            if ed.title.is_empty() {
                                                ed.title = p
                                                    .file_stem()
                                                    .or(p.file_name())
                                                    .map(|s| s.to_string_lossy().into_owned())
                                                    .unwrap_or_default();
                                            }
                                        }
                                    }
                                });
                                ui.end_row();
                                if ed.source == Source::Path {
                                    ui.label("Arguments");
                                    ui.text_edit_singleline(&mut ed.args);
                                    ui.end_row();
                                }
                            }
                            Source::StartItem => {
                                ui.label("Start item");
                                ui.horizontal(|ui| {
                                    ui.label(&ed.target);
                                    if ui.button("Choose…").clicked()
                                        && let Some(p) = rfd::FileDialog::new()
                                            .set_directory(paths::expand_env(
                                                r"%ProgramData%\Microsoft\Windows\Start Menu\Programs",
                                            ))
                                            .add_filter("Shortcuts", &["lnk"])
                                            .pick_file()
                                        && let Ok(s) = read_shortcut(&p)
                                    {
                                        ed.target = s.target.display().to_string();
                                        ed.args = s.args;
                                        ed.title = p
                                            .file_stem()
                                            .map(|s| s.to_string_lossy().into_owned())
                                            .unwrap_or_default();
                                    }
                                });
                                ui.end_row();
                            }
                            Source::Utility => {
                                ui.label("Tool");
                                let list = crate::hotkeys::utilities();
                                egui::ComboBox::from_id_salt("jl-util")
                                    .width(280.0)
                                    .selected_text(list[ed.utility].0)
                                    .show_ui(ui, |ui| {
                                        for (i, (n, _, _)) in list.iter().enumerate() {
                                            if ui.selectable_value(&mut ed.utility, i, *n).clicked() {
                                                ed.title = (*n).to_owned();
                                            }
                                        }
                                    });
                                ui.end_row();
                            }
                            Source::App => {
                                ui.label("App");
                                if self.apps.is_empty() {
                                    ui.spinner();
                                } else {
                                    egui::ComboBox::from_id_salt("jl-app")
                                        .width(280.0)
                                        .selected_text(
                                            ed.app.map(|i| self.apps[i].0.clone()).unwrap_or_default(),
                                        )
                                        .show_ui(ui, |ui| {
                                            for (i, (n, _)) in self.apps.iter().enumerate() {
                                                if ui.selectable_label(ed.app == Some(i), n).clicked() {
                                                    ed.app = Some(i);
                                                    ed.title = n.clone();
                                                }
                                            }
                                        });
                                }
                                ui.end_row();
                            }
                        }
                        ui.label("Title");
                        ui.text_edit_singleline(&mut ed.title);
                        ui.end_row();
                        ui.label("Group");
                        egui::ComboBox::from_id_salt("jl-group")
                            .selected_text(groups.get(ed.group).cloned().unwrap_or_default())
                            .show_ui(ui, |ui| {
                                for (i, g) in groups.iter().enumerate() {
                                    ui.selectable_value(&mut ed.group, i, g);
                                }
                            });
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        result = Some(true);
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        result = Some(false);
                    }
                });
            });
        if want_apps && self.apps.is_empty() && self.apps_task.is_none() {
            self.apps_task = Some(wm_core::task::BackgroundTask::spawn(ui.ctx(), |_| {
                crate::hotkeys::store_apps()
            }));
        }
        match result {
            Some(true) => {
                let ed = self.editor.take().expect("open");
                let (target, args) = match ed.source {
                    Source::Utility => {
                        let (_, exe, a) = crate::hotkeys::utilities()[ed.utility];
                        (paths::expand_env(exe), a.to_owned())
                    }
                    Source::App => match ed.app.and_then(|i| self.apps.get(i)) {
                        Some((_, id)) => (format!(r"shell:AppsFolder\{id}"), String::new()),
                        None => (String::new(), String::new()),
                    },
                    Source::Url => (ed.target.trim().to_owned(), String::new()),
                    _ => (
                        ed.target.trim().trim_matches('"').to_owned(),
                        ed.args.trim().to_owned(),
                    ),
                };
                if target.is_empty() || ed.title.trim().is_empty() {
                    self.message = Some(Err("Choose what the item opens and give it a title.".into()));
                    return;
                }
                let item = Item {
                    title: ed.title.trim().to_owned(),
                    target: PathBuf::from(target),
                    args,
                };
                let list = &mut self.lists[self.list];
                if let Some((_, g, i)) = ed.at {
                    list.groups[g].items.remove(i);
                }
                let g = ed.group.min(list.groups.len().saturating_sub(1));
                list.groups[g].items.push(item.clone());
                self.selected = Some((g, list.groups[g].items.len() - 1));
                let verb = if ed.at.is_some() { "Updated" } else { "Added" };
                self.save_and_apply(ctx, format!("{verb} \"{}\" in the jump list", item.title));
            }
            Some(false) => self.editor = None,
            None => {}
        }
    }
}

impl Tool for JumpListLauncher {
    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn id(&self) -> &'static str {
        "jumplist"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if !self.loaded {
            self.lists = store_path().map(|p| load_from(&p)).unwrap_or_default();
            if self.lists.is_empty() {
                self.lists.push(List {
                    name: MAIN_LIST.into(),
                    groups: vec![Group::default()],
                });
            }
            self.loaded = true;
        }
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            match ask {
                Ask::DeleteList(i) => {
                    let removed = self.lists.remove(i);
                    if !ctx.config.dry_run {
                        let _ = apply(&List {
                            name: removed.name.clone(),
                            groups: Vec::new(),
                        });
                        let _ = ctx.journal.recycle(
                            TOOL,
                            format!("Removed the jump list launcher \"{}\"", removed.name),
                            vec![launcher_path(&removed.name)],
                        );
                    }
                    self.list = 0;
                    self.save_and_apply(ctx, format!("Deleted the list \"{}\"", removed.name));
                }
                Ask::Restore(f) => {
                    match std::fs::read_to_string(&f)
                        .map_err(anyhow::Error::from)
                        .and_then(|t| {
                            toml::from_str::<Saved>(&t)
                                .context("not a jump list backup")
                                .map(|s| s.list)
                        }) {
                        Ok(lists) if !lists.is_empty() => {
                            self.lists = lists;
                            self.list = 0;
                            if !ctx.config.dry_run {
                                for l in &self.lists {
                                    let _ = apply(l);
                                    if app_id(&l.name).is_some() {
                                        let _ = create_launcher(&l.name);
                                    }
                                }
                            }
                            self.save_and_apply(ctx, format!("Restored {} jump lists", self.lists.len()));
                        }
                        Ok(_) => self.message = Some(Err("The backup contains no lists.".into())),
                        Err(e) => self.message = Some(Err(format!("{e:#}"))),
                    }
                }
            }
        }
        self.editor_window(ui, ctx);

        let sel_item = self
            .selected
            .and_then(|(g, i)| self.lists[self.list].groups.get(g)?.items.get(i).cloned());
        let groups_len = self.lists[self.list].groups.len();
        let target_group = self
            .selected
            .map_or(0, |(g, _)| g)
            .min(groups_len.saturating_sub(1));
        let new_editor = |source: Source| Editor {
            at: None,
            group: target_group,
            source,
            title: String::new(),
            target: String::new(),
            args: String::new(),
            utility: 0,
            app: None,
        };

        // The original's top row: the list, Create, Delete, Backup, Restore, Exit.
        let mut top = None;
        frame::options(ui, |ui| {
            let mut chosen = self.list;
            egui::ComboBox::from_id_salt("jl-list")
                .selected_text(self.lists[self.list].name.clone())
                .width(180.0)
                .show_ui(ui, |ui| {
                    for (i, l) in self.lists.iter().enumerate() {
                        ui.selectable_value(&mut chosen, i, format!("{} ({})", l.name, l.count()));
                    }
                });
            if chosen != self.list {
                self.list = chosen;
                self.selected = None;
                self.edit = None;
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.new_list)
                    .hint_text("New list name")
                    .desired_width(130.0),
            );
            let name = self.new_list.trim().to_owned();
            let ok = !name.is_empty()
                && !name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
                && !self.lists.iter().any(|l| l.name.eq_ignore_ascii_case(&name));
            if ui
                .add_enabled(ok, egui::Button::new(format!("{}  Create", icons::PLUS)))
                .on_hover_text("A new list gets its own launcher in Start, which you can pin")
                .clicked()
            {
                top = Some("create");
            }
            if ui
                .add_enabled(
                    self.lists[self.list].name != MAIN_LIST,
                    egui::Button::new(format!("{}  Delete", icons::X)),
                )
                .clicked()
            {
                top = Some("delete");
            }
            ui.separator();
            if ui.button(format!("{}  Backup", icons::FLOPPY_DISK)).clicked() {
                top = Some("backup");
            }
            if ui
                .button(format!("{}  Restore", icons::CLOCK_COUNTER_CLOCKWISE))
                .clicked()
            {
                top = Some("restore");
            }
            ui.separator();
            if ui.button(format!("{}  Exit", icons::SIGN_OUT)).clicked() {
                top = Some("exit");
            }
        });
        match top {
            Some("create") => {
                let name = self.new_list.trim().to_owned();
                let r = if ctx.config.dry_run {
                    Ok(PathBuf::new())
                } else {
                    create_launcher(&name)
                };
                match r {
                    Ok(_) => {
                        self.lists.push(List {
                            name: name.clone(),
                            groups: vec![Group::default()],
                        });
                        self.list = self.lists.len() - 1;
                        self.selected = None;
                        self.new_list.clear();
                        self.save_and_apply(ctx, format!("Added the list \"{name}\""));
                    }
                    Err(e) => self.message = Some(Err(format!("{e:#}"))),
                }
            }
            Some("delete") => self.confirm.ask(
                format!(
                    "Delete the list \"{}\" and its Start menu launcher?",
                    self.lists[self.list].name
                ),
                Ask::DeleteList(self.list),
            ),
            Some("backup") => {
                if let Some(f) = rfd::FileDialog::new()
                    .set_file_name("Jump lists.toml")
                    .add_filter("Jump lists", &["toml"])
                    .save_file()
                {
                    self.message = Some(
                        save_to(&f, &self.lists)
                            .map(|_| format!("Saved to {}.", f.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some("restore") => {
                if let Some(f) = rfd::FileDialog::new()
                    .add_filter("Jump lists", &["toml"])
                    .pick_file()
                {
                    self.confirm.ask(
                        format!("Replace your jump lists with the ones in {}?", f.display()),
                        Ask::Restore(f),
                    );
                }
            }
            Some("exit") => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }

        // The original's action column.
        let mut act = None;
        let can_up = self.selected.is_some_and(|(_, i)| i > 0);
        let can_down = self
            .selected
            .is_some_and(|(g, i)| i + 1 < self.lists[self.list].groups[g].items.len());
        let count = self.lists[self.list].count();
        frame::right(ui, 200.0, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_group)
                    .hint_text("New group name")
                    .desired_width(ui.available_width()),
            );
            let g = self.new_group.trim().to_owned();
            let ok = !g.is_empty()
                && !self.lists[self.list]
                    .groups
                    .iter()
                    .any(|x| x.name.eq_ignore_ascii_case(&g));
            if frame::action(ui, icons::PLUS, "Add Group", ok) {
                act = Some("group");
            }
            if frame::action(ui, icons::PLUS, "Add File", true) {
                act = Some("file");
            }
            if frame::action(ui, icons::PLUS, "Add Folder", true) {
                act = Some("folder");
            }
            if frame::action(ui, icons::PLUS, "Add URL", true) {
                act = Some("url");
            }
            if frame::action(ui, icons::PLUS, "Add Start Menu", true) {
                act = Some("start");
            }
            if frame::action(ui, icons::PLUS, "Add Utility", true) {
                act = Some("utility");
            }
            if frame::action(ui, icons::PLUS, "Add Windows app", true) {
                act = Some("app");
            }
            ui.separator();
            if frame::action(ui, icons::ARROW_UP, "Up", can_up) {
                act = Some("up");
            }
            if frame::action(ui, icons::ARROW_DOWN, "Down", can_down) {
                act = Some("down");
            }
            ui.separator();
            if frame::action(ui, icons::X, "Remove", sel_item.is_some()) {
                act = Some("remove");
            }
            if frame::action(ui, icons::X, "Remove All", count > 0) {
                act = Some("removeall");
            }
        });
        match act {
            Some("group") => {
                let g = self.new_group.trim().to_owned();
                self.lists[self.list].groups.push(Group {
                    name: g.clone(),
                    items: Vec::new(),
                });
                self.new_group.clear();
                self.save_and_apply(ctx, format!("Added the group \"{g}\""));
            }
            Some(kind @ ("file" | "folder")) => {
                let picked = if kind == "file" {
                    rfd::FileDialog::new().pick_file()
                } else {
                    rfd::FileDialog::new().pick_folder()
                };
                if let Some(p) = picked {
                    let item = Item {
                        title: p
                            .file_stem()
                            .or(p.file_name())
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.display().to_string()),
                        target: p,
                        args: String::new(),
                    };
                    let list = &mut self.lists[self.list];
                    list.groups[target_group].items.push(item.clone());
                    self.selected = Some((target_group, list.groups[target_group].items.len() - 1));
                    self.edit = None;
                    self.save_and_apply(ctx, format!("Added \"{}\" to the jump list", item.title));
                }
            }
            Some("url") => self.editor = Some(new_editor(Source::Url)),
            Some("start") => self.editor = Some(new_editor(Source::StartItem)),
            Some("utility") => {
                let mut e = new_editor(Source::Utility);
                e.title = crate::hotkeys::utilities()[0].0.to_owned();
                self.editor = Some(e);
            }
            Some("app") => {
                self.editor = Some(new_editor(Source::App));
                if self.apps.is_empty() && self.apps_task.is_none() {
                    self.apps_task = Some(wm_core::task::BackgroundTask::spawn(ui.ctx(), |_| {
                        crate::hotkeys::store_apps()
                    }));
                }
            }
            Some(dir @ ("up" | "down")) => {
                if let Some((g, i)) = self.selected {
                    let j = if dir == "up" { i - 1 } else { i + 1 };
                    self.lists[self.list].groups[g].items.swap(i, j);
                    self.selected = Some((g, j));
                    self.edit = None;
                    self.save_and_apply(ctx, "Reordered the jump list".into());
                }
            }
            Some("remove") => {
                if let Some((g, i)) = self.selected.take() {
                    let removed = self.lists[self.list].groups[g].items.remove(i);
                    self.edit = None;
                    self.save_and_apply(ctx, format!("Removed \"{}\" from the jump list", removed.title));
                }
            }
            Some("removeall") => {
                for g in &mut self.lists[self.list].groups {
                    g.items.clear();
                }
                self.selected = None;
                self.edit = None;
                self.save_and_apply(ctx, "Removed every item from the jump list".into());
            }
            _ => {}
        }

        // Status bar: the list's name and count, and the original's maximum.
        let hidden_headings =
            self.lists[self.list].groups.iter().any(|g| !g.name.is_empty()) && !recent_items_on();
        let name = self.lists[self.list].name.clone();
        let mut max_changed = None;
        frame::status(ui, &format!("Name: {name}  |  Count: {count}"), |ui| {
            let mut max = max_items();
            if ui
                .add(egui::DragValue::new(&mut max).range(0..=60))
                .on_hover_text("How many recent items jump lists show")
                .changed()
            {
                max_changed = Some(max);
            }
            ui.weak("Maximum allowable number");
            if hidden_headings {
                ui.colored_label(ui.visuals().warn_fg_color, "Group headings are hidden")
                    .on_hover_text(
                        "\"Show recently opened items\" is off (Settings > Personalization > Start), so \
                         Windows hides group headings: groups appear under Tasks, separated by lines.",
                    );
            }
        });
        if let Some(max) = max_changed {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                set_max_items(ctx, max)
                    .map(|_| "Saved.".to_owned())
                    .map_err(|e| format!("{e:#}"))
            });
        }
        self.edit_bar(ui, ctx, sel_item.as_ref());

        let mut click = None;
        let mut remove_group = None;
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            ui.strong("Title");
            ui.separator();
            for (g, group) in self.lists[self.list].groups.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  {}",
                            icons::FOLDERS,
                            if group.name.is_empty() {
                                "Tasks"
                            } else {
                                &group.name
                            }
                        ))
                        .strong(),
                    );
                    ui.weak(format!("{} items", group.items.len()));
                    if !group.name.is_empty() && ui.small_button("Remove group").clicked() {
                        remove_group = Some(g);
                    }
                });
                for (i, it) in group.items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add_space(22.0);
                        let r = ui
                            .selectable_label(self.selected == Some((g, i)), &it.title)
                            .on_hover_text(format!("{} {}", it.target.display(), it.args).trim().to_owned());
                        if r.clicked() {
                            click = Some((g, i));
                        }
                        if r.double_clicked() {
                            let s = spec_for(it);
                            let _ = wm_core::shell::shell_open(&s.target.display().to_string(), &s.args);
                        }
                    });
                }
                ui.add_space(6.0);
            }
        });
        if click.is_some() && click != self.selected {
            self.selected = click;
            self.edit = None;
        }
        if let Some(g) = remove_group {
            let removed = self.lists[self.list].groups.remove(g);
            self.selected = None;
            self.edit = None;
            self.save_and_apply(ctx, format!("Removed the group \"{}\"", removed.name));
        }
    }
}

impl JumpListLauncher {
    /// The original's Edit group: Title, Parameter and Path of the selected item.
    fn edit_bar(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>, sel: Option<&Item>) {
        if self.edit.is_none()
            && let Some(it) = sel
        {
            self.edit = Some((it.title.clone(), it.target.display().to_string(), it.args.clone()));
        }
        let mut save = false;
        frame::edit_bar(ui, |ui| {
            let enabled = sel.is_some();
            let mut empty = (String::new(), String::new(), String::new());
            let (title, path, args) = self.edit.as_mut().filter(|_| enabled).unwrap_or(&mut empty);
            let w = ((ui.available_width() - 200.0) / 2.0).max(160.0);
            ui.add_enabled_ui(enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized([64.0, 20.0], egui::Label::new("Title"));
                    ui.add(egui::TextEdit::singleline(title).desired_width(w));
                    ui.add_sized([70.0, 20.0], egui::Label::new("Parameter"));
                    ui.add(egui::TextEdit::singleline(args).desired_width(w));
                });
                ui.horizontal(|ui| {
                    ui.add_sized([64.0, 20.0], egui::Label::new("Path"));
                    ui.add(egui::TextEdit::singleline(path).desired_width(w * 2.0 + 78.0));
                    if ui.button(icons::FOLDER_OPEN).on_hover_text("Browse").clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        *path = p.display().to_string();
                    }
                });
                save = ui
                    .add_enabled(
                        !title.trim().is_empty() && !path.trim().is_empty(),
                        egui::Button::new(format!("{}  Save", icons::FLOPPY_DISK)),
                    )
                    .clicked();
            })
            .response
            .on_disabled_hover_text("Select an item to edit it");
        });
        if save
            && let (Some((g, i)), Some((title, path, args))) = (self.selected, self.edit.take())
            && let Some(it) = self.lists[self.list]
                .groups
                .get_mut(g)
                .and_then(|grp| grp.items.get_mut(i))
        {
            it.title = title.trim().to_owned();
            it.target = PathBuf::from(path.trim().trim_matches('"'));
            it.args = args.trim().to_owned();
            let what = format!("Updated \"{}\" in the jump list", it.title);
            self.save_and_apply(ctx, what);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<List> {
        vec![
            List {
                name: MAIN_LIST.into(),
                groups: vec![Group {
                    name: String::new(),
                    items: vec![Item {
                        title: "Notepad".into(),
                        target: r"C:\Windows\notepad.exe".into(),
                        args: String::new(),
                    }],
                }],
            },
            List {
                name: "Work".into(),
                groups: vec![
                    Group::default(),
                    Group {
                        name: "Sites".into(),
                        items: vec![Item {
                            title: "Example".into(),
                            target: "https://example.com".into(),
                            args: String::new(),
                        }],
                    },
                ],
            },
        ]
    }

    #[test]
    fn persistence_roundtrip_and_old_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.toml");
        assert_eq!(load_from(&path).len(), 1, "a missing file gives the main list");
        save_to(&path, &sample()).unwrap();
        assert_eq!(load_from(&path), sample());
        std::fs::write(&path, "[[item]]\ntitle = \"Old\"\ntarget = 'C:\\x.exe'\n").unwrap();
        let old = load_from(&path);
        assert_eq!(old[0].name, MAIN_LIST);
        assert_eq!(old[0].groups[0].items[0].title, "Old");
    }

    #[test]
    fn specs_and_ids() {
        let s = spec_for(&Item {
            title: "T".into(),
            target: r"C:\Temp".into(),
            args: String::new(),
        });
        assert!(s.target.ends_with("explorer.exe"));
        assert_eq!(s.args, "\"C:\\Temp\"");
        let p = spec_for(&Item {
            title: "N".into(),
            target: r"C:\Windows\notepad.exe".into(),
            args: "x".into(),
        });
        assert_eq!(p.args, "x");
        let a = spec_for(&Item {
            title: "A".into(),
            target: r"shell:AppsFolder\X!App".into(),
            args: String::new(),
        });
        assert_eq!(a.args, r"shell:AppsFolder\X!App");
        assert_eq!(app_id(MAIN_LIST), None);
        assert_eq!(
            app_id("My Work!").as_deref(),
            Some("WinManagerRS.JumpList.MyWork")
        );
    }

    #[test]
    fn apply_and_clear_lists_with_groups() {
        // The main list belongs to the test executable; the named list to its own id.
        for l in sample() {
            let named = l.groups.iter().any(|g| !g.name.is_empty() && !g.items.is_empty());
            let a = apply(&l).unwrap();
            assert_eq!(a.as_tasks, named && !recent_items_on(), "{}", l.name);
            apply(&List {
                name: l.name.clone(),
                groups: Vec::new(),
            })
            .unwrap();
        }
    }

    #[test]
    fn launcher_carries_the_list_identity() {
        let name = format!("WM Test {}", std::process::id());
        let lnk = create_launcher(&name).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
                let _ = std::fs::remove_dir(launcher_dir());
            }
        }
        let _c = Cleanup(lnk.clone());
        let s = read_shortcut(&lnk).unwrap();
        assert_eq!(s.args, "--open jumplist");
        unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&windows::Win32::UI::Shell::ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.cast::<IPersistFile>()
                .unwrap()
                .Load(
                    &HSTRING::from(lnk.as_os_str()),
                    windows::Win32::System::Com::STGM_READ,
                )
                .unwrap();
            let store: IPropertyStore = link.cast().unwrap();
            let v = store.GetValue(&PKEY_AppUserModel_ID).unwrap();
            assert_eq!(v.to_string(), app_id(&name).unwrap());
        }
    }
}
