//! System Information: OS, CPU, memory, board/BIOS, graphics, storage, network.

use sysinfo::{Disks, Motherboard, Networks, Product, System};
use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;
use wm_core::{
    AppCtx, Category, Tool, egui,
    task::BackgroundTask,
    ui::{Message, format_bytes, format_duration, show_message},
};

pub struct Section {
    pub title: &'static str,
    pub rows: Vec<(String, String)>,
}

#[derive(Default)]
pub struct SystemInfo {
    task: Option<BackgroundTask<Vec<Section>>>,
    sections: Option<Result<Vec<Section>, String>>,
    message: Message,
}

impl Tool for SystemInfo {
    fn id(&self) -> &'static str {
        "sysinfo"
    }

    fn category(&self) -> Category {
        Category::Information
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.task.is_none() && self.sections.is_none() {
            self.task = Some(BackgroundTask::spawn(ui.ctx(), |_| Ok(collect())));
        }
        if let Some(task) = &mut self.task {
            task.poll();
            if let Some(result) = task.take_result() {
                self.sections = Some(result);
                self.task = None;
            }
        }

        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.task.is_none(),
                    egui::Button::new(ctx.i18n.tr("common-refresh")),
                )
                .clicked()
            {
                self.sections = None;
                self.message = None;
            }
            if let Some(Ok(sections)) = &self.sections
                && ui.button(ctx.i18n.tr("common-copy")).clicked()
            {
                ui.ctx().copy_text(to_text(sections));
                self.message = Some(Ok(ctx.i18n.tr("common-copied")));
            }
            if self.task.is_some() {
                ui.spinner();
                ui.label(ctx.i18n.tr("common-loading"));
            }
        });
        show_message(ui, &self.message);
        ui.add_space(6.0);

        match &self.sections {
            Some(Ok(sections)) => {
                for section in sections {
                    egui::CollapsingHeader::new(section.title)
                        .default_open(true)
                        .show(ui, |ui| {
                            egui::Grid::new(section.title)
                                .num_columns(2)
                                .striped(true)
                                .spacing([24.0, 4.0])
                                .show(ui, |ui| {
                                    for (k, v) in &section.rows {
                                        ui.weak(k);
                                        ui.label(v);
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }
    }
}

fn to_text(sections: &[Section]) -> String {
    let mut out = String::new();
    for s in sections {
        out.push_str(&format!("[{}]\n", s.title));
        for (k, v) in &s.rows {
            out.push_str(&format!("{k}: {v}\n"));
        }
        out.push('\n');
    }
    out
}

/// Windows 11 still reports "Windows 10 …" as `ProductName`; builds ≥ 22000 are Windows 11.
pub fn windows_product_name(product_name: &str, build: u32) -> String {
    if build >= 22_000 && product_name.starts_with("Windows 10") {
        product_name.replacen("Windows 10", "Windows 11", 1)
    } else {
        product_name.to_owned()
    }
}

fn reg_str(key: &RegKey, name: &str) -> Option<String> {
    key.get_value::<String, _>(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// True for firmware placeholder strings that carry no information.
pub fn is_oem_placeholder(s: &str) -> bool {
    let s = s.trim().to_ascii_lowercase();
    [
        "to be filled by o.e.m.",
        "system product name",
        "system manufacturer",
        "system version",
        "default string",
        "not applicable",
        "not specified",
        "none",
        "o.e.m.",
    ]
    .contains(&s.as_str())
        || s.chars().all(|c| c == '0' || c == 'x' || c == '.')
}

fn push(rows: &mut Vec<(String, String)>, k: &str, v: Option<String>) {
    if let Some(v) = v.filter(|v| !is_oem_placeholder(v)) {
        rows.push((k.to_owned(), v));
    }
}

fn os_section() -> Section {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let mut rows = Vec::new();
    if let Ok(cv) = hklm.open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion") {
        let build: u32 = reg_str(&cv, "CurrentBuild")
            .and_then(|b| b.parse().ok())
            .unwrap_or(0);
        let ubr: Option<u32> = cv.get_value("UBR").ok();
        push(
            &mut rows,
            "Edition",
            reg_str(&cv, "ProductName").map(|p| windows_product_name(&p, build)),
        );
        push(&mut rows, "Version", reg_str(&cv, "DisplayVersion"));
        rows.push((
            "Build".into(),
            match ubr {
                Some(u) => format!("{build}.{u}"),
                None => build.to_string(),
            },
        ));
        if let Ok(ts) = cv.get_value::<u32, _>("InstallDate") {
            push(
                &mut rows,
                "Installed",
                chrono::DateTime::from_timestamp(ts as i64, 0)
                    .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string()),
            );
        }
        push(&mut rows, "Registered owner", reg_str(&cv, "RegisteredOwner"));
    }
    push(&mut rows, "Computer name", System::host_name().ok());
    rows.push(("Architecture".into(), System::cpu_arch()));
    push(&mut rows, "Kernel", System::kernel_long_version().ok());
    push(&mut rows, "Uptime", System::uptime().ok().map(format_duration));
    Section {
        title: "Operating system",
        rows,
    }
}

fn cpu_and_memory(sys: &System) -> [Section; 2] {
    let mut cpu = Vec::new();
    if let Some(c) = sys.cpus().first() {
        cpu.push(("Processor".into(), c.brand().trim().to_owned()));
        cpu.push(("Vendor".into(), c.vendor_id().to_owned()));
        cpu.push((
            "Base frequency".into(),
            format!("{:.2} GHz", c.frequency() as f64 / 1000.0),
        ));
    }
    push(
        &mut cpu,
        "Physical cores",
        System::physical_core_count().ok().map(|n| n.to_string()),
    );
    cpu.push(("Logical processors".into(), sys.cpus().len().to_string()));

    let mem = vec![
        ("Installed".into(), format_bytes(sys.total_memory())),
        (
            "In use".into(),
            format!(
                "{} ({:.0}%)",
                format_bytes(sys.used_memory()),
                sys.used_memory() as f64 * 100.0 / sys.total_memory().max(1) as f64
            ),
        ),
        ("Page file".into(), format_bytes(sys.total_swap())),
    ];
    [
        Section {
            title: "Processor",
            rows: cpu,
        },
        Section {
            title: "Memory",
            rows: mem,
        },
    ]
}

fn board_section() -> Section {
    let mut rows = Vec::new();
    push(&mut rows, "System", Product::name().ok());
    push(&mut rows, "System family", Product::family().ok());
    if let Ok(mb) = Motherboard::new() {
        let name = [mb.vendor_name(), mb.name()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        push(&mut rows, "Motherboard", Some(name).filter(|s| !s.is_empty()));
        push(&mut rows, "Board version", mb.version());
    }
    if let Ok(bios) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"HARDWARE\DESCRIPTION\System\BIOS") {
        push(&mut rows, "BIOS vendor", reg_str(&bios, "BIOSVendor"));
        push(&mut rows, "BIOS version", reg_str(&bios, "BIOSVersion"));
        push(&mut rows, "BIOS date", reg_str(&bios, "BIOSReleaseDate"));
    }
    Section {
        title: "Motherboard & BIOS",
        rows,
    }
}

fn graphics_section() -> Section {
    const DISPLAY_CLASS: &str =
        r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let mut rows = Vec::new();
    if let Ok(class) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(DISPLAY_CLASS) {
        for name in class
            .enum_keys()
            .flatten()
            .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
        {
            let Ok(adapter) = class.open_subkey(&name) else {
                continue;
            };
            let Some(desc) = reg_str(&adapter, "DriverDesc") else {
                continue;
            };
            let vram = adapter
                .get_value::<u64, _>("HardwareInformation.qwMemorySize")
                .ok()
                .or_else(|| {
                    adapter
                        .get_value::<u32, _>("HardwareInformation.MemorySize")
                        .ok()
                        .map(u64::from)
                })
                .filter(|v| *v > 0);
            let mut value = desc;
            if let Some(v) = vram {
                value.push_str(&format!(" — {}", format_bytes(v)));
            }
            if let Some(ver) = reg_str(&adapter, "DriverVersion") {
                value.push_str(&format!(" (driver {ver})"));
            }
            rows.push((format!("Adapter {}", rows.len() + 1), value));
        }
    }
    Section {
        title: "Graphics",
        rows,
    }
}

fn storage_section() -> Section {
    let mut rows = Vec::new();
    if let Ok(disks) = Disks::new_with_refreshed_list() {
        for d in disks.iter() {
            let total = d.total_space();
            let used = total.saturating_sub(d.available_space());
            rows.push((
                format!(
                    "{} ({})",
                    d.mount_point().display(),
                    d.file_system().to_string_lossy()
                ),
                format!(
                    "{} used of {} — {}{}",
                    format_bytes(used),
                    format_bytes(total),
                    d.kind(),
                    if d.is_removable() { ", removable" } else { "" }
                ),
            ));
        }
    }
    Section {
        title: "Storage",
        rows,
    }
}

fn network_section() -> Section {
    let mut rows = Vec::new();
    if let Ok(nets) = Networks::new_with_refreshed_list() {
        let mut list: Vec<_> = nets.iter().collect();
        list.sort_by_key(|(name, _)| name.to_string_lossy().to_lowercase());
        for (name, data) in list {
            let ips: Vec<String> = data.ip_networks().iter().map(|ip| ip.addr.to_string()).collect();
            if ips.is_empty() {
                continue;
            }
            rows.push((
                name.to_string_lossy().into_owned(),
                format!("{} — MAC {}", ips.join(", "), data.mac_address()),
            ));
        }
    }
    Section {
        title: "Network adapters",
        rows,
    }
}

fn collect() -> Vec<Section> {
    let mut sections = vec![os_section()];
    match System::new_all() {
        Ok(sys) => sections.extend(cpu_and_memory(&sys)),
        Err(e) => tracing::warn!("sysinfo: {e}"),
    }
    sections.push(board_section());
    sections.push(graphics_section());
    sections.push(storage_section());
    sections.push(network_section());
    sections.retain(|s| !s.rows.is_empty());
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oem_placeholders() {
        assert!(is_oem_placeholder("To be filled by O.E.M."));
        assert!(is_oem_placeholder(" System Product Name "));
        assert!(is_oem_placeholder("0000"));
        assert!(!is_oem_placeholder("TUF GAMING B850M-PLUS WIFI"));
        assert!(!is_oem_placeholder("Rev 1.xx"));
    }

    #[test]
    fn product_name_fix() {
        assert_eq!(windows_product_name("Windows 10 Pro", 26_100), "Windows 11 Pro");
        assert_eq!(windows_product_name("Windows 10 Pro", 19_045), "Windows 10 Pro");
        assert_eq!(
            windows_product_name("Windows Server 2022", 20_348),
            "Windows Server 2022"
        );
    }

    #[test]
    fn collect_returns_core_sections() {
        let sections = collect();
        let titles: Vec<_> = sections.iter().map(|s| s.title).collect();
        for t in ["Operating system", "Processor", "Memory", "Storage"] {
            assert!(titles.contains(&t), "missing {t} in {titles:?}");
        }
        assert!(to_text(&sections).contains("[Processor]"));
    }
}
