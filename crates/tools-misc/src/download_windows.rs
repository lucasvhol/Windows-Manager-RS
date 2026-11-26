//! Download Windows 11: Microsoft's own ways to get or reinstall Windows, with a note on
//! which suits what, and this PC's current version. Every link points at microsoft.com;
//! nothing is downloaded by this app.

use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    shell::shell_open,
    ui::{Message, ToolButton, frame, section, show_message},
};

/// (title, what it's for, link)
pub const OPTIONS: [(&str, &str, &str); 5] = [
    (
        "Windows 11 Installation Assistant",
        "Upgrades this PC to the latest Windows 11 in place, keeping your files and apps.",
        "https://go.microsoft.com/fwlink/?linkid=2171764",
    ),
    (
        "Media Creation Tool",
        "Makes a bootable USB drive or an ISO file, to install on another PC or start fresh.",
        "https://go.microsoft.com/fwlink/?linkid=2156295",
    ),
    (
        "Windows 11 download page (ISO)",
        "Microsoft's page with the disk image (ISO) in every language, for virtual machines or clean installs.",
        "https://www.microsoft.com/software-download/windows11",
    ),
    (
        "PC Health Check",
        "Checks whether this PC meets Windows 11's requirements (TPM 2.0, Secure Boot, processor).",
        "https://aka.ms/GetPCHealthCheckApp",
    ),
    (
        "Windows 10 download page",
        "Media Creation Tool and ISO for Windows 10.",
        "https://www.microsoft.com/software-download/windows10",
    ),
];

/// Only Microsoft's own addresses are ever opened.
pub fn is_microsoft(url: &str) -> bool {
    [
        "https://www.microsoft.com/",
        "https://go.microsoft.com/",
        "https://aka.ms/",
    ]
    .iter()
    .any(|p| url.starts_with(p))
}

fn current_windows() -> String {
    let key = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let read = |n: &str| {
        wm_core::registry::read_raw(key, n)
            .map(|v| wm_core::registry::decode_string(&v))
            .unwrap_or_default()
    };
    let build: u32 = read("CurrentBuildNumber").parse().unwrap_or(0);
    // ProductName still says "Windows 10" on Windows 11; the build tells them apart.
    let name = if build >= 22000 {
        read("ProductName").replace("Windows 10", "Windows 11")
    } else {
        read("ProductName")
    };
    format!("{name} {} (build {build})", read("DisplayVersion"))
}

#[derive(Default)]
pub struct DownloadWindows {
    message: Message,
}

impl Tool for DownloadWindows {
    fn id(&self) -> &'static str {
        "downloadwin"
    }

    fn category(&self) -> Category {
        Category::Misc
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if frame::toolbar(
            ui,
            &[Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Windows Update"))],
            |_| {},
        ) == Some(0)
        {
            self.message = shell_open("ms-settings:windowsupdate", "")
                .err()
                .map(|e| Err(format!("{e:#}")));
        }
        frame::status(ui, &format!("This PC runs {}.", current_windows()), |ui| {
            ui.weak("All links lead to microsoft.com; your browser downloads the file.");
        });
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            ui.weak("For regular updates, Windows Update is the simplest way.");
            ui.add_space(6.0);
            for (i, (title, what, url)) in OPTIONS.into_iter().enumerate() {
                let icon = [
                    icons::DOWNLOAD,
                    icons::USB,
                    icons::ARCHIVE,
                    icons::FIRST_AID_KIT,
                    icons::WINDOWS_LOGO,
                ][i];
                section(
                    ui,
                    "",
                    |_| {},
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icon).size(22.0));
                            ui.vertical(|ui| {
                                ui.strong(title);
                                ui.weak(what);
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Copy link").clicked() {
                                    ui.ctx().copy_text(url.to_owned());
                                }
                                if ui.button("Open").on_hover_text(url).clicked() && is_microsoft(url) {
                                    self.message = shell_open(url, "").err().map(|e| Err(format!("{e:#}")));
                                }
                            });
                        });
                    },
                );
                ui.add_space(6.0);
            }
        });
        let _ = ctx;
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_microsoft_only() {
        assert!(OPTIONS.iter().all(|(_, _, u)| is_microsoft(u)));
        assert!(!is_microsoft("https://microsoft.com.evil.example/"));
        assert!(current_windows().contains("build"));
    }
}
