//! Run Shortcut Creator: short names for the Run dialog (Win+R) and the address bar.
//!
//! Plain programs use the documented App Paths keys (per user, or for all users).
//! Folders, files, web addresses, programs with arguments or a window state use a
//! shortcut named after the alias in a folder of this app that is added to the
//! user's PATH (the Run dialog also finds `.lnk` files on the PATH).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Vec2},
    icons,
    paths::{self, definitely_missing},
    process::{hidden_command, run_checked},
    registry,
    safety::UndoAction,
    shell::{ShortcutSpec, create_shortcut, read_shortcut, shell_open},
    ui::{Confirm, Message, ToolButton, frame, show_message},
};

const TOOL: &str = "runshortcuts";
const USER_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths";
const MACHINE_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths";
const ENV_KEY: &str = r"HKCU\Environment";

/// Window states offered for shortcuts: (label, show command).
pub const WINDOW: [(&str, i32); 3] = [("Normal", 0), ("Maximized", 3), ("Minimized", 7)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    AppPath,
    Shortcut,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub alias: String,
    pub kind: Kind,
    /// Registry key (App Paths) or shortcut file.
    pub location: String,
    pub target: String,
    pub args: String,
    pub show_cmd: i32,
    pub machine: bool,
    pub broken: bool,
}

impl Alias {
    pub fn what(&self) -> &'static str {
        describe(&self.target)
    }
}

/// "Web address", "Folder", "Program" or "File".
pub fn describe(target: &str) -> &'static str {
    let t = target.trim().trim_matches('"');
    if t.is_empty() {
        "—"
    } else if t.contains("://") {
        "Web address"
    } else if Path::new(&paths::expand_env(t)).is_dir() {
        "Folder"
    } else if Path::new(t).extension().is_some_and(|e| {
        ["exe", "com", "bat", "cmd", "msc"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    }) {
        "Program"
    } else {
        "File"
    }
}

pub fn shortcut_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(paths::expand_env(
        r"%LOCALAPPDATA%\Win Manager RS\Run Shortcuts",
    )))
}

pub fn enumerate() -> Vec<Alias> {
    let mut out = Vec::new();
    for (root, machine) in [(USER_KEY, false), (MACHINE_KEY, true)] {
        let Ok(k) = registry::open_read(root) else {
            continue;
        };
        for sub in k.enum_keys().flatten() {
            let key = format!(r"{root}\{sub}");
            let target = registry::read_raw(&key, "")
                .map(|v| registry::decode_string(&v))
                .unwrap_or_default();
            let alias = sub
                .strip_suffix(".exe")
                .or_else(|| sub.strip_suffix(".EXE"))
                .unwrap_or(&sub)
                .to_owned();
            out.push(Alias {
                alias,
                kind: Kind::AppPath,
                location: key,
                broken: definitely_missing(&target),
                target,
                args: String::new(),
                show_cmd: 0,
                machine,
            });
        }
    }
    if let Ok(dir) = shortcut_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for e in entries.flatten() {
            let p = e.path();
            if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
                continue;
            }
            let Ok(s) = read_shortcut(&p) else { continue };
            // Web addresses open through Explorer: show the address itself.
            let via_explorer = s
                .target
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("explorer.exe"));
            let (target, args) = if via_explorer {
                (s.args.trim().trim_matches('"').to_owned(), String::new())
            } else {
                (s.target.display().to_string(), s.args)
            };
            out.push(Alias {
                alias: p
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                kind: Kind::Shortcut,
                location: p.display().to_string(),
                broken: !target.contains("://") && definitely_missing(&target),
                target,
                args,
                show_cmd: s.show_cmd,
                machine: false,
            });
        }
    }
    out.sort_by_key(|a| a.alias.to_lowercase());
    out
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty() || alias.len() > 40 {
        bail!("The name must be 1–40 characters.");
    }
    if !alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Use only letters, digits, - and _.");
    }
    Ok(())
}

/// A command the Run dialog would find for this name already (App Paths or a file
/// on the PATH), which a new alias would collide with.
pub fn existing_command(alias: &str) -> Option<String> {
    for root in [USER_KEY, MACHINE_KEY] {
        if registry::open_read(&format!(r"{root}\{alias}.exe")).is_ok() {
            return Some(format!(r"{root}\{alias}.exe"));
        }
    }
    let path = std::env::var("PATH").unwrap_or_default() + ";" + &user_path().0;
    for dir in path.split(';').map(str::trim).filter(|d| !d.is_empty()) {
        for ext in ["com", "exe", "bat", "cmd", "lnk"] {
            let f = Path::new(&paths::expand_env(dir)).join(format!("{alias}.{ext}"));
            if f.is_file() {
                return Some(f.display().to_string());
            }
        }
    }
    None
}

