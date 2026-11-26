//! Compatibility Manager: the compatibility settings Windows keeps per program (the
//! Properties > Compatibility tab): compatibility mode, run as administrator, colour and
//! resolution modes, fullscreen optimizations and DPI override. Your own settings
//! (HKCU) can be added, changed and removed with undo; settings for all users (HKLM) are
//! shown and need admin to change.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Result, bail};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, registry,
    safety::UndoAction,
    ui::{Message, ToolButton, frame, section, show_message},
};

const USER: &str = r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";
const MACHINE: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";

pub const MODES: [(&str, &str); 9] = [
    ("WIN95", "Windows 95"),
    ("WIN98", "Windows 98 / Me"),
    ("WINXPSP2", "Windows XP (Service Pack 2)"),
    ("WINXPSP3", "Windows XP (Service Pack 3)"),
    ("VISTARTM", "Windows Vista"),
    ("VISTASP1", "Windows Vista (Service Pack 1)"),
    ("VISTASP2", "Windows Vista (Service Pack 2)"),
    ("WIN7RTM", "Windows 7"),
    ("WIN8RTM", "Windows 8"),
];

pub const FLAGS: [(&str, &str); 6] = [
    ("RUNASADMIN", "Run as administrator"),
    (
        "DISABLEDXMAXIMIZEDWINDOWEDMODE",
        "Disable fullscreen optimizations",
    ),
    ("256COLOR", "Reduced colour mode (8-bit, 256 colours)"),
    ("16BITCOLOR", "Reduced colour mode (16-bit)"),
    ("640X480", "Run in 640 × 480 screen resolution"),
    (
        "TRANSFORMLEGACYCOLORMANAGED",
        "Use legacy display ICC colour management",
    ),
];

/// High-DPI scaling override: (tokens, label).
pub const DPI: [(&str, &str); 4] = [
    ("", "Not overridden"),
    ("HIGHDPIAWARE", "Application"),
    ("DPIUNAWARE", "System"),
    ("GDIDPISCALING DPIUNAWARE", "System (Enhanced)"),
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub exe: String,
    pub mode: Option<String>,
    pub flags: BTreeSet<String>,
    /// Index into [`DPI`].
    pub dpi: usize,
    /// Tokens this tool doesn't edit, kept as they are.
    pub other: Vec<String>,
}

impl Entry {
    pub fn parse(exe: &str, value: &str) -> Entry {
        let mut e = Entry {
            exe: exe.to_owned(),
            ..Default::default()
        };
        let tokens: Vec<String> = value
            .split_whitespace()
            .filter(|t| *t != "~" && *t != "$")
            .map(str::to_owned)
            .collect();
        let has = |t: &str| tokens.iter().any(|x| x.eq_ignore_ascii_case(t));
        e.dpi = if has("GDIDPISCALING") && has("DPIUNAWARE") {
            3
        } else if has("DPIUNAWARE") {
            2
        } else if has("HIGHDPIAWARE") {
            1
        } else {
            0
        };
        for t in tokens {
            let u = t.to_ascii_uppercase();
            if MODES.iter().any(|(m, _)| *m == u) {
                // Windows writes the newest mode last; keep the first as the chosen one.
                if e.mode.is_none() {
                    e.mode = Some(u);
                }
            } else if FLAGS.iter().any(|(f, _)| *f == u) {
                e.flags.insert(u);
            } else if !matches!(u.as_str(), "HIGHDPIAWARE" | "DPIUNAWARE" | "GDIDPISCALING") {
                e.other.push(t);
            }
        }
        e
    }

    /// The registry value ("~ WIN7RTM RUNASADMIN"), or `None` when nothing is set.
    pub fn value(&self) -> Option<String> {
        let mut t: Vec<String> = Vec::new();
        t.extend(self.mode.clone());
        t.extend(self.flags.iter().cloned());
        if self.dpi > 0 {
            t.push(DPI[self.dpi].0.to_owned());
        }
        t.extend(self.other.iter().cloned());
        (!t.is_empty()).then(|| format!("~ {}", t.join(" ")))
    }

    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = &self.mode {
            parts.push(
                MODES
                    .iter()
                    .find(|x| x.0 == m)
                    .map_or(m.clone(), |x| x.1.to_owned()),
            );
        }
        for f in &self.flags {
            parts.push(
                FLAGS
                    .iter()
                    .find(|x| x.0 == f)
                    .map_or(f.clone(), |x| x.1.to_owned()),
            );
        }
        if self.dpi > 0 {
            parts.push(format!("DPI scaling by: {}", DPI[self.dpi].1));
        }
        parts.extend(self.other.iter().cloned());
        parts.join(", ")
    }
}

