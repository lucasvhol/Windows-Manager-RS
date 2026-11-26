//! Process Manager: live process list with signature and company columns, end /
//! suspend / restart / block, rule-based ending, a window list, and per-process details.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use egui_extras::{Column, TableBuilder};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS, OpenProcess, OpenThread, PROCESS_CREATION_FLAGS, PROCESS_SET_INFORMATION,
    PROCESS_TERMINATE, ResumeThread, SetPriorityClass, SuspendThread, THREAD_SUSPEND_RESUME,
    TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;
use wm_core::{
    AppCtx, Category, Layout, Tool, egui,
    fileinfo::{self, FileInfo, Signature},
    icons, registry,
    safety::UndoAction,
    task::{BackgroundTask, Sampler},
    ui::{Confirm, Message, ToolButton, format_bytes, frame, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub exe: Option<PathBuf>,
    /// Share of total CPU capacity, 0–100.
    pub cpu: f32,
    pub memory: u64,
    pub user: String,
    pub cmd: String,
    pub start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Pid,
    Cpu,
    Memory,
    User,
}

pub fn sort_rows(rows: &mut [ProcRow], key: SortKey, ascending: bool) {
    rows.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Pid => a.pid.cmp(&b.pid),
            SortKey::Cpu => a.cpu.total_cmp(&b.cpu),
            SortKey::Memory => a.memory.cmp(&b.memory),
            SortKey::User => a.user.to_lowercase().cmp(&b.user.to_lowercase()),
        };
        let ord = ord.then(a.pid.cmp(&b.pid));
        if ascending { ord } else { ord.reverse() }
    });
}

pub fn matches_filter(row: &ProcRow, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let f = filter.to_lowercase();
    row.name.to_lowercase().contains(&f)
        || row.pid.to_string() == f
        || row
            .exe
            .as_ref()
            .is_some_and(|p| p.to_string_lossy().to_lowercase().contains(&f))
}

struct SampleState {
    sys: System,
    users: Option<Users>,
}

fn sample(s: &mut SampleState) -> Vec<ProcRow> {
    s.sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_user(UpdateKind::OnlyIfNotSet),
    );
    let cpus = s.sys.cpus().len().max(1) as f32;
    s.sys
        .processes()
        .values()
        .filter(|p| p.pid().as_u32() != 0)
        .map(|p| ProcRow {
            pid: p.pid().as_u32(),
            parent: p.parent().map(|x| x.as_u32()),
            name: p.name().to_string_lossy().into_owned(),
            exe: p.exe().map(PathBuf::from),
            cpu: p.cpu_usage() / cpus,
            memory: p.memory(),
            user: p
                .user_id()
                .and_then(|u| s.users.as_ref()?.get_user_by_id(u))
                .map(|u| u.name().to_owned())
                .unwrap_or_default(),
            cmd: p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            start_time: p.start_time(),
        })
        .collect()
}

pub const PRIORITIES: [(&str, PROCESS_CREATION_FLAGS); 5] = [
    ("Idle", IDLE_PRIORITY_CLASS),
    ("Below normal", BELOW_NORMAL_PRIORITY_CLASS),
    ("Normal", NORMAL_PRIORITY_CLASS),
    ("Above normal", ABOVE_NORMAL_PRIORITY_CLASS),
    ("High", HIGH_PRIORITY_CLASS),
];

/// Processes Windows needs to keep running; never ended by rules or buttons.
const CRITICAL: [&str; 12] = [
    "system",
    "registry",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "lsaiso.exe",
    "fontdrvhost.exe",
    "dwm.exe",
    "memory compression",
];

pub fn is_critical(name: &str) -> bool {
    CRITICAL.contains(&name.to_ascii_lowercase().as_str())
}

fn terminate(pid: u32) -> anyhow::Result<()> {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, false, pid)?;
        let r = TerminateProcess(h, 1);
        let _ = CloseHandle(h);
        r?;
    }
    Ok(())
}

