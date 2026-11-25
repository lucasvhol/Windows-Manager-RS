//! My Task's "Live rules" page: edit the rules the background monitor checks, start or
//! stop the monitor, and see what it did.

use wm_core::{
    AppCtx, egui,
    rules::{self, Action, Rule, Rules, Trigger},
    tool_settings,
    ui::{Confirm, Message, show_message},
};

use crate::actions::ACTIONS;
use crate::monitor;

const ACTION_KINDS: [&str; 11] = [
    "Shut down",
    "Restart",
    "Sleep",
    "Hibernate",
    "Sign out",
    "Lock the PC",
    "Turn the monitor off",
    "Mute the sound",
    "Run a program",
    "Show a reminder",
    "Run a maintenance action",
];

fn default_action(kind: usize) -> Action {
    match kind {
        0 => Action::Shutdown,
        1 => Action::Restart,
        2 => Action::Sleep,
        3 => Action::Hibernate,
        4 => Action::SignOut,
        5 => Action::Lock,
        6 => Action::MonitorOff,
        7 => Action::Mute,
        8 => Action::Run {
            program: "notepad.exe".into(),
            args: String::new(),
        },
        9 => Action::Remind {
            text: "Time for a break.".into(),
        },
        _ => Action::Maintenance {
            id: ACTIONS[0].0.into(),
        },
    }
}

fn action_kind(a: &Action) -> usize {
    match a {
        Action::Shutdown => 0,
        Action::Restart => 1,
        Action::Sleep => 2,
        Action::Hibernate => 3,
        Action::SignOut => 4,
        Action::Lock => 5,
        Action::MonitorOff => 6,
        Action::Mute => 7,
        Action::Run { .. } => 8,
        Action::Remind { .. } => 9,
        Action::Maintenance { .. } => 10,
    }
}

fn new_rule() -> Rule {
    Rule {
        name: String::new(),
        enabled: true,
        trigger: Trigger::Idle { minutes: 15 },
        action: Action::Lock,
        repeat: true,
        countdown_secs: 60,
        keep_awake: false,
    }
}

pub fn validate(rule: &Rule, others: &[Rule], editing: Option<usize>) -> Result<(), String> {
    let name = rule.name.trim();
    if name.is_empty() {
        return Err("Give the rule a name.".into());
    }
    if others
        .iter()
        .enumerate()
        .any(|(i, r)| Some(i) != editing && r.name.eq_ignore_ascii_case(name))
    {
        return Err(format!("There's already a rule called \"{name}\"."));
    }
    match &rule.trigger {
        Trigger::ProcessStarts { name }
        | Trigger::ProcessExits { name }
        | Trigger::ProcessIoBelow { name, .. }
            if name.trim().is_empty() || name.contains(['\\', '/']) =>
        {
            return Err("Enter the program's file name, such as setup.exe.".into());
        }
        _ => {}
    }
    match &rule.action {
        Action::Run { program, .. } if program.trim().is_empty() => Err("Choose the program to run.".into()),
        Action::Remind { text } if text.trim().is_empty() => Err("Write the reminder text.".into()),
        _ => Ok(()),
    }
}

#[derive(Default)]
pub struct RulesPage {
    rules: Option<Rules>,
    draft: Option<(Option<usize>, Rule)>,
    confirm: Confirm<usize>,
    message: Message,
}

