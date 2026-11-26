//! System Information: summary, licence and scores, accounts, updates, printers,
//! codecs, environment, disk health, disk speed test and Windows reports.

use sysinfo::{Disks, Motherboard, Networks, Product, System};
use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, ToolButton, frame},
    ui::{Message, format_bytes, format_duration, show_message},
};

pub struct Section {
    pub title: &'static str,
    pub rows: Vec<(String, String)>,
}

/// A simple table produced by a background query.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub columns: Vec<&'static str>,
    pub rows: Vec<Vec<String>>,
}

fn json_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
        serde_json::Value::String(s) => s.trim().to_owned(),
        other => other.to_string(),
    }
}

/// Runs a PowerShell query and turns the listed properties into table columns.
fn ps_table(script: &str, columns: &[(&'static str, &str)]) -> anyhow::Result<Table> {
    let list = wm_core::process::json_list(wm_core::process::powershell_json(script)?);
    Ok(Table {
        columns: columns.iter().map(|(label, _)| *label).collect(),
        rows: list
            .iter()
            .map(|o| columns.iter().map(|(_, key)| json_str(&o[*key])).collect())
            .collect(),
    })
}

fn licence_status(n: i64) -> &'static str {
    match n {
        0 => "Unlicensed",
        1 => "Activated",
        2 | 3 | 6 => "Grace period",
        4 => "Not genuine",
        5 => "Notification mode",
        _ => "Unknown",
    }
}

fn query_software() -> anyhow::Result<Table> {
    let v = wm_core::process::powershell_json(
        r#"$p = Get-CimInstance SoftwareLicensingProduct -Filter "ApplicationID='55c92734-d682-4d71-983e-d6ec3f16059f' and PartialProductKey is not null" | Select-Object -First 1
$s = Get-CimInstance SoftwareLicensingService
$w = Get-CimInstance Win32_WinSAT -ErrorAction SilentlyContinue | Select-Object -First 1
[pscustomobject]@{ Licence = $p.Name; Status = [int]$p.LicenseStatus; Channel = $p.ProductKeyChannel; Partial = $p.PartialProductKey; Firmware = $s.OA3xOriginalProductKey;
  Cpu = $w.CPUScore; Memory = $w.MemoryScore; Graphics = $w.GraphicsScore; Gaming = $w.D3DScore; Disk = $w.DiskScore; Base = $w.WinSPRLevel } | ConvertTo-Json -Compress"#,
    )?;
    let mut rows = vec![
        vec!["Licence".into(), json_str(&v["Licence"])],
        vec![
            "Activation".into(),
            licence_status(v["Status"].as_i64().unwrap_or(-1)).into(),
        ],
        vec!["Key type".into(), json_str(&v["Channel"])],
        vec!["Installed key ends with".into(), json_str(&v["Partial"])],
        vec!["Key stored in firmware".into(), json_str(&v["Firmware"])],
    ];
    let scores: Vec<(&str, &str)> = vec![
        ("CPU", "Cpu"),
        ("Memory", "Memory"),
        ("Desktop graphics", "Graphics"),
        ("Gaming graphics", "Gaming"),
        ("Disk", "Disk"),
        ("Base score", "Base"),
    ];
    for (label, key) in scores {
        let s = json_str(&v[key]);
        if !s.is_empty() && s != "0" {
            rows.push(vec![format!("Experience index: {label}"), s]);
        }
    }
    rows.retain(|r| !r[1].is_empty());
    Ok(Table {
        columns: vec!["Item", "Value"],
        rows,
    })
}

fn query_users() -> anyhow::Result<Table> {
    ps_table(
        r#"$a = @(Get-LocalGroupMember -SID S-1-5-32-544 -ErrorAction SilentlyContinue | ForEach-Object { ($_.Name -split '\\')[-1] })
@(Get-LocalUser | ForEach-Object { [pscustomobject]@{ Name = $_.Name; Admin = ($a -contains $_.Name); Enabled = $_.Enabled;
  Password = $_.PasswordRequired; LastLogon = $(if ($_.LastLogon) { $_.LastLogon.ToString('yyyy-MM-dd HH:mm') } else { '' }); Description = $_.Description } }) | ConvertTo-Json -Compress"#,
        &[
            ("Account", "Name"),
            ("Administrator", "Admin"),
            ("Enabled", "Enabled"),
            ("Password required", "Password"),
            ("Last sign-in", "LastLogon"),
            ("Description", "Description"),
        ],
    )
}

fn query_updates() -> anyhow::Result<Table> {
    ps_table(
        r#"@(Get-HotFix | Select-Object HotFixID, Description, @{ n = 'InstalledOn'; e = { if ($_.InstalledOn) { $_.InstalledOn.ToString('yyyy-MM-dd') } } } | Sort-Object InstalledOn -Descending) | ConvertTo-Json -Compress"#,
        &[
            ("Update", "HotFixID"),
            ("Type", "Description"),
            ("Installed", "InstalledOn"),
        ],
    )
}

fn query_printers() -> anyhow::Result<Table> {
    ps_table(
        r#"@(Get-CimInstance Win32_Printer | Select-Object Name, DriverName, PortName, Default, WorkOffline) | ConvertTo-Json -Compress"#,
        &[
            ("Printer", "Name"),
            ("Driver", "DriverName"),
            ("Port", "PortName"),
            ("Default", "Default"),
            ("Offline", "WorkOffline"),
        ],
    )
}

fn query_disks() -> anyhow::Result<Table> {
    ps_table(
        r#"@(Get-PhysicalDisk | Sort-Object DeviceId | ForEach-Object { $r = $_ | Get-StorageReliabilityCounter -ErrorAction SilentlyContinue
  [pscustomobject]@{ Name = $_.FriendlyName; Media = [string]$_.MediaType; Bus = [string]$_.BusType; Health = [string]$_.HealthStatus;
    Size = '{0:N0} GB' -f ($_.Size / 1GB); Temp = $(if ($r.Temperature) { "$($r.Temperature) °C" }); Wear = $(if ($r.Wear -ne $null) { "$($r.Wear) %" });
    Hours = $r.PowerOnHours; Errors = $(if ($r) { [int64]$r.ReadErrorsTotal + [int64]$r.WriteErrorsTotal }) } }) | ConvertTo-Json -Compress"#,
        &[
            ("Disk", "Name"),
            ("Type", "Media"),
            ("Bus", "Bus"),
            ("Health", "Health"),
            ("Size", "Size"),
            ("Temperature", "Temp"),
            ("Wear", "Wear"),
            ("Power-on hours", "Hours"),
            ("Errors", "Errors"),
        ],
    )
}