fn set_priority(pid: u32, class: PROCESS_CREATION_FLAGS) -> anyhow::Result<()> {
    unsafe {
        let h = OpenProcess(PROCESS_SET_INFORMATION, false, pid)?;
        let r = SetPriorityClass(h, class);
        let _ = CloseHandle(h);
        r?;
    }
    Ok(())
}

/// Thread ids of a process.
fn threads_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return out;
        };
        let mut e = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        if Thread32First(snap, &mut e).is_ok() {
            loop {
                if e.th32OwnerProcessID == pid {
                    out.push(e.th32ThreadID);
                }
                if Thread32Next(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Suspends (or resumes) every thread of a process.
fn suspend(pid: u32, resume: bool) -> anyhow::Result<usize> {
    let mut n = 0;
    for tid in threads_of(pid) {
        unsafe {
            if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, tid) {
                let r = if resume { ResumeThread(h) } else { SuspendThread(h) };
                let _ = CloseHandle(h);
                if r != u32::MAX {
                    n += 1;
                }
            }
        }
    }
    if n == 0 {
        anyhow::bail!("access denied or the process has exited");
    }
    Ok(n)
}

#[derive(Debug, Clone)]
struct Module {
    path: String,
    size: u32,
}

fn modules_of(pid: u32) -> Vec<Module> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) else {
            return out;
        };
        let mut e = MODULEENTRY32W {
            dwSize: size_of::<MODULEENTRY32W>() as u32,
            ..Default::default()
        };
        if Module32FirstW(snap, &mut e).is_ok() {
            loop {
                let end = e
                    .szExePath
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(e.szExePath.len());
                out.push(Module {
                    path: String::from_utf16_lossy(&e.szExePath[..end]),
                    size: e.modBaseSize,
                });
                if Module32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Visible top-level windows with a title: (title, pid).
fn windows_list() -> Vec<(String, u32)> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        unsafe {
            let out = &mut *(l.0 as *mut Vec<(String, u32)>);
            if IsWindowVisible(h).as_bool() {
                let len = GetWindowTextLengthW(h);
                if len > 0 {
                    let mut buf = vec![0u16; len as usize + 1];
                    let n = GetWindowTextW(h, &mut buf);
                    let mut pid = 0u32;
                    GetWindowThreadProcessId(h, Some(&mut pid));
                    out.push((String::from_utf16_lossy(&buf[..n as usize]), pid));
                }
            }
        }
        true.into()
    }
    let mut out: Vec<(String, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

// --- Blocking through Image File Execution Options ---------------------------

const IFEO: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";

/// A harmless program Windows starts instead of a blocked one (it exits at once).
fn block_target() -> String {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{windir}\System32\systray.exe")
}

fn ifeo_key(exe_name: &str) -> String {
    format!(r"{IFEO}\{exe_name}")
}

pub fn blocked_programs() -> Vec<String> {
    let target = block_target().to_ascii_lowercase();
    let Ok(key) = registry::open_read(IFEO) else {
        return Vec::new();
    };
    key.enum_keys()
        .flatten()
        .filter(|name| {
            registry::read_raw(&ifeo_key(name), "Debugger")
                .is_some_and(|v| registry::decode_string(&v).to_ascii_lowercase() == target)
        })
        .collect()
}

fn block_program(exe_name: &str, journal: &wm_core::safety::ChangeJournal) -> anyhow::Result<()> {
    let key = ifeo_key(exe_name);
    let snap = registry::snapshot(&key, "Debugger");
    registry::write_raw(&key, "Debugger", &registry::string(&block_target()))?;
    journal.record(
        "procman",
        format!("Blocked {exe_name} from starting"),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

fn unblock_program(exe_name: &str, journal: &wm_core::safety::ChangeJournal) -> anyhow::Result<()> {
    let key = ifeo_key(exe_name);
    let snap = registry::snapshot(&key, "Debugger");
    registry::delete_value(&key, "Debugger")?;
    journal.record(
        "procman",
        format!("Unblocked {exe_name}"),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

// --- Tool --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    All,
    NotMicrosoft,
    Unsigned,
    MyProcesses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Processes,
    Windows,
    Blocked,
}

enum Ask {
    End(Vec<(u32, String)>),
    EndTree(u32),
    Block(String),
}

pub struct ProcessManager {
    sampler: Option<Sampler<Vec<ProcRow>>>,
    rows: Vec<ProcRow>,
    info: HashMap<PathBuf, FileInfo>,
    info_task: Option<BackgroundTask<HashMap<PathBuf, FileInfo>>>,
    asked: HashSet<PathBuf>,
    filter: String,
    view: View,
    page: Page,
    sort: SortKey,
    ascending: bool,
    selected: Option<u32>,
    modules: Option<(u32, Vec<Module>)>,
    cpu_limit: f32,
    memory_limit_mb: u64,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self {
            sampler: None,
            rows: Vec::new(),
            info: HashMap::new(),
            info_task: None,
            asked: HashSet::new(),
            filter: String::new(),
            view: View::All,
            page: Page::Processes,
            sort: SortKey::Cpu,
            ascending: false,
            selected: None,
            modules: None,
            cpu_limit: 50.0,
            memory_limit_mb: 2048,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl ProcessManager {
    fn header(&mut self, ui: &mut egui::Ui, label: &str, key: SortKey) {
        let arrow = match (self.sort == key, self.ascending) {
            (true, true) => " ⬆",
            (true, false) => " ⬇",
            _ => "",
        };
        if ui
            .add(egui::Button::new(format!("{label}{arrow}")).frame(false))
            .clicked()
        {
            if self.sort == key {
                self.ascending = !self.ascending;
            } else {
                self.sort = key;
                self.ascending = matches!(key, SortKey::Name | SortKey::Pid | SortKey::User);
            }
        }
    }

    fn info_of(&self, r: &ProcRow) -> Option<&FileInfo> {
        r.exe.as_ref().and_then(|p| self.info.get(p))
    }

    /// Starts reading signature / company for executables not seen yet.
    fn request_info(&mut self, ctx: &egui::Context) {
        if let Some(t) = &mut self.info_task {
            t.poll();
            if let Some(r) = t.take_result() {
                if let Ok(m) = r {
                    self.info.extend(m);
                }
                self.info_task = None;
            }
            return;
        }
        let new: Vec<PathBuf> = self
            .rows
            .iter()
            .filter_map(|r| r.exe.clone())
            .filter(|p| !self.asked.contains(p))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !new.is_empty() {
            self.asked.extend(new.iter().cloned());
            self.info_task = Some(BackgroundTask::spawn(ctx, move |_| fileinfo::describe(&new)));
        }
    }

    fn visible(&self, r: &ProcRow) -> bool {
        if !matches_filter(r, &self.filter) {
            return false;
        }
        let info = self.info_of(r);
        match self.view {
            View::All => true,
            View::NotMicrosoft => info.is_none_or(|i| !i.company.to_ascii_lowercase().contains("microsoft")),
            View::Unsigned => {
                info.is_some_and(|i| matches!(i.signature, Signature::Unsigned | Signature::Untrusted))
            }
            View::MyProcesses => {
                let me = std::env::var("USERNAME").unwrap_or_default();
                r.user.eq_ignore_ascii_case(&me)
            }
        }
    }

    fn end(&mut self, ctx: &AppCtx<'_>, targets: &[(u32, String)]) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let mut ended = Vec::new();
        let mut failed = Vec::new();
        for (pid, name) in targets {
            if is_critical(name) || *pid == std::process::id() {
                continue;
            }
            match terminate(*pid) {
                Ok(()) => ended.push(format!("{name} ({pid})")),
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }
        if !ended.is_empty() {
            let _ = ctx
                .journal
                .record("procman", format!("Ended {}", ended.join(", ")), UndoAction::None);
        }
        self.message = Some(if failed.is_empty() {
            Ok(format!(
                "Ended {} process{}.",
                ended.len(),
                if ended.len() == 1 { "" } else { "es" }
            ))
        } else {
            Err(format!(
                "Ended {}; could not end {}",
                ended.len(),
                failed.join("; ")
            ))
        });
    }

    /// Pids of `root` and all its descendants (children first).
    fn tree(&self, root: u32) -> Vec<(u32, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            for c in self.rows.iter().filter(|r| r.parent == Some(p) && r.pid != p) {
                stack.push(c.pid);
            }
            if let Some(r) = self.rows.iter().find(|r| r.pid == p) {
                out.push((p, r.name.clone()));
            }
        }
        out.reverse();
        out
    }

    fn rule_targets(&self, rule: usize) -> Vec<(u32, String)> {
        self.rows
            .iter()
            .filter(|r| !is_critical(&r.name) && r.pid != std::process::id() && r.pid > 4)
            .filter(|r| {
                let info = self.info_of(r);
                match rule {
                    0 => info.is_some_and(|i| i.signature == Signature::Unsigned),
                    1 => info.is_some_and(|i| i.signature == Signature::Untrusted),
                    2 => r.cpu >= self.cpu_limit,
                    _ => r.memory >= self.memory_limit_mb * 1024 * 1024,
                }
            })
            .map(|r| (r.pid, r.name.clone()))
            .collect()
    }

    fn processes_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let selected_row = self
            .selected
            .and_then(|pid| self.rows.iter().find(|r| r.pid == pid))
            .cloned();
        let has = selected_row.is_some();
        let crit = selected_row.as_ref().is_some_and(|r| is_critical(&r.name));
        let buttons = [
            Some(ToolButton::new(icons::X, "End").enabled(has && !crit)),
            Some(ToolButton::new(icons::TREE_STRUCTURE, "End tree").enabled(has && !crit)),
            Some(ToolButton::new(icons::PAUSE, "Suspend").enabled(has && !crit)),
            Some(ToolButton::new(icons::PLAY, "Resume").enabled(has)),
            Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Restart").enabled(has && !crit)),
            None,
            Some(ToolButton::new(icons::STOP_CIRCLE, "Block").enabled(has && !crit && ctx.elevated)),
            Some(ToolButton::new(icons::FOLDER_OPEN, "Open folder").enabled(has)),
            Some(ToolButton::new(icons::MAGNIFYING_GLASS, "Look up").enabled(has)),
        ];
        let clicked = frame::toolbar(ui, &buttons, |_| {});
        if let Some(r) = &selected_row {
            match clicked {
                Some(0) => self.confirm.ask(
                    format!("End \"{}\" (PID {})? Unsaved data in it will be lost.", r.name, r.pid),
                    Ask::End(vec![(r.pid, r.name.clone())]),
                ),
                Some(1) => {
                    let n = self.tree(r.pid).len();
                    self.confirm.ask(
                        format!("End \"{}\" and the {} process{} it started?", r.name, n - 1, if n == 2 { "" } else { "es" }),
                        Ask::EndTree(r.pid),
                    )
                }
                Some(2) | Some(3) => {
                    let resume = clicked == Some(3);
                    self.message = Some(
                        suspend(r.pid, resume)
                            .map(|_| format!("{} {}.", r.name, if resume { "resumed" } else { "suspended" }))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
                Some(4) => {
                    self.message = Some(match &r.exe {
                        None => Err("The program's path is unknown.".into()),
                        Some(exe) => {
                            let args: Vec<String> = shell_words(&r.cmd).into_iter().skip(1).collect();
                            terminate(r.pid)
                                .and_then(|()| {
                                    std::thread::sleep(Duration::from_millis(400));
                                    std::process::Command::new(exe).args(&args).spawn()?;
                                    Ok(())
                                })
                                .map(|()| format!("Restarted {}.", r.name))
                                .map_err(|e| format!("{e:#}"))
                        }
                    })
                }
                Some(6) => self.confirm.ask(
                    format!(
                        "Block \"{}\" from starting for every user? Windows will start a harmless placeholder instead. \
                         Unblock it on the Blocked programs page or from the Change Journal.",
                        r.name
                    ),
                    Ask::Block(r.name.clone()),
                ),
                Some(7) => {
                    if let Some(exe) = &r.exe {
                        let _ = std::process::Command::new("explorer").arg(format!("/select,{}", exe.display())).spawn();
                    }
                }
                Some(8) => {
                    let q: String = r.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '+' }).collect();
                    let _ = std::process::Command::new("explorer")
                        .arg(format!("https://www.bing.com/search?q={q}+process"))
                        .spawn();
                }
                _ => {}
            }
        }

        let filter_hint = ctx.i18n.tr("common-filter");
        frame::options(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  {filter_hint}", icons::MAGNIFYING_GLASS))
                    .desired_width(200.0),
            );
            egui::ComboBox::from_id_salt("view")
                .selected_text(match self.view {
                    View::All => "All processes",
                    View::NotMicrosoft => "Not from Microsoft",
                    View::Unsigned => "Unsigned or untrusted",
                    View::MyProcesses => "My processes",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.view, View::All, "All processes");
                    ui.selectable_value(&mut self.view, View::MyProcesses, "My processes");
                    ui.selectable_value(&mut self.view, View::NotMicrosoft, "Not from Microsoft");
                    ui.selectable_value(&mut self.view, View::Unsigned, "Unsigned or untrusted");
                });
            ui.add_enabled_ui(has, |ui| {
                ui.menu_button("Priority", |ui| {
                    for (label, class) in PRIORITIES {
                        if ui.button(label).clicked()
                            && let Some(r) = &selected_row
                        {
                            self.message = Some(if ctx.config.dry_run {
                                Ok(ctx.i18n.tr("common-dry-run-skip"))
                            } else {
                                set_priority(r.pid, class)
                                    .map(|()| format!("{} priority set to {label}.", r.name))
                                    .map_err(|e| format!("Could not change priority: {e}"))
                            });
                            ui.close();
                        }
                    }
                });
            });
            ui.menu_button("End all…", |ui| {
                ui.set_min_width(260.0);
                let labels = [
                    "Unsigned programs".to_owned(),
                    "Programs with untrusted signatures".to_owned(),
                ];
                for (i, l) in labels.iter().enumerate() {
                    let n = self.rule_targets(i).len();
                    if ui
                        .add_enabled(n > 0, egui::Button::new(format!("{l} ({n})")))
                        .clicked()
                    {
                        self.ask_rule(i, l);
                        ui.close();
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.cpu_limit)
                            .range(5.0..=100.0)
                            .suffix(" % CPU"),
                    );
                    let n = self.rule_targets(2).len();
                    if ui
                        .add_enabled(n > 0, egui::Button::new(format!("Using more ({n})")))
                        .clicked()
                    {
                        self.ask_rule(2, &format!("Programs using {} % CPU or more", self.cpu_limit));
                        ui.close();
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.memory_limit_mb)
                            .range(64..=65536)
                            .suffix(" MB"),
                    );
                    let n = self.rule_targets(3).len();
                    if ui
                        .add_enabled(n > 0, egui::Button::new(format!("Using more ({n})")))
                        .clicked()
                    {
                        self.ask_rule(
                            3,
                            &format!("Programs using {} MB of memory or more", self.memory_limit_mb),
                        );
                        ui.close();
                    }
                });
                ui.weak("Windows' own critical processes are never included.");
            });
        });

        let mut rows: Vec<ProcRow> = self.rows.iter().filter(|r| self.visible(r)).cloned().collect();
        sort_rows(&mut rows, self.sort, self.ascending);
        let checking = self.info_task.is_some();
        frame::status(ui, &format!("{} processes", rows.len()), |ui| {
            if checking {
                ui.weak("checking signatures…");
                ui.spinner();
            }
        });
        if let Some(r) = &selected_row {
            egui::Panel::bottom("proc_details")
                .resizable(true)
                .default_size(180.0)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)))
                .show(ui, |ui| self.details(ui, r));
        }
        let clicked_row = frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            self.proc_table(ui, &rows)
        });
        if clicked_row.is_some() && clicked_row != self.selected {
            self.selected = clicked_row;
            self.modules = None;
        }
    }

    fn proc_table(&mut self, ui: &mut egui::Ui, rows: &[ProcRow]) -> Option<u32> {
        let mut clicked_row = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(180.0).at_least(110.0).clip(true))
            .column(Column::initial(60.0))
            .column(Column::initial(60.0))
            .column(Column::initial(80.0))
            .column(Column::initial(100.0).clip(true))
            .column(Column::initial(150.0).clip(true))
            .column(Column::initial(80.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                h.col(|ui| self.header(ui, "Name", SortKey::Name));
                h.col(|ui| self.header(ui, "PID", SortKey::Pid));
                h.col(|ui| self.header(ui, "CPU", SortKey::Cpu));
                h.col(|ui| self.header(ui, "Memory", SortKey::Memory));
                h.col(|ui| self.header(ui, "User", SortKey::User));
                h.col(|ui| {
                    ui.strong("Company");
                });
                h.col(|ui| {
                    ui.strong("Signature");
                });
                h.col(|ui| {
                    ui.strong("Path");
                });
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    let info = self.info_of(r);
                    row.set_selected(self.selected == Some(r.pid));
                    row.col(|ui| {
                        ui.label(&r.name);
                    });
                    row.col(|ui| {
                        ui.label(r.pid.to_string());
                    });
                    row.col(|ui| {
                        ui.label(format!("{:.1}%", r.cpu));
                    });
                    row.col(|ui| {
                        ui.label(format_bytes(r.memory));
                    });
                    row.col(|ui| {
                        ui.label(&r.user);
                    });
                    row.col(|ui| {
                        ui.label(info.map(|i| i.company.as_str()).unwrap_or(""));
                    });
                    row.col(|ui| {
                        let s = info.map(|i| i.signature).unwrap_or_default();
                        match s {
                            Signature::Unsigned | Signature::Untrusted => {
                                ui.colored_label(ui.visuals().warn_fg_color, s.label());
                            }
                            _ => {
                                ui.weak(s.label());
                            }
                        }
                    });
                    row.col(|ui| {
                        ui.weak(
                            r.exe
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                        );
                    });
                    if row.response().clicked() {
                        clicked_row = Some(r.pid);
                    }
                });
            });
        clicked_row
    }

    fn ask_rule(&mut self, rule: usize, label: &str) {
        let targets = self.rule_targets(rule);
        let names: Vec<String> = targets.iter().map(|(p, n)| format!("{n} ({p})")).collect();
        self.confirm.ask(
            format!(
                "{label}: end these {} processes?\n\n{}",
                targets.len(),
                names.join("\n")
            ),
            Ask::End(targets),
        );
    }

    fn details(&mut self, ui: &mut egui::Ui, r: &ProcRow) {
        let info = self.info_of(r).cloned().unwrap_or_default();
        ui.strong(format!("{} (PID {})", r.name, r.pid));
        egui::ScrollArea::vertical()
            .id_salt("details")
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Grid::new("proc_details")
                    .num_columns(2)
                    .spacing([12.0, 3.0])
                    .show(ui, |ui| {
                        let started = chrono::DateTime::from_timestamp(r.start_time as i64, 0)
                            .map(|t| {
                                t.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M:%S")
                                    .to_string()
                            })
                            .unwrap_or_default();
                        let parent = r
                            .parent
                            .and_then(|p| {
                                self.rows
                                    .iter()
                                    .find(|x| x.pid == p)
                                    .map(|x| format!("{} ({p})", x.name))
                            })
                            .unwrap_or_default();
                        for (k, v) in [
                            ("Description", info.description.clone()),
                            ("Company", info.company.clone()),
                            ("Version", info.version.clone()),
                            ("Signed by", info.signer.clone()),
                            ("Command line", r.cmd.clone()),
                            ("Started", started),
                            ("Started by", parent),
                            ("Threads", threads_of(r.pid).len().to_string()),
                        ] {
                            if !v.is_empty() {
                                ui.weak(k);
                                ui.add(egui::Label::new(v).wrap());
                                ui.end_row();
                            }
                        }
                    });
                ui.collapsing("Loaded modules", |ui| {
                    let mods = match &self.modules {
                        Some((pid, m)) if *pid == r.pid => m.clone(),
                        _ => {
                            let m = modules_of(r.pid);
                            self.modules = Some((r.pid, m.clone()));
                            m
                        }
                    };
                    if mods.is_empty() {
                        ui.weak("Not available (the process is protected or needs administrator rights).");
                    }
                    for m in mods {
                        ui.horizontal(|ui| {
                            ui.label(&m.path);
                            ui.weak(format_bytes(m.size as u64));
                        });
                    }
                });
            });
    }

    fn windows_page(&mut self, ui: &mut egui::Ui) {
        ui.label("Every visible window and the process that owns it. Select one to find its process.");
        ui.add_space(4.0);
        egui::Grid::new("windows")
            .striped(true)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.strong("Window");
                ui.strong("Process");
                ui.strong("");
                ui.end_row();
                for (title, pid) in windows_list() {
                    let name = self
                        .rows
                        .iter()
                        .find(|r| r.pid == pid)
                        .map(|r| r.name.clone())
                        .unwrap_or_default();
                    ui.label(&title);
                    ui.label(format!("{name} ({pid})"));
                    if ui.small_button("Show process").clicked() {
                        self.selected = Some(pid);
                        self.page = Page::Processes;
                        self.view = View::All;
                        self.filter.clear();
                    }
                    ui.end_row();
                }
            });
    }

    fn blocked_page(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Programs blocked from starting with this tool. Windows starts a harmless placeholder instead.",
        );
        ui.add_space(4.0);
        let list = blocked_programs();
        if list.is_empty() {
            ui.weak("No programs are blocked.");
        }
        for name in list {
            ui.horizontal(|ui| {
                ui.label(format!("{}  {name}", icons::STOP_CIRCLE));
                if ui
                    .add_enabled(ctx.elevated, egui::Button::new("Unblock").small())
                    .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"))
                    .clicked()
                {
                    self.message = Some(
                        unblock_program(&name, ctx.journal)
                            .map(|()| format!("Unblocked {name}."))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            });
        }
        show_message(ui, &self.message);
    }
}

