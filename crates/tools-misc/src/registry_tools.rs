//! Registry Tools: search keys / values / data (whole match, case, type, scope), a
//! jumper that opens any pasted key in Registry Editor, a key editor (values with undo,
//! permissions, desktop shortcut, export), .reg import with backup, key comparison, and
//! favourite keys.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use winreg::RegKey;
use winreg::enums::KEY_READ;
use wm_core::{
    AppCtx, Category, Layout, Tool, acl, egui, icons, paths,
    process::{hidden_command, run_checked},
    registry,
    task::{BackgroundTask, Progress},
    ui::{Confirm, Message, frame, section_nav, show_message},
};

use crate::registry_extra::{self as rx, Kind, Matching, TypeFilter};

const MAX_RESULTS: usize = 2000;
const ROOTS: [&str; 5] = ["HKCU", "HKLM", "HKCR", "HKU", "HKCC"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub key: String,
    pub value: Option<String>,
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchIn {
    pub keys: bool,
    pub values: bool,
    pub data: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    pub matching: Matching,
    pub types: TypeFilter,
}

fn data_text(v: &winreg::RegValue<'_>) -> (u32, String) {
    let raw = registry::RawValue {
        vtype: v.vtype.clone() as u32,
        bytes: v.bytes.to_vec(),
    };
    let text = match raw.vtype {
        1 | 2 | 7 | 4 | 11 => rx::decode(&raw).replace('\n', " | "),
        _ => String::new(),
    };
    (raw.vtype, text)
}

/// Depth-first search below `path` ("" or "*" = every root); stops at `MAX_RESULTS` or
/// when cancelled.
pub fn search<T>(
    path: &str,
    needle: &str,
    what: SearchIn,
    opts: SearchOptions,
    progress: Option<&Progress<T>>,
) -> Result<Vec<Hit>> {
    if needle.is_empty() {
        bail!("enter something to search for");
    }
    let starts: Vec<String> = if path.trim().is_empty() || path.trim() == "*" {
        ROOTS.iter().map(|r| r.to_string()).collect()
    } else {
        vec![rx::normalize_key(path)?]
    };
    let m = opts.matching;
    let mut hits = Vec::new();
    let mut visited = 0u64;
    for start_path in starts {
        let (root, sub) = registry::split_path(&start_path)?;
        let start = if sub.is_empty() {
            root
        } else {
            root.open_subkey_with_flags(sub, KEY_READ)?
        };
        let mut stack: Vec<(RegKey, String)> = vec![(start, start_path.clone())];
        while let Some((key, name)) = stack.pop() {
            visited += 1;
            if let Some(p) = progress {
                if p.is_cancelled() {
                    return Ok(hits);
                }
                if visited.is_multiple_of(2000) {
                    p.report(0.0, format!("{visited} keys searched, {} found…", hits.len()));
                }
            }
            for (vname, value) in key.enum_values().flatten() {
                let (vtype, data) = data_text(&value);
                if !opts.types.accepts(vtype) {
                    continue;
                }
                let name_hit = what.values && m.matches(&vname, needle);
                let data_hit = what.data && !data.is_empty() && m.matches(&data, needle);
                if name_hit || data_hit {
                    hits.push(Hit {
                        key: name.clone(),
                        value: Some(vname),
                        data,
                    });
                    if hits.len() >= MAX_RESULTS {
                        return Ok(hits);
                    }
                }
            }
            let children: Vec<String> = key.enum_keys().flatten().collect();
            for child in children.into_iter().rev() {
                let full = format!(r"{name}\{child}");
                if what.keys && opts.types == TypeFilter::Any && m.matches(&child, needle) {
                    hits.push(Hit {
                        key: full.clone(),
                        value: None,
                        data: String::new(),
                    });
                    if hits.len() >= MAX_RESULTS {
                        return Ok(hits);
                    }
                }
                // Keys we may not read (access denied) are skipped silently.
                if let Ok(k) = key.open_subkey_with_flags(&child, KEY_READ) {
                    stack.push((k, full));
                }
            }
        }
    }
    Ok(hits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diff {
    OnlyLeft(String),
    OnlyRight(String),
    Different(String, String, String),
}

fn values_of(path: &str) -> Result<BTreeMap<String, String>> {
    let k = registry::open_read(&rx::normalize_key(path)?)?;
    Ok(k.enum_values()
        .flatten()
        .map(|(n, v)| (n, data_text(&v).1))
        .collect())
}

pub fn diff(left: &BTreeMap<String, String>, right: &BTreeMap<String, String>) -> Vec<Diff> {
    let mut out = Vec::new();
    for (k, v) in left {
        match right.get(k) {
            None => out.push(Diff::OnlyLeft(k.clone())),
            Some(w) if w != v => out.push(Diff::Different(k.clone(), v.clone(), w.clone())),
            _ => {}
        }
    }
    out.extend(
        right
            .keys()
            .filter(|k| !left.contains_key(*k))
            .map(|k| Diff::OnlyRight(k.clone())),
    );
    out
}

use wm_core::registry::open_in_regedit;
#[cfg(test)]
use wm_core::registry::regedit_path;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Favorites {
    #[serde(default)]
    key: Vec<String>,
}

fn favorites_path() -> Result<PathBuf> {
    Ok(paths::config_dir()?.join("registry-favorites.toml"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Search,
    Jump,
    Editor,
    Import,
    Compare,
    Favorites,
}

/// A value being added or edited.
#[derive(Debug, Clone, Default)]
struct Draft {
    name: String,
    kind: Kind,
    text: String,
    /// Editing an existing value (its name can't change).
    existing: bool,
}

enum Ask {
    DeleteValue(String),
    Grant,
    Import(PathBuf),
}

pub struct RegistryTools {
    tab: Tab,
    root: String,
    needle: String,
    what: SearchIn,
    opts: SearchOptions,
    task: Option<BackgroundTask<Vec<Hit>>>,
    hits: Vec<Hit>,
    jump: String,
    edit_key: String,
    loaded_key: String,
    values: Option<Result<Vec<rx::ValueRow>, String>>,
    perms: Option<Result<Vec<rx::Ace>, String>>,
    draft: Option<Draft>,
    confirm: Confirm<usize>,
    pending: Vec<Ask>,
    left: String,
    right: String,
    diffs: Option<Result<Vec<Diff>, String>>,
    favorites: Option<Vec<String>>,
    new_favorite: String,
    message: Message,
}

impl Default for RegistryTools {
    fn default() -> Self {
        Self {
            tab: Tab::Search,
            root: r"HKCU\Software".into(),
            needle: String::new(),
            what: SearchIn {
                keys: true,
                values: true,
                data: true,
            },
            opts: SearchOptions::default(),
            task: None,
            hits: Vec::new(),
            jump: String::new(),
            edit_key: String::new(),
            loaded_key: String::new(),
            values: None,
            perms: None,
            draft: None,
            confirm: Confirm::default(),
            pending: Vec::new(),
            left: String::new(),
            right: String::new(),
            diffs: None,
            favorites: None,
            new_favorite: String::new(),
            message: None,
        }
    }
}

impl RegistryTools {
    fn export(&mut self, key: &str) {
        let Some(file) = rfd::FileDialog::new()
            .add_filter("Registration file", &["reg"])
            .set_file_name("export.reg")
            .save_file()
        else {
            return;
        };
        self.message = Some(
            run_checked(hidden_command("reg").args(["export", key]).arg(&file).arg("/y"))
                .map(|_| format!("Exported {key} to {}.", file.display()))
                .map_err(|e| format!("{e:#}")),
        );
    }

    fn row_actions(&mut self, ui: &mut egui::Ui, key: &str) {
        if ui
            .small_button("Open")
            .on_hover_text("Open in Registry Editor")
            .clicked()
        {
            self.message = open_in_regedit(key).err().map(|e| Err(format!("{e:#}")));
        }
        if ui
            .small_button("Edit")
            .on_hover_text("Edit this key's values here")
            .clicked()
        {
            self.open_editor(key);
        }
        if ui.small_button("★").on_hover_text("Add to favourites").clicked() {
            self.add_favorite(key.to_owned());
        }
    }

    fn open_editor(&mut self, key: &str) {
        self.edit_key = key.to_owned();
        self.tab = Tab::Editor;
        self.load_key();
    }

    fn load_key(&mut self) {
        match rx::normalize_key(&self.edit_key) {
            Ok(k) => {
                self.values = Some(rx::list_values(&k).map_err(|e| format!("{e:#}")));
                self.perms = Some(
                    acl::reg_dacl_sddl(&k)
                        .map(|s| rx::parse_aces(&s))
                        .map_err(|e| format!("{e:#}")),
                );
                self.edit_key = k.clone();
                self.loaded_key = k;
            }
            Err(e) => {
                self.values = Some(Err(format!("{e:#}")));
                self.perms = None;
                self.loaded_key.clear();
            }
        }
        self.draft = None;
    }

    fn add_favorite(&mut self, key: String) {
        let mut list = self.favorites.clone().unwrap_or_default();
        if !list.contains(&key) {
            list.push(key);
        }
        self.message = favorites_path()
            .and_then(|p| {
                Ok(std::fs::write(
                    p,
                    toml::to_string_pretty(&Favorites { key: list.clone() })?,
                )?)
            })
            .err()
            .map(|e| Err(format!("{e:#}")));
        self.favorites = Some(list);
    }

    fn ask(&mut self, question: String, what: Ask) {
        self.pending.push(what);
        self.confirm.ask(question, self.pending.len() - 1);
    }

    fn run_pending(&mut self, ctx: &AppCtx<'_>, i: usize) {
        let Some(what) = self.pending.drain(..).nth(i) else {
            return;
        };
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let key = self.loaded_key.clone();
        self.message = Some(
            match what {
                Ask::DeleteValue(name) => rx::remove_value(ctx, &key, &name)
                    .map(|()| "Value deleted. Undo it from the Change Journal.".into()),
                Ask::Grant => rx::grant_me_full_control(ctx, &key).map(|()| {
                    "You now have full control of this key. Undo restores the previous permissions.".into()
                }),
                Ask::Import(path) => rx::import(ctx, &path)
                    .map(|n| format!("Imported {} ({n} key(s) backed up first).", path.display())),
            }
            .map_err(|e| format!("{e:#}")),
        );
        if !key.is_empty() {
            self.load_key();
        }
    }

    fn search_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                match r {
                    Ok(h) => self.hits = h,
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        ui.horizontal(|ui| {
            ui.label("In");
            ui.add(
                egui::TextEdit::singleline(&mut self.root)
                    .hint_text("* = every root")
                    .desired_width(200.0),
            );
            egui::ComboBox::from_id_salt("rt-scope")
                .selected_text("Scope")
                .show_ui(ui, |ui| {
                    for s in [
                        "*",
                        "HKCU",
                        r"HKCU\Software",
                        "HKLM",
                        r"HKLM\SOFTWARE",
                        "HKCR",
                        "HKU",
                    ] {
                        let label = if s == "*" { "Every root" } else { s };
                        if ui.selectable_label(self.root == s, label).clicked() {
                            self.root = s.into();
                        }
                    }
                });
            ui.label("find");
            ui.add(egui::TextEdit::singleline(&mut self.needle).desired_width(200.0));
            let busy = self.task.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Search")).clicked() {
                let (root, needle, what, opts) = (
                    self.root.trim().to_owned(),
                    self.needle.clone(),
                    self.what,
                    self.opts,
                );
                self.hits.clear();
                self.message = None;
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    search(&root, &needle, what, opts, Some(p))
                }));
            }
            if let Some(t) = &self.task {
                ui.spinner();
                ui.weak(&t.status);
                if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                    t.cancel();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Look at");
            ui.checkbox(&mut self.what.keys, "keys");
            ui.checkbox(&mut self.what.values, "value names");
            ui.checkbox(&mut self.what.data, "data");
            ui.separator();
            ui.checkbox(&mut self.opts.matching.whole, "Match whole name");
            ui.checkbox(&mut self.opts.matching.case_sensitive, "Match case");
            egui::ComboBox::from_id_salt("rt-type")
                .selected_text(self.opts.types.label())
                .show_ui(ui, |ui| {
                    for t in TypeFilter::ALL {
                        ui.selectable_value(&mut self.opts.types, t, t.label());
                    }
                });
        });
        ui.weak(format!(
            "{} results{}",
            self.hits.len(),
            if self.hits.len() >= MAX_RESULTS {
                " (limit reached; narrow the search)"
            } else {
                ""
            }
        ));
        let hits = self.hits.clone();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show_rows(ui, 22.0, hits.len(), |ui, range| {
                for h in &hits[range] {
                    ui.horizontal(|ui| {
                        self.row_actions(ui, &h.key);
                        ui.label(&h.key);
                        if let Some(v) = &h.value {
                            ui.strong(if v.is_empty() { "(Default)" } else { v });
                            ui.weak(&h.data);
                        }
                    });
                }
            });
    }

    fn jump_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(
            "Paste a key in any form (Computer\\HKEY_…, HKLM/…, [HKCU\\…] from a .reg file) to open it.",
        );
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.jump)
                    .hint_text(r"HKEY_CURRENT_USER\Software\Microsoft\Windows")
                    .desired_width(460.0),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let go = ui.button("Open in Registry Editor").clicked() || enter;
            let edit = ui.button("Edit here").clicked();
            if go || edit {
                match rx::normalize_key(&self.jump) {
                    Ok(k) if registry::open_read(&k).is_err() => {
                        self.message = Some(Err(format!("{k} doesn't exist or can't be read.")));
                    }
                    Ok(k) if edit => self.open_editor(&k),
                    Ok(k) => {
                        self.message = Some(
                            open_in_regedit(&k)
                                .map(|()| format!("Opened {k}."))
                                .map_err(|e| format!("{e:#}")),
                        );
                    }
                    Err(e) => self.message = Some(Err(format!("{e:#}"))),
                }
            }
        });
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.edit_key)
                    .hint_text(r"HKCU\Software\…")
                    .desired_width(420.0),
            );
            if ui.button("Load").clicked() {
                self.load_key();
            }
        });
        if self.loaded_key.is_empty() {
            if let Some(Err(e)) = &self.values {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            return;
        }
        let key = self.loaded_key.clone();
        let writable = !registry::needs_admin(&key) || ctx.elevated;
        ui.horizontal(|ui| {
            ui.strong(&key);
            if ui.small_button("Open in Registry Editor").clicked() {
                self.message = open_in_regedit(&key).err().map(|e| Err(format!("{e:#}")));
            }
            if ui.small_button("Export…").clicked() {
                self.export(&key);
            }
            if ui
                .small_button("Desktop shortcut")
                .on_hover_text("A shortcut on the desktop that opens Registry Editor at this key.")
                .clicked()
            {
                self.message = Some(
                    rx::desktop_shortcut(&key)
                        .map(|p| format!("Created {}.", p.display()))
                        .map_err(|e| format!("{e:#}")),
                );
            }
            if ui.small_button("★").on_hover_text("Add to favourites").clicked() {
                self.add_favorite(key.clone());
            }
        });
        if !writable {
            ui.weak("Changing this key needs the app to run as administrator.");
        }

        ui.add_space(4.0);
        ui.strong("Values");
        let mut edit = None;
        let mut delete = None;
        match &self.values {
            Some(Ok(vals)) => {
                egui::Grid::new("rt-values")
                    .striped(true)
                    .num_columns(4)
                    .show(ui, |ui| {
                        for h in ["Name", "Type", "Data", ""] {
                            ui.strong(h);
                        }
                        ui.end_row();
                        for (name, vtype, text) in vals {
                            ui.label(if name.is_empty() { "(Default)" } else { name });
                            ui.weak(
                                Kind::from_vtype(*vtype)
                                    .map_or(format!("type {vtype}"), |k| k.label().to_owned()),
                            );
                            let short: String = text.replace('\n', " | ").chars().take(80).collect();
                            ui.label(short);
                            ui.horizontal(|ui| {
                                let known = Kind::from_vtype(*vtype);
                                if ui
                                    .add_enabled(
                                        writable && known.is_some(),
                                        egui::Button::new("Edit").small(),
                                    )
                                    .clicked()
                                {
                                    edit = Some(Draft {
                                        name: name.clone(),
                                        kind: known.unwrap_or_default(),
                                        text: text.clone(),
                                        existing: true,
                                    });
                                }
                                if ui
                                    .add_enabled(writable, egui::Button::new("Delete").small())
                                    .clicked()
                                {
                                    delete = Some(name.clone());
                                }
                            });
                            ui.end_row();
                        }
                    });
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }
        if let Some(d) = edit {
            self.draft = Some(d);
        }
        if let Some(name) = delete {
            let shown = if name.is_empty() {
                "(Default)".to_owned()
            } else {
                name.clone()
            };
            self.ask(
                format!("Delete the value \"{shown}\"? It can be restored from the Change Journal."),
                Ask::DeleteValue(name),
            );
        }
        if writable && self.draft.is_none() && ui.button("New value…").clicked() {
            self.draft = Some(Draft::default());
        }
        let mut close = false;
        if let Some(d) = &mut self.draft {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.add_enabled(
                        !d.existing,
                        egui::TextEdit::singleline(&mut d.name)
                            .hint_text("empty = (Default)")
                            .desired_width(180.0),
                    );
                    egui::ComboBox::from_id_salt("rt-kind")
                        .selected_text(d.kind.label())
                        .show_ui(ui, |ui| {
                            for k in Kind::ALL {
                                ui.selectable_value(&mut d.kind, k, k.label());
                            }
                        });
                });
                let hint = match d.kind {
                    Kind::Dword | Kind::Qword => "123 or 0x7B",
                    Kind::Multi => "one string per line",
                    Kind::Binary => "hex bytes: 01 a0 ff",
                    _ => "",
                };
                ui.add(
                    egui::TextEdit::multiline(&mut d.text)
                        .hint_text(hint)
                        .desired_rows(if d.kind == Kind::Multi { 4 } else { 1 })
                        .desired_width(420.0),
                );
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.message = Some(if ctx.config.dry_run {
                            Ok(ctx.i18n.tr("common-dry-run-skip"))
                        } else {
                            rx::set_value(ctx, &key, d.name.trim(), d.kind, &d.text)
                                .map(|()| "Saved. Undo it from the Change Journal.".into())
                                .map_err(|e| format!("{e:#}"))
                        });
                        close = self.message.as_ref().is_some_and(|m| m.is_ok());
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        close = true;
                    }
                });
            });
        }
        if close {
            self.load_key();
        }

        ui.add_space(6.0);
        ui.strong("Permissions");
        match &self.perms {
            Some(Ok(aces)) => {
                egui::Grid::new("rt-perms")
                    .striped(true)
                    .num_columns(4)
                    .show(ui, |ui| {
                        for h in ["Account", "Type", "Access", "Applies to"] {
                            ui.strong(h);
                        }
                        ui.end_row();
                        for a in aces {
                            ui.label(acl::account_name(&a.sid));
                            if a.allow {
                                ui.label("Allow");
                            } else {
                                ui.colored_label(ui.visuals().warn_fg_color, "Deny");
                            }
                            ui.label(rx::rights_text(&a.rights));
                            ui.weak(format!(
                                "{}{}",
                                if a.subkeys {
                                    "this key and subkeys"
                                } else {
                                    "this key only"
                                },
                                if a.inherited { " (inherited)" } else { "" }
                            ));
                            ui.end_row();
                        }
                    });
                if ui
                    .button("Grant me full control")
                    .on_hover_text(
                        "Adds full control for your account; undo restores the previous permissions.",
                    )
                    .clicked()
                {
                    self.ask(format!("Give your account full control of {key}?"), Ask::Grant);
                }
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }
    }

    fn import_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("Merges a .reg file into the registry. Every key it touches is backed up first, so the import can be undone from the Change Journal.");
        if ui.button("Choose a .reg file…").clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("Registration file", &["reg"])
                .pick_file()
        {
            match rx::read_reg_text(&p).and_then(|t| rx::reg_file_keys(&t)) {
                Ok(keys) => {
                    let mut q = format!("Import {}? It changes {} key(s):", p.display(), keys.len());
                    for k in keys.iter().take(8) {
                        q += &format!("\n{k}");
                    }
                    if keys.len() > 8 {
                        q += "\n…";
                    }
                    self.ask(q, Ask::Import(p));
                }
                Err(e) => self.message = Some(Err(format!("{e:#}"))),
            }
        }
    }

    fn compare_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.left)
                    .hint_text(r"HKCU\Software\A")
                    .desired_width(300.0),
            );
            ui.label("vs");
            ui.add(
                egui::TextEdit::singleline(&mut self.right)
                    .hint_text(r"HKCU\Software\B")
                    .desired_width(300.0),
            );
            if ui.button("Compare").clicked() {
                self.diffs = Some(
                    values_of(self.left.trim())
                        .and_then(|l| Ok(diff(&l, &values_of(self.right.trim())?)))
                        .map_err(|e| format!("{e:#}")),
                );
            }
        });
        match &self.diffs {
            Some(Ok(d)) if d.is_empty() => {
                ui.label("The values of both keys are identical.");
            }
            Some(Ok(d)) => {
                egui::Grid::new("regdiff")
                    .num_columns(2)
                    .striped(true)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for x in d {
                            match x {
                                Diff::OnlyLeft(n) => {
                                    ui.label(n);
                                    ui.weak("only in the first key");
                                }
                                Diff::OnlyRight(n) => {
                                    ui.label(n);
                                    ui.weak("only in the second key");
                                }
                                Diff::Different(n, a, b) => {
                                    ui.label(n);
                                    ui.label(format!("{a}  ≠  {b}"));
                                }
                            }
                            ui.end_row();
                        }
                    });
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }
    }

    fn favorites_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_favorite)
                    .hint_text(r"HKLM\SOFTWARE\…")
                    .desired_width(360.0),
            );
            if ui.button("Add").clicked() {
                match rx::normalize_key(&self.new_favorite) {
                    Ok(key) if registry::open_read(&key).is_ok() => {
                        self.add_favorite(key);
                        self.new_favorite.clear();
                    }
                    Ok(key) => self.message = Some(Err(format!("{key} does not exist or cannot be read."))),
                    Err(e) => self.message = Some(Err(format!("{e:#}"))),
                }
            }
        });
        let favorites = self.favorites.clone().unwrap_or_default();
        let mut remove = None;
        for (i, key) in favorites.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("Open").clicked() {
                    self.message = open_in_regedit(key).err().map(|e| Err(format!("{e:#}")));
                }
                if ui.small_button("Edit").clicked() {
                    self.open_editor(key);
                }
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
                ui.label(key);
            });
        }
        if let Some(i) = remove {
            let mut list = favorites;
            list.remove(i);
            let _ = favorites_path().and_then(|p| {
                Ok(std::fs::write(
                    p,
                    toml::to_string_pretty(&Favorites { key: list.clone() })?,
                )?)
            });
            self.favorites = Some(list);
        }
    }
}