fn query_codecs() -> anyhow::Result<Table> {
    let mut rows = Vec::new();
    for (view, label) in [
        (
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Drivers32",
            "64-bit",
        ),
        (
            r"SOFTWARE\WOW6432Node\Microsoft\Windows NT\CurrentVersion\Drivers32",
            "32-bit",
        ),
    ] {
        if let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(view) {
            for (name, _) in k.enum_values().flatten() {
                let file: String = k.get_value(&name).unwrap_or_default();
                if !file.trim().is_empty() {
                    rows.push(vec![name, file, label.into()]);
                }
            }
        }
    }
    rows.sort();
    Ok(Table {
        columns: vec!["Type", "File", "Platform"],
        rows,
    })
}

fn query_environment() -> Table {
    let mut rows: Vec<Vec<String>> = std::env::vars().map(|(k, v)| vec![k, v]).collect();
    rows.sort_by_key(|r| r[0].to_ascii_lowercase());
    Table {
        columns: vec!["Variable", "Value"],
        rows,
    }
}

/// The original's two-level tree: Hardware and Software, each with its pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Hardware,
    Mainboard,
    Bios,
    Cpu,
    Memory,
    Drive,
    HddReport,
    DiskBenchmark,
    Video,
    Audio,
    Network,
    ExternalDevice,
    Printers,
    Software,
    ComputerSystem,
    OperatingSystem,
    ProductKey,
    UserAccount,
    SystemFolder,
    Environment,
    Codecs,
    EventLog,
    Hotfix,
    SystemReport,
}

