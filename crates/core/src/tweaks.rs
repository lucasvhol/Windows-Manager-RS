//! Declarative registry tweaks loaded from TOML, applied with exact undo snapshots,
//! plus a reusable panel that lists them as toggles.
//!
//! ```toml
//! [[tweak]]
//! id = "menu_delay"
//! title = "Faster menus"
//! description = "Shows menus after 100 ms instead of 400 ms."
//! group = "Responsiveness"
//! effect = "immediate"            # immediate | sign-out | restart
//! [[tweak.set]]
//! key = 'HKCU\Control Panel\Desktop'
//! name = "MenuShowDelay"
//! on = { sz = "100" }
//! off = { sz = "400" }            # or off = "delete"
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{
    AppCtx,
    egui::{self, Ui},
    registry::{self, RawValue},
    safety::{ChangeJournal, UndoAction},
    ui::{Message, show_message},
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Data {
    Sz(String),
    Dword(u32),
    Delete,
}

impl Data {
    fn to_raw(&self) -> Option<RawValue> {
        match self {
            Data::Sz(s) => Some(registry::string(s)),
            Data::Dword(d) => Some(registry::dword(*d)),
            Data::Delete => None,
        }
    }

    /// Whether the current registry value matches this data.
    pub fn matches(&self, current: Option<&RawValue>) -> bool {
        match (self, current) {
            (Data::Delete, None) => true,
            (Data::Sz(s), Some(v)) => registry::decode_string(v) == *s,
            (Data::Dword(d), Some(v)) => registry::decode_dword(v) == Some(*d),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Setting {
    pub key: String,
    pub name: String,
    pub on: Data,
    pub off: Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Effect {
    #[default]
    Immediate,
    SignOut,
    Restart,
    /// Takes effect when Explorer restarts (the panel offers a restart button).
    Explorer,
}

/// How a tweak is presented and applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// On/off switch over one or more `set` entries.
    #[default]
    Toggle,
    /// A numeric value in `target`, edited within `number.min..=number.max`.
    Number,
    /// One of several `option` values for `target`.
    Choice,
    /// A button that runs `action.program`.
    Action,
}

/// The single registry value a number or choice tweak edits.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Target {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NumberSpec {
    pub min: u32,
    pub max: u32,
    /// Windows default; `None` means the value is normally absent.
    #[serde(default)]
    pub default: Option<u32>,
    #[serde(default)]
    pub unit: String,
    /// Store as REG_SZ text instead of REG_DWORD.
    #[serde(default)]
    pub sz: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChoiceOption {
    pub label: String,
    pub value: Data,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ActionSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Confirmation question shown before running.
    #[serde(default)]
    pub confirm: Option<String>,
    /// Command (program followed by arguments) that reverses the action, if any.
    #[serde(default)]
    pub undo: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tweak {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub effect: Effect,
    #[serde(default)]
    pub kind: Kind,
    /// Forces the administrator requirement (e.g. for actions).
    #[serde(default)]
    pub admin: bool,
    #[serde(rename = "set", default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub target: Option<Target>,
    #[serde(default)]
    pub number: Option<NumberSpec>,
    #[serde(rename = "option", default)]
    pub options: Vec<ChoiceOption>,
    #[serde(default)]
    pub action: Option<ActionSpec>,
    /// Shown in red; turning such a switch on asks for confirmation (security-weakening options).
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Applied,
    NotApplied,
    /// Some settings match `on`, some don't (changed elsewhere).
    Partial,
}

impl Tweak {
    pub fn needs_admin(&self) -> bool {
        self.admin
            || self.settings.iter().any(|s| registry::needs_admin(&s.key))
            || self
                .target
                .as_ref()
                .is_some_and(|t| registry::needs_admin(&t.key))
    }

    /// Checks that the fields required by `kind` are present and consistent.
    pub fn validate(&self) -> Result<()> {
        let ok = match self.kind {
            Kind::Toggle => !self.settings.is_empty() && self.settings.iter().all(|s| s.on != s.off),
            Kind::Number => {
                self.target.is_some()
                    && self.number.as_ref().is_some_and(|n| {
                        n.min <= n.max && n.default.is_none_or(|d| (n.min..=n.max).contains(&d))
                    })
            }
            Kind::Choice => self.target.is_some() && self.options.len() >= 2,
            Kind::Action => self.action.as_ref().is_some_and(|a| !a.program.is_empty()),
        };
        if !ok {
            anyhow::bail!(
                "tweak `{}` is missing fields required for kind {:?}",
                self.id,
                self.kind
            );
        }
        Ok(())
    }

    fn record_values(
        &self,
        journal: &ChangeJournal,
        tool: &str,
        what: String,
        f: impl FnOnce(&Target) -> Result<()>,
    ) -> Result<()> {
        let t = self.target.as_ref().context("tweak has no target")?;
        let snap = registry::snapshot(&t.key, &t.name);
        if let Err(e) = f(t) {
            let _ = registry::restore(&snap);
            return Err(e);
        }
        journal.record(tool, what, UndoAction::RestoreRegValues { values: vec![snap] })?;
        Ok(())
    }

    /// Current value of a number tweak (the Windows default when the value is absent).
    pub fn current_number(&self) -> Option<u32> {
        let (t, n) = (self.target.as_ref()?, self.number.as_ref()?);
        match registry::read_raw(&t.key, &t.name) {
            Some(v) if n.sz => registry::decode_string(&v).trim().parse().ok(),
            Some(v) => registry::decode_dword(&v),
            None => n.default,
        }
    }

    pub fn set_number(&self, value: u32, journal: &ChangeJournal, tool: &str) -> Result<()> {
        let n = self.number.as_ref().context("not a number tweak")?;
        if !(n.min..=n.max).contains(&value) {
            anyhow::bail!("{value} is outside {}–{}", n.min, n.max);
        }
        let raw = if n.sz {
            registry::string(&value.to_string())
        } else {
            registry::dword(value)
        };
        self.record_values(
            journal,
            tool,
            format!("Set {} to {value}{}", self.title, n.unit),
            |t| registry::write_raw(&t.key, &t.name, &raw),
        )
    }

    /// Restores the Windows default (deleting the value when it is normally absent).
    pub fn reset_number(&self, journal: &ChangeJournal, tool: &str) -> Result<()> {
        let n = self.number.as_ref().context("not a number tweak")?;
        match n.default {
            Some(d) => self.set_number(d, journal, tool),
            None => self.record_values(
                journal,
                tool,
                format!("Reset {} to the Windows default", self.title),
                |t| {
                    registry::delete_value(&t.key, &t.name)?;
                    registry::prune_empty_class_override(&t.key);
                    Ok(())
                },
            ),
        }
    }

    /// Index of the option matching the current value, if any.
    pub fn current_choice(&self) -> Option<usize> {
        let t = self.target.as_ref()?;
        let current = registry::read_raw(&t.key, &t.name);
        self.options
            .iter()
            .position(|o| o.value.matches(current.as_ref()))
    }

    pub fn set_choice(&self, index: usize, journal: &ChangeJournal, tool: &str) -> Result<()> {
        let o = self.options.get(index).context("no such option")?;
        self.record_values(
            journal,
            tool,
            format!("Set {} to \"{}\"", self.title, o.label),
            |t| match o.value.to_raw() {
                Some(v) => registry::write_raw(&t.key, &t.name, &v),
                None => {
                    registry::delete_value(&t.key, &t.name)?;
                    registry::prune_empty_class_override(&t.key);
                    Ok(())
                }
            },
        )
    }

    /// Runs an action tweak and journals it (with its reverse command when declared).
    pub fn run_action(&self, journal: &ChangeJournal, tool: &str) -> Result<()> {
        let a = self.action.as_ref().context("not an action")?;
        crate::process::run_checked(crate::process::hidden_command(&a.program).args(&a.args))?;
        let undo = match a.undo.as_deref() {
            Some([program, args @ ..]) => UndoAction::RunCommand {
                program: program.clone(),
                args: args.to_vec(),
            },
            _ => UndoAction::None,
        };
        journal.record(tool, self.title.clone(), undo)?;
        Ok(())
    }

    pub fn state(&self) -> State {
        if self.kind != Kind::Toggle {
            return State::NotApplied;
        }
        let on = self
            .settings
            .iter()
            .filter(|s| s.on.matches(registry::read_raw(&s.key, &s.name).as_ref()))
            .count();
        match on {
            0 => State::NotApplied,
            n if n == self.settings.len() => State::Applied,
            _ => State::Partial,
        }
    }

    fn write(&self, apply: bool, journal: &ChangeJournal, tool: &str) -> Result<()> {
        let snapshots: Vec<_> = self
            .settings
            .iter()
            .map(|s| registry::snapshot(&s.key, &s.name))
            .collect();
        let result = (|| {
            for s in &self.settings {
                let data = if apply { &s.on } else { &s.off };
                match data.to_raw() {
                    Some(v) => registry::write_raw(&s.key, &s.name, &v)?,
                    None => {
                        registry::delete_value(&s.key, &s.name)?;
                        registry::prune_empty_class_override(&s.key);
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            // Roll back whatever was written before the failure.
            for s in &snapshots {
                let _ = registry::restore(s);
            }
            return Err(e);
        }
        journal.record(
            tool,
            format!(
                "{} tweak: {}",
                if apply { "Applied" } else { "Reverted" },
                self.title
            ),
            UndoAction::RestoreRegValues { values: snapshots },
        )?;
        Ok(())
    }

    pub fn apply(&self, journal: &ChangeJournal, tool: &str) -> Result<()> {
        self.write(true, journal, tool)
    }

    pub fn revert(&self, journal: &ChangeJournal, tool: &str) -> Result<()> {
        self.write(false, journal, tool)
    }
}

#[derive(Deserialize)]
struct File {
    tweak: Vec<Tweak>,
}

/// The applied state of a set of tweaks, saved to a file so it can be re-applied later
/// (for example after reinstalling Windows).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, Deserialize)]
pub struct SavedTweaks {
    /// Ids of switches that are on.
    #[serde(default)]
    pub on: Vec<String>,
    /// Number tweaks that differ from the Windows default.
    #[serde(default)]
    pub numbers: std::collections::BTreeMap<String, u32>,
    /// Choice tweaks: id → selected option label.
    #[serde(default)]
    pub choices: std::collections::BTreeMap<String, String>,
}

pub fn capture(tweaks: &[Tweak]) -> SavedTweaks {
    let mut s = SavedTweaks::default();
    for t in tweaks {
        match t.kind {
            Kind::Toggle if t.state() == State::Applied => s.on.push(t.id.clone()),
            Kind::Number => {
                if let (Some(v), Some(n)) = (t.current_number(), &t.number)
                    && Some(v) != n.default
                {
                    s.numbers.insert(t.id.clone(), v);
                }
            }
            Kind::Choice => {
                if let Some(c) = t.current_choice().filter(|&c| c != 0) {
                    s.choices.insert(t.id.clone(), t.options[c].label.clone());
                }
            }
            _ => {}
        }
    }
    s
}

/// Re-applies saved state; returns (changes made, problems). Settings that need
/// administrator rights are reported, not applied, when `elevated` is false.
pub fn restore(
    tweaks: &[Tweak],
    saved: &SavedTweaks,
    journal: &ChangeJournal,
    tool: &str,
    elevated: bool,
) -> (usize, Vec<String>) {
    let (mut done, mut problems) = (0, Vec::new());
    for t in tweaks {
        let wanted = match t.kind {
            Kind::Toggle => saved.on.contains(&t.id) && t.state() != State::Applied,
            Kind::Number => saved
                .numbers
                .get(&t.id)
                .is_some_and(|v| t.current_number() != Some(*v)),
            Kind::Choice => saved
                .choices
                .get(&t.id)
                .is_some_and(|l| t.current_choice().map(|c| &t.options[c].label) != Some(l)),
            Kind::Action => false,
        };
        if !wanted {
            continue;
        }
        if t.needs_admin() && !elevated {
            problems.push(format!("{} (needs administrator)", t.title));
            continue;
        }
        let r = match t.kind {
            Kind::Toggle => t.apply(journal, tool),
            Kind::Number => t.set_number(saved.numbers[&t.id], journal, tool),
            Kind::Choice => match t.options.iter().position(|o| o.label == saved.choices[&t.id]) {
                Some(k) => t.set_choice(k, journal, tool),
                None => Err(anyhow::anyhow!("option no longer exists")),
            },
            Kind::Action => Ok(()),
        };
        match r {
            Ok(()) => done += 1,
            Err(e) => problems.push(format!("{}: {e:#}", t.title)),
        }
    }
    (done, problems)
}

pub fn parse(toml_text: &str) -> Result<Vec<Tweak>> {
    let tweaks = toml::from_str::<File>(toml_text)
        .context("parsing tweak definitions")?
        .tweak;
    for t in &tweaks {
        t.validate()?;
    }
    Ok(tweaks)
}

/// A tool that is only a [`TweakPanel`], with an optional note above it.
pub struct TweakTool {
    id: &'static str,
    category: crate::Category,
    note: &'static str,
    panel: TweakPanel,
}

impl TweakTool {
    pub fn new(category: crate::Category, id: &'static str, note: &'static str, definitions: &str) -> Self {
        Self {
            id,
            category,
            note,
            panel: TweakPanel::new(id, definitions),
        }
    }
}

impl crate::Tool for TweakTool {
    fn id(&self) -> &'static str {
        self.id
    }

    fn category(&self) -> crate::Category {
        self.category
    }

    fn requires_admin(&self) -> bool {
        false
    }

    fn ui(&mut self, ui: &mut Ui, ctx: &mut AppCtx<'_>) {
        if !self.note.is_empty() {
            ui.label(self.note);
            ui.add_space(6.0);
        }
        self.panel.ui(ui, ctx);
    }
}

/// UI listing tweaks grouped, each with an on/off toggle.
pub struct TweakPanel {
    tool: &'static str,
    tweaks: Vec<Tweak>,
    states: Vec<State>,
    message: Message,
    /// Set after changing a tweak that only takes effect once Explorer restarts.
    explorer_restart_pending: bool,
    /// Values being edited for number tweaks, by tweak index.
    numbers: std::collections::HashMap<usize, u32>,
    confirm: crate::ui::Confirm<usize>,
    pending: Option<Op>,
    /// Switch changes waiting for Save: tweak index → desired on/off.
    staged: std::collections::BTreeMap<usize, bool>,
    restore_confirm: crate::ui::Confirm<()>,
    warn_confirm: crate::ui::Confirm<()>,
    group_sel: usize,
    filter: String,
}

impl TweakPanel {
    /// `toml_text` is trusted, compiled-in data; a parse error is a programming bug.
    pub fn new(tool: &'static str, toml_text: &str) -> Self {
        let tweaks = parse(toml_text).expect("built-in tweak definitions must parse");
        Self {
            tool,
            states: Vec::new(),
            tweaks,
            message: None,
            explorer_restart_pending: false,
            numbers: Default::default(),
            confirm: Default::default(),
            pending: None,
            staged: Default::default(),
            restore_confirm: Default::default(),
            warn_confirm: Default::default(),
            group_sel: 0,
            filter: String::new(),
        }
    }

    pub fn tweaks(&self) -> &[Tweak] {
        &self.tweaks
    }

    pub fn refresh(&mut self) {
        self.states = self.tweaks.iter().map(Tweak::state).collect();
    }

    pub fn ui(&mut self, ui: &mut Ui, ctx: &mut AppCtx<'_>) {
        if self.states.len() != self.tweaks.len() {
            self.refresh();
        }
        ui.horizontal(|ui| {
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.refresh();
                self.message = None;
            }
            let toggles = self.tweaks.iter().filter(|t| t.kind == Kind::Toggle).count();
            let applied = self.states.iter().filter(|s| **s == State::Applied).count();
            if toggles > 0 {
                ui.weak(format!("{applied} of {toggles} switches on"));
            }
            if self.explorer_restart_pending && ui.button("Restart Explorer to apply").clicked() {
                self.message = Some(
                    crate::process::restart_explorer()
                        .map(|()| "Explorer restarted.".to_owned())
                        .map_err(|e| format!("{e:#}")),
                );
                self.explorer_restart_pending = false;
            }
        });
        show_message(ui, &self.message);
        ui.add_space(6.0);

        let mut groups: Vec<String> = Vec::new();
        for t in &self.tweaks {
            if !groups.contains(&t.group) {
                groups.push(t.group.clone());
            }
        }
        if let Some(i) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.pending = Some(Op::Run(i));
        }
        let mut op: Option<Op> = None;
        let query = self.filter.trim().to_lowercase();
        // Many groups: left section list (one group at a time), as in the original's pages.
        let use_nav = groups.len() > 3;
        self.group_sel = self.group_sel.min(groups.len().saturating_sub(1));
        let shown: Vec<String> = if use_nav && query.is_empty() {
            groups.get(self.group_sel).cloned().into_iter().collect()
        } else {
            groups.clone()
        };
        let matches = |t: &Tweak| {
            query.is_empty()
                || t.title.to_lowercase().contains(&query)
                || t.description.to_lowercase().contains(&query)
        };
        let mut draw = |ui: &mut Ui, this: &mut Self| {
            for group in &shown {
                if !this.tweaks.iter().any(|t| t.group == *group && matches(t)) {
                    continue;
                }
                if !group.is_empty() {
                    ui.strong(group);
                }
                for (i, t) in this
                    .tweaks
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.group == *group && matches(t))
                {
                    let state = this.states[i];
                    let locked = t.needs_admin() && !ctx.elevated;
                    ui.horizontal(|ui| {
                        let hint = ctx.i18n.tr("shell-needs-admin");
                        match t.kind {
                            Kind::Toggle => {
                                // Switches are staged and applied together with Save.
                                let current = state == State::Applied;
                                let mut on = this.staged.get(&i).copied().unwrap_or(current);
                                let mut label = match state {
                                    State::Partial => format!("{} (partially applied)", t.title),
                                    _ => t.title.clone(),
                                };
                                if this.staged.contains_key(&i) {
                                    label.push_str("  •");
                                }
                                let resp = ui.add_enabled(!locked, egui::Checkbox::new(&mut on, label));
                                if resp.changed() {
                                    if on == current {
                                        this.staged.remove(&i);
                                    } else {
                                        this.staged.insert(i, on);
                                    }
                                }
                                resp.on_hover_text(&t.description).on_disabled_hover_text(hint);
                            }
                            Kind::Number => {
                                let n = t.number.as_ref().expect("validated");
                                let current = t.current_number();
                                let edit = this
                                    .numbers
                                    .entry(i)
                                    .or_insert_with(|| current.or(n.default).unwrap_or(n.min));
                                ui.label(&t.title);
                                ui.add_enabled(
                                    !locked,
                                    egui::DragValue::new(edit)
                                        .range(n.min..=n.max)
                                        .suffix(format!(" {}", n.unit).trim_end().to_owned()),
                                )
                                .on_hover_text(&t.description)
                                .on_disabled_hover_text(hint);
                                let edit = *edit;
                                if Some(edit) != current
                                    && ui
                                        .add_enabled(!locked, egui::Button::new("Apply").small())
                                        .clicked()
                                {
                                    op = Some(Op::SetNumber(i, edit));
                                }
                                let is_default = match n.default {
                                    Some(d) => current == Some(d),
                                    None => t
                                        .target
                                        .as_ref()
                                        .is_some_and(|tg| registry::read_raw(&tg.key, &tg.name).is_none()),
                                };
                                if !is_default
                                    && ui
                                        .add_enabled(!locked, egui::Button::new("Default").small())
                                        .clicked()
                                {
                                    op = Some(Op::ResetNumber(i));
                                }
                            }
                            Kind::Choice => {
                                ui.label(&t.title);
                                let current = t.current_choice();
                                let shown = current.map_or("(custom)", |c| t.options[c].label.as_str());
                                ui.add_enabled_ui(!locked, |ui| {
                                    egui::ComboBox::from_id_salt(&t.id).selected_text(shown).show_ui(
                                        ui,
                                        |ui| {
                                            for (k, o) in t.options.iter().enumerate() {
                                                if ui.selectable_label(current == Some(k), &o.label).clicked()
                                                    && current != Some(k)
                                                {
                                                    op = Some(Op::Choice(i, k));
                                                }
                                            }
                                        },
                                    );
                                })
                                .response
                                .on_hover_text(&t.description);
                            }
                            Kind::Action => {
                                if ui
                                    .add_enabled(!locked, egui::Button::new(&t.title))
                                    .on_hover_text(&t.description)
                                    .on_disabled_hover_text(hint)
                                    .clicked()
                                {
                                    match t.action.as_ref().and_then(|a| a.confirm.clone()) {
                                        Some(q) => this.confirm.ask(q, i),
                                        None => op = Some(Op::Run(i)),
                                    }
                                }
                            }
                        }
                        match t.effect {
                            Effect::Immediate => {}
                            Effect::SignOut => {
                                ui.weak("· after sign-out");
                            }
                            Effect::Restart => {
                                ui.weak("· after restart");
                            }
                            Effect::Explorer => {
                                ui.weak("· after Explorer restart");
                            }
                        }
                        if t.needs_admin() {
                            ui.weak("· admin");
                        }
                    });
                    ui.indent(&t.id, |ui| {
                        ui.weak(&t.description);
                        if let Some(w) = &t.warning {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!("{}  {w}", crate::icons::WARNING),
                            );
                        }
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(8.0);
            }
        };
        let many = self.tweaks.len() > 12;
        if use_nav {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(210.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text(format!("{}  Find a setting", crate::icons::MAGNIFYING_GLASS))
                            .desired_width(200.0),
                    );
                    ui.add_space(4.0);
                    let items: Vec<(&str, &str)> = groups
                        .iter()
                        .map(|g| (crate::icons::SLIDERS, g.as_str()))
                        .collect();
                    if crate::ui::section_nav(ui, &items, &mut self.group_sel) {
                        self.filter.clear();
                    }
                });
                ui.add_space(14.0);
                ui.vertical(|ui| draw(ui, self));
            });
        } else {
            if many {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text(format!("{}  Find a setting", crate::icons::MAGNIFYING_GLASS))
                        .desired_width(240.0),
                );
                ui.add_space(4.0);
            }
            draw(ui, self);
        }

        // Footer: Restore defaults / Save, as in the original's tweak pages.
        let has_toggles = self.tweaks.iter().any(|t| t.kind == Kind::Toggle);
        let mut batch: Vec<(usize, bool)> = Vec::new();
        if self.restore_confirm.show(ui.ctx(), ctx.i18n).is_some() {
            batch = (0..self.tweaks.len())
                .filter(|&i| self.tweaks[i].kind == Kind::Toggle && self.states[i] != State::NotApplied)
                .filter(|&i| !self.tweaks[i].needs_admin() || ctx.elevated)
                .map(|i| (i, false))
                .collect();
            self.staged.clear();
        }
        if self.warn_confirm.show(ui.ctx(), ctx.i18n).is_some() {
            batch = std::mem::take(&mut self.staged).into_iter().collect();
        }
        if has_toggles {
            ui.separator();
            ui.horizontal(|ui| {
                let n = self.staged.len();
                if n > 0 {
                    ui.colored_label(crate::ui::ACCENT, format!("{n} unsaved change{}", if n == 1 { "" } else { "s" }));
                    if ui.small_button(ctx.i18n.tr("common-cancel")).clicked() {
                        self.staged.clear();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(n > 0, egui::Button::new(format!("{}  Save", crate::icons::FLOPPY_DISK))).clicked() {
                        let risky: Vec<&str> = self
                            .staged
                            .iter()
                            .filter(|(i, on)| **on && self.tweaks[**i].warning.is_some())
                            .map(|(i, _)| self.tweaks[*i].title.as_str())
                            .collect();
                        if risky.is_empty() {
                            batch = std::mem::take(&mut self.staged).into_iter().collect();
                        } else {
                            self.warn_confirm.ask(
                                format!(
                                    "These settings lower your PC's protection:\n\n• {}\n\nSave anyway? You can undo them from the Change Journal.",
                                    risky.join("\n• ")
                                ),
                                (),
                            );
                        }
                    }
                    if ui.button(format!("{}  Restore defaults", crate::icons::ARROW_COUNTER_CLOCKWISE)).clicked() {
                        self.restore_confirm.ask(
                            "Turn off every switch on this page, restoring the Windows defaults? Each change is recorded in the Change Journal.",
                            (),
                        );
                    }
                });
            });
        }
        if !batch.is_empty() {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                let mut done = 0;
                let mut errors = Vec::new();
                for (i, on) in batch {
                    let t = &self.tweaks[i];
                    let r = if on {
                        t.apply(ctx.journal, self.tool)
                    } else {
                        t.revert(ctx.journal, self.tool)
                    };
                    match r {
                        Ok(()) => {
                            done += 1;
                            if t.effect == Effect::Explorer {
                                self.explorer_restart_pending = true;
                            }
                        }
                        Err(e) => errors.push(format!("{}: {e:#}", t.title)),
                    }
                }
                if errors.is_empty() {
                    Ok(format!(
                        "Saved {done} change{}.",
                        if done == 1 { "" } else { "s" }
                    ))
                } else {
                    Err(format!("Saved {done}; failed: {}", errors.join("; ")))
                }
            });
            self.refresh();
        }

        if let Some(op) = op.or_else(|| self.pending.take()) {
            let i = op.index();
            let t = &self.tweaks[i];
            self.message = Some(if ctx.config.dry_run {
                Ok(format!("{} ({})", ctx.i18n.tr("common-dry-run-skip"), t.title))
            } else {
                let (j, tool) = (ctx.journal, self.tool);
                let r = match op {
                    Op::SetNumber(_, v) => t
                        .set_number(v, j, tool)
                        .map(|()| format!("Set {} to {v}.", t.title)),
                    Op::ResetNumber(_) => t
                        .reset_number(j, tool)
                        .map(|()| format!("Restored the default for {}.", t.title)),
                    Op::Choice(_, k) => t
                        .set_choice(k, j, tool)
                        .map(|()| format!("{}: {}.", t.title, t.options[k].label)),
                    Op::Run(_) => t.run_action(j, tool).map(|()| format!("{} done.", t.title)),
                };
                if r.is_ok() && t.effect == Effect::Explorer {
                    self.explorer_restart_pending = true;
                }
                r.map_err(|e| format!("{e:#}"))
            });
            self.numbers.remove(&i);
            self.refresh();
        }
    }
}