/// Splits a command line into arguments, honouring double quotes.
pub fn shell_words(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in cmd.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Tool for ProcessManager {
    fn id(&self) -> &'static str {
        "procman"
    }

    fn category(&self) -> Category {
        Category::Information
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let sampler = self.sampler.get_or_insert_with(|| {
            Sampler::spawn(
                ui.ctx(),
                Duration::from_millis(1500),
                || SampleState {
                    sys: System::new().expect("sysinfo System"),
                    users: Users::new_with_refreshed_list().ok(),
                },
                sample,
            )
        });
        if let Some(rows) = sampler.latest() {
            self.rows = rows;
        }
        self.request_info(ui.ctx());

        match self.confirm.show(ui.ctx(), ctx.i18n) {
            Some(Ask::End(targets)) => self.end(ctx, &targets),
            Some(Ask::EndTree(pid)) => {
                let t = self.tree(pid);
                self.end(ctx, &t);
            }
            Some(Ask::Block(name)) => {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    block_program(&name, ctx.journal)
                        .map(|()| format!("{name} is blocked."))
                        .map_err(|e| format!("{e:#}"))
                });
            }
            None => {}
        }

        let pages = [Page::Processes, Page::Windows, Page::Blocked];
        let mut sel = pages.iter().position(|p| *p == self.page).unwrap_or(0);
        frame::left(ui, 190.0, |ui| {
            wm_core::ui::section_nav(
                ui,
                &[
                    (icons::LIST_CHECKS, "Processes"),
                    (icons::APP_WINDOW, "Windows"),
                    (icons::STOP_CIRCLE, "Blocked programs"),
                ],
                &mut sel,
            )
        });
        self.page = pages[sel];
        match self.page {
            Page::Processes => self.processes_page(ui, ctx),
            Page::Windows => {
                frame::status(ui, "Every visible window and the process that owns it", |_| {});
                frame::central(ui, |ui| self.windows_page(ui));
            }
            Page::Blocked => {
                frame::status(
                    ui,
                    "Programs blocked from starting with this tool; Windows starts a harmless placeholder instead",
                    |_| {},
                );
                frame::central(ui, |ui| self.blocked_page(ui, ctx));
            }
        }
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu: f32, memory: u64) -> ProcRow {
        ProcRow {
            pid,
            parent: None,
            name: name.into(),
            exe: Some(PathBuf::from(format!(r"C:\x\{name}"))),
            cpu,
            memory,
            user: String::new(),
            cmd: String::new(),
            start_time: 0,
        }
    }

    #[test]
    fn sorting() {
        let mut rows = vec![
            row(3, "b.exe", 5.0, 10),
            row(1, "A.exe", 1.0, 30),
            row(2, "c.exe", 9.0, 20),
        ];
        sort_rows(&mut rows, SortKey::Name, true);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [1, 3, 2]);
        sort_rows(&mut rows, SortKey::Cpu, false);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [2, 3, 1]);
        sort_rows(&mut rows, SortKey::Memory, false);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn filtering() {
        let r = row(42, "Notepad.exe", 0.0, 0);
        assert!(matches_filter(&r, ""));
        assert!(matches_filter(&r, "note"));
        assert!(matches_filter(&r, "42"));
        assert!(matches_filter(&r, r"c:\x"));
        assert!(!matches_filter(&r, "chrome"));
    }

    #[test]
    fn critical_and_words() {
        assert!(is_critical("CSRSS.exe"));
        assert!(!is_critical("notepad.exe"));
        assert_eq!(
            shell_words(r#""C:\a b\x.exe" --flag "two words""#),
            [r"C:\a b\x.exe", "--flag", "two words"]
        );
    }

    #[test]
    fn sample_includes_current_process_with_details() {
        let mut s = SampleState {
            sys: System::new().unwrap(),
            users: Users::new_with_refreshed_list().ok(),
        };
        let rows = sample(&mut s);
        let me = rows.iter().find(|r| r.pid == std::process::id()).expect("self");
        assert!(!me.user.is_empty(), "{me:?}");
        assert!(!threads_of(me.pid).is_empty());
        assert!(!modules_of(me.pid).is_empty());
    }

    #[test]
    fn terminate_suspend_and_priority_on_child_process() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        set_priority(child.id(), BELOW_NORMAL_PRIORITY_CLASS).unwrap();
        assert!(suspend(child.id(), false).unwrap() >= 1);
        assert!(suspend(child.id(), true).unwrap() >= 1);
        terminate(child.id()).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(1));
    }

    #[test]
    fn window_list_works() {
        // Just exercises the callback; a test runner may have no visible windows.
        let _ = windows_list();
    }
}