/// (depth, icon, label, page)
const NAV: [(usize, &str, &str, Page); 24] = [
    (0, icons::CPU, "Hardware", Page::Hardware),
    (1, icons::CPU, "Mainboard", Page::Mainboard),
    (1, icons::CPU, "BIOS", Page::Bios),
    (1, icons::CPU, "CPU", Page::Cpu),
    (1, icons::STACK, "Memory", Page::Memory),
    (1, icons::HARD_DRIVES, "Drive", Page::Drive),
    (2, icons::FILE_TEXT, "HDD Report", Page::HddReport),
    (2, icons::GAUGE, "Disk Benchmark", Page::DiskBenchmark),
    (1, icons::MONITOR, "Video", Page::Video),
    (1, icons::BELL, "Audio", Page::Audio),
    (1, icons::NETWORK, "Network", Page::Network),
    (1, icons::USB, "External Device", Page::ExternalDevice),
    (1, icons::PRINTER, "Printers", Page::Printers),
    (0, icons::WINDOWS_LOGO, "Software", Page::Software),
    (1, icons::DESKTOP, "Computer System", Page::ComputerSystem),
    (1, icons::WINDOWS_LOGO, "Operating System", Page::OperatingSystem),
    (1, icons::KEY, "Microsoft Product Key", Page::ProductKey),
    (1, icons::USER, "User Account", Page::UserAccount),
    (1, icons::FOLDER_OPEN, "System folder", Page::SystemFolder),
    (1, icons::LIST_BULLETS, "Environment", Page::Environment),
    (1, icons::FILE_TEXT, "Installed Codecs", Page::Codecs),
    (1, icons::LIST_CHECKS, "Event Log", Page::EventLog),
    (1, icons::DOWNLOAD, "Hotfix", Page::Hotfix),
    (1, icons::CLIPBOARD_TEXT, "System Report", Page::SystemReport),
];

/// Which collected sections (and which of their rows) a page shows.
fn section_filter(page: Page, s: &Section) -> Option<Section> {
    let keep =
        |rows: Vec<(String, String)>| Some(Section { title: s.title, rows }).filter(|x| !x.rows.is_empty());
    let all = || keep(s.rows.clone());
    let only = |pred: &dyn Fn(&str) -> bool| keep(s.rows.iter().filter(|(k, _)| pred(k)).cloned().collect());
    const SYSTEM_ROWS: [&str; 6] = [
        "System",
        "System family",
        "Computer name",
        "Registered owner",
        "Architecture",
        "Uptime",
    ];
    match (page, s.title) {
        (Page::Hardware, "Operating system") => None,
        (Page::Hardware, _) => all(),
        (Page::Mainboard, "Motherboard & BIOS") => only(&|k| !k.starts_with("BIOS")),
        (Page::Bios, "Motherboard & BIOS") => only(&|k| k.starts_with("BIOS")),
        (Page::Cpu, "Processor") => all(),
        (Page::Memory, "Memory") => all(),
        (Page::Drive, "Storage") => all(),
        (Page::Video, "Graphics") => all(),
        (Page::Network, "Network adapters") => all(),
        (Page::Software | Page::OperatingSystem, "Operating system") => all(),
        (Page::ComputerSystem, "Motherboard & BIOS" | "Operating system") => {
            only(&|k| SYSTEM_ROWS.contains(&k))
        }
        _ => None,
    }
}

fn query_audio() -> anyhow::Result<Table> {
    ps_table(
        r#"@(Get-CimInstance Win32_SoundDevice | Select-Object Name, Manufacturer, Status) | ConvertTo-Json -Compress"#,
        &[
            ("Device", "Name"),
            ("Manufacturer", "Manufacturer"),
            ("Status", "Status"),
        ],
    )
}

fn query_external() -> anyhow::Result<Table> {
    ps_table(
        r#"@(Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'USB*' -or $_.InstanceId -like 'HID*' -or $_.InstanceId -like 'BTHENUM*' -or $_.InstanceId -like 'SWD\PRINTENUM*' } | Sort-Object Class, FriendlyName | Select-Object FriendlyName, Class, Status) | ConvertTo-Json -Compress"#,
        &[
            ("Device", "FriendlyName"),
            ("Class", "Class"),
            ("Status", "Status"),
        ],
    )
}