/// A change requested from the panel.
#[derive(Debug, Clone, Copy)]
enum Op {
    SetNumber(usize, u32),
    ResetNumber(usize),
    Choice(usize, usize),
    Run(usize),
}

impl Op {
    fn index(self) -> usize {
        match self {
            Op::SetNumber(i, _) | Op::ResetNumber(i) | Op::Choice(i, _) | Op::Run(i) => i,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        format!(r"HKCU\Software\win-manager-rs-test-tweak-{}", std::process::id())
    }

    fn sample(key: &str) -> String {
        format!(
            r#"
[[tweak]]
id = "t1"
title = "Test tweak"
description = "d"
group = "G"
effect = "sign-out"
[[tweak.set]]
key = '{key}'
name = "Str"
on = {{ sz = "100" }}
off = {{ sz = "400" }}
[[tweak.set]]
key = '{key}'
name = "Flag"
on = {{ dword = 1 }}
off = "delete"
"#
        )
    }

    #[test]
    fn parse_apply_revert_and_undo() {
        let key = key();
        let tweaks = parse(&sample(&key)).unwrap();
        assert_eq!(tweaks.len(), 1);
        let t = &tweaks[0];
        assert_eq!(t.effect, Effect::SignOut);
        assert!(!t.needs_admin());

        registry::write_raw(&key, "Str", &registry::string("400")).unwrap();
        assert_eq!(t.state(), State::NotApplied);

        let dir = tempfile::tempdir().unwrap();
        let journal = ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();

        t.apply(&journal, "test").unwrap();
        assert_eq!(t.state(), State::Applied);

        // Undo via the journal restores "400" and removes the created DWORD.
        let id = journal.entries()[0].id;
        journal.undo(id).unwrap();
        assert_eq!(t.state(), State::NotApplied);
        assert_eq!(registry::read_raw(&key, "Flag"), None);

        t.apply(&journal, "test").unwrap();
        registry::delete_value(&key, "Flag").unwrap();
        assert_eq!(t.state(), State::Partial);
        t.revert(&journal, "test").unwrap();
        assert_eq!(t.state(), State::NotApplied);

        let (root, sub) = registry::split_path(&key).unwrap();
        root.delete_subkey_all(sub).unwrap();
    }

    #[test]
    fn number_and_choice_tweaks_with_undo() {
        let key = format!(r"HKCU\Software\win-manager-rs-test-kinds-{}", std::process::id());
        let text = format!(
            r#"
[[tweak]]
id = "n"
title = "Delay"
description = "d"
kind = "number"
target = {{ key = '{key}', name = "Delay" }}
number = {{ min = 0, max = 1000, default = 400, unit = "ms", sz = true }}

[[tweak]]
id = "c"
title = "Mode"
description = "d"
kind = "choice"
target = {{ key = '{key}', name = "Mode" }}
[[tweak.option]]
label = "Default"
value = "delete"
[[tweak.option]]
label = "Two"
value = {{ dword = 2 }}

[[tweak]]
id = "a"
title = "Echo"
description = "d"
kind = "action"
action = {{ program = "cmd", args = ["/c", "exit", "0"] }}
"#
        );
        let tweaks = parse(&text).unwrap();
        let (n, c, a) = (&tweaks[0], &tweaks[1], &tweaks[2]);
        let dir = tempfile::tempdir().unwrap();
        let journal = ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();

        assert_eq!(n.current_number(), Some(400), "absent value reads as the default");
        n.set_number(150, &journal, "t").unwrap();
        assert_eq!(n.current_number(), Some(150));
        assert_eq!(
            registry::read_raw(&key, "Delay"),
            Some(registry::string("150")),
            "stored as REG_SZ"
        );
        assert!(n.set_number(5000, &journal, "t").is_err(), "out of range");
        n.reset_number(&journal, "t").unwrap();
        assert_eq!(n.current_number(), Some(400));

        assert_eq!(c.current_choice(), Some(0));
        c.set_choice(1, &journal, "t").unwrap();
        assert_eq!(c.current_choice(), Some(1));
        journal.undo(journal.entries()[0].id).unwrap();
        assert_eq!(c.current_choice(), Some(0), "undo restores the absent value");

        a.run_action(&journal, "t").unwrap();
        assert_eq!(journal.entries()[0].description, "Echo");

        let (root, sub) = registry::split_path(&key).unwrap();
        let _ = root.delete_subkey_all(sub);
    }

    #[test]
    fn capture_and_restore_roundtrip() {
        let key = format!(r"HKCU\Software\win-manager-rs-test-saved-{}", std::process::id());
        let tweaks = parse(&sample(&key)).unwrap();
        let t = &tweaks[0];
        let dir = tempfile::tempdir().unwrap();
        let journal = ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();

        t.apply(&journal, "t").unwrap();
        let saved = capture(&tweaks);
        assert_eq!(saved.on, vec!["t1".to_owned()]);
        let text = toml::to_string(&saved).unwrap();

        t.revert(&journal, "t").unwrap();
        let back: SavedTweaks = toml::from_str(&text).unwrap();
        let (done, problems) = restore(&tweaks, &back, &journal, "t", false);
        assert_eq!((done, problems.len()), (1, 0));
        assert_eq!(t.state(), State::Applied);
        let (done, _) = restore(&tweaks, &back, &journal, "t", false);
        assert_eq!(done, 0, "already in the saved state");

        let (root, sub) = registry::split_path(&key).unwrap();
        let _ = root.delete_subkey_all(sub);
    }

    #[test]
    fn invalid_definitions_are_rejected() {
        let missing_target = r#"
[[tweak]]
id = "x"
title = "x"
description = "x"
kind = "number"
number = { min = 0, max = 5 }
"#;
        assert!(parse(missing_target).is_err());
        let bad_default = r#"
[[tweak]]
id = "x"
title = "x"
description = "x"
kind = "number"
target = { key = 'HKCU\x', name = "y" }
number = { min = 1, max = 5, default = 9 }
"#;
        assert!(parse(bad_default).is_err());
        let empty_toggle = r#"
[[tweak]]
id = "x"
title = "x"
description = "x"
"#;
        assert!(parse(empty_toggle).is_err());
    }

    #[test]
    fn data_matching() {
        assert!(Data::Delete.matches(None));
        assert!(!Data::Delete.matches(Some(&registry::dword(0))));
        assert!(Data::Dword(5).matches(Some(&registry::dword(5))));
        assert!(Data::Sz("x".into()).matches(Some(&registry::string("x"))));
        assert!(!Data::Sz("x".into()).matches(None));
    }
}
