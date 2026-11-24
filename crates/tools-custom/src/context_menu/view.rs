//! Context Menu Manager window: 14 sections (10 shell locations, a chosen file type,
//! Open With, New menu, Send To), details with company and signature, an editor for
//! new commands, and repair.

use std::collections::HashMap;
use std::path::PathBuf;

use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Tool,
    egui::{self, Vec2},
    fileinfo::{self, FileInfo},
    icons,
    process::restart_explorer,
    registry,
    shell::{reveal_in_explorer, show_properties},
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, section_nav, show_message, status_bar, toolbar},
};

use super::builtins::{BUILTINS, NewCommand, Source, add_command};
use super::{
    Entry, Kind, LOCATIONS, NewItem, OpenWithApp, SendToItem, add_new_item, add_send_to,
    classic_menu_enabled, delete_entry, delete_key_journaled, enumerate, enumerate_new_items,
    enumerate_open_with, enumerate_send_to, file_type_classes, send_to_dir, set_classic_menu, set_enabled,
    set_extended, set_new_item_enabled, set_open_with_hidden, set_position, set_send_to_enabled,
};

const FILE_TYPE: usize = LOCATIONS.len();
const OPEN_WITH: usize = FILE_TYPE + 1;
const NEW_MENU: usize = FILE_TYPE + 2;
const SEND_TO: usize = FILE_TYPE + 3;

enum Ask {
    Delete(Box<Entry>),
    Repair { enable: Vec<Entry>, remove: Vec<Entry> },
    DeleteOpenWith(Box<OpenWithApp>),
    RemoveSendTo(PathBuf),
}

struct Editor {
    label: String,
    builtin: Option<usize>,
    path: String,
    args: String,
    admin: bool,
    locations: Vec<&'static str>,
    extended: bool,
    position: String,
    all_users: bool,
}

