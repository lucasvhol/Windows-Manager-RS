//! Run Shortcut Creator: short names for the Run dialog (Win+R), e.g. typing `np`
//! opens Notepad++. Uses the documented App Paths registry keys.

use std::path::Path;

use anyhow::{Result, bail};
use wm_core::{
    AppCtx, Category, Tool, egui,
    process::{hidden_command, run_checked},
    registry,
    ui::{Confirm, Message, show_message},
};

const USER_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths";
const MACHINE_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths";

#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub alias: String,
    pub key: String,
    pub target: String,
    pub machine: bool,
}

pub fn enumerate() -> Vec<Alias> {
    let mut out = Vec::new();
    for (root, machine) in [(USER_KEY, false), (MACHINE_KEY, true)] {
        let Ok(k) = registry::open_read(root) else {
            continue;
        };
        for sub in k.enum_keys().flatten() {
            let key = format!(r"{root}\{sub}");
            let target = registry::read_raw(&key, "")
                .map(|v| registry::decode_string(&v))
                .unwrap_or_default();
            let alias = sub
                .strip_suffix(".exe")
                .or_else(|| sub.strip_suffix(".EXE"))
                .unwrap_or(&sub)
                .to_owned();
            out.push(Alias {
                alias,
                key,
                target,
                machine,
            });
        }
    }
    out.sort_by_key(|a| a.alias.to_lowercase());
    out
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty() || alias.len() > 40 {
        bail!("The name must be 1–40 characters.");
    }
    if !alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Use only letters, digits, - and _.");
    }
    Ok(())
}

#[derive(Default)]
pub struct RunShortcuts {
    items: Option<Vec<Alias>>,
    filter: String,
    alias: String,
    target: String,
    confirm: Confirm<Alias>,
    message: Message,
}

impl RunShortcuts {
    fn add(&mut self, ctx: &AppCtx<'_>) -> Result<String> {
        let alias = self.alias.trim().to_owned();
        validate_alias(&alias)?;
        let target = self.target.trim().trim_matches('"').to_owned();
        if !Path::new(&target).is_file() {
            bail!("Choose an existing program file.");
        }
        if self
            .items
            .iter()
            .flatten()
            .any(|a| a.alias.eq_ignore_ascii_case(&alias))
        {
            bail!("\"{alias}\" already exists.");
        }
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        let key = format!(r"{USER_KEY}\{alias}.exe");
        let undo = ctx.journal.backup_reg_key(&key)?;
        registry::write_raw(&key, "", &registry::string(&target))?;
        if let Some(dir) = Path::new(&target).parent() {
            registry::write_raw(&key, "Path", &registry::string(&dir.display().to_string()))?;
        }
        ctx.journal.record(
            "runshortcuts",
            format!("Added Run alias \"{alias}\" → {target}"),
            undo,
        )?;
        Ok(format!("Press Win+R and type \"{alias}\" to open it."))
    }
}

impl Tool for RunShortcuts {
    fn id(&self) -> &'static str {
        "runshortcuts"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Create short names you can type in the Run dialog (Win+R) or the address bar.");
        let items = self.items.get_or_insert_with(enumerate).clone();

        if let Some(a) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                ctx.journal
                    .backup_reg_key(&a.key)
                    .and_then(|undo| {
                        run_checked(hidden_command("reg").args(["delete", &a.key, "/f"]))?;
                        ctx.journal
                            .record("runshortcuts", format!("Removed Run alias \"{}\"", a.alias), undo)
                    })
                    .map(|_| format!("Removed \"{}\".", a.alias))
                    .map_err(|e| format!("{e:#}"))
            });
            self.items = None;
        }

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.alias)
                    .hint_text("Short name")
                    .desired_width(120.0),
            );
            ui.label("opens");
            ui.add(
                egui::TextEdit::singleline(&mut self.target)
                    .hint_text("Program")
                    .desired_width(320.0),
            );
            if ui.button("Browse…").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("Programs", &["exe"])
                    .pick_file()
            {
                self.target = p.display().to_string();
            }
            if ui.button("Add").clicked() {
                self.message = Some(self.add(ctx).map_err(|e| format!("{e:#}")));
                self.items = None;
            }
        });
        show_message(ui, &self.message);
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            ui.weak(format!("{} names", items.len()));
        });

        let f = self.filter.to_lowercase();
        let mut remove = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("app_paths")
                .num_columns(4)
                .striped(true)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    for a in items.iter().filter(|a| {
                        f.is_empty()
                            || a.alias.to_lowercase().contains(&f)
                            || a.target.to_lowercase().contains(&f)
                    }) {
                        ui.strong(&a.alias);
                        ui.label(&a.target);
                        ui.weak(if a.machine { "all users" } else { "you" });
                        if ui
                            .add_enabled(!a.machine || ctx.elevated, egui::Button::new("Remove").small())
                            .clicked()
                        {
                            remove = Some(a.clone());
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(a) = remove {
            let warn = if a.machine {
                " Programs or scripts may rely on this system-wide name."
            } else {
                ""
            };
            self.confirm
                .ask(format!("Remove the Run name \"{}\"?{warn}", a.alias), a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_validation() {
        assert!(validate_alias("np").is_ok());
        assert!(validate_alias("my-tool_2").is_ok());
        assert!(validate_alias("").is_err());
        assert!(validate_alias("has space").is_err());
        assert!(validate_alias(r"..\evil").is_err());
    }

    #[test]
    fn lists_real_app_paths() {
        let items = enumerate();
        assert!(
            items.iter().any(|a| a.machine),
            "Windows ships machine-wide App Paths"
        );
    }
}
