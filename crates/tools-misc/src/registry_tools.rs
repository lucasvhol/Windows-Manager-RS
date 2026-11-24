//! Registry Tools: search keys/values/data, export a key to a .reg file, compare two
//! keys, and keep favourite keys that open directly in Registry Editor.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use winreg::RegKey;
use winreg::enums::KEY_READ;
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    process::{hidden_command, run_checked},
    registry,
    task::{BackgroundTask, Progress},
    ui::{Message, show_message},
};

const MAX_RESULTS: usize = 2000;

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

fn data_text(v: &winreg::RegValue<'_>) -> String {
    let raw = registry::RawValue {
        vtype: v.vtype.clone() as u32,
        bytes: v.bytes.to_vec(),
    };
    match raw.vtype {
        1 | 2 => registry::decode_string(&raw),
        7 => registry::decode_string(&raw), // first string of a MULTI_SZ is enough for matching
        4 => registry::decode_dword(&raw)
            .map(|d| format!("0x{d:08X} ({d})"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Depth-first search below `path`; stops at `MAX_RESULTS` or when cancelled.
pub fn search<T>(
    path: &str,
    needle: &str,
    what: SearchIn,
    progress: Option<&Progress<T>>,
) -> Result<Vec<Hit>> {
    let needle = needle.to_lowercase();
    if needle.is_empty() {
        bail!("enter something to search for");
    }
    let (root, sub) = registry::split_path(path)?;
    let start = if sub.is_empty() {
        root
    } else {
        root.open_subkey_with_flags(sub, KEY_READ)?
    };
    let mut hits = Vec::new();
    let mut stack: Vec<(RegKey, String)> = vec![(start, path.trim_end_matches('\\').to_owned())];
    let mut visited = 0u64;
    while let Some((key, name)) = stack.pop() {
        visited += 1;
        if let Some(p) = progress {
            if p.is_cancelled() {
                break;
            }
            if visited.is_multiple_of(2000) {
                p.report(0.0, format!("{visited} keys searched, {} found…", hits.len()));
            }
        }
        for (vname, value) in key.enum_values().flatten() {
            let data = data_text(&value);
            let name_hit = what.values && vname.to_lowercase().contains(&needle);
            let data_hit = what.data && data.to_lowercase().contains(&needle);
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
            if what.keys && child.to_lowercase().contains(&needle) {
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
    Ok(hits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diff {
    OnlyLeft(String),
    OnlyRight(String),
    Different(String, String, String),
}

fn values_of(path: &str) -> Result<BTreeMap<String, String>> {
    let k = registry::open_read(path)?;
    Ok(k.enum_values()
        .flatten()
        .map(|(n, v)| (n, data_text(&v)))
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

/// `HKCU\Software` → `Computer\HKEY_CURRENT_USER\Software` (Registry Editor's address format).
pub fn regedit_path(key: &str) -> String {
    let (root, rest) = key.split_once('\\').unwrap_or((key, ""));
    let upper = root.to_ascii_uppercase();
    let long = match upper.as_str() {
        "HKCU" => "HKEY_CURRENT_USER",
        "HKLM" => "HKEY_LOCAL_MACHINE",
        "HKCR" => "HKEY_CLASSES_ROOT",
        "HKU" => "HKEY_USERS",
        "HKCC" => "HKEY_CURRENT_CONFIG",
        other => other,
    };
    if rest.is_empty() {
        format!(r"Computer\{long}")
    } else {
        format!(r"Computer\{long}\{rest}")
    }
}

/// Opens Registry Editor at `key` (sets the key it remembers, then starts it).
pub fn open_in_regedit(key: &str) -> Result<()> {
    registry::write_raw(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Applets\Regedit",
        "LastKey",
        &registry::string(&regedit_path(key)),
    )?;
    wm_core::shell::shell_open("regedit.exe", "-m")
}

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
    Compare,
    Favorites,
}

pub struct RegistryTools {
    tab: Tab,
    root: String,
    needle: String,
    what: SearchIn,
    task: Option<BackgroundTask<Vec<Hit>>>,
    hits: Vec<Hit>,
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
            task: None,
            hits: Vec::new(),
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
        if ui.small_button("Export…").clicked() {
            self.export(key);
        }
        if ui.small_button("★").on_hover_text("Add to favourites").clicked() {
            self.add_favorite(key.to_owned());
        }
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
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Search, "Search");
            ui.selectable_value(&mut self.tab, Tab::Compare, "Compare keys");
            ui.selectable_value(&mut self.tab, Tab::Favorites, "Favourites");
        });
        show_message(ui, &self.message);
        ui.separator();

        match self.tab {
            Tab::Search => {
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
                    ui.add(egui::TextEdit::singleline(&mut self.root).desired_width(220.0));
                    ui.label("find");
                    ui.add(egui::TextEdit::singleline(&mut self.needle).desired_width(200.0));
                    ui.checkbox(&mut self.what.keys, "keys");
                    ui.checkbox(&mut self.what.values, "value names");
                    ui.checkbox(&mut self.what.data, "data");
                    let busy = self.task.is_some();
                    if ui.add_enabled(!busy, egui::Button::new("Search")).clicked() {
                        let (root, needle, what) =
                            (self.root.trim().to_owned(), self.needle.clone(), self.what);
                        self.hits.clear();
                        self.message = None;
                        self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                            search(&root, &needle, what, Some(p))
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
                egui::ScrollArea::vertical().auto_shrink(false).show_rows(
                    ui,
                    22.0,
                    hits.len(),
                    |ui, range| {
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
                    },
                );
            }
            Tab::Compare => {
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
            Tab::Favorites => {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_favorite)
                            .hint_text(r"HKLM\SOFTWARE\…")
                            .desired_width(360.0),
                    );
                    if ui.button("Add").clicked() {
                        let key = self.new_favorite.trim().to_owned();
                        if registry::open_read(&key).is_ok() {
                            self.add_favorite(key);
                            self.new_favorite.clear();
                        } else {
                            self.message = Some(Err(format!("{key} does not exist or cannot be read.")));
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

        let all = SearchIn {
            keys: true,
            values: true,
            data: true,
        };
        let hits = search::<()>(&base, "needle", all, None).unwrap();
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
            None,
        )
        .unwrap();
        assert_eq!(only_keys.len(), 1);
        assert!(search::<()>(&base, "", all, None).is_err());

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
