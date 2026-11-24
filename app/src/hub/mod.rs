//! The hub: category rail, tool tiles and the Home overview.

pub mod catalog;
pub mod widgets;

use wm_core::{egui, i18n::I18n, icons, registry, safety::ChangeJournal, ui::format_bytes};

/// Static facts about this PC shown in the rail footer and on Home.
pub struct Summary {
    /// e.g. "Windows 11 Pro 64-bit 26H2"
    pub os: String,
    /// e.g. "Build 26300.10075"
    pub build: String,
    pub cpu: String,
    pub memory: String,
}

/// Windows 11 still reports "Windows 10 …" as ProductName; builds ≥ 22000 are Windows 11.
pub fn product_name(raw: &str, build: u32) -> String {
    if build >= 22_000 && raw.starts_with("Windows 10") {
        raw.replacen("Windows 10", "Windows 11", 1)
    } else {
        raw.to_owned()
    }
}

impl Summary {
    pub fn collect() -> Self {
        let key = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        let s = |name: &str| {
            registry::read_raw(key, name)
                .map(|v| registry::decode_string(&v))
                .unwrap_or_default()
        };
        let build_no: u32 = s("CurrentBuild").parse().unwrap_or(0);
        let ubr = registry::read_raw(key, "UBR").and_then(|v| registry::decode_dword(&v));
        let bits = if cfg!(target_pointer_width = "64") {
            "64-bit"
        } else {
            "32-bit"
        };
        let (cpu, memory) = match sysinfo::System::new() {
            Ok(mut sys) => {
                sys.refresh_memory();
                sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
                (
                    sys.cpus()
                        .first()
                        .map(|c| c.brand().trim().to_owned())
                        .unwrap_or_default(),
                    format_bytes(sys.total_memory()),
                )
            }
            Err(_) => (String::new(), String::new()),
        };
        Summary {
            os: format!(
                "{} {bits} {}",
                product_name(&s("ProductName"), build_no),
                s("DisplayVersion")
            )
            .trim()
            .to_owned(),
            build: match ubr {
                Some(u) => format!("Build {build_no}.{u}"),
                None => format!("Build {build_no}"),
            },
            cpu,
            memory,
        }
    }
}

/// What the Home page asks the shell to do.
#[derive(Default)]
pub struct HomeActions {
    pub open: Vec<String>,
}

pub fn home(
    ui: &mut egui::Ui,
    summary: &Summary,
    journal: &ChangeJournal,
    i18n: &I18n,
    recent: &[String],
    elevated: bool,
) -> HomeActions {
    let mut actions = HomeActions::default();
    let title = |id: &str| i18n.tr(&format!("{id}-title"));
    ui.add_space(4.0);
    ui.label(egui::RichText::new(i18n.tr("hub-welcome")).size(20.0).strong());
    ui.weak(i18n.tr("hub-welcome-sub"));
    ui.add_space(10.0);

    ui.columns(2, |cols| {
        widgets::card(&mut cols[0], &i18n.tr("hub-this-pc"), icons::DESKTOP, |ui| {
            egui::Grid::new("home_pc")
                .num_columns(2)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    for (k, v) in [
                        ("Windows", summary.os.clone()),
                        ("", summary.build.clone()),
                        ("Processor", summary.cpu.clone()),
                        ("Memory", summary.memory.clone()),
                        (
                            "Rights",
                            if elevated {
                                "Administrator".into()
                            } else {
                                "Standard user".into()
                            },
                        ),
                    ] {
                        ui.weak(k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            if ui
                .button(format!("{}  {}", icons::CPU, title("sysinfo")))
                .clicked()
            {
                actions.open.push("sysinfo".into());
            }
        });
        cols[0].add_space(10.0);
        widgets::card(
            &mut cols[0],
            &i18n.tr("hub-recent-changes"),
            icons::CLOCK_COUNTER_CLOCKWISE,
            |ui| {
                let entries = journal.entries();
                if entries.is_empty() {
                    ui.weak(i18n.tr("journal-empty"));
                }
                for e in entries.iter().take(5) {
                    ui.horizontal(|ui| {
                        ui.weak(e.time.format("%m-%d %H:%M").to_string());
                        ui.label(&e.description);
                    });
                }
                if ui.button(i18n.tr("journal-title")).clicked() {
                    actions.open.push("journal".into());
                }
            },
        );

        widgets::card(
            &mut cols[1],
            &i18n.tr("hub-maintenance"),
            icons::LIST_CHECKS,
            |ui| {
                for (n, (step, tools)) in catalog::MAINTENANCE.iter().enumerate() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{}", n + 1))
                                .strong()
                                .color(widgets::ACCENT),
                        );
                        ui.label(*step);
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(18.0);
                        for id in *tools {
                            if ui
                                .small_button(format!("{} {}", catalog::tool_icon(id), title(id)))
                                .clicked()
                            {
                                actions.open.push((*id).to_owned());
                            }
                        }
                    });
                    ui.add_space(2.0);
                }
            },
        );
        if !recent.is_empty() {
            cols[1].add_space(10.0);
            widgets::card(&mut cols[1], &i18n.tr("hub-recent-tools"), icons::CLOCK, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for id in recent {
                        if ui
                            .button(format!("{} {}", catalog::tool_icon(id), title(id)))
                            .clicked()
                        {
                            actions.open.push(id.clone());
                        }
                    }
                });
            });
        }
    });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_names_and_summary() {
        assert_eq!(product_name("Windows 10 Pro", 26_300), "Windows 11 Pro");
        assert_eq!(product_name("Windows 10 Home", 19_045), "Windows 10 Home");
        let s = Summary::collect();
        assert!(s.os.starts_with("Windows"), "{}", s.os);
        assert!(s.build.starts_with("Build "));
    }

    #[test]
    fn catalog_has_icons_and_no_duplicate_tiles() {
        let mut seen = Vec::new();
        for (_, ids) in catalog::TILES {
            for id in *ids {
                assert!(!seen.contains(id), "{id} has two tiles");
                seen.push(*id);
                assert_ne!(
                    catalog::tool_icon(id),
                    wm_core::icons::APP_WINDOW,
                    "{id} needs an icon"
                );
            }
        }
    }
}
