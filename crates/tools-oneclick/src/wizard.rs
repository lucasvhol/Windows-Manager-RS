//! Optimization Wizard: a guided sequence that takes the user through the tools that
//! matter for a faster PC, one step at a time (programs, updates, uninstalling,
//! startup, tasks, services, junk, registry, tweaks, drives), and ends by offering a
//! scheduled clean. It detects SSDs, touch screens and display scaling, and adapts its
//! advice to them. Tweaks are applied in the wizard itself and journaled one by one.

use std::collections::HashMap;

use wm_core::{
    AppCtx, Category, Layout, Tool, drives, egui, icons, machine,
    safety::restore_point,
    shell::shell_open,
    task::BackgroundTask,
    tweaks::{State, Tweak, parse},
    ui::{Message, frame, request_open_tool, show_message},
};

use crate::my_task::{self, Schedule};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    Balanced,
    Privacy,
    Performance,
}

const BALANCED: &[&str] = &[
    "menu_show_delay",
    "startup_delay",
    "show_extensions",
    "advertising_id",
    "tips_notifications",
    "silent_app_installs",
    "settings_suggestions",
    "lock_screen_facts",
    "end_task",
];
const PRIVACY_EXTRA: &[&str] = &[
    "tailored_experiences",
    "language_list",
    "track_launches",
    "inking_typing",
    "feedback_prompts",
    "recall_snapshots",
    "no_recent_files",
    "start_no_web_search",
    "start_no_recommendations",
    "diagnostic_required",
    "activity_history",
];
const PERFORMANCE_EXTRA: &[&str] = &[
    "mouse_hover_time",
    "hung_app_timeout",
    "wait_to_kill_app",
    "min_animate",
    "transparency",
    "ntfs_last_access",
    "multimedia_responsiveness",
];

/// Tweaks that make a touch screen harder to use: animations show where a tap landed.
const NOT_FOR_TOUCH: &[&str] = &["min_animate"];

pub fn recommended(goal: Goal) -> Vec<&'static str> {
    let extra = match goal {
        Goal::Balanced => &[][..],
        Goal::Privacy => PRIVACY_EXTRA,
        Goal::Performance => PERFORMANCE_EXTRA,
    };
    BALANCED.iter().chain(extra).copied().collect()
}

/// Every tweak definition in the app, by id.
pub fn catalog() -> HashMap<String, Tweak> {
    [
        wm_tools_optimizer::speed::DEFINITIONS,
        wm_tools_security::PRIVACY_TWEAKS,
        wm_tools_custom::EXPLORER_TWEAKS,
        wm_tools_custom::VISUAL_TWEAKS,
        wm_tools_custom::START_TWEAKS,
    ]
    .iter()
    .flat_map(|t| parse(t).unwrap_or_default())
    .map(|t| (t.id.clone(), t))
    .collect()
}

/// What the wizard knows about this PC.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// Fixed drives: (letter, solid state or unknown).
    pub drives: Vec<(char, Option<bool>)>,
    pub touch_points: u32,
    pub scale: u32,
}

impl Environment {
    pub fn detect() -> Self {
        Self {
            drives: drives::list()
                .into_iter()
                .filter(|d| d.kind == drives::DriveKind::Fixed)
                .map(|d| (d.letter, drives::is_ssd(d.letter)))
                .collect(),
            touch_points: machine::touch_points(),
            scale: machine::display_scale(),
        }
    }

    /// Hard disks benefit from defragmenting; SSDs only need TRIM, which Windows
    /// schedules itself.
    pub fn hard_disks(&self) -> Vec<char> {
        self.drives
            .iter()
            .filter(|(_, ssd)| *ssd == Some(false))
            .map(|(l, _)| *l)
            .collect()
    }

    pub fn touch(&self) -> bool {
        self.touch_points > 0
    }

