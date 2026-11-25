//! File Explorer Tweaker settings too rich for a switch: what dragging files does
//! (for everything or per file type), and the names Explorer gives new items and copies.

use anyhow::{Result, bail};
use wm_core::{
    AppCtx, egui, registry,
    safety::UndoAction,
    tweaks::ExtraSetting,
    ui::{Message, show_message},
};

const TOOL: &str = "explorertweaks";

// ---------------------------------------------------------------------------------
// Drag action

const CLASSES: &str = r"HKCU\Software\Classes";
const DROP_VALUE: &str = "DefaultDropEffect";
/// Keys read for every file and folder. Both are set so files and folders agree.
const ALL_KEYS: [&str; 2] = ["AllFilesystemObjects", "*"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropAction {
    /// Windows decides: move on the same drive, copy to another drive.
    Default,
    Copy,
    Move,
    Link,
}

impl DropAction {
    pub const ALL: [DropAction; 4] = [
        DropAction::Default,
        DropAction::Copy,
        DropAction::Move,
        DropAction::Link,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DropAction::Default => "Windows default",
            DropAction::Copy => "Copy",
            DropAction::Move => "Move",
            DropAction::Link => "Create shortcut",
        }
    }

    /// The `DROPEFFECT_*` value, or `None` for no value.
    pub fn value(self) -> Option<u32> {
        match self {
            DropAction::Default => None,
            DropAction::Copy => Some(1),
            DropAction::Move => Some(2),
            DropAction::Link => Some(4),
        }
    }

    pub fn from_value(v: Option<u32>) -> Self {
        match v {
            Some(1) => DropAction::Copy,
            Some(2) => DropAction::Move,
            Some(4) => DropAction::Link,
            _ => DropAction::Default,
        }
    }
}

/// Per-type settings live under SystemFileAssociations, which Explorer reads for a
/// file type whichever app opens it, and which can be created per user without
/// shadowing the type's own key.
fn type_key(ext: &str) -> String {
    format!(r"{CLASSES}\SystemFileAssociations\{ext}")
}

fn read_action(key: &str) -> DropAction {
    DropAction::from_value(registry::read_raw(key, DROP_VALUE).and_then(|v| registry::decode_dword(&v)))
}

fn write_action(keys: &[String], action: DropAction) -> Result<Vec<registry::ValueSnapshot>> {
    let snaps: Vec<_> = keys.iter().map(|k| registry::snapshot(k, DROP_VALUE)).collect();
    for k in keys {
        match action.value() {
            Some(v) => registry::write_raw(k, DROP_VALUE, &registry::dword(v))?,
            None => {
                registry::delete_value(k, DROP_VALUE)?;
                registry::prune_empty_class_override(k);
            }
        }
    }
    Ok(snaps)
}

/// Turns what the user typed ("txt", ".TXT") into ".txt", or explains why not.
pub fn normalize_ext(input: &str) -> Result<String> {
    let s = input.trim().trim_start_matches('.').to_ascii_lowercase();
    if s.is_empty() {
        bail!("Type a file extension, such as txt.");
    }
    if s.chars()
        .any(|c| c.is_whitespace() || r#"\/:*?"<>|."#.contains(c))
    {
        bail!("\"{}\" isn't a file extension.", input.trim());
    }
    Ok(format!(".{s}"))
}

/// File types that have their own drag action set.
fn typed_actions() -> Vec<(String, DropAction)> {
    let Ok(k) = registry::open_read(&format!(r"{CLASSES}\SystemFileAssociations")) else {
        return Vec::new();
    };
    let mut out: Vec<(String, DropAction)> = k
        .enum_keys()
        .flatten()
        .filter(|name| name.starts_with('.'))
        .filter_map(|name| match read_action(&type_key(&name)) {
            DropAction::Default => None,
            a => Some((name.to_ascii_lowercase(), a)),
        })
        .collect();
    out.sort();
    out
}

pub struct DragActionSetting {
    all: DropAction,
    types: Vec<(String, DropAction)>,
    new_ext: String,
    new_action: DropAction,
    message: Message,
}

impl Default for DragActionSetting {
    fn default() -> Self {
        let mut s = Self {
            all: DropAction::Default,
            types: Vec::new(),
            new_ext: String::new(),
            new_action: DropAction::Move,
            message: None,
        };
        s.reload();
        s
    }
}

impl DragActionSetting {
    fn reload(&mut self) {
        self.all = read_action(&format!(r"{CLASSES}\{}", ALL_KEYS[0]));
        self.types = typed_actions();
    }

    fn apply(&mut self, ctx: &AppCtx<'_>, ext: Option<&str>, action: DropAction) -> Result<String> {
        let (keys, what) = match ext {
            None => (
                ALL_KEYS
                    .iter()
                    .map(|k| format!(r"{CLASSES}\{k}"))
                    .collect::<Vec<_>>(),
                "all files and folders".to_owned(),
            ),
            Some(e) => (vec![type_key(e)], format!("{e} files")),
        };
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        let snaps = write_action(&keys, action)?;
        ctx.journal.record(
            TOOL,
            format!("Drag action for {what} set to: {}", action.label()),
            UndoAction::RestoreRegValues { values: snaps },
        )?;
        Ok(format!("Dragging {what} now does: {}.", action.label()))
    }
}

fn action_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    action: &mut DropAction,
) -> bool {
    let before = *action;
    egui::ComboBox::from_id_salt(id)
        .selected_text(action.label())
        .show_ui(ui, |ui| {
            for a in DropAction::ALL {
                ui.selectable_value(action, a, a.label());
            }
        });
    *action != before
}