pub fn list(key: &str) -> Vec<Entry> {
    let Ok(k) = registry::open_read(key) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = k
        .enum_values()
        .flatten()
        .filter_map(|(name, _)| {
            let v = registry::read_raw(key, &name)?;
            Some(Entry::parse(&name, &registry::decode_string(&v)))
        })
        .collect();
    out.sort_by_key(|e| e.exe.to_lowercase());
    out
}

pub fn save(ctx: &AppCtx<'_>, e: &Entry) -> Result<()> {
    if e.exe.trim().is_empty() || !Path::new(&e.exe).is_absolute() {
        bail!("Choose the program's full path.");
    }
    let snap = registry::snapshot(USER, &e.exe);
    match e.value() {
        Some(v) => registry::write_raw(USER, &e.exe, &registry::string(&v))?,
        None => registry::delete_value(USER, &e.exe)?,
    }
    ctx.journal.record(
        "compat",
        format!(
            "Compatibility settings for {}: {}",
            e.exe,
            e.value().map_or("none".into(), |_| e.summary())
        ),
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

pub fn remove(ctx: &AppCtx<'_>, exes: &[String]) -> Result<()> {
    let snaps: Vec<_> = exes.iter().map(|x| registry::snapshot(USER, x)).collect();
    for x in exes {
        registry::delete_value(USER, x)?;
    }
    ctx.journal.record(
        "compat",
        format!("Removed compatibility settings for {} program(s)", exes.len()),
        UndoAction::RestoreRegValues { values: snaps },
    )?;
    Ok(())
}

#[derive(Default)]
pub struct CompatManager {
    user: Option<Vec<Entry>>,
    machine: Option<Vec<Entry>>,
    draft: Option<Entry>,
    message: Message,
}

impl CompatManager {
    fn reload(&mut self) {
        self.user = Some(list(USER));
        self.machine = Some(list(MACHINE));
    }
}

impl Tool for CompatManager {
    fn id(&self) -> &'static str {
        "compat"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.user.is_none() {
            self.reload();
        }
        let user = self.user.clone().unwrap_or_default();
        let machine = self.machine.clone().unwrap_or_default();
        let missing: Vec<String> = user
            .iter()
            .filter(|e| !Path::new(&e.exe).exists())
            .map(|e| e.exe.clone())
            .collect();
        let clean_label = format!("Clean up ({})", missing.len());
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add program")),
                Some(ToolButton::new(icons::BROOM, &clean_label).enabled(!missing.is_empty())),
                None,
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            ],
            |_| {},
        ) {
            Some(0) => self.draft = Some(Entry::default()),
            Some(1) => {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    remove(ctx, &missing)
                        .map(|()| {
                            "Removed the settings of missing programs. Undo it from the Change Journal."
                                .into()
                        })
                        .map_err(|e| format!("{e:#}"))
                });
                self.user = None;
            }
            Some(3) => {
                self.user = None;
                self.message = None;
            }
            _ => {}
        }
        frame::status(
            ui,
            &format!(
                "{} programs with your settings, {} for all users, {} missing",
                user.len(),
                machine.len(),
                missing.len()
            ),
            |ui| {
                ui.weak("The same as a program's Properties > Compatibility tab.");
            },
        );
        let mut edit = None;
        let mut remove_one = None;
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            section(
                ui,
                &format!("Your settings ({})", user.len()),
                |_| {},
                |ui| {
                    if user.is_empty() {
                        ui.weak("No program has compatibility settings for your account. Use Add program to add one.");
                    }
                    egui::Grid::new("compat-user")
                        .striped(true)
                        .num_columns(3)
                        .show(ui, |ui| {
                            for e in &user {
                                if Path::new(&e.exe).exists() {
                                    ui.label(&e.exe);
                                } else {
                                    ui.colored_label(
                                        ui.visuals().warn_fg_color,
                                        format!("{} (missing)", e.exe),
                                    );
                                }
                                ui.weak(e.summary());
                                ui.horizontal(|ui| {
                                    if ui.small_button("Edit").clicked() {
                                        edit = Some(e.clone());
                                    }
                                    if ui.small_button("Remove").clicked() {
                                        remove_one = Some(e.exe.clone());
                                    }
                                });
                                ui.end_row();
                            }
                        });
                },
            );
            ui.add_space(8.0);
            if let Some(e) = edit.take() {
                self.draft = Some(e);
            }
            if let Some(x) = remove_one.take() {
                self.message = Some(if ctx.config.dry_run {
                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                } else {
                    remove(ctx, &[x])
                        .map(|()| "Removed. Undo it from the Change Journal.".into())
                        .map_err(|e| format!("{e:#}"))
                });
                self.user = None;
            }

            let mut close = false;
            if let Some(d) = &mut self.draft {
                let title = if d.exe.is_empty() {
                    "New compatibility settings".to_owned()
                } else {
                    "Edit compatibility settings".to_owned()
                };
                section(
                    ui,
                    &title,
                    |_| {},
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Program");
                            ui.add(egui::TextEdit::singleline(&mut d.exe).desired_width(380.0));
                            if ui.small_button("Browse…").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .add_filter("Programs", &["exe"])
                                    .pick_file()
                            {
                                d.exe = p.display().to_string();
                            }
                        });
                        ui.horizontal(|ui| {
                            let mut on = d.mode.is_some();
                            ui.checkbox(&mut on, "Run in compatibility mode for");
                            if on && d.mode.is_none() {
                                d.mode = Some("WIN8RTM".into());
                            }
                            if !on {
                                d.mode = None;
                            }
                            if let Some(m) = &mut d.mode {
                                egui::ComboBox::from_id_salt("compat-mode")
                                    .selected_text(
                                        MODES.iter().find(|x| x.0 == m).map_or(m.as_str(), |x| x.1),
                                    )
                                    .show_ui(ui, |ui| {
                                        for (code, label) in MODES {
                                            ui.selectable_value(m, code.to_owned(), label);
                                        }
                                    });
                            }
                        });
                        for (code, label) in FLAGS {
                            let mut on = d.flags.contains(code);
                            if ui.checkbox(&mut on, label).changed() {
                                if on {
                                    d.flags.insert(code.to_owned());
                                } else {
                                    d.flags.remove(code);
                                }
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.label("High DPI scaling performed by");
                            egui::ComboBox::from_id_salt("compat-dpi")
                                .selected_text(DPI[d.dpi].1)
                                .show_ui(ui, |ui| {
                                    for (i, (_, label)) in DPI.iter().enumerate() {
                                        ui.selectable_value(&mut d.dpi, i, *label);
                                    }
                                });
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                self.message = Some(if ctx.config.dry_run {
                                    Ok(ctx.i18n.tr("common-dry-run-skip"))
                                } else {
                                    save(ctx, d)
                                        .map(|()| {
                                            "Saved; it applies the next time the program starts.".into()
                                        })
                                        .map_err(|e| format!("{e:#}"))
                                });
                                close = self.message.as_ref().is_some_and(|m| m.is_ok());
                            }
                            if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                                close = true;
                            }
                        });
                    },
                );
            }
            if close {
                self.draft = None;
                self.user = None;
            }
            if self.draft.is_some() {
                ui.add_space(8.0);
            }

            section(
                ui,
                &format!("For all users ({})", machine.len()),
                |_| {},
                |ui| {
                    ui.weak(
                "Set by installers or by administrators; changing these needs the app to run as administrator.",
            );
                    egui::Grid::new("compat-machine")
                        .striped(true)
                        .num_columns(2)
                        .show(ui, |ui| {
                            for e in &machine {
                                ui.label(&e.exe);
                                ui.weak(e.summary());
                                ui.end_row();
                            }
                        });
                },
            );
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
    fn values_parse_and_format() {
        let e = Entry::parse(
            r"C:\x\a.exe",
            "~ WIN8RTM WIN7RTM RUNASADMIN GDIDPISCALING DPIUNAWARE",
        );
        assert_eq!(e.mode.as_deref(), Some("WIN8RTM"));
        assert!(e.flags.contains("RUNASADMIN"));
        assert_eq!(e.dpi, 3);
        // Windows' tab holds one mode; a second one (WIN7RTM here) is dropped on save.
        assert!(e.other.is_empty());
        assert_eq!(
            e.value().unwrap(),
            "~ WIN8RTM RUNASADMIN GDIDPISCALING DPIUNAWARE"
        );
        let keep = Entry::parse("x", "$ DWM8And16BitMitigation");
        assert_eq!(keep.other, ["DWM8And16BitMitigation"]);
        assert_eq!(keep.value().unwrap(), "~ DWM8And16BitMitigation");
        assert_eq!(Entry::default().value(), None);
        assert!(
            Entry::parse("x", "~ HIGHDPIAWARE")
                .summary()
                .contains("Application")
        );
    }

    #[test]
    fn real_lists_read_and_save_with_undo() {
        let _ = list(USER);
        let _ = list(MACHINE);
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        let exe = dir.path().join("wm-compat-test.exe").display().to_string();
        let mut e = Entry {
            exe: exe.clone(),
            mode: Some("WIN7RTM".into()),
            ..Default::default()
        };
        e.flags.insert("RUNASADMIN".into());
        save(&ctx, &e).unwrap();
        assert!(
            list(USER)
                .iter()
                .any(|x| x.exe == exe && x.flags.contains("RUNASADMIN"))
        );
        let id = journal.entries().into_iter().max_by_key(|e| e.id).unwrap().id;
        journal.undo(id).unwrap();
        assert!(
            !list(USER).iter().any(|x| x.exe == exe),
            "undo removes the new entry"
        );
        assert!(save(&ctx, &Entry::default()).is_err());
    }
}