/// The per-user PATH and whether it is stored as an expandable string.
fn user_path() -> (String, bool) {
    match registry::read_raw(ENV_KEY, "Path") {
        Some(v) => (registry::decode_string(&v), v.vtype == 2),
        None => (String::new(), true),
    }
}

fn expand_string(s: &str) -> registry::RawValue {
    registry::RawValue {
        vtype: 2, // REG_EXPAND_SZ
        bytes: s
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect(),
    }
}

/// Tells Explorer (and so the Run dialog) that the environment changed.
fn broadcast_environment() {
    let text: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(text.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2000,
            None,
        );
    }
}

/// `path` with `dir` appended unless it is already listed.
pub fn path_with(path: &str, dir: &str) -> Option<String> {
    let present = path.split(';').any(|p| {
        paths::expand_env(p.trim())
            .trim_end_matches('\\')
            .eq_ignore_ascii_case(dir.trim_end_matches('\\'))
    });
    if present {
        return None;
    }
    let base = path.trim_end_matches(';');
    Some(if base.is_empty() {
        dir.to_owned()
    } else {
        format!("{base};{dir}")
    })
}

/// Makes sure the shortcut folder exists and is on the user's PATH.
fn ensure_on_path(ctx: &AppCtx<'_>) -> Result<PathBuf> {
    let dir = shortcut_dir()?;
    std::fs::create_dir_all(&dir)?;
    let (path, expandable) = user_path();
    if let Some(new) = path_with(&path, &dir.display().to_string()) {
        let snap = registry::snapshot(ENV_KEY, "Path");
        let value = if expandable {
            expand_string(&new)
        } else {
            registry::string(&new)
        };
        registry::write_raw(ENV_KEY, "Path", &value)?;
        ctx.journal.record(
            TOOL,
            format!("Added {} to your PATH for Run shortcuts", dir.display()),
            UndoAction::RestoreRegValues { values: vec![snap] },
        )?;
        broadcast_environment();
    }
    Ok(dir)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewAlias {
    pub alias: String,
    pub target: String,
    pub args: String,
    pub show_cmd: i32,
    pub all_users: bool,
}

/// Plain programs go to App Paths; everything else becomes a PATH shortcut.
pub fn uses_app_paths(a: &NewAlias) -> bool {
    describe(&a.target) == "Program" && a.args.trim().is_empty() && a.show_cmd == 0
}

pub fn add(ctx: &AppCtx<'_>, a: &NewAlias) -> Result<String> {
    let alias = a.alias.trim();
    validate_alias(alias)?;
    let target = a.target.trim().trim_matches('"');
    if target.is_empty() {
        bail!("Choose what the name opens");
    }
    if !target.contains("://") && !Path::new(&paths::expand_env(target)).exists() {
        bail!("{target} does not exist");
    }
    if let Some(c) = existing_command(alias) {
        bail!("\"{alias}\" is already a command ({c})");
    }
    if uses_app_paths(a) {
        if a.all_users && !ctx.elevated {
            bail!("Adding a name for all users needs administrator rights");
        }
        let key = format!(
            r"{}\{alias}.exe",
            if a.all_users { MACHINE_KEY } else { USER_KEY }
        );
        let undo = ctx.journal.backup_reg_key(&key)?;
        registry::write_raw(&key, "", &registry::string(target))?;
        if let Some(dir) = Path::new(target).parent() {
            registry::write_raw(&key, "Path", &registry::string(&dir.display().to_string()))?;
        }
        ctx.journal
            .record(TOOL, format!("Added Run name \"{alias}\" → {target}"), undo)?;
        return Ok(key);
    }
    if a.all_users {
        bail!("Only a program without arguments can be added for all users");
    }
    let dir = ensure_on_path(ctx)?;
    let lnk = dir.join(format!("{alias}.lnk"));
    let spec = if target.contains("://") {
        ShortcutSpec {
            target: PathBuf::from(paths::expand_env(r"%SystemRoot%\explorer.exe")),
            args: format!("\"{target}\""),
            show_cmd: a.show_cmd,
            ..Default::default()
        }
    } else {
        ShortcutSpec {
            target: PathBuf::from(paths::expand_env(target)),
            args: a.args.trim().to_owned(),
            show_cmd: a.show_cmd,
            ..Default::default()
        }
    };
    create_shortcut(&lnk, &spec)?;
    ctx.journal.record(
        TOOL,
        format!("Added Run name \"{alias}\" → {target}"),
        UndoAction::DeleteCreatedFiles {
            paths: vec![lnk.clone()],
        },
    )?;
    Ok(lnk.display().to_string())
}

pub fn remove(ctx: &AppCtx<'_>, a: &Alias) -> Result<()> {
    match a.kind {
        Kind::AppPath => {
            let undo = ctx.journal.backup_reg_key(&a.location)?;
            run_checked(hidden_command("reg").args(["delete", &a.location, "/f"]))?;
            ctx.journal
                .record(TOOL, format!("Removed Run name \"{}\"", a.alias), undo)?;
        }
        Kind::Shortcut => {
            ctx.journal.recycle(
                TOOL,
                format!("Removed Run name \"{}\"", a.alias),
                vec![PathBuf::from(&a.location)],
            )?;
        }
    }
    Ok(())
}

/// Opens the alias's target the way the Run dialog would.
pub fn execute(a: &Alias) -> Result<()> {
    match a.kind {
        Kind::Shortcut => shell_open(&a.location, ""),
        Kind::AppPath => shell_open(&a.target, ""),
    }
}

/// Your own names (both kinds) as JSON.
pub fn backup(items: &[Alias], file: &Path) -> Result<usize> {
    let mine: Vec<NewAlias> = items
        .iter()
        .filter(|a| !a.machine)
        .map(|a| NewAlias {
            alias: a.alias.clone(),
            target: a.target.clone(),
            args: a.args.clone(),
            show_cmd: a.show_cmd,
            all_users: false,
        })
        .collect();
    std::fs::write(file, serde_json::to_vec_pretty(&mine)?)
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(mine.len())
}

/// Adds every name from a backup that does not exist yet. Returns (added, skipped).
pub fn restore(ctx: &AppCtx<'_>, file: &Path) -> Result<(usize, usize)> {
    let list: Vec<NewAlias> = serde_json::from_slice(&std::fs::read(file)?).context("reading the backup")?;
    let (mut added, mut skipped) = (0, 0);
    for a in list {
        match add(ctx, &a) {
            Ok(_) => added += 1,
            Err(_) => skipped += 1,
        }
    }
    Ok((added, skipped))
}

enum Ask {
    Remove(Box<Alias>),
    Restore(PathBuf),
}

#[derive(Default)]
pub struct RunShortcuts {
    items: Option<Vec<Alias>>,
    selected: Option<String>,
    filter: String,
    draft: Option<NewAlias>,
    confirm: Confirm<Ask>,
    message: Message,
}

impl RunShortcuts {
    fn done(&mut self, r: Result<String>) {
        self.message = Some(r.map_err(|e| format!("{e:#}")));
        self.items = None;
    }

    fn draft_window(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(d) = &mut self.draft else { return };
        let mut result = None;
        egui::Window::new("New Run name")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                egui::Grid::new("run-draft")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Name to type");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.alias)
                                .hint_text("e.g. np")
                                .desired_width(140.0),
                        );
                        ui.end_row();
                        ui.label("Opens");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut d.target)
                                    .hint_text("Program, file, folder or https://…")
                                    .desired_width(260.0),
                            );
                            if ui.button("File…").clicked()
                                && let Some(p) = rfd::FileDialog::new().pick_file()
                            {
                                d.target = p.display().to_string();
                            }
                            if ui.button("Folder…").clicked()
                                && let Some(p) = rfd::FileDialog::new().pick_folder()
                            {
                                d.target = p.display().to_string();
                            }
                        });
                        ui.end_row();
                        ui.label("Arguments");
                        ui.add(egui::TextEdit::singleline(&mut d.args).desired_width(260.0));
                        ui.end_row();
                        ui.label("Window");
                        ui.horizontal(|ui| {
                            for (l, v) in WINDOW {
                                ui.radio_value(&mut d.show_cmd, v, l);
                            }
                        });
                        ui.end_row();
                        ui.label("");
                        let plain = uses_app_paths(d);
                        ui.add_enabled(
                            plain && ctx.elevated,
                            egui::Checkbox::new(&mut d.all_users, "For all users"),
                        )
                        .on_disabled_hover_text(if plain {
                            "Needs administrator rights"
                        } else {
                            "Only plain programs (no arguments, normal window) can be added for all users"
                        });
                        ui.end_row();
                    });
                if !d.target.trim().is_empty() {
                    ui.weak(format!(
                        "{} — stored as {}",
                        describe(&d.target),
                        if uses_app_paths(d) {
                            "an App Paths entry"
                        } else {
                            "a Run shortcut"
                        }
                    ));
                }
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        result = Some(true);
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        result = Some(false);
                    }
                });
            });
        match result {
            Some(true) => {
                if ctx.config.dry_run {
                    self.draft = None;
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    return;
                }
                let d = self.draft.clone().expect("open");
                match add(ctx, &d) {
                    Ok(_) => {
                        self.draft = None;
                        self.done(Ok(format!("Press Win+R and type \"{}\".", d.alias.trim())));
                    }
                    Err(e) => self.message = Some(Err(format!("{e:#}"))),
                }
            }
            Some(false) => self.draft = None,
            None => {}
        }
    }
}

