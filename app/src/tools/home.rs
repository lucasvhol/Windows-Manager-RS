use wm_core::{AppCtx, Category, Tool, egui};

pub struct Home;

/// Highest phase that has shipped.
const DONE_THROUGH: u8 = 4;

/// Planned delivery phase for each category (see the project plan).
const ROADMAP: &[(Category, u8, &str)] = &[
    (
        Category::Information,
        1,
        "System Info, Process Manager, Event Viewer, Device Manager, Hardware Monitor",
    ),
    (
        Category::Optimizer,
        2,
        "Startup, Services, Task Scheduler, Boot Configuration, System Speed",
    ),
    (
        Category::Cleaner,
        3,
        "Disk Analyzer, Junk Files, Duplicates, Desktop, Uninstallers, Registry Cleaner, Component Store",
    ),
    (
        Category::Customization,
        4,
        "Context Menu, Win+X, Start Menu, Hotkeys, Run Shortcuts, JumpList, Visual & Explorer tweaks",
    ),
    (
        Category::Security,
        5,
        "Privacy, Settings Security, File Security, Security Lock, File Undelete",
    ),
    (
        Category::Network,
        6,
        "IP Switcher, Wi-Fi, URL/hosts Manager, Network Tools, Network Monitor",
    ),
    (
        Category::Misc,
        7,
        "My Task, File Splitter, Super Copy, Registry Tools, Windows Utilities",
    ),
    (
        Category::OneClick,
        7,
        "1-Click Cleaner, 1-Click Fixer, Optimization Wizard, Repair Center",
    ),
];

impl Tool for Home {
    fn id(&self) -> &'static str {
        "home"
    }

    fn category(&self) -> Category {
        Category::Home
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(ctx.i18n.tr("home-welcome"));
        ui.add_space(12.0);
        ui.strong(ctx.i18n.tr("home-roadmap"));
        egui::Grid::new("roadmap")
            .striped(true)
            .num_columns(3)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                for (cat, phase, tools) in ROADMAP {
                    let done = if *phase <= DONE_THROUGH { " ✔" } else { "" };
                    ui.label(format!("Phase {phase}{done}"));
                    ui.strong(ctx.i18n.tr(cat.i18n_key()));
                    ui.label(*tools);
                    ui.end_row();
                }
            });
    }
}