impl Tool for RegistryTools {
    fn id(&self) -> &'static str {
        "regtools"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.favorites.is_none() {
            self.favorites = Some(
                favorites_path()
                    .ok()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|t| toml::from_str::<Favorites>(&t).ok())
                    .map(|f| f.key)
                    .unwrap_or_default(),
            );
        }
        if let Some(i) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.run_pending(ctx, i);
        } else if !self.confirm.is_open() {
            self.pending.clear();
        }
        const TABS: [(Tab, &str, &str, &str); 6] = [
            (
                Tab::Search,
                icons::MAGNIFYING_GLASS,
                "Search",
                "Finds keys, value names and data under a key.",
            ),
            (
                Tab::Jump,
                icons::ARROW_SQUARE_OUT,
                "Jump to key",
                "Opens the Registry Editor at a key you type or paste.",
            ),
            (
                Tab::Editor,
                icons::PENCIL_SIMPLE,
                "Edit key",
                "Every change can be undone from the Change Journal.",
            ),
            (
                Tab::Import,
                icons::DOWNLOAD,
                "Import",
                "Lists the keys a .reg file changes and backs them up before importing it.",
            ),
            (
                Tab::Compare,
                icons::SWAP,
                "Compare keys",
                "Lists the differences between two keys.",
            ),
            (
                Tab::Favorites,
                icons::BOOKMARK_SIMPLE,
                "Favourites",
                "Keys you visit often, one click away.",
            ),
        ];
        let mut sel = TABS.iter().position(|t| t.0 == self.tab).unwrap_or(0);
        let nav: Vec<(&str, &str)> = TABS.iter().map(|t| (t.1, t.2)).collect();
        if frame::left(ui, 180.0, |ui| section_nav(ui, &nav, &mut sel)) {
            self.message = None;
        }
        self.tab = TABS[sel].0;
        frame::status(ui, TABS[sel].3, |_| {});
        let body = |this: &mut Self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>| {
            show_message(ui, &this.message);
            match this.tab {
                Tab::Search => this.search_ui(ui, ctx),
                Tab::Jump => this.jump_ui(ui),
                Tab::Editor => this.editor_ui(ui, ctx),
                Tab::Import => this.import_ui(ui),
                Tab::Compare => this.compare_ui(ui),
                Tab::Favorites => this.favorites_ui(ui),
            }
        };
        if self.tab == Tab::Search {
            frame::central_fixed(ui, |ui| body(self, ui, ctx));
        } else {
            frame::central(ui, |ui| body(self, ui, ctx));
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
    fn search_finds_keys_values_and_data() {
        let base = format!(r"HKCU\Software\wm-regtools-test-{}", std::process::id());
        registry::write_raw(&format!(r"{base}\Alpha\NeedleKey"), "x", &registry::dword(1)).unwrap();
        registry::write_raw(&format!(r"{base}\Beta"), "NeedleName", &registry::string("plain")).unwrap();
        registry::write_raw(
            &format!(r"{base}\Beta"),
            "other",
            &registry::string("has a NEEDLE inside"),
        )
        .unwrap();
        registry::write_raw(&format!(r"{base}\Beta"), "count", &registry::dword(77)).unwrap();

        let all = SearchIn {
            keys: true,
            values: true,
            data: true,
        };
        let any = SearchOptions::default();
        let hits = search::<()>(&base, "needle", all, any, None).unwrap();
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert!(
            hits.iter()
                .any(|h| h.key.ends_with(r"Alpha\NeedleKey") && h.value.is_none())
        );
        let only_keys = search::<()>(
            &base,
            "needle",
            SearchIn {
                keys: true,
                values: false,
                data: false,
            },
            any,
            None,
        )
        .unwrap();
        assert_eq!(only_keys.len(), 1);
        assert!(search::<()>(&base, "", all, any, None).is_err());

        let case = SearchOptions {
            matching: Matching {
                whole: false,
                case_sensitive: true,
            },
            ..any
        };
        assert_eq!(search::<()>(&base, "NEEDLE", all, case, None).unwrap().len(), 1);
        let whole = SearchOptions {
            matching: Matching {
                whole: true,
                case_sensitive: false,
            },
            ..any
        };
        assert_eq!(
            search::<()>(&base, "needlename", all, whole, None).unwrap().len(),
            1
        );
        let dwords = SearchOptions {
            types: TypeFilter::Dword,
            ..any
        };
        let d = search::<()>(&base, "77", all, dwords, None).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        // Long-form paths work too.
        let long = base.replacen("HKCU", r"Computer\HKEY_CURRENT_USER", 1);
        assert_eq!(search::<()>(&long, "needle", all, any, None).unwrap().len(), 3);

        let (root, sub) = registry::split_path(&base).unwrap();
        root.delete_subkey_all(sub).unwrap();
    }

    #[test]
    fn diffs_and_paths() {
        let l: BTreeMap<String, String> = [("a".into(), "1".into()), ("b".into(), "2".into())].into();
        let r: BTreeMap<String, String> = [("b".into(), "3".into()), ("c".into(), "4".into())].into();
        assert_eq!(
            diff(&l, &r),
            vec![
                Diff::OnlyLeft("a".into()),
                Diff::Different("b".into(), "2".into(), "3".into()),
                Diff::OnlyRight("c".into())
            ]
        );
        assert_eq!(
            regedit_path(r"HKCU\Software\X"),
            r"Computer\HKEY_CURRENT_USER\Software\X"
        );
        assert_eq!(regedit_path("HKLM"), r"Computer\HKEY_LOCAL_MACHINE");
    }
}
