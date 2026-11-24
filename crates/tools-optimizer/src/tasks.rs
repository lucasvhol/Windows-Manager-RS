//! Task Scheduler Manager: browse scheduled tasks by folder (COM `ITaskService`),
//! enable/disable, run/end, create tasks with any common trigger, import/export task
//! XML, delete with an XML backup, and an Optimize questionnaire for Windows'
//! optional background tasks. Every change is journaled.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use egui_extras::{Column, TableBuilder};
use windows::Win32::Foundation::VARIANT_FALSE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, IRegisteredTask, ITaskFolder, ITaskService, TASK_ACTION_EXEC, TASK_ACTION_TYPE,
    TASK_ENUM_HIDDEN, TASK_STATE_DISABLED, TASK_STATE_QUEUED, TASK_STATE_READY, TASK_STATE_RUNNING,
    TASK_TRIGGER_BOOT, TASK_TRIGGER_DAILY, TASK_TRIGGER_EVENT, TASK_TRIGGER_IDLE, TASK_TRIGGER_LOGON,
    TASK_TRIGGER_MONTHLY, TASK_TRIGGER_MONTHLYDOW, TASK_TRIGGER_REGISTRATION,
    TASK_TRIGGER_SESSION_STATE_CHANGE, TASK_TRIGGER_TIME, TASK_TRIGGER_TYPE2, TASK_TRIGGER_WEEKLY,
    TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, Interface};
use wm_core::{
    AppCtx, Category, Tool, egui, icons, paths,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, show_message, toolbar},
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
    /// Human-readable trigger summary, e.g. "Daily at 03:00; At log on".
    pub triggers: String,
}

impl TaskRow {
    pub fn folder(&self) -> &str {
        folder_of(&self.path)
    }

    pub fn is_microsoft(&self) -> bool {
        self.path.starts_with(r"\Microsoft\")
    }
}

fn folder_of(path: &str) -> &str {
    path.rsplit_once('\\')
        .map_or("\\", |(f, _)| if f.is_empty() { "\\" } else { f })
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

/// "Daily at 03:00" from a trigger type and its ISO start boundary.
pub fn trigger_text(kind: TASK_TRIGGER_TYPE2, start: &str) -> String {
    let at = start
        .get(11..16)
        .filter(|t| t.as_bytes().get(2) == Some(&b':'))
        .map(|t| format!(" at {t}"))
        .unwrap_or_default();
    let on = start.get(..10).map(|d| format!(" on {d}")).unwrap_or_default();
    match kind {
        TASK_TRIGGER_TIME => format!("One time{on}{at}"),
        TASK_TRIGGER_DAILY => format!("Daily{at}"),
        TASK_TRIGGER_WEEKLY => format!("Weekly{at}"),
        TASK_TRIGGER_MONTHLY | TASK_TRIGGER_MONTHLYDOW => format!("Monthly{at}"),
        TASK_TRIGGER_LOGON => "At log on".into(),
        TASK_TRIGGER_BOOT => "At startup".into(),
        TASK_TRIGGER_IDLE => "On idle".into(),
        TASK_TRIGGER_EVENT => "On an event".into(),
        TASK_TRIGGER_REGISTRATION => "When created or modified".into(),
        TASK_TRIGGER_SESSION_STATE_CHANGE => "On session change".into(),
        _ => "Custom".into(),
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
        let (mut author, mut description, mut actions, mut triggers) =
            (String::new(), String::new(), Vec::new(), Vec::new());
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
            if let Ok(coll) = def.Triggers() {
                let mut count = 0;
                let _ = coll.Count(&mut count);
                for i in 1..=count {
                    let Ok(t) = coll.get_Item(i) else { continue };
                    let mut kind = TASK_TRIGGER_TYPE2(-1);
                    let _ = t.Type(&mut kind);
                    triggers.push(trigger_text(kind, &bstr_out(|p| t.StartBoundary(p))));
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
            triggers: triggers.join("; "),
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

/// The task's definition XML (used as the deletion backup and for export).
pub fn task_xml(path: &str) -> Result<String> {
    unsafe {
        let service = connect()?;
        let (folder, name) = path.rsplit_once('\\').context("invalid task path")?;
        let folder = service.GetFolder(&BSTR::from(if folder.is_empty() { "\\" } else { folder }))?;
        Ok(folder.GetTask(&BSTR::from(name))?.Xml()?.to_string())
    }
}

/// Writes XML as UTF-16 LE with a BOM, the encoding `schtasks /create /xml` expects.
fn write_utf16(file: &std::path::Path, xml: &str) -> Result<()> {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(file, bytes)?;
    Ok(())
}

/// Reads a task XML file in UTF-16 (with BOM) or UTF-8.
pub fn read_task_file(bytes: &[u8]) -> Result<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        return Ok(String::from_utf16(&units)?);
    }
    let text = std::str::from_utf8(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes))?;
    Ok(text.to_owned())
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
            write_utf16(&file, &xml)?;
            schtasks(&["/delete", "/tn", p, "/f"])?;
            let file = file.display().to_string();
            (
                format!("Deleted task {p} (backup: {file})"),
                undo_cmd(&["/create", "/tn", p, "/xml", &file, "/f"]),
            )
        }
    })
}