/// Where Windows keeps this account's and the system's folders.
fn query_folders() -> Table {
    let env = |v: &str| std::env::var(v).unwrap_or_default();
    let profile = env("USERPROFILE");
    let appdata = env("APPDATA");
    let windir = env("WINDIR");
    let rows: Vec<Vec<String>> = [
        ("Desktop", format!(r"{profile}\Desktop")),
        ("Documents", format!(r"{profile}\Documents")),
        ("Downloads", format!(r"{profile}\Downloads")),
        ("Music", format!(r"{profile}\Music")),
        ("Pictures", format!(r"{profile}\Pictures")),
        ("Videos", format!(r"{profile}\Videos")),
        ("User profile", profile.clone()),
        ("AppData (Roaming)", appdata.clone()),
        ("AppData (Local)", env("LOCALAPPDATA")),
        ("Temporary files", env("TEMP")),
        ("Start menu", format!(r"{appdata}\Microsoft\Windows\Start Menu")),
        (
            "Startup",
            format!(r"{appdata}\Microsoft\Windows\Start Menu\Programs\Startup"),
        ),
        ("Program Files", env("ProgramFiles")),
        ("Program Files (x86)", env("ProgramFiles(x86)")),
        ("Program data", env("ProgramData")),
        ("Windows", windir.clone()),
        ("System", format!(r"{windir}\System32")),
        ("Fonts", format!(r"{windir}\Fonts")),
    ]
    .into_iter()
    .map(|(k, v)| vec![k.to_owned(), v])
    .collect();
    Table {
        columns: vec!["Folder", "Location"],
        rows,
    }
}

