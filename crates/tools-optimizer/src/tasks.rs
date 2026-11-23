//! Task Scheduler Manager: browse scheduled tasks (COM `ITaskService`), enable/disable,
//! run/end, and delete with an XML backup so deletion can be undone.

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate};
use egui_extras::{Column, TableBuilder};
use windows::Win32::Foundation::VARIANT_FALSE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, IRegisteredTask, ITaskFolder, ITaskService, TASK_ACTION_EXEC, TASK_ACTION_TYPE,
    TASK_ENUM_HIDDEN, TASK_STATE_DISABLED, TASK_STATE_QUEUED, TASK_STATE_READY, TASK_STATE_RUNNING,
    TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, Interface};
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    /// Full path, e.g. `\Microsoft\Windows\Defrag\ScheduledDefrag`.
    pub path: String,
    pub name: String,
    pub state: &'static str,
    pub enabled: bool,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
    pub last_result: i32,
    pub author: String,
    pub description: String,
    pub actions: String,
}

impl TaskRow {
    pub fn folder(&self) -> &str {
        self.path
            .rsplit_once('\\')
            .map_or("\\", |(f, _)| if f.is_empty() { "\\" } else { f })
    }

    pub fn is_microsoft(&self) -> bool {
        self.path.starts_with(r"\Microsoft\")
    }
}

/// Converts an OLE Automation date (days since 1899-12-30, local time) to text.
/// Task Scheduler uses 0 or dates before 2000 for "never".
pub fn ole_date(value: f64) -> Option<String> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let base = NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    let t = base + Duration::milliseconds((value * 86_400_000.0).round() as i64);
    (t.date() >= NaiveDate::from_ymd_opt(2000, 1, 1)?).then(|| t.format("%Y-%m-%d %H:%M").to_string())
}

/// Text for a task's last run result code.
pub fn result_text(code: i32) -> String {
    match code as u32 {
        0 => "Success".into(),
        0x0004_1301 => "Running".into(),
        0x0004_1303 => "Has not run yet".into(),
        0x0004_1306 => "Terminated by user".into(),
        0x8004_131F => "Already running".into(),
        c => format!("0x{c:08X}"),
    }
}

fn bstr_out(f: impl FnOnce(*mut BSTR) -> windows::core::Result<()>) -> String {
    let mut b = BSTR::new();
    if f(&mut b).is_ok() {
        b.to_string()
    } else {
        String::new()
    }
}

unsafe fn connect() -> Result<ITaskService> {
    unsafe {
        // S_FALSE (already initialised) is fine; RPC_E_CHANGED_MODE would mean an STA thread.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let empty = VARIANT::default();
        service
            .Connect(&empty, &empty, &empty, &empty)
            .context("connecting to Task Scheduler")?;
        Ok(service)
    }
}

unsafe fn describe(task: &IRegisteredTask) -> Result<TaskRow> {
    unsafe {
        let state = match task.State()? {
            TASK_STATE_DISABLED => "Disabled",
            TASK_STATE_QUEUED => "Queued",
            TASK_STATE_READY => "Ready",
            TASK_STATE_RUNNING => "Running",
            _ => "Unknown",
        };
        let (mut author, mut description, mut actions) = (String::new(), String::new(), Vec::new());
        if let Ok(def) = task.Definition() {
            if let Ok(info) = def.RegistrationInfo() {
                author = bstr_out(|p| info.Author(p));
                description = bstr_out(|p| info.Description(p));
            }
            if let Ok(coll) = def.Actions() {
                let mut count = 0;
                let _ = coll.Count(&mut count);
                for i in 1..=count {
                    let Ok(action) = coll.get_Item(i) else { continue };
                    let mut kind = TASK_ACTION_TYPE(-1);
                    let _ = action.Type(&mut kind);
                    if kind == TASK_ACTION_EXEC
                        && let Ok(exec) = action.cast::<IExecAction>()
                    {
                        let path = bstr_out(|p| exec.Path(p));
                        let args = bstr_out(|p| exec.Arguments(p));
                        actions.push(if args.is_empty() {
                            path
                        } else {
                            format!("{path} {args}")
                        });
                    } else {
                        actions.push("(COM handler / other action)".into());
                    }
                }
            }
        }
        Ok(TaskRow {
            path: task.Path()?.to_string(),
            name: task.Name()?.to_string(),
            state,
            enabled: task.Enabled()? != VARIANT_FALSE,
            last_run: task.LastRunTime().ok().and_then(ole_date),
            next_run: task.NextRunTime().ok().and_then(ole_date),
            last_result: task.LastTaskResult().unwrap_or(0),
            author,
            description,
            actions: actions.join(" ; "),
        })
    }
}