/// Disables several tasks as one journal entry; failures are listed, not fatal.
pub fn disable_batch(label: &str, tasks: &[TaskRow]) -> Result<(String, UndoAction)> {
    let mut undo = Vec::new();
    let mut failed = Vec::new();
    for t in tasks {
        match schtasks(&["/change", "/tn", &t.path, "/disable"]) {
            Ok(()) => undo.push(undo_cmd(&["/change", "/tn", &t.path, "/enable"])),
            Err(e) => failed.push(format!("{}: {e:#}", t.name)),
        }
    }
    if undo.is_empty() && !failed.is_empty() {
        bail!("No tasks changed. {}", failed.join("; "));
    }
    let mut desc = format!("{label}: disabled {} tasks", undo.len());
    if !failed.is_empty() {
        desc += &format!(" ({} skipped: {})", failed.len(), failed.join("; "));
    }
    Ok((desc, UndoAction::Sequence { steps: undo }))
}

/// Registers a task from XML at `path` (fails if it already exists). Undo deletes it.
pub fn register_xml(path: &str, xml: &str) -> Result<(String, UndoAction)> {
    let dir = std::env::temp_dir().join(format!("wm-task-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("task.xml");
    write_utf16(&file, xml)?;
    let r = schtasks(&["/create", "/tn", path, "/xml", &file.display().to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
    r?;
    Ok((
        format!("Created task {path}"),
        undo_cmd(&["/delete", "/tn", path, "/f"]),
    ))
}

// ---------------------------------------------------------------- new tasks

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    Once,
    Daily,
    Weekly,
    Monthly,
    Logon,
    Boot,
    Idle,
    Event,
    Lock,
    Unlock,
}

impl TriggerKind {
    pub const ALL: [TriggerKind; 10] = [
        TriggerKind::Once,
        TriggerKind::Daily,
        TriggerKind::Weekly,
        TriggerKind::Monthly,
        TriggerKind::Logon,
        TriggerKind::Boot,
        TriggerKind::Idle,
        TriggerKind::Event,
        TriggerKind::Lock,
        TriggerKind::Unlock,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TriggerKind::Once => "One time",
            TriggerKind::Daily => "Daily",
            TriggerKind::Weekly => "Weekly",
            TriggerKind::Monthly => "Monthly",
            TriggerKind::Logon => "At log on",
            TriggerKind::Boot => "At startup",
            TriggerKind::Idle => "When the computer is idle",
            TriggerKind::Event => "When an event is logged",
            TriggerKind::Lock => "When the PC is locked",
            TriggerKind::Unlock => "When the PC is unlocked",
        }
    }

    fn timed(self) -> bool {
        matches!(
            self,
            TriggerKind::Once | TriggerKind::Daily | TriggerKind::Weekly | TriggerKind::Monthly
        )
    }
}

const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

#[derive(Debug, Clone, PartialEq)]
pub struct NewTask {
    pub folder: String,
    pub name: String,
    pub description: String,
    pub program: String,
    pub arguments: String,
    pub working_dir: String,
    pub trigger: TriggerKind,
    pub start: NaiveDateTime,
    /// Every N days / weeks.
    pub interval: u32,
    pub weekdays: [bool; 7],
    /// Days of the month, e.g. "1, 15".
    pub month_days: String,
    pub event_log: String,
    pub event_source: String,
    pub event_id: String,
    /// Run as SYSTEM (whether or not anyone is signed in). Needs administrator rights.
    pub as_system: bool,
    pub highest: bool,
}

impl NewTask {
    pub fn path(&self) -> String {
        let folder = self.folder.trim().trim_end_matches('\\');
        let folder = if folder.starts_with('\\') {
            folder.to_owned()
        } else {
            format!("\\{folder}")
        };
        format!("{}\\{}", folder.trim_end_matches('\\'), self.name.trim())
    }
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn parse_month_days(s: &str) -> Result<Vec<u32>> {
    let mut days = BTreeSet::new();
    for part in s.split([',', ' ', ';']).filter(|p| !p.is_empty()) {
        let d: u32 = part
            .parse()
            .with_context(|| format!("\"{part}\" is not a day number"))?;
        if !(1..=31).contains(&d) {
            bail!("day {d} is not between 1 and 31");
        }
        days.insert(d);
    }
    if days.is_empty() {
        bail!("enter at least one day of the month");
    }
    Ok(days.into_iter().collect())
}

fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{user}"),
        _ => user,
    }
}

/// Task Scheduler 1.2 XML for `spec`, run by `user` unless it runs as SYSTEM.
pub fn new_task_xml(spec: &NewTask, user: &str) -> Result<String> {
    let name = spec.name.trim();
    if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        bail!("enter a task name without \\ / : * ? \" < > |");
    }
    if spec.program.trim().is_empty() {
        bail!("choose the program to run");
    }
    let start = spec.start.format("%Y-%m-%dT%H:%M:%S");
    let interval = spec.interval.max(1);
    let user_el = format!("<UserId>{}</UserId>", xml_escape(user));
    let trigger = match spec.trigger {
        TriggerKind::Once => format!(
            "<TimeTrigger><StartBoundary>{start}</StartBoundary><Enabled>true</Enabled></TimeTrigger>"
        ),
        TriggerKind::Daily => format!(
            "<CalendarTrigger><StartBoundary>{start}</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>{interval}</DaysInterval></ScheduleByDay></CalendarTrigger>"
        ),
        TriggerKind::Weekly => {
            let days: String = WEEKDAYS
                .iter()
                .zip(spec.weekdays)
                .filter(|(_, on)| *on)
                .map(|(d, _)| format!("<{d} />"))
                .collect();
            if days.is_empty() {
                bail!("pick at least one day of the week");
            }
            format!(
                "<CalendarTrigger><StartBoundary>{start}</StartBoundary><Enabled>true</Enabled><ScheduleByWeek><DaysOfWeek>{days}</DaysOfWeek><WeeksInterval>{interval}</WeeksInterval></ScheduleByWeek></CalendarTrigger>"
            )
        }
        TriggerKind::Monthly => {
            let days: String = parse_month_days(&spec.month_days)?
                .iter()
                .map(|d| format!("<Day>{d}</Day>"))
                .collect();
            let months: String = MONTHS.iter().map(|m| format!("<{m} />")).collect();
            format!(
                "<CalendarTrigger><StartBoundary>{start}</StartBoundary><Enabled>true</Enabled><ScheduleByMonth><DaysOfMonth>{days}</DaysOfMonth><Months>{months}</Months></ScheduleByMonth></CalendarTrigger>"
            )
        }
        // Without a user the trigger fires for anyone signing in (needs administrator rights).
        TriggerKind::Logon if spec.as_system => "<LogonTrigger><Enabled>true</Enabled></LogonTrigger>".into(),
        TriggerKind::Logon => format!("<LogonTrigger><Enabled>true</Enabled>{user_el}</LogonTrigger>"),
        TriggerKind::Boot => "<BootTrigger><Enabled>true</Enabled></BootTrigger>".into(),
        TriggerKind::Idle => "<IdleTrigger><Enabled>true</Enabled></IdleTrigger>".into(),
        TriggerKind::Event => {
            let log = spec.event_log.trim();
            if log.is_empty() {
                bail!("enter the event log, e.g. System or Application");
            }
            let mut cond = Vec::new();
            if !spec.event_source.trim().is_empty() {
                cond.push(format!(
                    "Provider[@Name='{}']",
                    spec.event_source.trim().replace('\'', "")
                ));
            }
            if !spec.event_id.trim().is_empty() {
                let id: u32 = spec
                    .event_id
                    .trim()
                    .parse()
                    .context("the event ID must be a number")?;
                cond.push(format!("EventID={id}"));
            }
            let select = if cond.is_empty() {
                "*".to_owned()
            } else {
                format!("*[System[{}]]", cond.join(" and "))
            };
            let log = log.replace(['"', '\''], "");
            let query = format!(
                "<QueryList><Query Id=\"0\" Path=\"{log}\"><Select Path=\"{log}\">{select}</Select></Query></QueryList>"
            );
            format!(
                "<EventTrigger><Enabled>true</Enabled><Subscription>{}</Subscription></EventTrigger>",
                xml_escape(&query)
            )
        }
        TriggerKind::Lock | TriggerKind::Unlock => {
            let change = if spec.trigger == TriggerKind::Lock {
                "SessionLock"
            } else {
                "SessionUnlock"
            };
            let who = if spec.as_system {
                String::new()
            } else {
                user_el.clone()
            };
            format!(
                "<SessionStateChangeTrigger><Enabled>true</Enabled>{who}<StateChange>{change}</StateChange></SessionStateChangeTrigger>"
            )
        }
    };
    let level = if spec.highest || spec.as_system {
        "HighestAvailable"
    } else {
        "LeastPrivilege"
    };
    let principal = if spec.as_system {
        format!("<UserId>S-1-5-18</UserId><RunLevel>{level}</RunLevel>")
    } else {
        format!("{user_el}<LogonType>InteractiveToken</LogonType><RunLevel>{level}</RunLevel>")
    };
    let opt = |tag: &str, v: &str| {
        let v = v.trim();
        if v.is_empty() {
            String::new()
        } else {
            format!("<{tag}>{}</{tag}>", xml_escape(v))
        }
    };
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>{author}{desc}</RegistrationInfo>
  <Triggers>{trigger}</Triggers>
  <Principals><Principal id="Author">{principal}</Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT72H</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author"><Exec>{cmd}{args}{wd}</Exec></Actions>
</Task>
"#,
        author = opt("Author", user),
        desc = opt("Description", &spec.description),
        cmd = opt("Command", spec.program.trim().trim_matches('"')),
        args = opt("Arguments", &spec.arguments),
        wd = opt("WorkingDirectory", spec.working_dir.trim().trim_matches('"')),
    ))
}

