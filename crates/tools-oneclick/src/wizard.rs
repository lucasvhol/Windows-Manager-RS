//! Optimization Wizard: pick a goal, review the recommended tweaks, apply them in one
//! go (after a restore point when possible). Every tweak is journaled individually.

use std::collections::HashMap;

use wm_core::{
    AppCtx, Category, Tool, egui,
    safety::restore_point,
    task::BackgroundTask,
    tweaks::{State, Tweak, parse},
    ui::{Message, show_message},
};

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

pub struct Wizard {
    step: u8,
    goal: Goal,
    plan: Vec<(Tweak, State, bool)>,
    restore_point: bool,
    task: Option<BackgroundTask<()>>,
    report: Vec<String>,
    message: Message,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            step: 1,
            goal: Goal::Balanced,
            plan: Vec::new(),
            restore_point: true,
            task: None,
            report: Vec::new(),
            message: None,
        }
    }
}

impl Wizard {
    fn build_plan(&mut self) {
        let cat = catalog();
        self.plan = recommended(self.goal)
            .into_iter()
            .filter_map(|id| cat.get(id).cloned())
            .map(|t| {
                let state = t.state();
                (t, state, state != State::Applied)
            })
            .collect();
    }

    fn apply_all(&mut self, ctx: &AppCtx<'_>) {
        self.report.clear();
        for (t, state, selected) in &self.plan {
            if !*selected || *state == State::Applied {
                continue;
            }
            if t.needs_admin() && !ctx.elevated {
                self.report
                    .push(format!("Skipped (needs administrator): {}", t.title));
                continue;
            }
            match t.apply(ctx.journal, "wizard") {
                Ok(()) => self.report.push(format!("Applied: {}", t.title)),
                Err(e) => self.report.push(format!("Failed: {} ({e:#})", t.title)),
            }
        }
        self.step = 3;
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
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                if let Err(e) = r {
                    self.report.push(format!("Restore point not created: {e}"));
                }
                self.apply_all(ctx);
            }
        }
        ui.weak(format!("Step {} of 3", self.step));
        match self.step {
            1 => {
                ui.heading("What matters most to you?");
                for (g, title, desc) in [
                    (
                        Goal::Balanced,
                        "Balanced",
                        "Remove ads and nags, speed up menus, show file extensions.",
                    ),
                    (
                        Goal::Privacy,
                        "Privacy",
                        "Balanced, plus limit tracking, telemetry, recent-file history and web search.",
                    ),
                    (
                        Goal::Performance,
                        "Performance",
                        "Balanced, plus fewer animations and faster shutdown and file system tweaks.",
                    ),
                ] {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.goal, g, title);
                        ui.weak(desc);
                    });
                }
                ui.add_space(8.0);
                if ui.button("Next ›").clicked() {
                    self.build_plan();
                    self.step = 2;
                }
            }
            2 => {
                ui.heading("Review the changes");
                ui.label("Ticked items will be applied. Items already in place are shown for reference.");
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 90.0)
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
                                }
                            });
                        }
                    });
                ui.add_enabled(
                    ctx.elevated,
                    egui::Checkbox::new(&mut self.restore_point, "Create a restore point first"),
                )
                .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
                let count = self
                    .plan
                    .iter()
                    .filter(|(_, s, sel)| *sel && *s != State::Applied)
                    .count();
                ui.horizontal(|ui| {
                    if ui.button("‹ Back").clicked() {
                        self.step = 1;
                    }
                    if ui
                        .add_enabled(
                            count > 0 && self.task.is_none(),
                            egui::Button::new(format!("Apply {count} changes")),
                        )
                        .clicked()
                    {
                        if ctx.config.dry_run {
                            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                        } else if ctx.elevated && self.restore_point && ctx.config.create_restore_points {
                            self.task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                                restore_point::create("Before Optimization Wizard")
                            }));
                        } else {
                            self.apply_all(ctx);
                        }
                    }
                    if self.task.is_some() {
                        ui.spinner();
                        ui.label("Creating a restore point…");
                    }
                });
                show_message(ui, &self.message);
            }
            _ => {
                ui.heading("Done");
                for line in &self.report {
                    ui.label(line);
                }
                ui.weak("Some changes take effect after signing out or restarting. Every change can be undone from the Change Journal.");
                if ui.button("Start over").clicked() {
                    self.step = 1;
                    self.report.clear();
                }
            }
        }
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
    }
}