impl ExtraSetting for DragActionSetting {
    fn group(&self) -> &str {
        "Files"
    }

    fn title(&self) -> &str {
        "What dragging files does"
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.label(self.title());
        ui.indent("drag-action", |ui| {
            ui.weak("By default Windows moves files dragged within a drive and copies them to another drive. Hold Ctrl, Shift or Alt while dropping to choose for one drag.");
            ui.horizontal(|ui| {
                ui.label("All files and folders");
                let mut all = self.all;
                if action_combo(ui, "drag-all", &mut all) {
                    self.message = Some(self.apply(ctx, None, all).map_err(|e| format!("{e:#}")));
                    self.reload();
                }
            });
            let mut change: Option<(String, DropAction)> = None;
            for (ext, action) in &self.types {
                ui.horizontal(|ui| {
                    ui.label(format!("{ext} files"));
                    let mut a = *action;
                    if action_combo(ui, ("drag-type", ext), &mut a) {
                        change = Some((ext.clone(), a));
                    }
                    if ui.small_button("Remove").clicked() {
                        change = Some((ext.clone(), DropAction::Default));
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.label("Add a file type");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_ext)
                        .hint_text("e.g. zip")
                        .desired_width(70.0),
                );
                action_combo(ui, "drag-new", &mut self.new_action);
                let can_add = !self.new_ext.trim().is_empty() && self.new_action != DropAction::Default;
                if ui.add_enabled(can_add, egui::Button::new("Add")).clicked() {
                    match normalize_ext(&self.new_ext) {
                        Ok(ext) => {
                            change = Some((ext, self.new_action));
                            self.new_ext.clear();
                        }
                        Err(e) => self.message = Some(Err(format!("{e:#}"))),
                    }
                }
            });
            if let Some((ext, action)) = change {
                self.message = Some(self.apply(ctx, Some(&ext), action).map_err(|e| format!("{e:#}")));
                self.reload();
            }
            show_message(ui, &self.message);
        });
    }
}

// ---------------------------------------------------------------------------------
// Naming templates

const NAMING: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\NamingTemplates";
const NEW_VALUE: &str = "RenameNameTemplate";
const COPY_VALUE: &str = "CopyNameTemplate";
const COPY_DEFAULT: (&str, &str) = ("", " - Copy");

fn check_name_part(s: &str) -> Result<()> {
    if let Some(c) = s.chars().find(|c| r#"\/:*?"<>|"#.contains(*c)) {
        bail!("File names can't contain {c}");
    }
    if s.contains('%') {
        bail!("Leave out %; it has a special meaning in name templates.");
    }
    Ok(())
}

/// The CopyNameTemplate value for text before and after the original name.
pub fn copy_template(prefix: &str, suffix: &str) -> Result<String> {
    check_name_part(prefix)?;
    check_name_part(suffix)?;
    if prefix.trim().is_empty() && suffix.trim().is_empty() {
        bail!("Add some text before or after the name, or copies would get the same name.");
    }
    Ok(format!("{prefix}%s{suffix}"))
}