fn uptime_text() -> String {
    let Ok(s) = System::uptime() else {
        return "unknown".into();
    };
    format!(
        "{}d:{:02}h:{:02}m:{:02}s",
        s / 86_400,
        (s % 86_400) / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

#[derive(Default)]
struct Loaded {
    task: Option<BackgroundTask<Table>>,
    data: Option<Result<Table, String>>,
}

#[derive(Default)]
pub struct SystemInfo {
    page: usize,
    task: Option<BackgroundTask<Vec<Section>>>,
    sections: Option<Result<Vec<Section>, String>>,
    tables: std::collections::HashMap<usize, Loaded>,
    selected_row: Option<usize>,
    reveal_key: bool,
    bench_drive: Option<char>,
    bench_size: u64,
    bench: Option<BackgroundTask<crate::disk_bench::BenchResult>>,
    bench_result: Option<Result<crate::disk_bench::BenchResult, String>>,
    side_task: Option<BackgroundTask<String>>,
    external_ip: Option<String>,
    confirm: Confirm<String>,
    message: Message,
}

impl SystemInfo {
    /// The original's property tree: the collected sections this page shows, each a
    /// foldable group of name / value rows.
    fn sections_view(&mut self, ui: &mut egui::Ui, page: Page) {
        match &self.sections {
            Some(Ok(sections)) => {
                let shown: Vec<Section> = sections.iter().filter_map(|s| section_filter(page, s)).collect();
                if shown.is_empty() {
                    ui.weak("Nothing to show for this item on this PC.");
                }
                for section in &shown {
                    egui::CollapsingHeader::new(egui::RichText::new(section.title).strong())
                        .default_open(true)
                        .show(ui, |ui| {
                            egui::Grid::new(section.title)
                                .num_columns(2)
                                .striped(true)
                                .spacing([24.0, 4.0])
                                .show(ui, |ui| {
                                    for (k, v) in &section.rows {
                                        ui.weak(k);
                                        ui.add(egui::Label::new(v).selectable(true));
                                        ui.end_row();
                                    }
                                });
                        });
                }
                if page == Page::Network {
                    ui.horizontal(|ui| {
                        ui.weak("Public IP address");
                        match &self.external_ip {
                            Some(ip) => {
                                ui.label(ip);
                            }
                            None => {
                                if ui
                                    .small_button("Look up")
                                    .on_hover_text("Asks api.ipify.org which address your connection uses.")
                                    .clicked()
                                {
                                    self.external_ip = Some(match wm_core::net::external_ip() {
                                        Ok(ip) => ip,
                                        Err(e) => format!("not available ({e:#})"),
                                    });
                                }
                            }
                        }
                    });
                }
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {
                ui.spinner();
            }
        }
    }

    /// Shows a background-loaded table for `page`.
    fn table_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut AppCtx<'_>,
        load: fn() -> anyhow::Result<Table>,
    ) -> Option<Table> {
        let page = self.page;
        let entry = self.tables.entry(page).or_default();
        if entry.task.is_none() && entry.data.is_none() {
            entry.task = Some(BackgroundTask::spawn(ui.ctx(), move |_| load()));
        }
        if let Some(t) = &mut entry.task {
            t.poll();
            if let Some(r) = t.take_result() {
                entry.data = Some(r);
                entry.task = None;
            }
        }
        let busy = entry.task.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new(ctx.i18n.tr("common-refresh")))
                .clicked()
            {
                entry.data = None;
                self.selected_row = None;
            }
            if busy {
                ui.spinner();
                ui.label(ctx.i18n.tr("common-loading"));
            }
            if let Some(Ok(t)) = &entry.data {
                ui.weak(format!("{} items", t.rows.len()));
            }
        });
        match &entry.data {
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                None
            }
            Some(Ok(t)) => Some(t.clone()),
            None => None,
        }
    }

    fn draw_table(&mut self, ui: &mut egui::Ui, t: &Table, selectable: bool) {
        egui::Grid::new(("table", self.page))
            .striped(true)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for c in &t.columns {
                    ui.strong(*c);
                }
                ui.end_row();
                for (i, row) in t.rows.iter().enumerate() {
                    for (k, cell) in row.iter().enumerate() {
                        if k == 0 && selectable {
                            if ui.selectable_label(self.selected_row == Some(i), cell).clicked() {
                                self.selected_row = Some(i);
                            }
                        } else {
                            ui.label(cell);
                        }
                    }
                    ui.end_row();
                }
            });
    }

    fn licence(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(mut t) = self.table_page(ui, ctx, query_software) else {
            return;
        };
        ui.checkbox(&mut self.reveal_key, "Show the firmware product key");
        if !self.reveal_key {
            for r in &mut t.rows {
                if r[0] == "Key stored in firmware" && r[1].len() > 5 {
                    let tail = r[1][r[1].len() - 5..].to_owned();
                    r[1] = format!("XXXXX-XXXXX-XXXXX-XXXXX-{tail}");
                }
            }
        }
        self.draw_table(ui, &t, false);
        ui.add_space(6.0);
        if ui
            .add_enabled(
                ctx.elevated && self.side_task.is_none(),
                egui::Button::new("Run the Windows Experience assessment"),
            )
            .on_hover_text("Measures CPU, memory, graphics and disk (takes a few minutes).")
            .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"))
            .clicked()
        {
            self.side_task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                wm_core::process::run_checked(wm_core::process::hidden_command("winsat").arg("formal"))?;
                Ok("Assessment finished. Refresh to see the scores.".to_owned())
            }));
        }
    }

    fn updates(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(t) = self.table_page(ui, ctx, query_updates) else {
            return;
        };
        ui.horizontal(|ui| {
            let kb = self
                .selected_row
                .and_then(|i| t.rows.get(i))
                .map(|r| r[0].clone());
            if ui
                .add_enabled(
                    kb.is_some() && ctx.elevated && self.side_task.is_none(),
                    egui::Button::new(format!("{}  Uninstall update", icons::TRASH)),
                )
                .on_disabled_hover_text("Select an update (administrator rights needed)")
                .clicked()
                && let Some(kb) = kb
            {
                self.confirm.ask(
                    format!(
                        "Uninstall {kb}? Some updates can't be removed, and Windows may reinstall it later."
                    ),
                    kb,
                );
            }
            if ui.button("Update history…").clicked() {
                let _ = std::process::Command::new("explorer.exe")
                    .arg("ms-settings:windowsupdate-history")
                    .spawn();
            }
        });
        self.draw_table(ui, &t, true);
    }

    fn speed_test(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label("Measures how fast a drive reads and writes, bypassing Windows' cache. A temporary file is written and deleted afterwards.");
        ui.add_space(6.0);
        let drives: Vec<char> = sysinfo::Disks::new_with_refreshed_list()
            .map(|d| {
                d.iter()
                    .filter_map(|x| x.mount_point().to_string_lossy().chars().next())
                    .collect()
            })
            .unwrap_or_default();
        let drive = *self
            .bench_drive
            .get_or_insert(drives.first().copied().unwrap_or('C'));
        if self.bench_size == 0 {
            self.bench_size = 512;
        }
        let running = self.bench.is_some();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!running, |ui| {
                egui::ComboBox::from_id_salt("bench_drive")
                    .selected_text(format!("{drive}:"))
                    .show_ui(ui, |ui| {
                        for d in &drives {
                            ui.selectable_value(&mut self.bench_drive, Some(*d), format!("{d}:"));
                        }
                    });
                egui::ComboBox::from_id_salt("bench_size")
                    .selected_text(format!("{} MB", self.bench_size))
                    .show_ui(ui, |ui| {
                        for s in [256, 512, 1024, 2048] {
                            ui.selectable_value(&mut self.bench_size, s, format!("{s} MB"));
                        }
                    });
            });
            if !running && ui.button(format!("{}  Start", icons::PLAY)).clicked() {
                let (path, size) = (crate::disk_bench::test_file(drive), self.bench_size);
                self.bench_result = None;
                self.bench = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    crate::disk_bench::run(&path, size, 4.0, Some(p))
                }));
            }
            if running
                && ui.button(ctx.i18n.tr("common-cancel")).clicked()
                && let Some(b) = &self.bench
            {
                b.cancel();
            }
        });
        if let Some(b) = &mut self.bench {
            b.poll();
            ui.add(egui::ProgressBar::new(b.progress).text(b.status.clone()));
            if let Some(r) = b.take_result() {
                self.bench_result = Some(r);
                self.bench = None;
            }
        }
        match &self.bench_result {
            Some(Ok(r)) => {
                egui::Grid::new("bench")
                    .striped(true)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        ui.strong("Test");
                        ui.strong("Read");
                        ui.strong("Write");
                        ui.end_row();
                        ui.label("Sequential (1 MB blocks)");
                        ui.label(format!("{:.0} MB/s", r.seq_read_mbs));
                        ui.label(format!("{:.0} MB/s", r.seq_write_mbs));
                        ui.end_row();
                        ui.label("Random 4 KB (queue depth 1)");
                        ui.label(format!(
                            "{:.1} MB/s · {:.0} IOPS",
                            r.rnd_read_mbs(),
                            r.rnd_read_iops
                        ));
                        ui.label(format!(
                            "{:.1} MB/s · {:.0} IOPS",
                            r.rnd_write_mbs(),
                            r.rnd_write_iops
                        ));
                        ui.end_row();
                    });
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }
    }

    fn reports(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.label(
            "Windows' own diagnostic reports. They are saved to your Documents folder and opened when ready.",
        );
        ui.add_space(6.0);
        let docs = dirs_documents();
        let busy = self.side_task.is_some();
        egui::Grid::new("reports")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("Battery report"))
                    .clicked()
                {
                    let out = docs.join("battery-report.html");
                    self.side_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                        wm_core::process::run_checked(
                            wm_core::process::hidden_command("powercfg")
                                .args(["/batteryreport", "/output"])
                                .arg(&out),
                        )?;
                        let _ = std::process::Command::new("explorer").arg(&out).spawn();
                        Ok(format!("Saved {}.", out.display()))
                    }));
                }
                ui.weak("Battery capacity and wear over time (laptops).");
                ui.end_row();
                if ui
                    .add_enabled(
                        !busy && ctx.elevated,
                        egui::Button::new("Energy efficiency report"),
                    )
                    .on_disabled_hover_text(ctx.i18n.tr("shell-needs-admin"))
                    .clicked()
                {
                    let out = docs.join("energy-report.html");
                    self.side_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                        // powercfg /energy exits non-zero when it finds problems; the report is still written.
                        let _ = wm_core::process::hidden_command("powercfg")
                            .args(["/energy", "/duration", "30", "/output"])
                            .arg(&out)
                            .output();
                        anyhow::ensure!(out.exists(), "the report was not created");
                        let _ = std::process::Command::new("explorer").arg(&out).spawn();
                        Ok(format!("Saved {}.", out.display()))
                    }));
                }
                ui.weak("Observes the PC for 30 seconds and lists power problems.");
                ui.end_row();
                if ui.add_enabled(!busy, egui::Button::new("Sleep study")).clicked() {
                    let out = docs.join("sleep-study.html");
                    self.side_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                        wm_core::process::run_checked(
                            wm_core::process::hidden_command("powercfg")
                                .args(["/sleepstudy", "/output"])
                                .arg(&out),
                        )?;
                        let _ = std::process::Command::new("explorer").arg(&out).spawn();
                        Ok(format!("Saved {}.", out.display()))
                    }));
                }
                ui.weak("What used power while the PC was in Modern Standby.");
                ui.end_row();
                if ui.button("Performance report…").clicked() {
                    let _ = std::process::Command::new("perfmon").arg("/report").spawn();
                }
                ui.weak("Windows' 60-second system diagnostics report.");
                ui.end_row();
                if ui.button("System Information (msinfo32)…").clicked() {
                    let _ = std::process::Command::new("msinfo32").spawn();
                }
                ui.weak("Windows' complete hardware and software inventory.");
                ui.end_row();
            });
    }
}