impl RulesPage {
    fn save(&mut self) {
        if let Some(r) = &self.rules
            && let Err(e) = tool_settings::save(rules::SETTINGS_ID, r)
        {
            self.message = Some(Err(format!("{e:#}")));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        let list = self
            .rules
            .get_or_insert_with(|| tool_settings::load(rules::SETTINGS_ID))
            .clone();
        if let Some(i) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if let Some(r) = &mut self.rules
                && i < r.rules.len()
            {
                r.rules.remove(i);
            }
            self.save();
        }
        ui.label("Rules react to what happens on the PC — a program finishing, the PC sitting idle, a USB drive — while the rule monitor runs in the background with an icon in the notification area.");
        ui.horizontal(|ui| {
            let running = monitor::is_running();
            if running {
                ui.colored_label(egui::Color32::from_rgb(80, 180, 100), "● Monitor running");
                if ui.button("Stop").clicked() {
                    self.message = monitor::stop().err().map(|e| Err(format!("{e:#}")));
                }
            } else {
                ui.weak("○ Monitor stopped");
                if ui.button("Start").clicked() {
                    self.message = Some(
                        monitor::start()
                            .map(|()| "Monitor started.".into())
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            let mut at_sign_in = monitor::starts_at_sign_in();
            if ui.checkbox(&mut at_sign_in, "Start it when I sign in").changed() {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    monitor::set_start_at_sign_in(ctx, at_sign_in)
                        .map(|()| "Saved.".into())
                        .map_err(|e| format!("{e:#}"))
                });
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(2));
        });
        show_message(ui, &self.message);
        ui.separator();

        let mut toggle = None;
        let mut edit = None;
        let mut delete = None;
        if list.rules.is_empty() {
            ui.weak("No rules yet.");
        }
        egui::Grid::new("rules-grid")
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                for (i, r) in list.rules.iter().enumerate() {
                    let mut on = r.enabled;
                    if ui.checkbox(&mut on, &r.name).changed() {
                        toggle = Some((i, on));
                    }
                    ui.label(format!("When {}", lowercase_first(&r.trigger.describe())));
                    let mut what = r.action.describe();
                    if r.action.is_power() && r.countdown_secs > 0 {
                        what += &format!(" (after {} s)", r.countdown_secs);
                    }
                    if !r.repeat {
                        what += ", once";
                    }
                    if r.keep_awake {
                        what += ", keeps the PC awake";
                    }
                    ui.label(what);
                    ui.horizontal(|ui| {
                        if ui.small_button("Edit").clicked() {
                            edit = Some(i);
                        }
                        if ui.small_button("Delete").clicked() {
                            delete = Some(i);
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some((i, on)) = toggle
            && let Some(r) = &mut self.rules
        {
            r.rules[i].enabled = on;
            self.save();
        }
        if let Some(i) = edit {
            self.draft = Some((Some(i), list.rules[i].clone()));
        }
        if let Some(i) = delete {
            self.confirm
                .ask(format!("Delete the rule \"{}\"?", list.rules[i].name), i);
        }
        if self.draft.is_none() && ui.button("New rule…").clicked() {
            self.draft = Some((None, new_rule()));
        }
        self.editor_ui(ui, &list);

        ui.add_space(8.0);
        ui.strong("What the monitor did");
        let log = monitor::log();
        if log.lines.is_empty() {
            ui.weak("Nothing yet.");
        }
        for l in log.lines.iter().rev().take(10) {
            ui.weak(l);
        }
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui, list: &Rules) {
        let Some((index, rule)) = &mut self.draft else {
            return;
        };
        let mut close = false;
        let mut saved: Option<(Option<usize>, Rule)> = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut rule.name).desired_width(200.0));
            });
            ui.horizontal(|ui| {
                ui.label("When");
                let mut kind = monitor::trigger_kind(&rule.trigger);
                egui::ComboBox::from_id_salt("rule-trigger")
                    .selected_text(monitor::trigger_kinds()[kind])
                    .show_ui(ui, |ui| {
                        for (i, k) in monitor::trigger_kinds().iter().enumerate() {
                            ui.selectable_value(&mut kind, i, *k);
                        }
                    });
                if kind != monitor::trigger_kind(&rule.trigger) {
                    rule.trigger = monitor::default_trigger(kind);
                }
                trigger_fields(ui, &mut rule.trigger);
            });
            ui.horizontal(|ui| {
                ui.label("Do");
                let mut kind = action_kind(&rule.action);
                egui::ComboBox::from_id_salt("rule-action")
                    .selected_text(ACTION_KINDS[kind])
                    .show_ui(ui, |ui| {
                        for (i, k) in ACTION_KINDS.iter().enumerate() {
                            ui.selectable_value(&mut kind, i, *k);
                        }
                    });
                if kind != action_kind(&rule.action) {
                    rule.action = default_action(kind);
                }
                action_fields(ui, &mut rule.action);
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut rule.repeat, "Every time (otherwise once)");
                if rule.action.is_power() {
                    ui.label("Warning");
                    ui.add(egui::DragValue::new(&mut rule.countdown_secs).range(0..=3600).suffix(" s"))
                        .on_hover_text("Shown in the notification area; right-click the icon to cancel.");
                }
                ui.checkbox(&mut rule.keep_awake, "Keep the PC awake while waiting")
                    .on_hover_text("Stops Windows from sleeping while this rule is on (for example until a download finishes).");
            });
            ui.horizontal(|ui| {
                if ui.button("Save rule").clicked() {
                    match validate(rule, &list.rules, *index) {
                        Ok(()) => {
                            rule.name = rule.name.trim().to_owned();
                            saved = Some((*index, rule.clone()));
                        }
                        Err(e) => self.message = Some(Err(e)),
                    }
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if let Some((index, rule)) = saved {
            let r = self.rules.get_or_insert_with(Rules::default);
            match index {
                Some(i) if i < r.rules.len() => r.rules[i] = rule,
                _ => r.rules.push(rule),
            }
            self.save();
            if !monitor::is_running() {
                self.message = Some(Ok("Saved. Start the monitor for rules to take effect.".into()));
            } else {
                self.message = Some(Ok("Saved; the monitor picks it up within a few seconds.".into()));
            }
            close = true;
        }
        if close {
            self.draft = None;
        }
    }
}

fn lowercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) if !s.starts_with("CPU") && !s.starts_with("USB") && !s.starts_with("PC") => {
            f.to_lowercase().collect::<String>() + c.as_str()
        }
        _ => s.to_owned(),
    }
}