impl Tool for RunShortcuts {
    fn id(&self) -> &'static str {
        "runshortcuts"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let r = match ask {
                    Ask::Remove(a) => remove(ctx, &a).map(|_| format!("Removed \"{}\".", a.alias)),
                    Ask::Restore(f) => restore(ctx, &f)
                        .map(|(a, s)| format!("Added {a} names; {s} already existed or could not be added.")),
                };
                self.done(r);
            }
        }
        self.draft_window(ui, ctx);
        let items = self.items.get_or_insert_with(enumerate).clone();
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| items.iter().find(|a| &a.location == k))
            .cloned();
        let writable = selected.as_ref().is_some_and(|a| !a.machine || ctx.elevated);
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "New name")),
                Some(
                    ToolButton::new(icons::PLAY, "Run").enabled(selected.as_ref().is_some_and(|a| !a.broken)),
                ),
                Some(ToolButton::new(icons::TRASH, "Remove").enabled(writable)),
                None,
                Some(ToolButton::new(icons::FLOPPY_DISK, "Back up")),
                Some(ToolButton::new(icons::CLOCK_COUNTER_CLOCKWISE, "Restore")),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
            |_| {},
        ) {
            Some(0) => self.draft = Some(NewAlias::default()),
            Some(1) => {
                if let Some(a) = &selected {
                    self.message = execute(a).err().map(|e| Err(format!("{e:#}")));
                }
            }
            Some(2) => {
                if let Some(a) = &selected {
                    let warn = if a.machine {
                        " Programs or scripts may rely on this system-wide name."
                    } else {
                        ""
                    };
                    self.confirm.ask(
                        format!("Remove the Run name \"{}\"?{warn}", a.alias),
                        Ask::Remove(Box::new(a.clone())),
                    );
                }
            }
            Some(4) => {
                if let Some(f) = rfd::FileDialog::new()
                    .set_file_name("Run names.json")
                    .add_filter("JSON", &["json"])
                    .save_file()
                {
                    self.message = Some(
                        backup(&items, &f)
                            .map(|n| format!("Saved {n} names to {}.", f.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(5) => {
                if let Some(f) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {
                    self.confirm.ask(
                        format!("Add the Run names saved in {}?", f.display()),
                        Ask::Restore(f),
                    );
                }
            }
            Some(6) => self.items = None,
            _ => {}
        }
        let filter_hint = ctx.i18n.tr("common-filter");
        frame::options(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  {filter_hint}", icons::MAGNIFYING_GLASS))
                    .desired_width(200.0),
            );
        });
        let status = format!(
            "{} names, {} yours, {} whose target is missing",
            items.len(),
            items.iter().filter(|a| !a.machine).count(),
            items.iter().filter(|a| a.broken).count()
        );
        frame::status(ui, &status, |ui| {
            ui.weak("Type these names in the Run dialog (Win+R) or File Explorer's address bar.");
        });
        frame::central_fixed(ui, |ui| self.table(ui, &items));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl RunShortcuts {
    fn table(&mut self, ui: &mut egui::Ui, items: &[Alias]) {
        show_message(ui, &self.message);
        let f = self.filter.to_lowercase();
        let visible: Vec<&Alias> = items
            .iter()
            .filter(|a| {
                f.is_empty() || a.alias.to_lowercase().contains(&f) || a.target.to_lowercase().contains(&f)
            })
            .collect();
        let mut clicked = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(140.0).clip(true))
            .column(Column::initial(90.0))
            .column(Column::initial(70.0))
            .column(Column::initial(80.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["Name", "Type", "For", "Available", "Opens"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(21.0, visible.len(), |mut row| {
                    let a = visible[row.index()];
                    row.set_selected(self.selected.as_ref() == Some(&a.location));
                    row.col(|ui| {
                        if a.broken {
                            ui.colored_label(ui.visuals().error_fg_color, &a.alias);
                        } else {
                            ui.strong(&a.alias);
                        }
                    });
                    row.col(|ui| {
                        ui.weak(a.what());
                    });
                    row.col(|ui| {
                        ui.weak(if a.machine { "All users" } else { "You" });
                    });
                    row.col(|ui| {
                        if a.broken {
                            ui.colored_label(ui.visuals().error_fg_color, "Missing");
                        } else {
                            ui.weak("Yes");
                        }
                    });
                    row.col(|ui| {
                        ui.weak(format!("{} {}", a.target, a.args).trim().to_owned());
                    });
                    if row.response().clicked() {
                        clicked = Some(a.location.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_validation_and_kinds() {
        assert!(validate_alias("np").is_ok());
        assert!(validate_alias("my-tool_2").is_ok());
        assert!(validate_alias("").is_err());
        assert!(validate_alias("has space").is_err());
        assert!(validate_alias(r"..\evil").is_err());
        assert_eq!(describe("https://example.com"), "Web address");
        assert_eq!(describe(r"C:\Windows"), "Folder");
        assert_eq!(describe(r"C:\Windows\notepad.exe"), "Program");
        assert_eq!(describe(r"C:\x\report.pdf"), "File");
        let plain = NewAlias {
            alias: "n".into(),
            target: r"C:\Windows\notepad.exe".into(),
            ..Default::default()
        };
        assert!(uses_app_paths(&plain));
        assert!(!uses_app_paths(&NewAlias {
            show_cmd: 3,
            ..plain.clone()
        }));
        assert!(!uses_app_paths(&NewAlias {
            args: "-x".into(),
            ..plain
        }));
        assert!(
            existing_command("notepad").is_some(),
            "System32 commands are found"
        );
    }

    #[test]
    fn path_editing() {
        assert_eq!(path_with("", r"C:\R"), Some(r"C:\R".into()));
        assert_eq!(path_with(r"C:\A;", r"C:\R"), Some(r"C:\A;C:\R".into()));
        assert_eq!(path_with(r"C:\A;c:\r\", r"C:\R"), None);
    }

    #[test]
    fn lists_real_app_paths() {
        assert!(
            enumerate().iter().any(|a| a.machine),
            "Windows ships machine-wide App Paths"
        );
    }

    /// Adds an App Paths name and a shortcut name, then undoes both (and the PATH
    /// change, if this test made it).
    #[test]
    fn add_both_kinds_with_undo() {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        let pid = std::process::id();
        let (a1, a2) = (format!("wmt{pid}a"), format!("wmt{pid}b"));
        let path_before = registry::snapshot(ENV_KEY, "Path");
        struct Cleanup(String, PathBuf, registry::ValueSnapshot);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok((root, sub)) = registry::split_path(&self.0) {
                    let _ = root.delete_subkey_all(sub);
                }
                let _ = std::fs::remove_file(&self.1);
                let _ = registry::restore(&self.2);
            }
        }
        let lnk = shortcut_dir().unwrap().join(format!("{a2}.lnk"));
        let _c = Cleanup(format!(r"{USER_KEY}\{a1}.exe"), lnk.clone(), path_before.clone());

        add(
            &ctx,
            &NewAlias {
                alias: a1.clone(),
                target: r"C:\Windows\notepad.exe".into(),
                ..Default::default()
            },
        )
        .unwrap();
        add(
            &ctx,
            &NewAlias {
                alias: a2.clone(),
                target: "https://example.com".into(),
                show_cmd: 3,
                ..Default::default()
            },
        )
        .unwrap();
        let items = enumerate();
        let one = items.iter().find(|a| a.alias == a1).unwrap();
        let two = items.iter().find(|a| a.alias == a2).unwrap();
        assert_eq!((one.kind, two.kind), (Kind::AppPath, Kind::Shortcut));
        assert_eq!((two.target.as_str(), two.show_cmd), ("https://example.com", 3));
        let (path, _) = user_path();
        assert!(
            path.to_lowercase()
                .contains(&shortcut_dir().unwrap().display().to_string().to_lowercase())
        );
        assert!(
            add(
                &ctx,
                &NewAlias {
                    alias: a1.clone(),
                    target: r"C:\Windows".into(),
                    ..Default::default()
                }
            )
            .is_err(),
            "taken"
        );

        let b = dir.path().join("names.json");
        assert!(backup(&items, &b).unwrap() >= 2);
        for e in journal.entries() {
            journal.undo(e.id).unwrap();
        }
        assert!(enumerate().iter().all(|a| a.alias != a1 && a.alias != a2));
        assert_eq!(
            registry::snapshot(ENV_KEY, "Path"),
            path_before,
            "PATH is back to how it was"
        );
    }
}