unsafe fn walk(folder: &ITaskFolder, out: &mut Vec<TaskRow>) -> Result<()> {
    unsafe {
        let tasks = folder.GetTasks(TASK_ENUM_HIDDEN.0)?;
        for i in 1..=tasks.Count()? {
            if let Ok(t) = tasks.get_Item(&VARIANT::from(i))
                && let Ok(row) = describe(&t)
            {
                out.push(row);
            }
        }
        let subfolders = folder.GetFolders(0)?;
        for i in 1..=subfolders.Count()? {
            if let Ok(sub) = subfolders.get_Item(&VARIANT::from(i)) {
                // Folders we may not read (access denied) are skipped.
                let _ = walk(&sub, out);
            }
        }
        Ok(())
    }
}

pub fn enumerate() -> Result<Vec<TaskRow>> {
    unsafe {
        let service = connect()?;
        let root = service.GetFolder(&BSTR::from("\\"))?;
        let mut rows = Vec::new();
        walk(&root, &mut rows)?;
        rows.sort_by_key(|r| r.path.to_lowercase());
        Ok(rows)
    }
}

/// The task's definition XML (used as the deletion backup).
pub fn task_xml(path: &str) -> Result<String> {
    unsafe {
        let service = connect()?;
        let (folder, name) = path.rsplit_once('\\').context("invalid task path")?;
        let folder = service.GetFolder(&BSTR::from(if folder.is_empty() { "\\" } else { folder }))?;
        Ok(folder.GetTask(&BSTR::from(name))?.Xml()?.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Enable,
    Disable,
    Run,
    End,
    Delete,
}

fn schtasks(args: &[&str]) -> Result<()> {
    run_checked(hidden_command("schtasks").args(args)).map(|_| ())
}

fn undo_cmd(args: &[&str]) -> UndoAction {
    UndoAction::RunCommand {
        program: "schtasks".into(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// Performs `action` and returns (description, undo).
pub fn perform(task: &TaskRow, action: Action) -> Result<(String, UndoAction)> {
    let p = task.path.as_str();
    Ok(match action {
        Action::Enable => {
            schtasks(&["/change", "/tn", p, "/enable"])?;
            (
                format!("Enabled task {p}"),
                undo_cmd(&["/change", "/tn", p, "/disable"]),
            )
        }
        Action::Disable => {
            schtasks(&["/change", "/tn", p, "/disable"])?;
            (
                format!("Disabled task {p}"),
                undo_cmd(&["/change", "/tn", p, "/enable"]),
            )
        }
        Action::Run => {
            schtasks(&["/run", "/tn", p])?;
            (format!("Started task {p}"), UndoAction::None)
        }
        Action::End => {
            schtasks(&["/end", "/tn", p])?;
            (format!("Ended task {p}"), UndoAction::None)
        }
        Action::Delete => {
            let xml = task_xml(p)?;
            if xml.trim().is_empty() {
                bail!("could not back up the task definition; not deleting");
            }
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let safe: String = p
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect();
            let file = paths::backups_dir()?.join(format!("{stamp}-task{safe}.xml"));
            // schtasks /create /xml expects UTF-16 with BOM.
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
            std::fs::write(&file, bytes)?;
            schtasks(&["/delete", "/tn", p, "/f"])?;
            let file = file.display().to_string();
            (
                format!("Deleted task {p} (backup: {file})"),
                undo_cmd(&["/create", "/tn", p, "/xml", &file, "/f"]),
            )
        }
    })
}

#[derive(Default)]
pub struct TaskSchedulerManager {
    load: Option<BackgroundTask<Vec<TaskRow>>>,
    tasks: Option<Result<Vec<TaskRow>, String>>,
    filter: String,
    show_microsoft: bool,
    selected: Option<String>,
    confirm: Confirm<(TaskRow, Action)>,
    message: Message,
}

impl TaskSchedulerManager {
    fn act(&mut self, ctx: &AppCtx<'_>, task: &TaskRow, action: Action) {
        self.message = Some(if ctx.config.dry_run {
            Ok(ctx.i18n.tr("common-dry-run-skip"))
        } else {
            perform(task, action)
                .map(|(desc, undo)| {
                    let _ = ctx.journal.record("tasks", desc.clone(), undo);
                    desc + "."
                })
                .map_err(|e| format!("{e:#}"))
        });
        self.tasks = None;
    }
}

impl Tool for TaskSchedulerManager {
    fn id(&self) -> &'static str {
        "tasks"
    }

    fn category(&self) -> Category {
        Category::Optimizer
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.load.is_none() && self.tasks.is_none() {
            self.load = Some(BackgroundTask::spawn(ui.ctx(), |_| enumerate()));
        }
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                self.tasks = Some(r);
                self.load = None;
            }
        }
        if let Some((task, action)) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.act(ctx, &task, action);
        }

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            ui.checkbox(&mut self.show_microsoft, "Show Microsoft tasks");
            if ui
                .add_enabled(
                    self.load.is_none(),
                    egui::Button::new(ctx.i18n.tr("common-refresh")),
                )
                .clicked()
            {
                self.tasks = None;
            }
            if self.load.is_some() {
                ui.spinner();
                ui.label(ctx.i18n.tr("common-loading"));
            }
        });
        show_message(ui, &self.message);

        let tasks = match &self.tasks {
            Some(Ok(t)) => t.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        let f = self.filter.to_lowercase();
        let visible: Vec<&TaskRow> = tasks
            .iter()
            .filter(|t| self.show_microsoft || !t.is_microsoft())
            .filter(|t| {
                f.is_empty()
                    || t.path.to_lowercase().contains(&f)
                    || t.actions.to_lowercase().contains(&f)
                    || t.author.to_lowercase().contains(&f)
            })
            .collect();
        ui.weak(format!("{} of {} tasks shown", visible.len(), tasks.len()));
        if visible.is_empty() && !self.show_microsoft && f.is_empty() && !tasks.is_empty() {
            ui.horizontal(|ui| {
                ui.label("No third-party scheduled tasks were found; all tasks belong to Windows.");
                if ui.button("Show Windows tasks").clicked() {
                    self.show_microsoft = true;
                }
            });
            if !ctx.elevated {
                ui.weak("Tasks registered by other users or with restricted permissions only appear when running as administrator.");
            }
        }

        let selected = self
            .selected
            .as_ref()
            .and_then(|p| tasks.iter().find(|t| &t.path == p))
            .cloned();
        let mut detail_open = selected.is_some();
        egui::Panel::bottom("task_detail")
            .resizable(true)
            .default_size(170.0)
            .show_collapsible(ui, &mut detail_open, |ui| {
                let Some(t) = &selected else { return };
                ui.add_space(4.0);
                ui.strong(&t.path);
                egui::Grid::new("task_props")
                    .num_columns(2)
                    .spacing([16.0, 2.0])
                    .show(ui, |ui| {
                        for (k, v) in [
                            ("Author", t.author.clone()),
                            ("Description", t.description.clone()),
                            ("Actions", t.actions.clone()),
                            ("Last result", result_text(t.last_result)),
                        ] {
                            ui.weak(k);
                            ui.add(egui::Label::new(v).wrap());
                            ui.end_row();
                        }
                    });
                ui.add_enabled_ui(ctx.elevated || !t.is_microsoft(), |ui| {
                    ui.horizontal(|ui| {
                        if t.enabled {
                            if ui.button("Disable").clicked() {
                                self.act(ctx, t, Action::Disable);
                            }
                        } else if ui.button("Enable").clicked() {
                            self.act(ctx, t, Action::Enable);
                        }
                        if ui
                            .add_enabled(t.state != "Running", egui::Button::new("Run now"))
                            .clicked()
                        {
                            self.act(ctx, t, Action::Run);
                        }
                        if ui
                            .add_enabled(t.state == "Running", egui::Button::new("End"))
                            .clicked()
                        {
                            self.act(ctx, t, Action::End);
                        }
                        if ui.button("Delete…").clicked() {
                            let warning = if t.is_microsoft() {
                                " This is a Windows task; disabling it is safer than deleting it."
                            } else {
                                ""
                            };
                            self.confirm.ask(
                                format!(
                                    "Delete task {}? A backup of its definition is saved and the deletion \
                                     can be undone from the Change Journal.{warning}",
                                    t.path
                                ),
                                (t.clone(), Action::Delete),
                            );
                        }
                    });
                });
            });

        let mut clicked = None;
        let weak = ui.visuals().weak_text_color();
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(240.0).clip(true))
            .column(Column::initial(80.0))
            .column(Column::initial(125.0))
            .column(Column::initial(125.0))
            .column(Column::initial(200.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for title in ["Name", "Status", "Last run", "Next run", "Folder", "Action"] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let t = visible[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(t.path.as_str()));
                    row.col(|ui| {
                        ui.label(&t.name);
                    });
                    row.col(|ui| {
                        if t.enabled {
                            ui.label(t.state);
                        } else {
                            ui.colored_label(weak, t.state);
                        }
                    });
                    row.col(|ui| {
                        ui.label(t.last_run.as_deref().unwrap_or("Never"));
                    });
                    row.col(|ui| {
                        ui.label(t.next_run.as_deref().unwrap_or("—"));
                    });
                    row.col(|ui| {
                        ui.weak(t.folder());
                    });
                    row.col(|ui| {
                        ui.weak(&t.actions);
                    });
                    if row.response().clicked() {
                        clicked = Some(t.path.clone());
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
    fn ole_dates() {
        assert_eq!(ole_date(0.0), None);
        assert_eq!(ole_date(36_000.0), None); // 1998 → treated as "never"
        assert_eq!(ole_date(46_349.5).as_deref(), Some("2026-11-23 12:00"));
    }

    #[test]
    fn results_and_folders() {
        assert_eq!(result_text(0), "Success");
        assert_eq!(result_text(0x41303), "Has not run yet");
        assert_eq!(result_text(1), "0x00000001");
        let t = TaskRow {
            path: r"\Microsoft\Windows\Defrag\ScheduledDefrag".into(),
            name: "ScheduledDefrag".into(),
            state: "Ready",
            enabled: true,
            last_run: None,
            next_run: None,
            last_result: 0,
            author: String::new(),
            description: String::new(),
            actions: String::new(),
        };
        assert_eq!(t.folder(), r"\Microsoft\Windows\Defrag");
        assert!(t.is_microsoft());
        assert_eq!(
            TaskRow {
                path: r"\Top".into(),
                ..t
            }
            .folder(),
            "\\"
        );
    }

    #[test]
    fn enumerate_real_tasks() {
        let tasks = enumerate().unwrap();
        assert!(tasks.len() > 20, "got {}", tasks.len());
        assert!(tasks.iter().any(|t| t.is_microsoft()));
    }

    #[test]
    fn user_task_disable_delete_and_restore() {
        // A throwaway per-user task (no elevation needed); cleaned up at the end.
        let name = format!(r"\wm-test-task-{}", std::process::id());
        schtasks(&[
            "/create",
            "/tn",
            &name,
            "/tr",
            "cmd /c exit",
            "/sc",
            "once",
            "/st",
            "23:59",
            "/f",
        ])
        .unwrap();
        let row = enumerate()
            .unwrap()
            .into_iter()
            .find(|t| t.path == name)
            .expect("created task listed");
        assert!(row.enabled);

        let (_, undo) = perform(&row, Action::Disable).unwrap();
        assert!(
            !enumerate()
                .unwrap()
                .iter()
                .find(|t| t.path == name)
                .unwrap()
                .enabled
        );
        let UndoAction::RunCommand { args, .. } = undo else {
            panic!()
        };
        schtasks(&args.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();

        let (_, undo) = perform(&row, Action::Delete).unwrap();
        assert!(!enumerate().unwrap().iter().any(|t| t.path == name));
        let UndoAction::RunCommand { args, .. } = undo else {
            panic!()
        };
        schtasks(&args.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
        assert!(
            enumerate().unwrap().iter().any(|t| t.path == name),
            "restored from XML backup"
        );
        let _ = std::fs::remove_file(&args[4]); // the XML backup written by the test

        schtasks(&["/delete", "/tn", &name, "/f"]).unwrap();
    }
}