    pub fn tweak_suits(&self, id: &str) -> bool {
        !(self.touch() && NOT_FOR_TOUCH.contains(&id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Start,
    Tool(&'static str),
    Settings(&'static str),
    Tweaks,
    Drives,
    Finish,
}

struct Step {
    title: &'static str,
    page: Page,
    why: &'static str,
    tips: &'static [&'static str],
}

const STEPS: [Step; 14] = [
    Step {
        title: "Before you start",
        page: Page::Start,
        why: "The wizard opens each tool in turn and explains what to look for. Every step can be skipped, and Back returns to the one before.",
        tips: &[],
    },
    Step {
        title: "Close programs",
        page: Page::Tool("procman"),
        why: "Programs left running hold memory and processor time. Close the ones you are not using before you continue.",
        tips: &[
            "Sort by memory to find the heaviest programs.",
            "Leave anything published by Microsoft Windows alone unless you know what it is.",
        ],
    },
    Step {
        title: "Install updates",
        page: Page::Settings("ms-settings:windowsupdate"),
        why: "Install waiting updates first, so the cleanup steps do not remove files an update still needs.",
        tips: &["Restart afterwards if Windows asks you to, then reopen the wizard."],
    },
    Step {
        title: "Uninstall programs",
        page: Page::Tool("uninstaller"),
        why: "Every installed program can add startup entries, services and background updaters. Remove the ones you no longer use.",
        tips: &[
            "Sort by size or install date to find forgotten programs.",
            "After uninstalling, let the tool look for leftover files and registry entries.",
        ],
    },
    Step {
        title: "Remove unused apps",
        page: Page::Tool("apps"),
        why: "Store and built-in apps you never open still update and may run in the background.",
        tips: &["Apps removed here can be reinstalled from the Microsoft Store."],
    },
    Step {
        title: "Startup programs",
        page: Page::Tool("startup"),
        why: "Each program that starts with Windows makes sign-in slower. Turn off the ones you can start yourself when you need them.",
        tips: &["Turning an entry off keeps it, so you can turn it back on later."],
    },
    Step {
        title: "Scheduled tasks",
        page: Page::Tool("tasks"),
        why: "Programs also start through scheduled tasks, often updaters and helpers. Turn off those of programs you removed or do not need.",
        tips: &["Leave the tasks in the Microsoft folder alone unless you know what they do."],
    },
    Step {
        title: "Services",
        page: Page::Tool("services"),
        why: "Services run in the background all the time. Only change those whose purpose you know.",
        tips: &["Prefer Manual to Disabled: a manual service still starts when something needs it."],
    },
    Step {
        title: "Junk files",
        page: Page::Tool("junk"),
        why: "Temporary files, caches and old logs take space and slow down searches and backups.",
        tips: &["Scan first, review what was found, then clean."],
    },
    Step {
        title: "Registry",
        page: Page::Tool("regclean"),
        why: "Programs that were removed often leave entries behind. Cleaning them tidies up menus and file types.",
        tips: &["Every fix is saved first and can be undone from the Change Journal."],
    },
    Step {
        title: "Windows settings",
        page: Page::Tweaks,
        why: "Settings that remove ads and nags, speed up menus and limit background activity, chosen for your goal.",
        tips: &[],
    },
    Step {
        title: "Drives",
        page: Page::Drives,
        why: "Hard disks get slower as files fragment. Solid-state drives do not; Windows only needs to trim them, which it does on a schedule.",
        tips: &[],
    },
    Step {
        title: "Registry defrag",
        page: Page::Tool("regdefrag"),
        why: "Deleted keys leave empty space in the registry files. Compacting rebuilds them without it; Windows restarts to finish.",
        tips: &[
            "It comes last because Windows restarts right after compacting.",
            "Needs the app to run as administrator. Skip it if the analysis finds little to save.",
        ],
    },
    Step {
        title: "Finish",
        page: Page::Finish,
        why: "Keep the PC tidy by cleaning it regularly.",
        tips: &[],
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    Todo,
    Opened,
    Done,
    Skipped,
}

const TASK_NAME: &str = "Weekly 1-Click Clean";

pub struct Wizard {
    step: usize,
    progress: [Progress; STEPS.len()],
    env: Option<Environment>,
    detect: Option<BackgroundTask<Environment>>,
    goal: Goal,
    plan: Vec<(Tweak, State, bool)>,
    restore_point: bool,
    restore_task: Option<BackgroundTask<()>>,
    /// The restore point was asked for from the tweaks step, which applies after it.
    apply_after_restore: bool,
    restore_made: bool,
    report: Vec<String>,
    message: Message,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            step: 0,
            progress: [Progress::Todo; STEPS.len()],
            env: None,
            detect: None,
            goal: Goal::Balanced,
            plan: Vec::new(),
            restore_point: true,
            restore_task: None,
            apply_after_restore: false,
            restore_made: false,
            report: Vec::new(),
            message: None,
        }
    }
}

impl Wizard {
    fn build_plan(&mut self) {
        let cat = catalog();
        let env = self.env.clone().unwrap_or_default();
        self.plan = recommended(self.goal)
            .into_iter()
            .filter_map(|id| cat.get(id).cloned())
            .map(|t| {
                let state = t.state();
                let on = state != State::Applied && env.tweak_suits(&t.id);
                (t, state, on)
            })
            .collect();
    }

    fn apply_all(&mut self, ctx: &AppCtx<'_>) {
        for (t, state, selected) in &mut self.plan {
            if !*selected || *state == State::Applied {
                continue;
            }
            if t.needs_admin() && !ctx.elevated {
                self.report
                    .push(format!("Skipped (needs administrator): {}", t.title));
                continue;
            }
            match t.apply(ctx.journal, "wizard") {
                Ok(()) => {
                    *state = State::Applied;
                    self.report.push(format!("Applied: {}", t.title));
                }
                Err(e) => self.report.push(format!("Failed: {} ({e:#})", t.title)),
            }
        }
        self.progress[self.step] = Progress::Done;
    }

    fn start_restore_point(&mut self, ui: &egui::Ui) {
        self.restore_task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
            restore_point::create("Before Optimization Wizard")
        }));
    }