// ---------------------------------------------------------------- optimize

pub struct Question {
    pub text: &'static str,
    /// Task paths disabled when the answer is "No".
    pub tasks: &'static [&'static str],
}

pub const QUESTIONS: &[Question] = &[
    Question {
        text: "Take part in the Customer Experience Improvement Program and compatibility telemetry?",
        tasks: &[
            r"\Microsoft\Windows\Customer Experience Improvement Program\Consolidator",
            r"\Microsoft\Windows\Customer Experience Improvement Program\UsbCeip",
            r"\Microsoft\Windows\Application Experience\Microsoft Compatibility Appraiser",
            r"\Microsoft\Windows\Application Experience\Microsoft Compatibility Appraiser Exp",
            r"\Microsoft\Windows\Application Experience\ProgramDataUpdater",
            r"\Microsoft\Windows\Autochk\Proxy",
            r"\Microsoft\Windows\DiskDiagnostic\Microsoft-Windows-DiskDiagnosticDataCollector",
        ],
    },
    Question {
        text: "Let Windows ask for feedback?",
        tasks: &[
            r"\Microsoft\Windows\Feedback\Siuf\DmClient",
            r"\Microsoft\Windows\Feedback\Siuf\DmClientOnScenarioDownload",
        ],
    },
    Question {
        text: "Send error reports to Microsoft?",
        tasks: &[r"\Microsoft\Windows\Windows Error Reporting\QueueReporting"],
    },
    Question {
        text: "Use offline maps?",
        tasks: &[
            r"\Microsoft\Windows\Maps\MapsToastTask",
            r"\Microsoft\Windows\Maps\MapsUpdateTask",
        ],
    },
    Question {
        text: "Use Microsoft Family Safety on this PC?",
        tasks: &[
            r"\Microsoft\Windows\Shell\FamilySafetyMonitor",
            r"\Microsoft\Windows\Shell\FamilySafetyRefreshTask",
        ],
    },
    Question {
        text: "Sync Xbox game saves?",
        tasks: &[r"\Microsoft\XblGameSave\XblGameSaveTask"],
    },
    Question {
        text: "Use speech recognition?",
        tasks: &[r"\Microsoft\Windows\Speech\SpeechModelDownloadTask"],
    },
    Question {
        text: "Show location notifications?",
        tasks: &[
            r"\Microsoft\Windows\Location\Notifications",
            r"\Microsoft\Windows\Location\WindowsActionDialog",
        ],
    },
    Question {
        text: "Share media with Windows Media Player?",
        tasks: &[r"\Microsoft\Windows\Windows Media Sharing\UpdateLibrary"],
    },
    Question {
        text: "Is this a store demonstration PC?",
        tasks: &[r"\Microsoft\Windows\RetailDemo\CleanupOfflineContent"],
    },
];

