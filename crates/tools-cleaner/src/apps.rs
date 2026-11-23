//! Windows Apps Uninstaller: remove Microsoft Store / provisioned apps for the current user.
//! Uses the Appx PowerShell module (stable property names, locale-independent JSON).

use std::path::Path;

use anyhow::{Context, Result};
use egui_extras::{Column, TableBuilder};
use serde::Deserialize;
use wm_core::{
    AppCtx, Category, Tool, egui,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, show_message},
};

/// Packages that keep the Store, app installation and security UI working.
const PROTECTED: &[&str] = &[
    "Microsoft.WindowsStore",
    "Microsoft.StorePurchaseApp",
    "Microsoft.DesktopAppInstaller",
    "Microsoft.SecHealthUI",
    "Microsoft.Windows.ShellExperienceHost",
    "Microsoft.Windows.StartMenuExperienceHost",
    "Microsoft.WindowsAppRuntime",
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Package {
    pub name: String,
    pub package_full_name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub install_location: Option<String>,
    #[serde(default)]
    pub publisher: String,
    #[serde(skip)]
    pub display_name: String,
}

impl Package {
    pub fn is_protected(&self) -> bool {
        PROTECTED.iter().any(|p| self.name.starts_with(p))
    }
}

/// `<DisplayName>` from the package manifest, unless it is an unresolved resource reference.
pub fn manifest_display_name(manifest: &str) -> Option<String> {
    let props = &manifest[manifest.find("<Properties>")?..];
    let start = props.find("<DisplayName>")? + "<DisplayName>".len();
    let end = start + props[start..].find("</DisplayName>")?;
    let name = props[start..end].trim();
    (!name.is_empty() && !name.starts_with("ms-resource:")).then(|| name.replace("&amp;", "&"))
}

/// `Microsoft.ZuneMusic` → `ZuneMusic`; `5319275A.WhatsAppDesktop` → `WhatsAppDesktop`.
pub fn pretty_name(package_name: &str) -> String {
    package_name.rsplit('.').next().unwrap_or(package_name).to_owned()
}

fn powershell(script: &str) -> Result<String> {
    let out = run_checked(hidden_command("powershell").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn enumerate() -> Result<Vec<Package>> {
    let json = powershell(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         ConvertTo-Json -Compress -InputObject @(Get-AppxPackage | Where-Object { \
           -not $_.IsFramework -and -not $_.NonRemovable -and \"$($_.SignatureKind)\" -ne 'System' } | \
         Select-Object Name, PackageFullName, Version, InstallLocation, Publisher)",
    )?;
    let mut packages: Vec<Package> =
        serde_json::from_str(json.trim()).context("parsing Get-AppxPackage output")?;
    for p in &mut packages {
        p.display_name = p
            .install_location
            .as_deref()
            .and_then(|loc| std::fs::read_to_string(Path::new(loc).join("AppxManifest.xml")).ok())
            .and_then(|m| manifest_display_name(&m))
            .unwrap_or_else(|| pretty_name(&p.name));
    }
    packages.sort_by_key(|p| p.display_name.to_lowercase());
    Ok(packages)
}

pub fn remove(package: &Package) -> Result<()> {
    // Full names contain only [A-Za-z0-9._~-]; reject anything else before quoting.
    if !package
        .package_full_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c))
    {
        anyhow::bail!("unexpected package name {}", package.package_full_name);
    }
    powershell(&format!(
        "Remove-AppxPackage -Package '{}' -ErrorAction Stop",
        package.package_full_name
    ))?;
    Ok(())
}

/// Per-app removal outcome (display name, result).
type RemovalResults = Vec<(String, Result<(), String>)>;

#[derive(Default)]
pub struct AppsUninstaller {
    load: Option<BackgroundTask<Vec<Package>>>,
    packages: Option<Result<Vec<Package>, String>>,
    removing: Option<BackgroundTask<RemovalResults>>,
    marked: std::collections::HashSet<String>,
    filter: String,
    confirm: Confirm<Vec<Package>>,
    message: Message,
}

impl Tool for AppsUninstaller {
    fn id(&self) -> &'static str {
        "apps"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.load.is_none() && self.packages.is_none() {
            self.load = Some(BackgroundTask::spawn(ui.ctx(), |_| enumerate()));
        }
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                self.packages = Some(r);
                self.load = None;
            }
        }
        if let Some(t) = &mut self.removing {
            t.poll();
            if let Some(Ok(results)) = t.take_result() {
                self.removing = None;
                let ok: Vec<&String> = results
                    .iter()
                    .filter(|(_, r)| r.is_ok())
                    .map(|(n, _)| n)
                    .collect();
                let failed: Vec<String> = results
                    .iter()
                    .filter_map(|(n, r)| r.as_ref().err().map(|e| format!("{n}: {e}")))
                    .collect();
                for n in &ok {
                    let _ = ctx.journal.record(
                        "apps",
                        format!("Removed Windows app {n} (reinstall it from the Microsoft Store)"),
                        UndoAction::None,
                    );
                }
                self.message = Some(if failed.is_empty() {
                    Ok(format!("Removed {} apps.", ok.len()))
                } else {
                    Err(format!("Removed {}; failed: {}", ok.len(), failed.join("; ")))
                });
                self.marked.clear();
                self.packages = None;
            }
        }
        if let Some(list) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                self.removing = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    let n = list.len() as f32;
                    Ok(list
                        .iter()
                        .enumerate()
                        .map(|(i, pkg)| {
                            p.report(i as f32 / n, format!("Removing {}…", pkg.display_name));
                            (
                                pkg.display_name.clone(),
                                remove(pkg).map_err(|e| format!("{e:#}")),
                            )
                        })
                        .collect())
                }));
            }
        }

        let packages = match &self.packages {
            Some(Ok(p)) => p.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => {
                ui.spinner();
                return;
            }
        };
        let chosen: Vec<Package> = packages
            .iter()
            .filter(|p| self.marked.contains(&p.package_full_name))
            .cloned()
            .collect();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            ui.weak(format!("{} removable apps", packages.len()));
            ui.separator();
            let busy = self.removing.is_some();
            if ui
                .add_enabled(
                    !chosen.is_empty() && !busy,
                    egui::Button::new(format!("Remove {} apps…", chosen.len())),
                )
                .clicked()
            {
                let names: Vec<&str> = chosen.iter().map(|p| p.display_name.as_str()).collect();
                self.confirm.ask(
                    format!(
                        "Remove {} for your account? They can be reinstalled from the Microsoft Store, \
                         but their local data will be deleted.",
                        names.join(", ")
                    ),
                    chosen.clone(),
                );
            }
            if let Some(t) = &self.removing {
                ui.spinner();
                ui.weak(&t.status);
            }
        });
        show_message(ui, &self.message);

        let f = self.filter.to_lowercase();
        let visible: Vec<&Package> = packages
            .iter()
            .filter(|p| {
                f.is_empty()
                    || p.display_name.to_lowercase().contains(&f)
                    || p.name.to_lowercase().contains(&f)
            })
            .collect();
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(30.0))
            .column(Column::initial(260.0).clip(true))
            .column(Column::initial(280.0).clip(true))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in ["", "App", "Package", "Version"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let p = visible[row.index()];
                    row.col(|ui| {
                        let mut on = self.marked.contains(&p.package_full_name);
                        let r = ui.add_enabled(!p.is_protected(), egui::Checkbox::without_text(&mut on));
                        if r.changed() {
                            if on {
                                self.marked.insert(p.package_full_name.clone());
                            } else {
                                self.marked.remove(&p.package_full_name);
                            }
                        }
                        r.on_disabled_hover_text(
                            "Needed for the Store, app installation or Windows Security.",
                        );
                    });
                    row.col(|ui| {
                        ui.label(&p.display_name);
                    });
                    row.col(|ui| {
                        ui.weak(&p.name);
                    });
                    row.col(|ui| {
                        ui.weak(&p.version);
                    });
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names() {
        let m = "<Package><Properties><DisplayName>Clipchamp &amp; Co</DisplayName></Properties></Package>";
        assert_eq!(manifest_display_name(m).as_deref(), Some("Clipchamp & Co"));
        let r = "<Properties><DisplayName>ms-resource:AppName</DisplayName></Properties>";
        assert_eq!(manifest_display_name(r), None);
        assert_eq!(pretty_name("Microsoft.ZuneMusic"), "ZuneMusic");
    }

    #[test]
    fn protection_and_name_validation() {
        let p = Package {
            name: "Microsoft.WindowsStore".into(),
            package_full_name: "Microsoft.WindowsStore_1.0_x64__8wekyb3d8bbwe".into(),
            version: String::new(),
            install_location: None,
            publisher: String::new(),
            display_name: String::new(),
        };
        assert!(p.is_protected());
        let evil = Package {
            package_full_name: "x'; Remove-Item C:\\ -Recurse; '".into(),
            ..p
        };
        assert!(
            remove(&evil).is_err(),
            "injection-shaped names are refused before PowerShell runs"
        );
    }

    #[test]
    fn lists_real_packages() {
        let packages = enumerate().unwrap();
        assert!(
            packages
                .iter()
                .all(|p| !p.package_full_name.is_empty() && !p.display_name.is_empty())
        );
    }
}
