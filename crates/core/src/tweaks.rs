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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tweak {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub effect: Effect,
    #[serde(rename = "set")]
    pub settings: Vec<Setting>,
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
        self.settings.iter().any(|s| registry::needs_admin(&s.key))
    }

    pub fn state(&self) -> State {
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

pub fn parse(toml_text: &str) -> Result<Vec<Tweak>> {
    Ok(toml::from_str::<File>(toml_text)
        .context("parsing tweak definitions")?
        .tweak)
}

/// UI listing tweaks grouped, each with an on/off toggle.
pub struct TweakPanel {
    tool: &'static str,
    tweaks: Vec<Tweak>,
    states: Vec<State>,
    message: Message,
    /// Set after changing a tweak that only takes effect once Explorer restarts.
    explorer_restart_pending: bool,
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
            let applied = self.states.iter().filter(|s| **s == State::Applied).count();
            ui.weak(format!("{applied} of {} tweaks applied", self.tweaks.len()));
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

        let mut groups: Vec<&str> = Vec::new();
        for t in &self.tweaks {
            if !groups.contains(&t.group.as_str()) {
                groups.push(&t.group);
            }
        }
        let mut action: Option<(usize, bool)> = None;
        for group in groups {
            if !group.is_empty() {
                ui.strong(group);
            }
            for (i, t) in self.tweaks.iter().enumerate().filter(|(_, t)| t.group == group) {
                let state = self.states[i];
                let locked = t.needs_admin() && !ctx.elevated;
                ui.horizontal(|ui| {
                    let mut on = state == State::Applied;
                    let label = match state {
                        State::Partial => format!("{} (partially applied)", t.title),
                        _ => t.title.clone(),
                    };
                    let resp = ui.add_enabled(!locked, egui::Checkbox::new(&mut on, label));
                    if resp.changed() {
                        action = Some((i, on));
                    }
                    let resp = resp.on_hover_text(&t.description);
                    if locked {
                        resp.on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"));
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
                });
                ui.add_space(4.0);
            }
            ui.add_space(8.0);
        }

        if let Some((i, apply)) = action {
            let t = &self.tweaks[i];
            self.message = Some(if ctx.config.dry_run {
                Ok(format!("{} ({})", ctx.i18n.tr("common-dry-run-skip"), t.title))
            } else {
                let r = if apply {
                    t.apply(ctx.journal, self.tool)
                } else {
                    t.revert(ctx.journal, self.tool)
                };
                if r.is_ok() && t.effect == Effect::Explorer {
                    self.explorer_restart_pending = true;
                }
                r.map(|()| format!("{} {}.", if apply { "Applied" } else { "Reverted" }, t.title))
                    .map_err(|e| format!("{e:#}"))
            });
            self.refresh();
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
    fn data_matching() {
        assert!(Data::Delete.matches(None));
        assert!(!Data::Delete.matches(Some(&registry::dword(0))));
        assert!(Data::Dword(5).matches(Some(&registry::dword(5))));
        assert!(Data::Sz("x".into()).matches(Some(&registry::string("x"))));
        assert!(!Data::Sz("x".into()).matches(None));
    }
}
