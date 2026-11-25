//! My Task: schedule maintenance actions (see [`crate::actions`]) with Windows Task
//! Scheduler. Tasks live in the `\Win Manager RS\` folder and run this executable with
//! `--run <action>`, so they work while the app is closed.

use anyhow::{Context, Result, bail};
use wm_core::{
    AppCtx, Category, Tool, egui,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    ui::{Confirm, Message, show_message},
};
use wm_tools_optimizer::tasks::{self, Action, TaskRow};

use crate::actions::ACTIONS;

pub const FOLDER: &str = r"\Win Manager RS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Daily,
    Weekly(usize),
    AtLogon,
}

pub const DAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

/// `schtasks /create` arguments for a new task.
pub fn create_args(
    name: &str,
    exe: &str,
    action: &str,
    schedule: Schedule,
    time: &str,
    highest: bool,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "/create".into(),
        "/tn".into(),
        format!(r"{FOLDER}\{name}"),
        "/tr".into(),
        format!("\"{exe}\" --run {action}"),
    ];
    match schedule {
        Schedule::Daily => a.extend(["/sc".into(), "DAILY".into(), "/st".into(), time.into()]),
        Schedule::Weekly(d) => a.extend([
            "/sc".into(),
            "WEEKLY".into(),
            "/d".into(),
            DAYS[d % 7].into(),
            "/st".into(),
            time.into(),
        ]),
        Schedule::AtLogon => a.extend(["/sc".into(), "ONLOGON".into()]),
    }
    if highest {
        a.extend(["/rl".into(), "HIGHEST".into()]);
    }
    a.push("/f".into());
    a
}

pub fn valid_time(t: &str) -> bool {
    let Some((h, m)) = t.split_once(':') else {
        return false;
    };
    h.len() == 2
        && m.len() == 2
        && h.parse::<u8>().is_ok_and(|h| h < 24)
        && m.parse::<u8>().is_ok_and(|m| m < 60)
}

pub fn valid_name(n: &str) -> bool {
    !n.trim().is_empty() && n.len() <= 60 && n.chars().all(|c| c.is_alphanumeric() || " -_".contains(c))
}

