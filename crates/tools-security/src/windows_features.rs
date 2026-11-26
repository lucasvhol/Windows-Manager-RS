//! Windows Features: the optional parts of Windows (Hyper-V, Windows Sandbox, .NET 3.5,
//! SMB 1.0, Telnet…) with their state; tick and untick, then Apply. Listing works for
//! every account (WMI); changing runs DISM (administrator). Each change is journaled with
//! the opposite DISM command as its undo.

use anyhow::{Result, bail};
use egui_extras::{Column, TableBuilder};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    process::{hidden_command, json_list, powershell_json},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, empty_state, frame, show_message},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureState {
    On,
    Off,
    /// Off, and its files were removed from the PC (Windows downloads them to turn it on).
    Removed,
    Unknown,
}

impl FeatureState {
    fn from_install_state(v: i64) -> Self {
        match v {
            1 => FeatureState::On,
            2 => FeatureState::Off,
            3 => FeatureState::Removed,
            _ => FeatureState::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FeatureState::On => "On",
            FeatureState::Off => "Off",
            FeatureState::Removed => "Off (files removed; downloaded when turned on)",
            FeatureState::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feature {
    /// DISM name (`TelnetClient`).
    pub name: String,
    /// Display name (`Telnet Client`).
    pub caption: String,
    pub state: FeatureState,
}

pub fn list() -> Result<Vec<Feature>> {
    let v = powershell_json(
        "Get-CimInstance Win32_OptionalFeature | Select-Object Name, Caption, InstallState | ConvertTo-Json -Compress",
    )?;
    let mut out: Vec<Feature> = json_list(v)
        .into_iter()
        .filter_map(|f| {
            let name = f.get("Name")?.as_str()?.to_owned();
            let caption = f
                .get("Caption")
                .and_then(|c| c.as_str())
                .filter(|c| !c.trim().is_empty())
                .unwrap_or(&name)
                .to_owned();
            let state = FeatureState::from_install_state(f.get("InstallState")?.as_i64()?);
            Some(Feature { name, caption, state })
        })
        .collect();
    out.sort_by_key(|f| f.caption.to_lowercase());
    Ok(out)
}

/// DISM feature names are letters, digits, `-`, `_` and `.` only (never quoted).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The DISM arguments that turn a feature on (with the features it needs) or off.
pub fn dism_args(name: &str, on: bool) -> Vec<String> {
    let mut a = vec!["/online".to_owned()];
    if on {
        a.push("/enable-feature".into());
        a.push(format!("/featurename:{name}"));
        a.push("/all".into());
    } else {
        a.push("/disable-feature".into());
        a.push(format!("/featurename:{name}"));
    }
    a.extend(["/norestart".to_owned(), "/quiet".to_owned()]);
    a
}

/// Runs DISM; Ok(true) when Windows needs a restart to finish.
pub fn set_feature(name: &str, on: bool) -> Result<bool> {
    if !valid_name(name) {
        bail!("\"{name}\" is not a feature name");
    }
    let out = hidden_command("dism").args(dism_args(name, on)).output()?;
    match out.status.code() {
        Some(0) => Ok(false),
        Some(3010) => Ok(true),
        code => {
            let text = String::from_utf8_lossy(&out.stdout);
            let detail = text
                .lines()
                .map(str::trim)
                .filter(|l| l.starts_with("Error") || l.contains("error"))
                .collect::<Vec<_>>()
                .join(" ");
            bail!(
                "DISM couldn't turn {name} {} (code {}){}",
                if on { "on" } else { "off" },
                code.map_or("none".into(), |c| c.to_string()),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )
        }
    }
}

/// What a run did: a line per change, whether a restart is needed, and the journal
/// entries (description, undo) for the changes that worked.
pub struct Outcome {
    pub lines: Vec<String>,
    pub restart: bool,
    pub undo: Vec<(String, UndoAction)>,
}

/// Applies the changes one by one.
fn apply(changes: &[(String, String, bool)]) -> Outcome {
    let mut lines = Vec::new();
    let mut undo = Vec::new();
    let mut restart = false;
    for (name, caption, on) in changes {
        match set_feature(name, *on) {
            Ok(r) => {
                restart |= r;
                let what = if *on { "Turned on" } else { "Turned off" };
                undo.push((
                    format!("{what} the Windows feature {caption}"),
                    UndoAction::RunCommand {
                        program: "dism".into(),
                        args: dism_args(name, !on),
                    },
                ));
                lines.push(format!("✔ {what}: {caption}"));
            }
            Err(e) => lines.push(format!("✖ {e:#}")),
        }
    }
    Outcome { lines, restart, undo }
}

// ---------------------------------------------------------------------------------
// UI

#[derive(Default)]
pub struct WindowsFeatures {
    load: Option<BackgroundTask<Vec<Feature>>>,
    features: Option<Vec<Feature>>,
    /// Ticked state the user wants, by feature name, where it differs from now.
    wanted: std::collections::BTreeMap<String, bool>,
    filter: String,
    only_on: bool,
    running: Option<BackgroundTask<Outcome>>,
    log: Vec<String>,
    confirm: Confirm<Vec<(String, String, bool)>>,
    message: Message,
}

impl WindowsFeatures {
    fn changes(&self) -> Vec<(String, String, bool)> {
        let Some(features) = &self.features else {
            return Vec::new();
        };
        features
            .iter()
            .filter_map(|f| {
                let on = *self.wanted.get(&f.name)?;
                (on != (f.state == FeatureState::On)).then(|| (f.name.clone(), f.caption.clone(), on))
            })
            .collect()
    }
}

impl Tool for WindowsFeatures {
    fn id(&self) -> &'static str {
        "winfeatures"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.features.is_none() && self.load.is_none() {
            self.load = Some(BackgroundTask::spawn(ui.ctx(), |_| list()));
        }
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                self.load = None;
                match r {
                    Ok(f) => self.features = Some(f),
                    Err(e) => {
                        self.features = Some(Vec::new());
                        self.message = Some(Err(e));
                    }
                }
            }
        }
        if let Some(t) = &mut self.running {
            t.poll();
            if let Some(r) = t.take_result() {
                self.running = None;
                match r {
                    Ok(out) => {
                        for (desc, undo) in out.undo {
                            if let Err(e) = ctx.journal.record("winfeatures", desc, undo) {
                                tracing::warn!("journal: {e:#}");
                            }
                        }
                        self.log = out.lines;
                        self.message = Some(Ok(if out.restart {
                            "Done. Restart Windows to finish.".into()
                        } else {
                            "Done.".into()
                        }));
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
                self.wanted.clear();
                self.features = None;
            }
        }
        if let Some(changes) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                self.log.clear();
                self.message = None;
                self.running = Some(BackgroundTask::spawn(ui.ctx(), move |_| Ok(apply(&changes))));
            }
        }

        let changes = self.changes();
        let busy = self.running.is_some() || self.load.is_some();
        let apply_label = format!("Apply {}", changes.len());
        match frame::toolbar(
            ui,
            &[
                Some(
                    ToolButton::new(icons::CHECK, &apply_label)
                        .enabled(ctx.elevated && !busy && !changes.is_empty()),
                ),
                Some(
                    ToolButton::new(icons::ARROW_COUNTER_CLOCKWISE, "Revert")
                        .enabled(!busy && !self.wanted.is_empty()),
                ),
                None,
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(!busy)),
            ],
            |ui| {
                if busy {
                    ui.weak(if self.running.is_some() {
                        "Applying… (DISM)"
                    } else {
                        "Loading…"
                    });
                    ui.spinner();
                }
            },
        ) {
            Some(0) => {
                let list = changes
                    .iter()
                    .map(|(_, c, on)| format!("{} {c}", if *on { "Turn on" } else { "Turn off" }))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.confirm.ask(
                    format!("{list}\n\nThis can take a few minutes. Each change can be undone from the Change Journal."),
                    changes.clone(),
                );
            }
            Some(1) => self.wanted.clear(),
            Some(3) => {
                self.features = None;
                self.wanted.clear();
            }
            _ => {}
        }
        let filter_hint = format!("{}  {}", icons::MAGNIFYING_GLASS, ctx.i18n.tr("common-filter"));
        frame::options(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(filter_hint)
                    .desired_width(200.0),
            );
            ui.checkbox(&mut self.only_on, "Only features that are on");
        });
        let status = self.features.as_ref().map_or_else(
            || "Reading the features…".to_owned(),
            |f| {
                format!(
                    "{} features, {} on, {} change(s) to apply",
                    f.len(),
                    f.iter().filter(|f| f.state == FeatureState::On).count(),
                    changes.len()
                )
            },
        );
        frame::status(ui, &status, |ui| {
            if ctx.elevated {
                ui.weak("Tick what you want, untick what you don't, then Apply. Some need a restart.");
            } else {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Changing features needs the app to run as administrator.",
                );
            }
        });
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            for l in &self.log {
                ui.label(l);
            }
            let Some(features) = &self.features else {
                return;
            };
            let filter = self.filter.trim().to_lowercase();
            let rows: Vec<&Feature> = features
                .iter()
                .filter(|f| {
                    filter.is_empty()
                        || f.caption.to_lowercase().contains(&filter)
                        || f.name.to_lowercase().contains(&filter)
                })
                .filter(|f| !self.only_on || f.state == FeatureState::On)
                .collect();
            if rows.is_empty() {
                empty_state(
                    ui,
                    icons::FUNNEL,
                    "No features match",
                    "Clear the filter to see them all.",
                );
                return;
            }
            let mut toggled = None;
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::initial(380.0).clip(true))
                .column(Column::initial(270.0).clip(true))
                .column(Column::remainder().clip(true))
                .header(22.0, |mut h| {
                    for t in ["Feature", "Name", "Now"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, rows.len(), |mut row| {
                        let f = rows[row.index()];
                        let is_on = f.state == FeatureState::On;
                        let mut want = self.wanted.get(&f.name).copied().unwrap_or(is_on);
                        let changed = want != is_on;
                        row.col(|ui| {
                            let text = if changed {
                                egui::RichText::new(&f.caption).strong()
                            } else {
                                egui::RichText::new(&f.caption)
                            };
                            if ui
                                .add_enabled(!busy, egui::Checkbox::new(&mut want, text))
                                .changed()
                            {
                                toggled = Some((f.name.clone(), is_on, want));
                            }
                        });
                        row.col(|ui| {
                            ui.weak(&f.name);
                        });
                        row.col(|ui| {
                            if changed {
                                ui.label(format!(
                                    "{} → {}",
                                    f.state.label(),
                                    if want { "on" } else { "off" }
                                ));
                            } else {
                                ui.weak(f.state.label());
                            }
                        });
                    });
                });
            if let Some((name, is_on, want)) = toggled {
                if want == is_on {
                    self.wanted.remove(&name);
                } else {
                    self.wanted.insert(name, want);
                }
            }
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_dism_arguments() {
        assert!(valid_name("Microsoft-Windows-Subsystem-Linux"));
        assert!(valid_name("NetFx3"));
        assert!(!valid_name("a b"));
        assert!(!valid_name("x\" /quiet"));
        assert_eq!(
            dism_args("TFTP", true),
            [
                "/online",
                "/enable-feature",
                "/featurename:TFTP",
                "/all",
                "/norestart",
                "/quiet"
            ]
        );
        assert_eq!(
            dism_args("TFTP", false),
            [
                "/online",
                "/disable-feature",
                "/featurename:TFTP",
                "/norestart",
                "/quiet"
            ]
        );
        assert!(set_feature("bad name", true).is_err());
    }

    /// Live: turns the TFTP Client on and off again (administrator; takes a minute).
    #[test]
    #[ignore]
    fn live_tftp_round_trip() {
        let state = || {
            list()
                .unwrap()
                .into_iter()
                .find(|f| f.name == "TFTP")
                .map(|f| f.state)
        };
        assert_eq!(state(), Some(FeatureState::Off), "starts off");
        let on = set_feature("TFTP", true);
        let after_on = state();
        let off = set_feature("TFTP", false);
        let after_off = state();
        on.unwrap();
        off.unwrap();
        assert_eq!(after_on, Some(FeatureState::On));
        assert_eq!(after_off, Some(FeatureState::Off));
    }

    #[test]
    fn listing_works_without_admin() {
        let f = list().unwrap();
        assert!(f.len() > 20, "Windows has dozens of optional features");
        assert!(f.iter().all(|f| valid_name(&f.name)));
    }
}