fn dirs_documents() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|p| std::path::PathBuf::from(p).join("Documents"))
        .unwrap_or_else(std::env::temp_dir)
}

impl Tool for SystemInfo {
    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn id(&self) -> &'static str {
        "sysinfo"
    }

    fn category(&self) -> Category {
        Category::Information
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(kb) = self.confirm.show(ui.ctx(), ctx.i18n) {
            let digits: String = kb.chars().filter(char::is_ascii_digit).collect();
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let _ = ctx.journal.record(
                    "sysinfo",
                    format!("Requested removal of update {kb}"),
                    UndoAction::None,
                );
                self.side_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                    wm_core::process::run_checked(wm_core::process::hidden_command("wusa.exe").args([
                        "/uninstall".to_owned(),
                        format!("/kb:{digits}"),
                        "/quiet".into(),
                        "/norestart".into(),
                    ]))?;
                    Ok(format!("Removed {kb}. Restart to finish."))
                }));
            }
        }
        if let Some(t) = &mut self.side_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.message = Some(r);
                self.side_task = None;
                self.tables.remove(&self.page);
            }
        }
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

        let loading = self.task.is_some();
        let have = matches!(self.sections, Some(Ok(_)));
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(!loading)),
                None,
                Some(ToolButton::new(icons::COPY, "Copy").enabled(have)),
                Some(ToolButton::new(icons::ARROW_SQUARE_OUT, "Export").enabled(have)),
            ],
            |_| {},
        ) {
            Some(0) => {
                self.sections = None;
                self.tables.clear();
                self.message = None;
            }
            Some(2) => {
                if let Some(Ok(sections)) = &self.sections {
                    ui.ctx().copy_text(to_text(sections));
                    self.message = Some(Ok(ctx.i18n.tr("common-copied")));
                }
            }
            Some(3) => {
                if let Some(Ok(sections)) = &self.sections
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Text", &["txt"])
                        .set_file_name("system-information.txt")
                        .save_file()
                {
                    self.message = Some(
                        std::fs::write(&path, to_text(sections))
                            .map(|()| format!("Saved {}.", path.display()))
                            .map_err(|e| e.to_string()),
                    );
                }
            }
            _ => {}
        }
        // Like the original: the current date and how long Windows has been running.
        let now = chrono::Local::now().format("%A, %B %-d, %Y %-I:%M:%S %p");
        frame::status(
            ui,
            &format!("Current Date: {now} | System Uptime: {}", uptime_text()),
            |ui| {
                if loading || self.side_task.is_some() {
                    ui.spinner();
                }
            },
        );
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));

        // The Hardware / Software tree.
        let mut picked = None;
        frame::left(ui, 210.0, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, (depth, icon, label, _)) in NAV.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_space(*depth as f32 * 16.0);
                    let text = if *depth == 0 {
                        egui::RichText::new(format!("{icon}  {label}")).strong()
                    } else {
                        egui::RichText::new(format!("{icon}  {label}"))
                    };
                    if ui.selectable_label(self.page == i, text).clicked() {
                        picked = Some(i);
                    }
                });
            }
        });
        if let Some(i) = picked
            && i != self.page
        {
            self.page = i;
            self.selected_row = None;
            self.message = None;
        }

        let page = NAV[self.page].3;
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            match page {
                Page::HddReport => {
                    if !ctx.elevated {
                        ui.weak("Temperature, wear and error counters need administrator rights.");
                    }
                    if let Some(t) = self.table_page(ui, ctx, query_disks) {
                        self.draw_table(ui, &t, false);
                    }
                }
                Page::DiskBenchmark => self.speed_test(ui, ctx),
                Page::Audio => {
                    if let Some(t) = self.table_page(ui, ctx, query_audio) {
                        self.draw_table(ui, &t, false);
                    }
                }
                Page::ExternalDevice => {
                    if let Some(t) = self.table_page(ui, ctx, query_external) {
                        self.draw_table(ui, &t, false);
                    }
                }
                Page::Printers => {
                    if let Some(t) = self.table_page(ui, ctx, query_printers) {
                        self.draw_table(ui, &t, false);
                    }
                    if ui.button("Printers & scanners…").clicked() {
                        let _ = std::process::Command::new("explorer.exe")
                            .arg("ms-settings:printers")
                            .spawn();
                    }
                }
                Page::ProductKey => self.licence(ui, ctx),
                Page::UserAccount => {
                    if let Some(t) = self.table_page(ui, ctx, query_users) {
                        self.draw_table(ui, &t, false);
                    }
                }
                Page::SystemFolder => {
                    let t = query_folders();
                    self.draw_table(ui, &t, false);
                }
                Page::Environment => {
                    let t = query_environment();
                    self.draw_table(ui, &t, false);
                }
                Page::Codecs => {
                    if let Some(t) = self.table_page(ui, ctx, query_codecs) {
                        self.draw_table(ui, &t, false);
                    }
                }
                Page::EventLog => {
                    ui.label("Windows' logs of errors, warnings and events.");
                    ui.add_space(6.0);
                    if ui
                        .button(format!("{}  Open Event Viewer", icons::LIST_CHECKS))
                        .clicked()
                    {
                        wm_core::ui::request_open_tool(ui.ctx(), "events");
                    }
                }
                Page::Hotfix => self.updates(ui, ctx),
                Page::SystemReport => self.reports(ui, ctx),
                other => self.sections_view(ui, other),
            }
        });
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
    fn pages_pick_their_sections() {
        let rows = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
        };
        let board = Section {
            title: "Motherboard & BIOS",
            rows: rows(&[("System", "X"), ("Motherboard", "Y"), ("BIOS version", "1.0")]),
        };
        let os = Section {
            title: "Operating system",
            rows: rows(&[("Edition", "Pro"), ("Computer name", "PC")]),
        };
        let keys = |s: Option<Section>| -> Vec<String> {
            s.map(|s| s.rows.into_iter().map(|r| r.0).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            keys(section_filter(Page::Mainboard, &board)),
            ["System", "Motherboard"]
        );
        assert_eq!(keys(section_filter(Page::Bios, &board)), ["BIOS version"]);
        assert!(section_filter(Page::Hardware, &os).is_none());
        assert_eq!(keys(section_filter(Page::ComputerSystem, &os)), ["Computer name"]);
        assert_eq!(keys(section_filter(Page::ComputerSystem, &board)), ["System"]);
        assert!(section_filter(Page::Cpu, &board).is_none());
        assert_eq!(
            NAV.iter().filter(|n| n.0 == 0).count(),
            2,
            "Hardware and Software"
        );
        assert!(uptime_text().contains("d:"));
        assert!(query_folders().rows.len() > 10);
    }

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
    fn every_query_runs() {
        let sw = query_software().unwrap();
        assert!(sw.rows.iter().any(|r| r[0] == "Activation"), "{sw:?}");
        let users = query_users().unwrap();
        assert!(!users.rows.is_empty());
        assert_eq!(users.columns.len(), users.rows[0].len());
        query_updates().unwrap();
        query_printers().unwrap();
        query_disks().unwrap();
        assert!(!query_codecs().unwrap().rows.is_empty());
        assert!(
            query_environment()
                .rows
                .iter()
                .any(|r| r[0].eq_ignore_ascii_case("PATH"))
        );
        assert_eq!(licence_status(1), "Activated");
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
