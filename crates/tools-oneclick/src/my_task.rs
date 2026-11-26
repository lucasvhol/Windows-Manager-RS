//! My Task: schedule maintenance actions (see [`crate::actions`]) with Windows Task
//! Scheduler. Tasks live in the `\Win Manager RS\` folder and run this executable with
//! `--run <action>`, so they work while the app is closed.

use anyhow::{Result, bail};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    ui::{Confirm, Message, frame, section, section_nav, show_message},
};
use wm_tools_optimizer::tasks::{self, Action, TaskRow};

use crate::actions::ACTIONS;

pub use wm_core::scheduler::{DAYS, FOLDER, Schedule, schedule, valid_time};

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
    live: bool,
    rules: crate::my_task_rules::RulesPage,
}

impl Default for MyTask {
    fn default() -> Self {
        Self {
            live: false,
            rules: Default::default(),
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

impl Tool for MyTask {
    fn id(&self) -> &'static str {
        "mytask"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let mut page = usize::from(self.live);
        // The original's bottom bar: Task Scheduler on the left, Exit on the right.
        let mut exit = false;
        frame::footer(
            ui,
            |ui| {
                if ui.button("Task Scheduler").clicked() {
                    wm_core::ui::request_open_tool(ui.ctx(), "tasks");
                }
            },
            |ui| {
                exit = ui.button("Exit").clicked();
            },
        );
        if exit {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        frame::left(ui, 180.0, |ui| {
            section_nav(
                ui,
                &[
                    (icons::CALENDAR_CHECK, "Scheduled Task"),
                    (icons::LIGHTNING, "Event Rules"),
                ],
                &mut page,
            )
        });
        self.live = page == 1;
        if self.live {
            frame::central(ui, |ui| self.rules.ui(ui, ctx));
            return;
        }
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
        let count = match &self.tasks {
            Some(Ok(l)) => format!("{} scheduled tasks", l.len()),
            _ => String::new(),
        };
        frame::status(ui, &count, |ui| {
            ui.weak("Tasks run in the background, even when this app is closed, and record what they did in the Change Journal.");
        });
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            section(
                ui,
                "New task",
                |_| {},
                |ui| {
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
                            ui.add(egui::TextEdit::singleline(&mut self.time).desired_width(64.0));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            ctx.elevated,
                            egui::Checkbox::new(&mut self.highest, "Run with highest privileges"),
                        )
                        .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                        if ui.button(format!("{}  Create task", icons::PLUS)).clicked() {
                            self.message = Some(self.create(ctx).map_err(|e| format!("{e:#}")));
                            self.tasks = None;
                        }
                    });
                },
            );
            ui.add_space(8.0);
            section(
                ui,
                "Scheduled tasks",
                |_| {},
                |ui| match self.tasks.clone() {
                    Some(Ok(list)) if list.is_empty() => {
                        ui.weak("No scheduled tasks yet. Create one above.");
                    }
                    Some(Ok(list)) => {
                        egui::Grid::new("mytasks")
                            .num_columns(5)
                            .striped(true)
                            .spacing([16.0, 6.0])
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
                                            self.confirm.ask(
                                                format!("Delete the scheduled task \"{}\"?", t.name),
                                                t.clone(),
                                            );
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
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::process::{hidden_command, run_checked};
    use wm_core::scheduler::create_args;

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