fn trigger_fields(ui: &mut egui::Ui, t: &mut Trigger) {
    let name = |ui: &mut egui::Ui, n: &mut String| {
        ui.add(
            egui::TextEdit::singleline(n)
                .hint_text("program.exe")
                .desired_width(140.0),
        );
    };
    let secs = |ui: &mut egui::Ui, s: &mut u32| {
        ui.label("for");
        ui.add(egui::DragValue::new(s).range(1..=86_400).suffix(" s"));
    };
    match t {
        Trigger::CpuAbove { percent, secs: s } | Trigger::CpuBelow { percent, secs: s } => {
            ui.add(egui::DragValue::new(percent).range(1.0..=100.0).suffix(" %"));
            secs(ui, s);
        }
        Trigger::ProcessStarts { name: n } | Trigger::ProcessExits { name: n } => name(ui, n),
        Trigger::ProcessIoBelow {
            name: n,
            kbps,
            secs: s,
        } => {
            name(ui, n);
            ui.label("below");
            ui.add(
                egui::DragValue::new(kbps)
                    .range(0.0..=1_000_000.0)
                    .suffix(" KB/s"),
            );
            secs(ui, s);
        }
        Trigger::Idle { minutes } | Trigger::TakeRest { minutes } => {
            ui.add(egui::DragValue::new(minutes).range(1..=1440).suffix(" min"));
        }
        Trigger::NetAbove { kbps, secs: s } | Trigger::NetBelow { kbps, secs: s } => {
            ui.add(
                egui::DragValue::new(kbps)
                    .range(0.0..=10_000_000.0)
                    .suffix(" KB/s"),
            );
            secs(ui, s);
        }
        Trigger::UsbInserted | Trigger::UsbRemoved | Trigger::Locked | Trigger::Unlocked => {}
    }
}

fn action_fields(ui: &mut egui::Ui, a: &mut Action) {
    match a {
        Action::Run { program, args } => {
            ui.add(
                egui::TextEdit::singleline(program)
                    .hint_text("program")
                    .desired_width(200.0),
            );
            if ui.small_button("Browse…").clicked()
                && let Some(p) = rfd_pick()
            {
                *program = p;
            }
            ui.add(
                egui::TextEdit::singleline(args)
                    .hint_text("arguments")
                    .desired_width(120.0),
            );
        }
        Action::Remind { text } => {
            ui.add(egui::TextEdit::singleline(text).desired_width(260.0));
        }
        Action::Maintenance { id } => {
            egui::ComboBox::from_id_salt("rule-maint")
                .selected_text(ACTIONS.iter().find(|x| x.0 == id).map_or(id.as_str(), |x| x.1))
                .width(320.0)
                .show_ui(ui, |ui| {
                    for (aid, desc, admin) in ACTIONS {
                        if !admin {
                            ui.selectable_value(id, (*aid).to_owned(), *desc);
                        }
                    }
                });
        }
        _ => {}
    }
}

fn rfd_pick() -> Option<String> {
    rfd::FileDialog::new()
        .add_filter("Programs", &["exe", "bat", "cmd", "lnk"])
        .pick_file()
        .map(|p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_validation() {
        let mut r = new_rule();
        assert!(validate(&r, &[], None).is_err(), "needs a name");
        r.name = "Lock when idle".into();
        assert!(validate(&r, &[], None).is_ok());
        assert!(
            validate(&r, std::slice::from_ref(&r), None).is_err(),
            "duplicate name"
        );
        assert!(
            validate(&r, std::slice::from_ref(&r), Some(0)).is_ok(),
            "editing itself"
        );
        r.trigger = Trigger::ProcessExits {
            name: r"C:\x\setup.exe".into(),
        };
        assert!(validate(&r, &[], None).is_err());
        r.trigger = Trigger::ProcessExits {
            name: "setup.exe".into(),
        };
        r.action = Action::Remind { text: " ".into() };
        assert!(validate(&r, &[], None).is_err());
        for k in 0..ACTION_KINDS.len() {
            assert_eq!(action_kind(&default_action(k)), k);
        }
        assert_eq!(lowercase_first("No input for 3 min"), "no input for 3 min");
        assert_eq!(lowercase_first("CPU above 9% for 1 s"), "CPU above 9% for 1 s");
    }
}