/// Splits a CopyNameTemplate value back into the text before and after the name.
pub fn split_copy_template(v: &str) -> Option<(String, String)> {
    let (p, s) = v.split_once("%s")?;
    Some((p.to_owned(), s.to_owned()))
}

/// The RenameNameTemplate value: the user's text, where "*" stands for the item type
/// ("Folder", "Text Document"…), which Explorer writes as %s.
pub fn new_template(text: &str) -> Result<String> {
    let t = text.trim();
    if t.is_empty() {
        bail!("Type a name, or use Reset for \"New folder\".");
    }
    let parts: Vec<&str> = t.split('*').collect();
    if parts.len() > 2 {
        bail!("Use * at most once.");
    }
    for p in &parts {
        check_name_part(p)?;
    }
    Ok(parts.join("%s"))
}

pub fn new_template_text(v: &str) -> String {
    v.replacen("%s", "*", 1)
}

fn read_sz(name: &str) -> Option<String> {
    registry::read_raw(NAMING, name).map(|v| registry::decode_string(&v))
}

pub struct NamingTemplateSetting {
    new_saved: Option<String>,
    new_text: String,
    copy_saved: Option<String>,
    prefix: String,
    suffix: String,
    message: Message,
    restart_offered: bool,
}

impl Default for NamingTemplateSetting {
    fn default() -> Self {
        let mut s = Self {
            new_saved: None,
            new_text: String::new(),
            copy_saved: None,
            prefix: String::new(),
            suffix: String::new(),
            message: None,
            restart_offered: false,
        };
        s.reload();
        s
    }
}

impl NamingTemplateSetting {
    fn reload(&mut self) {
        self.new_saved = read_sz(NEW_VALUE);
        self.new_text = self
            .new_saved
            .as_deref()
            .map_or_else(|| "New *".to_owned(), new_template_text);
        self.copy_saved = read_sz(COPY_VALUE);
        let (p, s) = self
            .copy_saved
            .as_deref()
            .and_then(split_copy_template)
            .unwrap_or_else(|| (COPY_DEFAULT.0.into(), COPY_DEFAULT.1.into()));
        self.prefix = p;
        self.suffix = s;
    }

    fn save(&mut self, ctx: &AppCtx<'_>, name: &str, value: Option<String>, what: &str) -> Result<String> {
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        let snap = registry::snapshot(NAMING, name);
        match &value {
            Some(v) => registry::write_raw(NAMING, name, &registry::string(v))?,
            None => registry::delete_value(NAMING, name)?,
        }
        let desc = match &value {
            Some(v) => format!("{what} name template set to: {v}"),
            None => format!("{what} name template reset"),
        };
        ctx.journal
            .record(TOOL, desc, UndoAction::RestoreRegValues { values: vec![snap] })?;
        self.restart_offered = true;
        Ok("Saved. Restart Explorer for new windows to use it.".into())
    }

    fn run(&mut self, ctx: &AppCtx<'_>, name: &str, value: Result<Option<String>>, what: &str) {
        self.message = Some(
            value
                .and_then(|v| self.save(ctx, name, v, what))
                .map_err(|e| format!("{e:#}")),
        );
        self.reload();
    }
}

impl ExtraSetting for NamingTemplateSetting {
    fn group(&self) -> &str {
        "Files"
    }