pub struct ContextMenuManager {
    section: usize,
    entries: Option<Vec<Entry>>,
    open_with: Option<Vec<OpenWithApp>>,
    new_items: Option<BackgroundTask<Vec<NewItem>>>,
    new_list: Option<Vec<NewItem>>,
    send_to: Option<Vec<SendToItem>>,
    info: HashMap<String, FileInfo>,
    info_task: Option<BackgroundTask<HashMap<PathBuf, FileInfo>>>,
    selected: Option<String>,
    ext: String,
    new_ext: String,
    filter: String,
    editor: Option<Editor>,
    restart_pending: bool,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for ContextMenuManager {
    fn default() -> Self {
        Self {
            section: 0,
            entries: None,
            open_with: None,
            new_items: None,
            new_list: None,
            send_to: None,
            info: HashMap::new(),
            info_task: None,
            selected: None,
            ext: "txt".into(),
            new_ext: String::new(),
            filter: String::new(),
            editor: None,
            restart_pending: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

fn scope(key: &str) -> &'static str {
    if registry::needs_admin(key) {
        "All users"
    } else {
        "You"
    }
}

impl ContextMenuManager {
    fn reload(&mut self) {
        self.entries = None;
        self.open_with = None;
        self.new_list = None;
        self.send_to = None;
    }

    fn classes(&self) -> Vec<String> {
        if self.section == FILE_TYPE {
            file_type_classes(&self.ext)
        } else {
            LOCATIONS[self.section]
                .classes
                .iter()
                .map(|c| (*c).to_owned())
                .collect()
        }
    }

    fn report(&mut self, r: anyhow::Result<String>) {
        self.message = Some(r.map_err(|e| format!("{e:#}")));
        self.reload();
    }

    fn dry(&mut self, ctx: &AppCtx<'_>) -> bool {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
        }
        ctx.config.dry_run
    }

    fn load_info(&mut self, ui: &egui::Ui, entries: &[Entry]) {
        if let Some(t) = &mut self.info_task {
            t.poll();
            if let Some(Ok(map)) = t.take_result() {
                self.info_task = None;
                for (p, i) in map {
                    self.info.insert(p.display().to_string().to_lowercase(), i);
                }
            }
            return;
        }
        let wanted: Vec<PathBuf> = entries
            .iter()
            .filter_map(|e| e.target.clone())
            .filter(|t| !self.info.contains_key(&t.to_lowercase()) && std::path::Path::new(t).exists())
            .map(PathBuf::from)
            .collect();
        if !wanted.is_empty() {
            for w in &wanted {
                // Placeholder so a failed lookup is not retried every frame.
                self.info
                    .insert(w.display().to_string().to_lowercase(), FileInfo::default());
            }
            self.info_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                fileinfo::describe(&wanted)
            }));
        }
    }

    fn handle_confirm(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) else {
            return;
        };
        if self.dry(ctx) {
            return;
        }
        let r: anyhow::Result<String> = match ask {
            Ask::Delete(e) => delete_entry(ctx, &e).map(|_| format!("Removed \"{}\".", e.display)),
            Ask::Repair { enable, remove } => (|| {
                for e in &enable {
                    set_enabled(ctx, e, true)?;
                }
                for e in &remove {
                    delete_entry(ctx, e)?;
                }
                Ok(format!(
                    "Turned {} items back on and removed {} broken items. All of it can be undone in the Change Journal.",
                    enable.len(),
                    remove.len()
                ))
            })(),
            Ask::DeleteOpenWith(a) => {
                delete_key_journaled(ctx, &a.key, format!("Removed \"{}\" from Open with", a.display))
                    .map(|_| format!("Removed \"{}\" from Open with.", a.display))
            }
            Ask::RemoveSendTo(p) => ctx
                .journal
                .recycle(
                    super::TOOL,
                    format!("Removed {} from Send To", p.display()),
                    vec![p.clone()],
                )
                .map(|_| "Moved the item to the Recycle Bin.".into()),
        };
        self.report(r);
    }

    fn editor_window(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ed) = &mut self.editor else { return };
        let mut close = false;
        let mut add = false;
        egui::Window::new("Add a context menu command")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                egui::Grid::new("cm-editor")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Command");
                        egui::ComboBox::from_id_salt("cm-builtin")
                            .width(300.0)
                            .selected_text(
                                ed.builtin
                                    .map_or("A program of your choice", |i| BUILTINS[i].label),
                            )
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(ed.builtin.is_none(), "A program of your choice")
                                    .clicked()
                                {
                                    ed.builtin = None;
                                }
                                for (i, b) in BUILTINS.iter().enumerate() {
                                    if ui.selectable_label(ed.builtin == Some(i), b.label).clicked() {
                                        ed.builtin = Some(i);
                                        ed.label = b.label.trim_end_matches('…').to_owned();
                                    }
                                }
                            });
                        ui.end_row();
                        if ed.builtin.is_none() {
                            ui.label("Program");
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut ed.path).desired_width(230.0));
                                if ui.button("Browse…").clicked()
                                    && let Some(f) = rfd::FileDialog::new()
                                        .add_filter("Programs", &["exe", "bat", "cmd", "com"])
                                        .pick_file()
                                {
                                    ed.path = f.display().to_string();
                                    if ed.label.is_empty() {
                                        ed.label = f
                                            .file_stem()
                                            .map(|s| s.to_string_lossy().into_owned())
                                            .unwrap_or_default();
                                    }
                                }
                            });
                            ui.end_row();
                            ui.label("Arguments");
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.args)
                                    .hint_text(r#""{p}" = the clicked item or folder"#)
                                    .desired_width(300.0),
                            );
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(&mut ed.admin, "Run as administrator");
                            ui.end_row();
                        }
                        ui.label("Name in menu");
                        ui.add(egui::TextEdit::singleline(&mut ed.label).desired_width(300.0));
                        ui.end_row();
                        ui.label("Show on");
                        let needs_path = ed.builtin.is_some_and(|i| BUILTINS[i].needs_path);
                        ui.vertical(|ui| {
                            for l in LOCATIONS.iter() {
                                let usable = !needs_path
                                    || matches!(
                                        l.id,
                                        "files"
                                            | "all"
                                            | "folders"
                                            | "background"
                                            | "desktop"
                                            | "drives"
                                            | "libraries"
                                    );
                                let mut on = ed.locations.contains(&l.id) && usable;
                                if ui
                                    .add_enabled(usable, egui::Checkbox::new(&mut on, l.label))
                                    .changed()
                                {
                                    ed.locations.retain(|x| *x != l.id);
                                    if on {
                                        ed.locations.push(l.id);
                                    }
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("Options");
                        ui.vertical(|ui| {
                            ui.checkbox(&mut ed.extended, "Only when Shift is held");
                            ui.horizontal(|ui| {
                                ui.label("Position");
                                for (v, l) in [("", "Default"), ("Top", "Top"), ("Bottom", "Bottom")] {
                                    ui.radio_value(&mut ed.position, v.to_owned(), l);
                                }
                            });
                            ui.add_enabled(
                                ctx.elevated,
                                egui::Checkbox::new(&mut ed.all_users, "For all users"),
                            )
                            .on_disabled_hover_text("Needs administrator rights");
                        });
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        add = true;
                    }
                    if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                        close = true;
                    }
                });
            });
        if add {
            let ed = self.editor.as_ref().expect("open");
            let needs_path = ed.builtin.is_some_and(|i| BUILTINS[i].needs_path);
            let cmd = NewCommand {
                label: ed.label.clone(),
                locations: ed
                    .locations
                    .iter()
                    .copied()
                    .filter(|l| !needs_path || !matches!(*l, "thispc" | "recyclebin" | "network"))
                    .collect(),
                source: match ed.builtin {
                    Some(i) => Source::Builtin(i),
                    None => Source::Program {
                        path: ed.path.trim().trim_matches('"').to_owned(),
                        args: ed.args.clone(),
                        admin: ed.admin,
                    },
                },
                extended: ed.extended,
                position: ed.position.clone(),
                all_users: ed.all_users,
            };
            if !self.dry(ctx) {
                match add_command(ctx, &cmd) {
                    Ok(keys) => {
                        self.report(Ok(format!(
                            "Added \"{}\" in {} places. On Windows 11 it is under \"Show more options\".",
                            cmd.label.trim(),
                            keys.len()
                        )));
                        close = true;
                    }
                    Err(e) => self.message = Some(Err(format!("{e:#}"))),
                }
            }
        }
        if close {
            self.editor = None;
        }
    }

    fn menu_section(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.section == FILE_TYPE {
            ui.horizontal(|ui| {
                ui.label("File type: .");
                let r = ui.add(egui::TextEdit::singleline(&mut self.ext).desired_width(90.0));
                if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.button("Show").clicked()
                {
                    self.entries = None;
                    self.selected = None;
                }
                ui.weak(format!("Looks in: {}", self.classes().join(", ")));
            });
        }
        let classes = self.classes();
        let entries = self.entries.get_or_insert_with(|| enumerate(&classes)).clone();
        self.load_info(ui, &entries);
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| entries.iter().find(|e| &e.key == k))
            .cloned();
        let can_write = selected
            .as_ref()
            .is_some_and(|e| e.writable(ctx.elevated) && !e.protected());

        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add command")),
                Some(ToolButton::new(icons::TRASH, "Delete").enabled(can_write)),
                None,
                Some(
                    ToolButton::new(icons::INFO, "Properties").enabled(
                        selected
                            .as_ref()
                            .and_then(|e| e.target.as_ref())
                            .is_some_and(|t| std::path::Path::new(t).exists()),
                    ),
                ),
                Some(ToolButton::new(icons::TREE_STRUCTURE, "Registry").enabled(selected.is_some())),
                None,
                Some(ToolButton::new(icons::FIRST_AID_KIT, "Repair")),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
        ) {
            Some(0) => {
                let here = if self.section < LOCATIONS.len() {
                    vec![LOCATIONS[self.section].id]
                } else {
                    vec!["files"]
                };
                self.editor = Some(Editor {
                    label: String::new(),
                    builtin: None,
                    path: String::new(),
                    args: r#""{p}""#.into(),
                    admin: false,
                    locations: here,
                    extended: false,
                    position: String::new(),
                    all_users: false,
                });
            }
            Some(1) => {
                if let Some(e) = &selected {
                    let warn = if e.ours() {
                        ""
                    } else {
                        " It was added by another program, which may need it."
                    };
                    self.confirm.ask(
                        format!(
                            "Remove \"{}\" from the menu? A backup is kept in the Change Journal.{warn}",
                            e.display
                        ),
                        Ask::Delete(Box::new(e.clone())),
                    );
                }
            }
            Some(3) => {
                if let Some(t) = selected.as_ref().and_then(|e| e.target.clone()) {
                    let _ = show_properties(std::path::Path::new(&t));
                }
            }
            Some(4) => {
                if let Some(e) = &selected {
                    let _ = registry::open_in_regedit(&e.key);
                }
            }
            Some(6) => {
                let all_classes: Vec<String> = LOCATIONS
                    .iter()
                    .flat_map(|l| l.classes.iter().map(|c| (*c).to_owned()))
                    .collect();
                let all = enumerate(&all_classes);
                let enable: Vec<Entry> = all
                    .iter()
                    .filter(|e| !e.enabled && e.toggleable() && e.writable(ctx.elevated) && !e.broken)
                    .cloned()
                    .collect();
                let remove: Vec<Entry> = all
                    .iter()
                    .filter(|e| e.broken && !e.protected() && e.writable(ctx.elevated))
                    .cloned()
                    .collect();
                if enable.is_empty() && remove.is_empty() {
                    self.message = Some(Ok(
                        "Nothing to repair: no turned-off or broken entries you can change.".into(),
                    ));
                } else {
                    self.confirm.ask(
                        format!(
                            "Turn {} turned-off entries back on and remove {} entries whose program no longer exists? \
                             Everything is backed up first.",
                            enable.len(),
                            remove.len()
                        ),
                        Ask::Repair { enable, remove },
                    );
                }
            }
            Some(7) => {
                self.reload();
                self.message = None;
            }
            _ => {}
        }
        show_message(ui, &self.message);
        if let Some(e) = &selected {
            self.details(ui, ctx, e);
        }

        let f = self.filter.to_lowercase();
        let visible: Vec<&Entry> = entries
            .iter()
            .filter(|e| {
                f.is_empty() || e.display.to_lowercase().contains(&f) || e.name.to_lowercase().contains(&f)
            })
            .collect();
        let status = format!(
            "{} entries, {} turned off, {} broken",
            visible.len(),
            visible.iter().filter(|e| !e.enabled).count(),
            visible.iter().filter(|e| e.broken).count()
        );
        egui::Panel::bottom("cm-status")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                status_bar(ui, &status, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text(ctx.i18n.tr("common-filter"))
                            .desired_width(160.0),
                    );
                })
            });
        let mut toggle = None;
        let mut clicked = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(24.0))
            .column(Column::initial(230.0).clip(true))
            .column(Column::initial(72.0))
            .column(Column::initial(44.0))
            .column(Column::initial(60.0))
            .column(Column::initial(150.0).clip(true))
            .column(Column::initial(80.0))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in [
                    "",
                    "Name",
                    "Type",
                    "Shift",
                    "Position",
                    "Company",
                    "Signature",
                    "For",
                    "",
                ] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(21.0, visible.len(), |mut row| {
                    let e = visible[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(e.key.as_str()));
                    let info = e.target.as_ref().and_then(|t| self.info.get(&t.to_lowercase()));
                    row.col(|ui| {
                        let mut on = e.enabled;
                        let ok = e.toggleable() && e.writable(ctx.elevated);
                        if ui
                            .add_enabled(ok, egui::Checkbox::without_text(&mut on))
                            .on_disabled_hover_text(if e.protected() {
                                "Core Explorer command".to_owned()
                            } else {
                                ctx.i18n.tr("shell-needs-admin")
                            })
                            .changed()
                        {
                            toggle = Some((e.clone(), on));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&e.display).on_hover_text(&e.key);
                    });
                    row.col(|ui| {
                        ui.weak(match e.kind {
                            Kind::Command => "Command",
                            Kind::Handler => "Extension",
                        });
                    });
                    row.col(|ui| {
                        if e.extended {
                            ui.label("Yes");
                        }
                    });
                    row.col(|ui| {
                        ui.weak(&e.position);
                    });
                    row.col(|ui| {
                        ui.weak(info.map(|i| i.company.as_str()).unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.weak(info.map(|i| i.signature.label()).unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.weak(scope(&e.key));
                    });
                    row.col(|ui| {
                        if e.broken {
                            ui.colored_label(ui.visuals().warn_fg_color, "Program missing");
                        } else if e.ours() {
                            ui.weak("Added here");
                        }
                    });
                    if row.response().clicked() {
                        clicked = Some(e.key.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
        if let Some((e, on)) = toggle
            && !self.dry(ctx)
        {
            let r = set_enabled(ctx, &e, on).map(|_| {
                format!(
                    "{} \"{}\".",
                    if on { "Turned on" } else { "Turned off" },
                    e.display
                )
            });
            self.report(r);
        }
    }

    fn details(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>, e: &Entry) {
        let writable = e.writable(ctx.elevated) && !e.protected();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(&e.display);
                ui.weak(&e.key);
            });
            if let Some(t) = &e.target {
                ui.horizontal(|ui| {
                    ui.label("Runs:");
                    if ui.link(t).clicked() {
                        reveal_in_explorer(std::path::Path::new(t));
                    }
                    if let Some(i) = self.info.get(&t.to_lowercase()) {
                        ui.weak(format!("{} {}", i.description, i.version));
                    }
                });
            }
            if e.kind == Kind::Command {
                ui.horizontal(|ui| {
                    let mut shift = e.extended;
                    if ui
                        .add_enabled(
                            writable,
                            egui::Checkbox::new(&mut shift, "Only when Shift is held"),
                        )
                        .changed()
                        && !self.dry(ctx)
                    {
                        let r = set_extended(ctx, e, shift).map(|_| "Saved.".to_owned());
                        self.report(r);
                    }
                    ui.separator();
                    ui.label("Position:");
                    for (v, l) in [("", "Default"), ("Top", "Top"), ("Bottom", "Bottom")] {
                        if ui
                            .add_enabled(
                                writable,
                                egui::RadioButton::new(e.position.eq_ignore_ascii_case(v), l),
                            )
                            .clicked()
                            && !e.position.eq_ignore_ascii_case(v)
                            && !self.dry(ctx)
                        {
                            let r = set_position(ctx, e, v).map(|_| "Saved.".to_owned());
                            self.report(r);
                        }
                    }
                });
            }
        });
    }

    fn open_with_section(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let apps = self.open_with.get_or_insert_with(enumerate_open_with).clone();
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| apps.iter().find(|a| &a.key == k))
            .cloned();
        match toolbar(
            ui,
            &[
                Some(
                    ToolButton::new(icons::TRASH, "Delete").enabled(
                        selected
                            .as_ref()
                            .is_some_and(|a| ctx.elevated || !registry::needs_admin(&a.key)),
                    ),
                ),
                Some(ToolButton::new(icons::TREE_STRUCTURE, "Registry").enabled(selected.is_some())),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
        ) {
            Some(0) => {
                if let Some(a) = &selected {
                    self.confirm.ask(
                        format!(
                            "Remove \"{}\" from Open with? A backup is kept in the Change Journal.",
                            a.display
                        ),
                        Ask::DeleteOpenWith(Box::new(a.clone())),
                    );
                }
            }
            Some(1) => {
                if let Some(a) = &selected {
                    let _ = registry::open_in_regedit(&a.key);
                }
            }
            Some(2) => self.open_with = None,
            _ => {}
        }
        ui.weak("Untick a program to leave it out of every \"Open with\" list.");
        show_message(ui, &self.message);
        let mut toggle = None;
        let mut clicked = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(24.0))
            .column(Column::initial(220.0).clip(true))
            .column(Column::initial(150.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["", "Program", "File", "For", "Runs"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(21.0, apps.len(), |mut row| {
                    let a = &apps[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(a.key.as_str()));
                    row.col(|ui| {
                        let mut shown = !a.hidden;
                        let ok = ctx.elevated || !registry::needs_admin(&a.key);
                        if ui
                            .add_enabled(ok, egui::Checkbox::without_text(&mut shown))
                            .changed()
                        {
                            toggle = Some((a.clone(), !shown));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&a.display);
                    });
                    row.col(|ui| {
                        ui.weak(&a.exe);
                    });
                    row.col(|ui| {
                        ui.weak(scope(&a.key));
                    });
                    row.col(|ui| {
                        if a.broken {
                            ui.colored_label(ui.visuals().warn_fg_color, "Program missing");
                        } else {
                            ui.weak(a.target.clone().unwrap_or_default());
                        }
                    });
                    if row.response().clicked() {
                        clicked = Some(a.key.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
        if let Some((a, hide)) = toggle
            && !self.dry(ctx)
        {
            let r = set_open_with_hidden(ctx, &a, hide).map(|_| "Saved.".to_owned());
            self.report(r);
        }
    }

    fn new_menu_section(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.new_list.is_none() && self.new_items.is_none() {
            self.new_items = Some(BackgroundTask::spawn(ui.ctx(), |_| Ok(enumerate_new_items())));
        }
        if let Some(t) = &mut self.new_items {
            t.poll();
            if let Some(r) = t.take_result() {
                self.new_items = None;
                self.new_list = Some(r.unwrap_or_default());
            }
        }
        ui.horizontal(|ui| {
            ui.label("Add a file type: .");
            ui.add(egui::TextEdit::singleline(&mut self.new_ext).desired_width(80.0));
            if ui
                .add_enabled(
                    !self.new_ext.trim().is_empty(),
                    egui::Button::new("Add to New menu"),
                )
                .clicked()
                && !self.dry(ctx)
            {
                let r =
                    add_new_item(ctx, &self.new_ext).map(|name| format!("Added \"{name}\" to the New menu."));
                self.new_ext.clear();
                self.report(r);
            }
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.new_list = None;
            }
        });
        ui.weak("Untick a file type to remove it from the desktop and folder \"New\" menu.");
        show_message(ui, &self.message);
        let Some(items) = self.new_list.clone() else {
            ui.spinner();
            return;
        };
        let mut toggle = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(24.0))
            .column(Column::initial(260.0).clip(true))
            .column(Column::initial(90.0))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["", "File type", "Extension", "For", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(21.0, items.len(), |mut row| {
                    let i = &items[row.index()];
                    row.col(|ui| {
                        let mut on = i.enabled;
                        let ok = ctx.elevated || !registry::needs_admin(&i.parent);
                        if ui
                            .add_enabled(ok, egui::Checkbox::without_text(&mut on))
                            .changed()
                        {
                            toggle = Some((i.clone(), on));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&i.display);
                    });
                    row.col(|ui| {
                        ui.weak(&i.ext);
                    });
                    row.col(|ui| {
                        ui.weak(scope(&i.parent));
                    });
                    row.col(|ui| {
                        if ui.small_button("Registry").clicked() {
                            let _ = registry::open_in_regedit(&i.key());
                        }
                    });
                });
            });
        if let Some((i, on)) = toggle
            && !self.dry(ctx)
        {
            let r = set_new_item_enabled(ctx, &i, on).map(|_| "Saved.".to_owned());
            self.report(r);
        }
    }

    fn send_to_section(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let items = self.send_to.get_or_insert_with(enumerate_send_to).clone();
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| items.iter().find(|i| &i.path.display().to_string() == k))
            .cloned();
        match toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add program")),
                Some(ToolButton::new(icons::FOLDERS, "Add folder")),
                Some(
                    ToolButton::new(icons::TRASH, "Remove")
                        .enabled(selected.as_ref().is_some_and(|s| s.enabled)),
                ),
                None,
                Some(ToolButton::new(icons::FOLDER_OPEN, "Open folder")),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
        ) {
            Some(i @ (0 | 1)) => {
                let picked = if i == 0 {
                    rfd::FileDialog::new()
                        .add_filter("Programs", &["exe", "bat", "cmd", "lnk"])
                        .pick_file()
                } else {
                    rfd::FileDialog::new().pick_folder()
                };
                if let Some(t) = picked
                    && !self.dry(ctx)
                {
                    let name = t
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let r = add_send_to(ctx, &t, &name).map(|_| format!("Added \"{name}\" to Send To."));
                    self.report(r);
                }
            }
            Some(2) => {
                if let Some(s) = &selected {
                    self.confirm.ask(
                        format!("Move \"{}\" to the Recycle Bin?", s.display),
                        Ask::RemoveSendTo(s.path.clone()),
                    );
                }
            }
            Some(4) => {
                let _ = std::process::Command::new("explorer").arg(send_to_dir()).spawn();
            }
            Some(5) => self.send_to = None,
            _ => {}
        }
        ui.weak("Untick an item to hide it from the \"Send to\" menu; it is kept and can be ticked again.");
        show_message(ui, &self.message);
        let mut toggle = None;
        let mut clicked = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(24.0))
            .column(Column::initial(240.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["", "Item", "File"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(21.0, items.len(), |mut row| {
                    let s = &items[row.index()];
                    let key = s.path.display().to_string();
                    row.set_selected(self.selected.as_deref() == Some(key.as_str()));
                    row.col(|ui| {
                        let mut on = s.enabled;
                        if ui.checkbox(&mut on, "").changed() {
                            toggle = Some((s.clone(), on));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&s.display);
                    });
                    row.col(|ui| {
                        ui.weak(
                            s.path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        );
                    });
                    if row.response().clicked() {
                        clicked = Some(key);
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
        if let Some((s, on)) = toggle
            && !self.dry(ctx)
        {
            let r = set_send_to_enabled(ctx, &s, on).map(|_| "Saved.".to_owned());
            self.report(r);
        }
    }
}

impl Tool for ContextMenuManager {
    fn id(&self) -> &'static str {
        "contextmenu"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.handle_confirm(ui, ctx);
        self.editor_window(ui, ctx);
        ui.horizontal(|ui| {
            let mut classic = classic_menu_enabled();
            if ui
                .checkbox(
                    &mut classic,
                    "Always show the full classic context menu (Windows 11)",
                )
                .changed()
                && !self.dry(ctx)
            {
                let r = set_classic_menu(ctx, classic)
                    .map(|_| "Saved. Restart Explorer to see the change.".to_owned());
                self.restart_pending = r.is_ok();
                self.report(r);
            }
            if self.restart_pending && ui.button("Restart Explorer").clicked() {
                self.message = Some(
                    restart_explorer()
                        .map(|()| "Explorer restarted.".into())
                        .map_err(|e| format!("{e:#}")),
                );
                self.restart_pending = false;
            }
        });
        let mut nav: Vec<(&str, &str)> = LOCATIONS.iter().map(|l| (l.icon, l.label)).collect();
        nav.extend([
            (icons::FILE_SEARCH, "File type…"),
            (icons::APP_WINDOW, "Open with"),
            (icons::PLUS, "New menu"),
            (icons::ARROW_SQUARE_OUT, "Send to"),
        ]);
        egui::Panel::left("cm-sections")
            .resizable(false)
            .exact_size(190.0)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                right: 8,
                ..Default::default()
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if section_nav(ui, &nav, &mut self.section) {
                        self.entries = None;
                        self.selected = None;
                        self.message = None;
                    }
                });
            });
        match self.section {
            OPEN_WITH => self.open_with_section(ui, ctx),
            NEW_MENU => self.new_menu_section(ui, ctx),
            SEND_TO => self.send_to_section(ui, ctx),
            _ => self.menu_section(ui, ctx),
        }
    }
}