pub fn our_tasks() -> Result<Vec<TaskRow>> {
    Ok(tasks::enumerate()?
        .into_iter()
        .filter(|t| t.path.starts_with(&format!(r"{FOLDER}\")))
        .collect())
}

pub struct MyTask {
    tasks: Option<Result<Vec<TaskRow>, String>>,
    name: String,
    action: usize,
    schedule: Schedule,
    time: String,
    highest: bool,
    confirm: Confirm<TaskRow>,
    message: Message,
}

impl Default for MyTask {
    fn default() -> Self {
        Self {
            tasks: None,
            name: "Weekly cleanup".into(),
            action: 0,
            schedule: Schedule::Weekly(6),
            time: "10:00".into(),
            highest: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl MyTask {
    fn create(&mut self, ctx: &AppCtx<'_>) -> Result<String> {
        let name = self.name.trim().to_owned();
        if !valid_name(&name) {
            bail!("Use letters, digits, spaces, - and _ for the task name.");
        }
        if !matches!(self.schedule, Schedule::AtLogon) && !valid_time(&self.time) {
            bail!("Enter the time as HH:MM (24-hour).");
        }
        let (action, _, needs_admin) = ACTIONS[self.action];
        if needs_admin && !self.highest {
            bail!("\"{action}\" needs administrator rights: tick \"Run with highest privileges\".");
        }
        schedule(ctx, &name, action, self.schedule, &self.time, self.highest)
    }
}

/// Creates (or replaces) a task in our folder that runs `--run <action>`, journaled
/// with an undo that deletes it. Inputs are assumed valid.
pub fn schedule(
    ctx: &AppCtx<'_>,
    name: &str,
    action: &str,
    when: Schedule,
    time: &str,
    highest: bool,
) -> Result<String> {
    if ctx.config.dry_run {
        return Ok(ctx.i18n.tr("common-dry-run-skip"));
    }
    let exe = std::env::current_exe()?.display().to_string();
    let args = create_args(name, &exe, action, when, time, highest);
    run_checked(hidden_command("schtasks").args(&args)).context("creating the task")?;
    let path = format!(r"{FOLDER}\{name}");
    ctx.journal.record(
        "mytask",
        format!("Scheduled \"{name}\" ({action})"),
        UndoAction::RunCommand {
            program: "schtasks".into(),
            args: vec!["/delete".into(), "/tn".into(), path, "/f".into()],
        },
    )?;
    Ok(format!("Scheduled \"{name}\"."))
}

impl Tool for MyTask {
    fn id(&self) -> &'static str {
        "mytask"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.tasks.is_none() {
            self.tasks = Some(our_tasks().map_err(|e| format!("{e:#}")));
        }
        if let Some(t) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(
                tasks::perform(&t, Action::Delete)
                    .and_then(|(d, undo)| ctx.journal.record("mytask", d.clone(), undo).map(|_| d))
                    .map_err(|e| format!("{e:#}")),
            );
            self.tasks = None;
        }
        ui.label(
            "Schedule maintenance to run automatically. Tasks run this program in the background \
             (even when it is closed) and record what they did in the Change Journal.",
        );
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.name)
                        .hint_text("Task name")
                        .desired_width(180.0),
                );
                egui::ComboBox::from_id_salt("mytask_action")
                    .selected_text(ACTIONS[self.action].1)
                    .width(380.0)
                    .show_ui(ui, |ui| {
                        for (i, (_, desc, admin)) in ACTIONS.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.action,
                                i,
                                if *admin {
                                    format!("{desc} (admin)")
                                } else {
                                    (*desc).to_owned()
                                },
                            );
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.schedule, Schedule::Daily, "Daily");
                let weekly = matches!(self.schedule, Schedule::Weekly(_));
                if ui.selectable_label(weekly, "Weekly").clicked() && !weekly {
                    self.schedule = Schedule::Weekly(6);
                }
                ui.selectable_value(&mut self.schedule, Schedule::AtLogon, "At sign-in");
                if let Schedule::Weekly(d) = &mut self.schedule {
                    egui::ComboBox::from_id_salt("mytask_day")
                        .selected_text(DAYS[*d])
                        .width(70.0)
                        .show_ui(ui, |ui| {
                            for (i, day) in DAYS.iter().enumerate() {
                                ui.selectable_value(d, i, *day);
                            }
                        });
                }
                if !matches!(self.schedule, Schedule::AtLogon) {
                    ui.label("at");
                    ui.add(egui::TextEdit::singleline(&mut self.time).desired_width(50.0));
                }
            });
            ui.horizontal(|ui| {
                ui.add_enabled(
                    ctx.elevated,
                    egui::Checkbox::new(&mut self.highest, "Run with highest privileges"),
                )
                .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                if ui.button("Create task").clicked() {
                    self.message = Some(self.create(ctx).map_err(|e| format!("{e:#}")));
                    self.tasks = None;
                }
            });
        });
        show_message(ui, &self.message);
        ui.separator();

        match self.tasks.clone() {
            Some(Ok(list)) if list.is_empty() => {
                ui.weak("No scheduled tasks yet.");
            }
            Some(Ok(list)) => {
                egui::Grid::new("mytasks")
                    .num_columns(5)
                    .striped(true)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for h in ["Task", "Next run", "Last run", "Last result", ""] {
                            ui.strong(h);
                        }
                        ui.end_row();
                        for t in list {
                            ui.label(&t.name);
                            ui.label(t.next_run.clone().unwrap_or_else(|| "—".into()));
                            ui.label(t.last_run.clone().unwrap_or_else(|| "Never".into()));
                            ui.weak(tasks::result_text(t.last_result));
                            ui.horizontal(|ui| {
                                if ui.small_button("Run now").clicked() {
                                    self.message = Some(
                                        tasks::perform(&t, Action::Run)
                                            .map(|(d, _)| d)
                                            .map_err(|e| format!("{e:#}")),
                                    );
                                }
                                if ui.small_button("Delete").clicked() {
                                    self.confirm
                                        .ask(format!("Delete the scheduled task \"{}\"?", t.name), t.clone());
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schtasks_arguments() {
        let a = create_args(
            "Weekly cleanup",
            r"C:\Apps\win-manager.exe",
            "junk-clean",
            Schedule::Weekly(6),
            "10:00",
            false,
        );
        assert_eq!(a[2], r"\Win Manager RS\Weekly cleanup");
        assert_eq!(a[4], r#""C:\Apps\win-manager.exe" --run junk-clean"#);
        assert!(a.windows(2).any(|w| w == ["/d", "SUN"]));
        assert!(!a.contains(&"/rl".to_owned()));
        let b = create_args("x", "e", "restore-point", Schedule::AtLogon, "", true);
        assert!(
            b.windows(2).any(|w| w == ["/sc", "ONLOGON"]) && b.windows(2).any(|w| w == ["/rl", "HIGHEST"])
        );
    }

    #[test]
    fn validation() {
        assert!(valid_time("09:30") && !valid_time("24:00") && !valid_time("9:30") && !valid_time("ab:cd"));
        assert!(valid_name("Weekly cleanup") && !valid_name("bad\\name") && !valid_name("  "));
    }

    #[test]
    fn create_list_and_delete_real_task() {
        let name = format!("wm-test-{}", std::process::id());
        let args = create_args(
            &name,
            r"C:\Windows\System32\cmd.exe",
            "flush-dns",
            Schedule::Daily,
            "23:59",
            false,
        );
        run_checked(hidden_command("schtasks").args(&args)).unwrap();
        let found = our_tasks()
            .unwrap()
            .into_iter()
            .find(|t| t.name == name)
            .expect("task listed");
        assert!(found.actions.contains("--run flush-dns"), "{}", found.actions);
        run_checked(hidden_command("schtasks").args(["/delete", "/tn", &found.path, "/f"])).unwrap();
        assert!(!our_tasks().unwrap().iter().any(|t| t.name == name));
    }
}