    fn title(&self) -> &str {
        "Names for new folders and copies"
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.label(self.title());
        ui.indent("naming-templates", |ui| {
            ui.weak("* stands for the kind of item, such as Folder or Text Document. Windows adds (2), (3)… when a name is taken.");
            ui.horizontal(|ui| {
                ui.label("New items");
                ui.add(egui::TextEdit::singleline(&mut self.new_text).desired_width(140.0));
                let current = self.new_saved.as_deref().map_or("New *".to_owned(), new_template_text);
                if ui
                    .add_enabled(self.new_text.trim() != current, egui::Button::new("Apply"))
                    .clicked()
                {
                    let v = new_template(&self.new_text).map(Some);
                    self.run(ctx, NEW_VALUE, v, "New item");
                }
                if ui
                    .add_enabled(self.new_saved.is_some(), egui::Button::new("Reset"))
                    .on_hover_text("Back to \"New folder\", \"New Text Document\"…")
                    .clicked()
                {
                    self.run(ctx, NEW_VALUE, Ok(None), "New item");
                }
            });
            ui.horizontal(|ui| {
                ui.label("Copies");
                ui.add(egui::TextEdit::singleline(&mut self.prefix).desired_width(70.0));
                ui.weak("name");
                ui.add(egui::TextEdit::singleline(&mut self.suffix).desired_width(70.0));
                let current = self
                    .copy_saved
                    .as_deref()
                    .and_then(split_copy_template)
                    .unwrap_or_else(|| (COPY_DEFAULT.0.into(), COPY_DEFAULT.1.into()));
                let changed = (self.prefix.as_str(), self.suffix.as_str()) != (current.0.as_str(), current.1.as_str());
                if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                    let v = copy_template(&self.prefix, &self.suffix).map(Some);
                    self.run(ctx, COPY_VALUE, v, "Copy");
                }
                if ui
                    .add_enabled(self.copy_saved.is_some(), egui::Button::new("Reset"))
                    .on_hover_text("Back to \"name - Copy\"")
                    .clicked()
                {
                    self.run(ctx, COPY_VALUE, Ok(None), "Copy");
                }
            });
            ui.weak(format!(
                "Example: {}",
                [
                    new_template(&self.new_text)
                        .map(|t| t.replacen("%s", "Folder", 1))
                        .unwrap_or_else(|_| "…".into()),
                    format!("{}Report{}.docx", self.prefix, self.suffix),
                ]
                .join("  ·  ")
            ));
            show_message(ui, &self.message);
            if self.restart_offered && ui.button("Restart Explorer").clicked() {
                self.restart_offered = false;
                self.message = Some(
                    wm_core::process::restart_explorer()
                        .map(|()| "Explorer restarted.".to_owned())
                        .map_err(|e| format!("{e:#}")),
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_values_round_trip() {
        for a in DropAction::ALL {
            assert_eq!(DropAction::from_value(a.value()), a);
        }
        assert_eq!(DropAction::from_value(Some(7)), DropAction::Default);
    }

    #[test]
    fn extensions_are_normalized() {
        assert_eq!(normalize_ext(" .ZIP ").unwrap(), ".zip");
        assert_eq!(normalize_ext("txt").unwrap(), ".txt");
        assert!(normalize_ext("").is_err());
        assert!(normalize_ext("a b").is_err());
        assert!(normalize_ext("tar.gz").is_err());
        assert!(normalize_ext(r"..\x").is_err());
    }

    #[test]
    fn copy_templates() {
        assert_eq!(copy_template("", " (copy)").unwrap(), "%s (copy)");
        assert_eq!(copy_template("Copy of ", "").unwrap(), "Copy of %s");
        assert!(copy_template("", " ").is_err());
        assert!(copy_template("", " 50%").is_err());
        assert!(copy_template("a:", "").is_err());
        assert_eq!(
            split_copy_template("Copy of %s (2)"),
            Some(("Copy of ".into(), " (2)".into()))
        );
        assert_eq!(split_copy_template("odd"), None);
    }

    #[test]
    fn new_item_templates() {
        assert_eq!(new_template("*").unwrap(), "%s");
        assert_eq!(new_template(" New * ").unwrap(), "New %s");
        assert_eq!(new_template("Untitled").unwrap(), "Untitled");
        assert!(new_template("* *").is_err());
        assert!(new_template("").is_err());
        assert_eq!(new_template_text("New %s"), "New *");
    }

    #[test]
    fn per_type_action_round_trips_and_cleans_up() {
        // A made-up extension so no real file type is touched.
        let ext = ".wmprobedrag";
        let key = type_key(ext);
        let snaps = write_action(std::slice::from_ref(&key), DropAction::Link).unwrap();
        assert_eq!(read_action(&key), DropAction::Link);
        assert!(
            typed_actions()
                .iter()
                .any(|(e, a)| e == ext && *a == DropAction::Link)
        );
        for s in &snaps {
            registry::restore(s).unwrap();
        }
        assert_eq!(read_action(&key), DropAction::Default);
        assert!(registry::open_read(&key).is_err(), "empty key should be pruned");
    }
}