    fn poll(&mut self, ctx: &AppCtx<'_>) {
        if let Some(t) = &mut self.detect {
            t.poll();
            if let Some(r) = t.take_result() {
                self.detect = None;
                self.env = r.ok();
                self.build_plan();
            }
        }
        if let Some(t) = &mut self.restore_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.restore_task = None;
                match r {
                    Ok(()) => {
                        self.restore_made = true;
                        self.message = Some(Ok("Restore point created.".into()));
                    }
                    Err(e) => {
                        self.report.push(format!("Restore point not created: {e}"));
                        self.message = Some(Err(format!("Restore point not created: {e}")));
                    }
                }
                if std::mem::take(&mut self.apply_after_restore) {
                    self.apply_all(ctx);
                }
            }
        }
    }

    fn environment(&self, ui: &mut egui::Ui) {
        let Some(env) = &self.env else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking at this PC…");
            });
            return;
        };
        egui::Grid::new("wizard-env")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.label("Drives");
                ui.label(if env.drives.is_empty() {
                    "none found".to_owned()
                } else {
                    env.drives
                        .iter()
                        .map(|(l, ssd)| {
                            let kind = match ssd {
                                Some(true) => "SSD",
                                Some(false) => "hard disk",
                                None => "type unknown",
                            };
                            format!("{l}: {kind}")
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                });
                ui.end_row();
                ui.label("Touch screen");
                ui.label(if env.touch() {
                    format!("yes ({} touch points)", env.touch_points)
                } else {
                    "no".to_owned()
                });
                ui.end_row();
                ui.label("Display scaling");
                ui.label(format!("{}%", env.scale));
                ui.end_row();
            });
        let mut notes = Vec::new();
        if env.touch() {
            notes.push("Animations stay on, because they show where a tap landed.");
        }
        if env.hard_disks().is_empty() {
            notes.push("No hard disks: the drive step has nothing to do.");
        }
        if env.scale >= 150 {
            notes.push("High display scaling: if text looks blurry in old programs, set their compatibility to override high-DPI scaling.");
        }
        for n in notes {
            ui.weak(format!("• {n}"));
        }
    }

    fn start_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.add_space(4.0);
        ui.strong("This PC");
        self.environment(ui);
        ui.add_space(8.0);
        ui.strong("Safety first");
        ui.horizontal(|ui| {
            let can = ctx.elevated && self.restore_task.is_none() && !self.restore_made;
            if ui
                .add_enabled(can, egui::Button::new("Create a restore point"))
                .on_disabled_hover_text(if ctx.elevated {
                    "A restore point was already created."
                } else {
                    "Needs the app to run as administrator."
                })
                .clicked()
            {
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                } else {
                    self.start_restore_point(ui);
                }
            }
            if self.restore_task.is_some() {
                ui.spinner();
                ui.label("Creating a restore point…");
            } else {
                ui.weak("Lets you return Windows to how it is now if something goes wrong.");
            }
        });
    }

    fn tweaks_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.horizontal(|ui| {
            ui.label("Goal:");
            let before = self.goal;
            ui.radio_value(&mut self.goal, Goal::Balanced, "Balanced")
                .on_hover_text("Remove ads and nags, speed up menus, show file extensions.");
            ui.radio_value(&mut self.goal, Goal::Privacy, "Privacy")
                .on_hover_text(
                    "Balanced, plus limit tracking, telemetry, recent-file history and web search.",
                );
            ui.radio_value(&mut self.goal, Goal::Performance, "Performance")
                .on_hover_text("Balanced, plus fewer animations and faster shutdown and file system tweaks.");
            if self.goal != before {
                self.build_plan();
            }
        });
        let env = self.env.clone().unwrap_or_default();
        egui::ScrollArea::vertical()
            .id_salt("wizard-tweaks")
            .max_height((ui.available_height() - 110.0).max(160.0))
            .show(ui, |ui| {
                for (t, state, selected) in &mut self.plan {
                    ui.horizontal(|ui| {
                        if *state == State::Applied {
                            let mut on = true;
                            ui.add_enabled(false, egui::Checkbox::new(&mut on, &t.title));
                            ui.weak("already applied");
                        } else {
                            let locked = t.needs_admin() && !ctx.elevated;
                            if locked {
                                *selected = false;
                            }
                            ui.add_enabled(!locked, egui::Checkbox::new(selected, &t.title))
                                .on_hover_text(&t.description)
                                .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                            if !env.tweak_suits(&t.id) {
                                ui.weak("left off: touch screen");
                            }
                        }
                    });
                }
            });
        let count = self
            .plan
            .iter()
            .filter(|(_, s, sel)| *sel && *s != State::Applied)
            .count();
        let offer_point = ctx.elevated && !self.restore_made && ctx.config.create_restore_points;
        if offer_point {
            ui.checkbox(&mut self.restore_point, "Create a restore point first");
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    count > 0 && self.restore_task.is_none(),
                    egui::Button::new(format!("Apply {count} changes")),
                )
                .clicked()
            {
                self.report.clear();
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                } else if offer_point && self.restore_point {
                    self.apply_after_restore = true;
                    self.start_restore_point(ui);
                } else {
                    self.apply_all(ctx);
                }
            }
            if self.restore_task.is_some() {
                ui.spinner();
                ui.label("Creating a restore point…");
            }
        });
        if !self.report.is_empty() {
            egui::ScrollArea::vertical()
                .id_salt("wizard-report")
                .max_height(80.0)
                .show(ui, |ui| {
                    for line in &self.report {
                        ui.label(line);
                    }
                });
        }
    }

    fn drives_page(&mut self, ui: &mut egui::Ui) {
        let Some(env) = &self.env else {
            self.environment(ui);
            return;
        };
        let hdd = env.hard_disks();
        if hdd.is_empty() {
            ui.label("All your drives are solid state (or of unknown type). Nothing to do here: defragmenting an SSD only wears it.");
        } else {
            let list: Vec<String> = hdd.iter().map(|l| format!("{l}:")).collect();
            ui.label(format!(
                "Hard disks: {}. Open Optimize Drives, select each of them and press Optimize. It can take an hour on a large, full disk.",
                list.join(", ")
            ));
        }
        ui.add_space(6.0);
        if ui.button("Open Optimize Drives").clicked() {
            self.progress[self.step] = Progress::Opened;
            self.message = shell_open("dfrgui.exe", "").err().map(|e| Err(format!("{e:#}")));
        }
    }

    fn finish_page(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let count = |p: Progress| {
            self.progress[..STEPS.len() - 1]
                .iter()
                .filter(|x| **x == p)
                .count()
        };
        let visited = count(Progress::Done) + count(Progress::Opened);
        ui.label(format!(
            "Steps done: {visited} · skipped: {} · not visited: {}",
            count(Progress::Skipped),
            count(Progress::Todo)
        ));
        ui.add_space(8.0);
        ui.strong("Clean regularly");
        ui.label(format!(
            "Schedule the 1-Click Cleaner to run every Sunday at 10:00 with its saved selection (task \"{TASK_NAME}\")."
        ));
        ui.horizontal(|ui| {
            if ui.button("Schedule it").clicked() {
                self.message = Some(
                    my_task::schedule(
                        ctx,
                        TASK_NAME,
                        "oneclick-clean",
                        Schedule::Weekly(6),
                        "10:00",
                        false,
                    )
                    .map_err(|e| format!("{e:#}")),
                );
                if matches!(self.message, Some(Ok(_))) {
                    self.progress[self.step] = Progress::Done;
                }
            }
            if ui.link("Choose another schedule in My Task").clicked() {
                request_open_tool(ui.ctx(), "mytask");
            }
            if ui.link("Choose what gets cleaned").clicked() {
                request_open_tool(ui.ctx(), "oneclickclean");
            }
        });
        ui.add_space(8.0);
        ui.weak("Some changes take effect after signing out or restarting. Every change can be undone from the Change Journal.");
        if ui.button("Start over").clicked() {
            let env = self.env.take();
            *self = Self::default();
            self.env = env;
            self.build_plan();
        }
    }
}