/// Enabled tasks the answers would disable (`answers[i]` = "Yes, I use it").
pub fn plan_optimize<'a>(tasks: &'a [TaskRow], answers: &[bool]) -> Vec<&'a TaskRow> {
    let wanted: BTreeSet<String> = QUESTIONS
        .iter()
        .zip(answers)
        .filter(|(_, yes)| !**yes)
        .flat_map(|(q, _)| q.tasks.iter().map(|t| t.to_lowercase()))
        .collect();
    tasks
        .iter()
        .filter(|t| t.enabled && wanted.contains(&t.path.to_lowercase()))
        .collect()
}

// ---------------------------------------------------------------- UI

enum Ask {
    One(TaskRow, Action),
    Batch(String, Vec<TaskRow>),
}

struct Form {
    spec: NewTask,
    date: String,
    time: String,
    error: Option<String>,
}

impl Form {
    fn new(folder: &str) -> Self {
        let now = chrono::Local::now().naive_local();
        let next = (now + Duration::hours(1))
            .with_minute(0)
            .and_then(|t| t.with_second(0))
            .unwrap_or(now);
        Self {
            date: next.format("%Y-%m-%d").to_string(),
            time: next.format("%H:%M").to_string(),
            spec: NewTask {
                folder: folder.into(),
                name: String::new(),
                description: String::new(),
                program: String::new(),
                arguments: String::new(),
                working_dir: String::new(),
                trigger: TriggerKind::Daily,
                start: next,
                interval: 1,
                weekdays: [true, false, false, false, false, false, false],
                month_days: "1".into(),
                event_log: "System".into(),
                event_source: String::new(),
                event_id: String::new(),
                as_system: false,
                highest: false,
            },
            error: None,
        }
    }

    fn build(&mut self) -> Result<(String, String)> {
        let date =
            NaiveDate::parse_from_str(self.date.trim(), "%Y-%m-%d").context("date must be YYYY-MM-DD")?;
        let time = NaiveTime::parse_from_str(self.time.trim(), "%H:%M").context("time must be HH:MM")?;
        self.spec.start = date.and_time(time);
        Ok((self.spec.path(), new_task_xml(&self.spec, &current_user())?))
    }
}

#[derive(Default)]
pub struct TaskSchedulerManager {
    load: Option<BackgroundTask<Vec<TaskRow>>>,
    tasks: Option<Result<Vec<TaskRow>, String>>,
    action: Option<BackgroundTask<(String, UndoAction)>>,
    filter: String,
    hide_microsoft: bool,
    folder: Option<String>,
    selected: Option<String>,
    confirm: Confirm<Ask>,
    form: Option<Form>,
    optimize: Option<Vec<bool>>,
    message: Message,
}

/// Folder → number of tasks directly in it; ancestors are included with 0.
fn folder_counts(tasks: &[&TaskRow]) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    map.insert("\\".to_owned(), 0);
    for t in tasks {
        let f = t.folder();
        *map.entry(f.to_owned()).or_insert(0) += 1;
        let mut cur = f;
        while cur != "\\" {
            cur = folder_of(cur);
            map.entry(cur.to_owned()).or_insert(0);
        }
    }
    map
}

