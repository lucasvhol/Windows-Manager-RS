//! Process Manager: live process list with end-process and priority control.

use std::path::PathBuf;
use std::time::Duration;

use egui_extras::{Column, TableBuilder};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_CREATION_FLAGS, PROCESS_SET_INFORMATION, PROCESS_TERMINATE,
    SetPriorityClass, TerminateProcess,
};
use wm_core::{
    AppCtx, Category, Tool, egui,
    safety::UndoAction,
    task::Sampler,
    ui::{Confirm, Message, format_bytes, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    /// Share of total CPU capacity, 0–100.
    pub cpu: f32,
    pub memory: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Pid,
    Cpu,
    Memory,
}

pub fn sort_rows(rows: &mut [ProcRow], key: SortKey, ascending: bool) {
    rows.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Pid => a.pid.cmp(&b.pid),
            SortKey::Cpu => a.cpu.total_cmp(&b.cpu),
            SortKey::Memory => a.memory.cmp(&b.memory),
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

fn sample(sys: &mut System) -> Vec<ProcRow> {
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_exe(UpdateKind::OnlyIfNotSet),
    );
    let cpus = sys.cpus().len().max(1) as f32;
    sys.processes()
        .values()
        .filter(|p| p.pid().as_u32() != 0)
        .map(|p| ProcRow {
            pid: p.pid().as_u32(),
            name: p.name().to_string_lossy().into_owned(),
            exe: p.exe().map(PathBuf::from),
            cpu: p.cpu_usage() / cpus,
            memory: p.memory(),
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

pub struct ProcessManager {
    sampler: Option<Sampler<Vec<ProcRow>>>,
    rows: Vec<ProcRow>,
    filter: String,
    sort: SortKey,
    ascending: bool,
    selected: Option<u32>,
    confirm: Confirm<(u32, String)>,
    message: Message,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self {
            sampler: None,
            rows: Vec::new(),
            filter: String::new(),
            sort: SortKey::Cpu,
            ascending: false,
            selected: None,
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
                self.ascending = matches!(key, SortKey::Name | SortKey::Pid);
            }
        }
    }

    fn end_process(&mut self, ctx: &AppCtx<'_>, pid: u32, name: &str) {
        if ctx.config.dry_run {
            self.message = Some(Ok(format!(
                "{} ({name}, PID {pid})",
                ctx.i18n.tr("common-dry-run-skip")
            )));
            return;
        }
        self.message = Some(match terminate(pid) {
            Ok(()) => {
                let _ = ctx.journal.record(
                    "procman",
                    format!("Ended process {name} (PID {pid})"),
                    UndoAction::None,
                );
                Ok(format!("Ended {name} (PID {pid})."))
            }
            Err(e) => Err(format!("Could not end {name}: {e}")),
        });
    }
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
                || System::new().expect("sysinfo System"),
                sample,
            )
        });
        if let Some(rows) = sampler.latest() {
            self.rows = rows;
        }

        if let Some((pid, name)) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.end_process(ctx, pid, &name);
        }

        let mut rows: Vec<ProcRow> = self
            .rows
            .iter()
            .filter(|r| matches_filter(r, &self.filter))
            .cloned()
            .collect();
        sort_rows(&mut rows, self.sort, self.ascending);
        let selected_row = self
            .selected
            .and_then(|pid| self.rows.iter().find(|r| r.pid == pid))
            .cloned();

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            ui.weak(format!("{} processes", rows.len()));
            ui.separator();
            ui.add_enabled_ui(selected_row.is_some(), |ui| {
                if ui.button("End process").clicked()
                    && let Some(r) = &selected_row
                {
                    self.confirm.ask(
                        format!(
                            "End \"{}\" (PID {})? Unsaved data in it will be lost.",
                            r.name, r.pid
                        ),
                        (r.pid, r.name.clone()),
                    );
                }
                if ui.button("Open file location").clicked()
                    && let Some(exe) = selected_row.as_ref().and_then(|r| r.exe.clone())
                {
                    let _ = std::process::Command::new("explorer")
                        .arg(format!("/select,{}", exe.display()))
                        .spawn();
                }
                ui.menu_button("Set priority", |ui| {
                    for (label, class) in PRIORITIES {
                        if ui.button(label).clicked()
                            && let Some(r) = &selected_row
                        {
                            self.message = Some(if ctx.config.dry_run {
                                Ok(ctx.i18n.tr("common-dry-run-skip"))
                            } else {
                                set_priority(r.pid, class)
                                    .map(|()| {
                                        let _ = ctx.journal.record(
                                            "procman",
                                            format!("Set priority of {} (PID {}) to {label}", r.name, r.pid),
                                            UndoAction::None,
                                        );
                                        format!("{} priority set to {label}.", r.name)
                                    })
                                    .map_err(|e| format!("Could not change priority: {e}"))
                            });
                            ui.close();
                        }
                    }
                });
            });
        });
        show_message(ui, &self.message);
        ui.add_space(4.0);

        let mut clicked = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(200.0).at_least(120.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::initial(70.0))
            .column(Column::initial(90.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                h.col(|ui| self.header(ui, "Name", SortKey::Name));
                h.col(|ui| self.header(ui, "PID", SortKey::Pid));
                h.col(|ui| self.header(ui, "CPU", SortKey::Cpu));
                h.col(|ui| self.header(ui, "Memory", SortKey::Memory));
                h.col(|ui| {
                    ui.strong("Path");
                });
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let r = &rows[row.index()];
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
                        ui.weak(
                            r.exe
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                        );
                    });
                    if row.response().clicked() {
                        clicked = Some(r.pid);
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

    fn row(pid: u32, name: &str, cpu: f32, memory: u64) -> ProcRow {
        ProcRow {
            pid,
            name: name.into(),
            exe: Some(PathBuf::from(format!(r"C:\x\{name}"))),
            cpu,
            memory,
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
    fn sample_includes_current_process() {
        let mut sys = System::new().unwrap();
        let rows = sample(&mut sys);
        assert!(rows.iter().any(|r| r.pid == std::process::id()));
    }

    #[test]
    fn terminate_and_priority_on_child_process() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        set_priority(child.id(), BELOW_NORMAL_PRIORITY_CLASS).unwrap();
        terminate(child.id()).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(1));
    }
}