impl Tool for Wizard {
    fn id(&self) -> &'static str {
        "wizard"
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.env.is_none() && self.detect.is_none() {
            self.detect = Some(BackgroundTask::spawn(ui.ctx(), |_| Ok(Environment::detect())));
        }
        self.poll(ctx);

        // The original's step band: "Step (n/N): name", with the step's icon.
        let step_no = self.step;
        let name = self.step_name(step_no, ctx);
        let band = ui.visuals().faint_bg_color;
        egui::Panel::top("wizard-step")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(band)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Step ({}/{}): {name}", step_no + 1, STEPS.len()))
                            .size(18.0)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(icons::MAGIC_WAND)
                                .size(26.0)
                                .color(ui.visuals().selection.bg_fill),
                        );
                    });
                });
            });

        // Create Task … Next > / Cancel / Skip along the bottom, like the original.
        let last = STEPS.len() - 1;
        let (mut back, mut next, mut skip) = (false, false, false);
        frame::footer(
            ui,
            |ui| {
                if ui
                    .button("Create Task")
                    .on_hover_text("Schedule maintenance with My Task.")
                    .clicked()
                {
                    request_open_tool(ui.ctx(), "mytask");
                }
            },
            |ui| {
                if step_no < last {
                    skip = ui.add_enabled(step_no > 0, egui::Button::new("Skip")).clicked();
                }
                if ui.button("Cancel").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if step_no < last {
                    next = ui.button("Next >").clicked();
                }
                back = ui.add_enabled(step_no > 0, egui::Button::new("< Back")).clicked();
            },
        );
        if back {
            self.step -= 1;
            self.message = None;
        } else if next {
            // Like the original, Next opens the step's tool (or Settings page) and moves on.
            match STEPS[self.step].page {
                Page::Tool(id) => request_open_tool(ui.ctx(), id),
                Page::Settings(uri) => {
                    self.message = shell_open(uri, "").err().map(|e| Err(format!("{e:#}")));
                }
                _ => {}
            }
            if self.progress[self.step] != Progress::Skipped {
                self.progress[self.step] = Progress::Done;
            }
            self.step += 1;
            if self.message.as_ref().is_none_or(|m| m.is_ok()) {
                self.message = None;
            }
        } else if skip {
            self.progress[self.step] = Progress::Skipped;
            self.step += 1;
            self.message = None;
        }
        frame::central(ui, |ui| self.step_body(ui, ctx));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl Wizard {
    /// The step's name in the band: the tool's own name for tool steps, as the original.
    fn step_name(&self, step: usize, ctx: &AppCtx<'_>) -> String {
        match STEPS[step].page {
            Page::Tool(id) => ctx.i18n.tr(&format!("{id}-title")),
            _ => STEPS[step].title.to_owned(),
        }
    }

    fn step_body(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let step = &STEPS[self.step];
        let icon = match step.page {
            Page::Start => icons::INFO,
            Page::Tool(_) => icons::ARROW_SQUARE_OUT,
            Page::Settings(_) => icons::ARROWS_CLOCKWISE,
            Page::Tweaks => icons::SLIDERS,
            Page::Drives => icons::HARD_DRIVES,
            Page::Finish => icons::CHECK,
        };
        ui.horizontal_top(|ui| {
            ui.label(
                egui::RichText::new(icon)
                    .size(44.0)
                    .color(ui.visuals().selection.bg_fill),
            );
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(step.why);
                for tip in step.tips {
                    ui.weak(format!("• {tip}"));
                }
                match step.page {
                    Page::Tool(_) => {
                        ui.add_space(6.0);
                        ui.weak(format!(
                            "Clicking Next opens {} in its own window; come back here when you are done. Skip moves on without opening it.",
                            self.step_name(self.step, ctx)
                        ));
                    }
                    Page::Settings(_) => {
                        ui.add_space(6.0);
                        ui.weak("Clicking Next opens this page of Windows Settings.");
                    }
                    _ => {}
                }
            });
        });
        ui.add_space(10.0);
        match step.page {
            Page::Start => self.start_page(ui, ctx),
            Page::Tool(_) | Page::Settings(_) => {}
            Page::Tweaks => self.tweaks_page(ui, ctx),
            Page::Drives => self.drives_page(ui),
            Page::Finish => self.finish_page(ui, ctx),
        }
        show_message(ui, &self.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recommended_tweak_exists() {
        let cat = catalog();
        for goal in [Goal::Balanced, Goal::Privacy, Goal::Performance] {
            for id in recommended(goal) {
                assert!(cat.contains_key(id), "unknown tweak id {id} in {goal:?}");
            }
        }
        for id in NOT_FOR_TOUCH {
            assert!(cat.contains_key(*id), "{id}");
        }
    }

    #[test]
    fn environment_drives_advice() {
        let env = Environment {
            drives: vec![('C', Some(true)), ('D', Some(false)), ('E', None)],
            touch_points: 10,
            scale: 150,
        };
        assert_eq!(env.hard_disks(), vec!['D']);
        assert!(!env.tweak_suits("min_animate"));
        assert!(env.tweak_suits("transparency"));
        let desk = Environment {
            drives: vec![('C', Some(true))],
            ..Default::default()
        };
        assert!(desk.hard_disks().is_empty());
        assert!(desk.tweak_suits("min_animate"));
    }

    #[test]
    fn steps_open_existing_tools() {
        let ids = [
            "procman",
            "uninstaller",
            "apps",
            "startup",
            "tasks",
            "services",
            "junk",
            "regclean",
            "regdefrag",
        ];
        let tool_steps: Vec<&str> = STEPS
            .iter()
            .filter_map(|s| match s.page {
                Page::Tool(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(tool_steps, ids);
        assert_eq!(STEPS.first().unwrap().page, Page::Start);
        assert_eq!(STEPS.last().unwrap().page, Page::Finish);
    }
}