impl TaskSchedulerManager {
    fn spawn(
        &mut self,
        ui: &egui::Ui,
        ctx: &AppCtx<'_>,
        work: impl FnOnce() -> Result<(String, UndoAction)> + Send + 'static,
    ) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        self.message = None;
        self.action = Some(BackgroundTask::spawn(ui.ctx(), move |_| work()));
    }

    fn run_ask(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>, ask: Ask) {
        match ask {
            Ask::One(t, a) => self.spawn(ui, ctx, move || perform(&t, a)),
            Ask::Batch(label, tasks) => self.spawn(ui, ctx, move || disable_batch(&label, &tasks)),
        }
    }

    fn poll(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                self.tasks = Some(r);
                self.load = None;
            }
        }
        if let Some(t) = &mut self.action {
            t.poll();
            if let Some(r) = t.take_result() {
                self.action = None;
                self.message = Some(r.map(|(desc, undo)| {
                    let _ = ctx.journal.record("tasks", desc.clone(), undo);
                    desc + "."
                }));
                self.tasks = None;
            }
        }
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.run_ask(ui, ctx, ask);
        }
    }

    fn folder_tree(&mut self, ui: &mut egui::Ui, counts: &BTreeMap<String, usize>, total: usize) {
        if ui
            .selectable_label(
                self.folder.is_none(),
                format!("{}  All tasks ({total})", icons::LIST_CHECKS),
            )
            .clicked()
        {
            self.folder = None;
        }
        self.folder_node(ui, counts, "\\");
    }

    fn folder_node(&mut self, ui: &mut egui::Ui, counts: &BTreeMap<String, usize>, folder: &str) {
        let children: Vec<&String> = counts
            .keys()
            .filter(|k| k.as_str() != "\\" && folder_of(k) == folder)
            .collect();
        let label = if folder == "\\" {
            "Task Scheduler Library".to_owned()
        } else {
            folder.rsplit('\\').next().unwrap_or(folder).to_owned()
        };
        let n = counts.get(folder).copied().unwrap_or(0);
        let text = if n > 0 {
            format!("{}  {label} ({n})", icons::FOLDER_OPEN)
        } else {
            format!("{}  {label}", icons::FOLDER_OPEN)
        };
        let selected = self.folder.as_deref() == Some(folder);
        if children.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                if ui.selectable_label(selected, text).clicked() {
                    self.folder = Some(folder.to_owned());
                }
            });
            return;
        }
        let id = ui.make_persistent_id(("task_folder", folder));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, folder == "\\")
            .show_header(ui, |ui| {
                if ui.selectable_label(selected, text).clicked() {
                    self.folder = Some(folder.to_owned());
                }
            })
            .body(|ui| {
                for c in children {
                    self.folder_node(ui, counts, c);
                }
            });
    }

    fn form_ui(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        let Some(form) = &mut self.form else { return };
        let mut create = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("new_task")).show(ui.ctx(), |ui| {
            ui.set_width(560.0);
            ui.heading(format!("{}  New task", icons::PLUS));
            ui.add_space(4.0);
            let s = &mut form.spec;
            egui::Grid::new("new_task_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut s.name);
                    ui.end_row();
                    ui.label("Folder");
                    ui.text_edit_singleline(&mut s.folder);
                    ui.end_row();
                    ui.label("Description");
                    ui.text_edit_singleline(&mut s.description);
                    ui.end_row();
                    ui.label("Program");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut s.program).desired_width(330.0));
                        if ui.button("Browse…").clicked()
                            && let Some(p) = rfd::FileDialog::new()
                                .add_filter("Programs", &["exe", "bat", "cmd", "ps1", "vbs"])
                                .pick_file()
                        {
                            s.program = p.display().to_string();
                        }
                    });
                    ui.end_row();
                    ui.label("Arguments");
                    ui.text_edit_singleline(&mut s.arguments);
                    ui.end_row();
                    ui.label("Start in");
                    ui.add(egui::TextEdit::singleline(&mut s.working_dir).hint_text("optional"));
                    ui.end_row();
                    ui.label("Trigger");
                    egui::ComboBox::from_id_salt("trigger_kind")
                        .selected_text(s.trigger.label())
                        .show_ui(ui, |ui| {
                            for k in TriggerKind::ALL {
                                ui.selectable_value(&mut s.trigger, k, k.label());
                            }
                        });
                    ui.end_row();
                    if s.trigger.timed() {
                        ui.label("Starting");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut form.date).desired_width(90.0));
                            ui.add(egui::TextEdit::singleline(&mut form.time).desired_width(50.0));
                            ui.weak("YYYY-MM-DD  HH:MM");
                        });
                        ui.end_row();
                    }
                    match s.trigger {
                        TriggerKind::Daily | TriggerKind::Weekly => {
                            ui.label("Every");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut s.interval).range(1..=365));
                                ui.label(if s.trigger == TriggerKind::Daily {
                                    "days"
                                } else {
                                    "weeks"
                                });
                            });
                            ui.end_row();
                        }
                        _ => {}
                    }
                    if s.trigger == TriggerKind::Weekly {
                        ui.label("On");
                        ui.horizontal_wrapped(|ui| {
                            for (i, d) in WEEKDAYS.iter().enumerate() {
                                ui.checkbox(&mut s.weekdays[i], &d[..3]);
                            }
                        });
                        ui.end_row();
                    }
                    if s.trigger == TriggerKind::Monthly {
                        ui.label("Days of the month");
                        ui.add(egui::TextEdit::singleline(&mut s.month_days).hint_text("1, 15"));
                        ui.end_row();
                    }
                    if s.trigger == TriggerKind::Event {
                        ui.label("Log");
                        ui.text_edit_singleline(&mut s.event_log);
                        ui.end_row();
                        ui.label("Source");
                        ui.add(egui::TextEdit::singleline(&mut s.event_source).hint_text("any"));
                        ui.end_row();
                        ui.label("Event ID");
                        ui.add(egui::TextEdit::singleline(&mut s.event_id).hint_text("any"));
                        ui.end_row();
                    }
                    ui.label("Run");
                    ui.vertical(|ui| {
                        ui.add_enabled_ui(ctx.elevated, |ui| {
                            ui.checkbox(&mut s.as_system, "As SYSTEM, whether or not anyone is signed in")
                                .on_disabled_hover_text("Needs administrator rights.");
                        });
                        ui.checkbox(&mut s.highest, "With highest privileges");
                    });
                    ui.end_row();
                });
            if !ctx.elevated && matches!(form.spec.trigger, TriggerKind::Boot) {
                ui.weak("Startup triggers need administrator rights.");
            }
            if let Some(e) = &form.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(format!("{}  Create", icons::CHECK)).clicked() {
                    match form.build() {
                        Ok(r) => create = Some(r),
                        Err(e) => form.error = Some(format!("{e:#}")),
                    }
                }
                if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if let Some((path, xml)) = create {
            self.form = None;
            self.spawn(ui, ctx, move || register_xml(&path, &xml));
        } else if close {
            self.form = None;
        }
    }

    fn optimize_ui(&mut self, ui: &egui::Ui, tasks: &[TaskRow]) {
        let Some(answers) = &mut self.optimize else { return };
        let mut apply = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("tasks_optimize")).show(ui.ctx(), |ui| {
            ui.set_width(560.0);
            ui.heading(format!("{}  Optimize tasks", icons::MAGIC_WAND));
            ui.label("Answer \"No\" for features you don't use; their background tasks will be disabled (not deleted). Undo from the Change Journal.");
            ui.add_space(6.0);
            egui::Grid::new("task_questions").num_columns(2).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
                for (i, q) in QUESTIONS.iter().enumerate() {
                    ui.label(q.text);
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut answers[i], true, "Yes");
                        ui.radio_value(&mut answers[i], false, "No");
                    });
                    ui.end_row();
                }
            });
            let plan = plan_optimize(tasks, answers);
            ui.add_space(6.0);
            ui.weak(format!("{} enabled tasks will be disabled.", plan.len()));
            ui.horizontal(|ui| {
                if ui.add_enabled(!plan.is_empty(), egui::Button::new("Apply")).clicked() {
                    apply = Some(plan.into_iter().cloned().collect::<Vec<_>>());
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if let Some(plan) = apply {
            self.optimize = None;
            let names: Vec<&str> = plan.iter().map(|t| t.path.as_str()).collect();
            self.confirm.ask(
                format!("Disable these {} tasks?\n\n{}", plan.len(), names.join("\n")),
                Ask::Batch("Optimize tasks".into(), plan),
            );
        } else if close {
            self.optimize = None;
        }
    }

    fn export(&mut self, task: &TaskRow) {
        let Some(file) = rfd::FileDialog::new()
            .add_filter("Task XML", &["xml"])
            .set_file_name(format!("{}.xml", task.name))
            .save_file()
        else {
            return;
        };
        self.message = Some(
            task_xml(&task.path)
                .and_then(|x| write_utf16(&file, &x))
                .map(|()| format!("Exported {} to {}.", task.path, file.display()))
                .map_err(|e| format!("{e:#}")),
        );
    }

    fn import(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        let Some(file) = rfd::FileDialog::new()
            .add_filter("Task XML", &["xml"])
            .pick_file()
        else {
            return;
        };
        let xml = match std::fs::read(&file)
            .map_err(anyhow::Error::from)
            .and_then(|b| read_task_file(&b))
        {
            Ok(x) => x,
            Err(e) => {
                self.message = Some(Err(format!("{}: {e:#}", file.display())));
                return;
            }
        };
        let folder = self.folder.clone().unwrap_or_else(|| "\\".into());
        let name = file
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let path = format!("{}\\{name}", folder.trim_end_matches('\\'));
        self.spawn(ui, ctx, move || register_xml(&path, &xml));
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
        self.poll(ui, ctx);
        self.form_ui(ui, ctx);

        let tasks = match &self.tasks {
            Some(Ok(t)) => t.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(ctx.i18n.tr("common-loading"));
                });
                return;
            }
        };
        self.optimize_ui(ui, &tasks);

        let selected = self
            .selected
            .as_ref()
            .and_then(|p| tasks.iter().find(|t| &t.path == p))
            .cloned();
        let idle = self.action.is_none();
        let can = |t: &TaskRow| idle && (ctx.elevated || !t.is_microsoft());
        let sel = selected.as_ref().filter(|t| can(t));
        let buttons = [
            Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(self.load.is_none())),
            None,
            Some(ToolButton::new(icons::PLUS, "New").enabled(idle)),
            Some(ToolButton::new(icons::DOWNLOAD, "Import").enabled(idle)),
            Some(ToolButton::new(icons::UPLOAD, "Export").enabled(selected.is_some())),
            None,
            Some(ToolButton::new(icons::PLAY, "Run").enabled(sel.is_some_and(|t| t.state != "Running"))),
            Some(ToolButton::new(icons::STOP, "End").enabled(sel.is_some_and(|t| t.state == "Running"))),
            Some(ToolButton::new(icons::CHECK, "Enable").enabled(sel.is_some_and(|t| !t.enabled))),
            Some(ToolButton::new(icons::PAUSE, "Disable").enabled(sel.is_some_and(|t| t.enabled))),
            Some(ToolButton::new(icons::TRASH, "Delete").enabled(sel.is_some())),
            None,
            Some(ToolButton::new(icons::MAGIC_WAND, "Optimize").enabled(idle && ctx.elevated)),
            Some(ToolButton::new(icons::ARROW_SQUARE_OUT, "Task Scheduler")),
        ];
        match toolbar(ui, &buttons) {
            Some(0) => {
                self.tasks = None;
                self.message = None;
            }
            Some(2) => self.form = Some(Form::new(self.folder.as_deref().unwrap_or("\\"))),
            Some(3) => self.import(ui, ctx),
            Some(4) => {
                if let Some(t) = &selected {
                    self.export(t);
                }
            }
            Some(i @ 6..=10) => {
                if let Some(t) = sel.cloned() {
                    let action = [
                        Action::Run,
                        Action::End,
                        Action::Enable,
                        Action::Disable,
                        Action::Delete,
                    ][i - 6];
                    match action {
                        Action::Delete => {
                            let warning = if t.is_microsoft() {
                                " This is a Windows task; disabling it is safer than deleting it."
                            } else {
                                ""
                            };
                            self.confirm.ask(
                                format!(
                                    "Delete task {}? A backup of its definition is saved and the deletion can be undone from the Change Journal.{warning}",
                                    t.path
                                ),
                                Ask::One(t, Action::Delete),
                            );
                        }
                        Action::Disable if t.is_microsoft() => self.confirm.ask(
                            format!("Disable the Windows task {}?", t.path),
                            Ask::One(t, Action::Disable),
                        ),
                        a => self.run_ask(ui, ctx, Ask::One(t, a)),
                    }
                }
            }
            Some(12) => self.optimize = Some(vec![true; QUESTIONS.len()]),
            Some(13) => {
                let _ = wm_core::shell::shell_open("taskschd.msc", "");
            }
            _ => {}
        }
        show_message(ui, &self.message);

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(ctx.i18n.tr("common-filter"))
                    .desired_width(220.0),
            );
            if ui
                .checkbox(&mut self.hide_microsoft, "Hide Windows tasks")
                .changed()
                && self.hide_microsoft
            {
                self.folder = None;
            }
            if self.load.is_some() || self.action.is_some() {
                ui.spinner();
            }
        });

        let f = self.filter.to_lowercase();
        let scoped: Vec<&TaskRow> = tasks
            .iter()
            .filter(|t| !(self.hide_microsoft && t.is_microsoft()))
            .collect();
        let counts = folder_counts(&scoped);
        let visible: Vec<&TaskRow> = scoped
            .iter()
            .copied()
            .filter(|t| self.folder.as_deref().is_none_or(|fo| t.folder() == fo))
            .filter(|t| {
                f.is_empty()
                    || t.path.to_lowercase().contains(&f)
                    || t.actions.to_lowercase().contains(&f)
                    || t.author.to_lowercase().contains(&f)
                    || t.description.to_lowercase().contains(&f)
            })
            .collect();

        let total = scoped.len();
        egui::Panel::left("task_folders")
            .resizable(true)
            .default_size(230.0)
            .show(ui, |ui| {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    self.folder_tree(ui, &counts, total);
                });
            });

        ui.weak(format!("{} of {} tasks shown", visible.len(), tasks.len()));
        if total == 0 && !tasks.is_empty() {
            ui.horizontal(|ui| {
                ui.label("No third-party scheduled tasks were found; all tasks belong to Windows.");
                if ui.button("Show Windows tasks").clicked() {
                    self.hide_microsoft = false;
                }
            });
        }

        let mut detail_open = selected.is_some();
        egui::Panel::bottom("task_detail")
            .resizable(true)
            .default_size(130.0)
            .show_collapsible(ui, &mut detail_open, |ui| {
                let Some(t) = &selected else { return };
                ui.add_space(4.0);
                ui.strong(&t.path);
                egui::Grid::new("task_props")
                    .num_columns(2)
                    .spacing([16.0, 2.0])
                    .show(ui, |ui| {
                        for (k, v) in [
                            ("Description", t.description.clone()),
                            ("Triggers", t.triggers.clone()),
                            ("Actions", t.actions.clone()),
                            ("Author", t.author.clone()),
                            ("Last result", result_text(t.last_result)),
                        ] {
                            if !v.is_empty() {
                                ui.weak(k);
                                ui.add(egui::Label::new(v).wrap());
                                ui.end_row();
                            }
                        }
                    });
            });

        let mut clicked = None;
        let weak = ui.visuals().weak_text_color();
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(210.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::initial(150.0).clip(true))
            .column(Column::initial(115.0))
            .column(Column::initial(115.0))
            .column(Column::initial(100.0).clip(true))
            .column(Column::initial(120.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for title in [
                    "Name",
                    "Status",
                    "Triggers",
                    "Next run",
                    "Last run",
                    "Last result",
                    "Author",
                    "Description",
                ] {
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
                        ui.label(&t.triggers);
                    });
                    row.col(|ui| {
                        ui.label(t.next_run.as_deref().unwrap_or("—"));
                    });
                    row.col(|ui| {
                        ui.label(t.last_run.as_deref().unwrap_or("Never"));
                    });
                    row.col(|ui| {
                        ui.weak(result_text(t.last_result));
                    });
                    row.col(|ui| {
                        ui.weak(&t.author);
                    });
                    row.col(|ui| {
                        ui.weak(&t.description);
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

    fn row(path: &str) -> TaskRow {
        TaskRow {
            path: path.into(),
            name: path.rsplit('\\').next().unwrap().into(),
            state: "Ready",
            enabled: true,
            last_run: None,
            next_run: None,
            last_result: 0,
            author: String::new(),
            description: String::new(),
            actions: String::new(),
            triggers: String::new(),
        }
    }

    #[test]
    fn ole_dates() {
        assert_eq!(ole_date(0.0), None);
        assert_eq!(ole_date(36_000.0), None); // 1998 → treated as "never"
        assert_eq!(ole_date(46_349.5).as_deref(), Some("2026-11-23 12:00"));
    }

    #[test]
    fn results_folders_and_triggers() {
        assert_eq!(result_text(0), "Success");
        assert_eq!(result_text(0x41303), "Has not run yet");
        assert_eq!(result_text(1), "0x00000001");
        let t = row(r"\Microsoft\Windows\Defrag\ScheduledDefrag");
        assert_eq!(t.folder(), r"\Microsoft\Windows\Defrag");
        assert!(t.is_microsoft());
        assert_eq!(row(r"\Top").folder(), "\\");
        assert_eq!(
            trigger_text(TASK_TRIGGER_DAILY, "2026-01-02T03:04:00"),
            "Daily at 03:04"
        );
        assert_eq!(
            trigger_text(TASK_TRIGGER_TIME, "2026-01-02T03:04:00"),
            "One time on 2026-01-02 at 03:04"
        );
        assert_eq!(trigger_text(TASK_TRIGGER_LOGON, ""), "At log on");

        let a = row(r"\A\B\x");
        let b = row(r"\y");
        let counts = folder_counts(&[&a, &b]);
        assert_eq!(counts.get("\\"), Some(&1));
        assert_eq!(counts.get(r"\A"), Some(&0), "ancestors are listed");
        assert_eq!(counts.get(r"\A\B"), Some(&1));
    }

    #[test]
    fn task_files_decode() {
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("<Task/>".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(read_task_file(&utf16).unwrap(), "<Task/>");
        assert_eq!(read_task_file(b"\xEF\xBB\xBF<Task/>").unwrap(), "<Task/>");
    }

    fn spec(trigger: TriggerKind) -> NewTask {
        let mut f = Form::new(r"\");
        f.spec.name = "T".into();
        f.spec.program = "cmd.exe".into();
        f.spec.arguments = "/c exit 0 & echo <hi>".into();
        f.spec.trigger = trigger;
        f.spec.start = NaiveDate::from_ymd_opt(2030, 1, 1)
            .unwrap()
            .and_hms_opt(23, 59, 0)
            .unwrap();
        f.spec
    }

    #[test]
    fn new_task_xml_validates_and_escapes() {
        let x = new_task_xml(&spec(TriggerKind::Weekly), "PC\\me").unwrap();
        assert!(x.contains("<Monday />") && x.contains("<WeeksInterval>1</WeeksInterval>"));
        assert!(x.contains("/c exit 0 &amp; echo &lt;hi&gt;"));
        assert!(x.contains("<UserId>PC\\me</UserId><LogonType>InteractiveToken</LogonType>"));
        let mut s = spec(TriggerKind::Monthly);
        s.month_days = "15, 1, 15".into();
        assert!(
            new_task_xml(&s, "me")
                .unwrap()
                .contains("<Day>1</Day><Day>15</Day>")
        );
        s.month_days = "32".into();
        assert!(new_task_xml(&s, "me").is_err());
        let mut s = spec(TriggerKind::Weekly);
        s.weekdays = [false; 7];
        assert!(new_task_xml(&s, "me").is_err());
        let mut s = spec(TriggerKind::Daily);
        s.name = "a\\b".into();
        assert!(new_task_xml(&s, "me").is_err());
        let mut s = spec(TriggerKind::Event);
        s.event_source = "Service Control Manager".into();
        s.event_id = "7036".into();
        let x = new_task_xml(&s, "me").unwrap();
        assert!(
            x.contains("Provider[@Name=&apos;Service Control Manager&apos;] and EventID=7036"),
            "{x}"
        );
        let mut s = spec(TriggerKind::Boot);
        s.as_system = true;
        assert!(
            new_task_xml(&s, "me")
                .unwrap()
                .contains("<UserId>S-1-5-18</UserId>")
        );
        assert_eq!(spec(TriggerKind::Once).path(), r"\T");
        let mut s = spec(TriggerKind::Once);
        s.folder = r"Mine\".into();
        assert_eq!(s.path(), r"\Mine\T");
    }

    #[test]
    fn optimize_plan_matches_enabled_tasks_only() {
        let mut off = row(r"\Microsoft\Windows\Maps\MapsUpdateTask");
        off.enabled = false;
        let tasks = vec![row(r"\Microsoft\Windows\Maps\MapsToastTask"), off, row(r"\Other")];
        let mut answers = vec![true; QUESTIONS.len()];
        assert!(plan_optimize(&tasks, &answers).is_empty());
        let maps = QUESTIONS.iter().position(|q| q.text.contains("maps")).unwrap();
        answers[maps] = false;
        let plan = plan_optimize(&tasks, &answers);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "MapsToastTask");
    }

    #[test]
    fn enumerate_real_tasks() {
        let tasks = enumerate().unwrap();
        assert!(tasks.len() > 20, "got {}", tasks.len());
        assert!(tasks.iter().any(|t| t.is_microsoft()));
        assert!(tasks.iter().any(|t| !t.triggers.is_empty()));
    }

    #[test]
    fn create_every_user_trigger_kind() {
        // Per-user tasks need no elevation; each is created from XML, listed, then deleted.
        for kind in [
            TriggerKind::Once,
            TriggerKind::Daily,
            TriggerKind::Weekly,
            TriggerKind::Monthly,
            TriggerKind::Logon,
            TriggerKind::Idle,
            TriggerKind::Event,
            TriggerKind::Lock,
            TriggerKind::Unlock,
        ] {
            let mut s = spec(kind);
            s.name = format!("wm-test-{}-{:?}", std::process::id(), kind);
            let path = s.path();
            let xml = new_task_xml(&s, &current_user()).unwrap();
            let (_, undo) = register_xml(&path, &xml).unwrap_or_else(|e| panic!("{kind:?}: {e:#}"));
            let listed = enumerate().unwrap().into_iter().find(|t| t.path == path);
            let UndoAction::RunCommand { args, .. } = undo else {
                panic!()
            };
            schtasks(&args.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
            let listed = listed.unwrap_or_else(|| panic!("{kind:?} task listed"));
            assert!(!listed.triggers.is_empty(), "{kind:?} trigger summarised");
        }
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

        let (_, undo) = disable_batch("test", std::slice::from_ref(&row)).unwrap();
        assert!(
            !enumerate()
                .unwrap()
                .iter()
                .find(|t| t.path == name)
                .unwrap()
                .enabled
        );
        let UndoAction::Sequence { steps } = undo else {
            panic!()
        };
        let UndoAction::RunCommand { args, .. } = &steps[0] else {
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
