//! Hardware Monitor: live CPU, memory, disk and network charts plus temperature sensors.
//!
//! Temperatures come from ACPI thermal zones via sysinfo, NVIDIA cards via NVML and
//! other graphics adapters via Windows (see [`crate::gpu_counters`]); many desktop boards expose
//! none without a vendor kernel driver, in which case the section explains that.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::gpu_counters;
use sysinfo::{Components, Disks, Networks, System};
use wm_core::{
    AppCtx, Category, Tool,
    egui::{self, Color32, Vec2},
    task::Sampler,
    ui::{format_bytes, sparkline},
};

const HISTORY: usize = 120;
const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub cpu: f32,
    /// Effective clock (MHz), including turbo boost.
    pub cpu_mhz: Option<f32>,
    pub per_core: Vec<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// Bytes per second.
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk_read: f64,
    pub disk_write: f64,
    pub temps: Vec<(String, f32)>,
    pub gpus: Vec<GpuSample>,
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone, Default)]
pub struct GpuSample {
    pub name: String,
    pub temp: Option<f32>,
    pub util: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub power_w: Option<f32>,
    pub fan: Option<f32>,
    pub clock_mhz: Option<f32>,
    pub fan_rpm: Option<f32>,
    /// Power draw as a percentage of the maximum (adapters without NVML).
    pub power_pct: Option<f32>,
}

/// NVIDIA GPUs through NVML (the driver's own library; absent on other vendors).
fn sample_gpus(nvml: &nvml_wrapper::Nvml) -> Vec<GpuSample> {
    use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
    let n = nvml.device_count().unwrap_or(0);
    (0..n)
        .filter_map(|i| nvml.device_by_index(i).ok())
        .map(|d| {
            let mem = d.memory_info().ok();
            GpuSample {
                name: d.name().unwrap_or_else(|_| "NVIDIA GPU".into()),
                temp: d.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f32),
                util: d.utilization_rates().ok().map(|u| u.gpu as f32),
                mem_used: mem.as_ref().map_or(0, |m| m.used),
                mem_total: mem.as_ref().map_or(0, |m| m.total),
                power_w: d.power_usage().ok().map(|mw| mw as f32 / 1000.0),
                fan: d.fan_speed(0).ok().map(|f| f as f32),
                clock_mhz: d.clock_info(Clock::Graphics).ok().map(|c| c as f32),
                ..Default::default()
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Battery {
    pub percent: u8,
    pub on_ac: bool,
    pub charging: bool,
}

fn battery() -> Option<Battery> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.ok()?;
    // 128 = no system battery, 255 = unknown.
    if s.BatteryFlag & 128 != 0 || s.BatteryFlag == 255 || s.BatteryLifePercent > 100 {
        return None;
    }
    Some(Battery {
        percent: s.BatteryLifePercent,
        on_ac: s.ACLineStatus == 1,
        charging: s.BatteryFlag & 8 != 0,
    })
}

struct State {
    sys: System,
    nets: Option<Networks>,
    disks: Option<Disks>,
    comps: Option<Components>,
    nvml: Option<nvml_wrapper::Nvml>,
    adapters: Vec<gpu_counters::Adapter>,
    counters: Option<gpu_counters::Counters>,
    last: Instant,
}

fn init() -> State {
    let mut sys = System::new().expect("sysinfo System");
    sys.refresh_cpu_usage();
    State {
        sys,
        nets: Networks::new_with_refreshed_list().ok(),
        disks: Disks::new_with_refreshed_list().ok(),
        comps: Components::new_with_refreshed_list().ok(),
        nvml: nvml_wrapper::Nvml::init().ok(),
        adapters: gpu_counters::adapters(),
        counters: gpu_counters::Counters::open(&[
            gpu_counters::GPU_ENGINE,
            gpu_counters::GPU_MEMORY,
            gpu_counters::CPU_PERFORMANCE,
            gpu_counters::CPU_FREQUENCY,
        ]),
        last: Instant::now(),
    }
}

/// Converts a byte delta over `secs` into a rate, guarding against zero intervals.
pub fn rate(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 { 0.0 } else { bytes as f64 / secs }
}

fn sample(s: &mut State) -> Sample {
    let secs = s.last.elapsed().as_secs_f64();
    s.last = Instant::now();
    s.sys.refresh_cpu_usage();
    s.sys.refresh_memory();

    let (mut rx, mut tx) = (0, 0);
    if let Some(nets) = &mut s.nets {
        nets.refresh(true);
        for data in nets.values() {
            rx += data.received();
            tx += data.transmitted();
        }
    }
    let (mut read, mut write) = (0, 0);
    if let Some(disks) = &mut s.disks {
        disks.refresh(true);
        for d in disks.iter() {
            let u = d.usage();
            read += u.read_bytes;
            write += u.written_bytes;
        }
    }
    let mut temps = Vec::new();
    if let Some(comps) = &mut s.comps {
        comps.refresh(true);
        for c in comps.iter() {
            if let Some(t) = c.temperature().filter(|t| t.is_finite() && *t > 0.0) {
                temps.push((c.label().to_owned(), t));
            }
        }
    }
    let mut gpus = s.nvml.as_ref().map(sample_gpus).unwrap_or_default();
    let nvml_covers = !gpus.is_empty();
    let mut cpu_mhz = None;
    if let Some(c) = &s.counters {
        c.collect();
        cpu_mhz = gpu_counters::cpu_mhz(c, 2, 3);
        let load = gpu_counters::gpu_load(&c.values(0));
        let mem = gpu_counters::gpu_memory(&c.values(1));
        for a in &s.adapters {
            // NVML reports NVIDIA cards in more detail.
            if nvml_covers && a.name.contains("NVIDIA") {
                continue;
            }
            let perf = a.perf();
            gpus.push(GpuSample {
                name: a.name.clone(),
                temp: perf.temp_c,
                util: load.get(&a.luid).copied(),
                mem_used: mem.get(&a.luid).copied().unwrap_or(0),
                mem_total: a.dedicated_memory,
                fan_rpm: perf.fan_rpm,
                power_pct: perf.power_pct,
                ..Default::default()
            });
        }
    }
    Sample {
        cpu: s.sys.global_cpu_usage(),
        cpu_mhz,
        per_core: s.sys.cpus().iter().map(|c| c.usage()).collect(),
        mem_used: s.sys.used_memory(),
        mem_total: s.sys.total_memory(),
        swap_used: s.sys.used_swap(),
        swap_total: s.sys.total_swap(),
        net_rx: rate(rx, secs),
        net_tx: rate(tx, secs),
        disk_read: rate(read, secs),
        disk_write: rate(write, secs),
        temps,
        gpus,
        battery: battery(),
    }
}

#[derive(Default)]
pub struct HardwareMonitor {
    sampler: Option<Sampler<Sample>>,
    history: VecDeque<Sample>,
    sensors: std::collections::BTreeMap<String, Sensor>,
    /// Sensor whose chart is shown.
    selected: Option<String>,
    log: Option<SensorLog>,
    log_error: Option<String>,
    alarm_on: bool,
    alarm_c: f32,
    last_alarm: Option<Instant>,
    alarm_text: Option<String>,
    disk_task: Option<wm_core::task::BackgroundTask<Vec<(String, f32)>>>,
    disk_temps: Vec<(String, f32)>,
    disk_checked: Option<Instant>,
}

/// SMART disk temperatures (needs administrator rights; slow, so polled once a minute).
/// One sensor's readings since the monitor opened (or since Reset).
#[derive(Debug, Clone)]
pub struct Sensor {
    pub current: f32,
    pub min: f32,
    pub max: f32,
    sum: f64,
    count: u32,
    pub unit: &'static str,
    pub history: VecDeque<f32>,
}

impl Sensor {
    fn new(value: f32, unit: &'static str) -> Self {
        Self {
            current: value,
            min: value,
            max: value,
            sum: 0.0,
            count: 0,
            unit,
            history: VecDeque::with_capacity(HISTORY),
        }
    }

    fn add(&mut self, value: f32) {
        self.current = value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += f64::from(value);
        self.count += 1;
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(value);
    }

    pub fn average(&self) -> f32 {
        if self.count == 0 {
            self.current
        } else {
            (self.sum / f64::from(self.count)) as f32
        }
    }
}

/// Number format for a unit (clocks and fan speeds need no decimals).
fn fmt_value(v: f32, unit: &str) -> String {
    match unit {
        "MHz" | "RPM" => format!("{v:.0} {unit}"),
        _ => format!("{v:.1} {unit}"),
    }
}

/// Writes every sample's sensor values to a CSV file, one column per sensor that
/// existed when logging started.
pub struct SensorLog {
    pub path: std::path::PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    columns: Vec<String>,
    pub rows: u64,
}

impl SensorLog {
    fn start(sensors: &std::collections::BTreeMap<String, Sensor>) -> anyhow::Result<Self> {
        use std::io::Write;
        let path = wm_core::paths::logs_dir()?.join(format!(
            "hardware-{}.csv",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        let columns: Vec<String> = sensors.keys().cloned().collect();
        let header: Vec<String> = std::iter::once("Time".to_owned())
            .chain(
                sensors
                    .iter()
                    .map(|(n, s)| csv_field(&format!("{n} ({})", s.unit))),
            )
            .collect();
        writeln!(file, "{}", header.join(","))?;
        Ok(Self {
            path,
            file,
            columns,
            rows: 0,
        })
    }

    fn write(&mut self, sensors: &std::collections::BTreeMap<String, Sensor>) -> std::io::Result<()> {
        use std::io::Write;
        let mut line = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        for c in &self.columns {
            line.push(',');
            if let Some(s) = sensors.get(c) {
                line += &format!("{:.1}", s.current);
            }
        }
        writeln!(self.file, "{line}")?;
        self.rows += 1;
        if self.rows.is_multiple_of(10) {
            self.file.flush()?;
        }
        Ok(())
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn disk_temperatures() -> anyhow::Result<Vec<(String, f32)>> {
    let list = wm_core::process::json_list(wm_core::process::powershell_json(
        r#"@(Get-PhysicalDisk | ForEach-Object { $r = $_ | Get-StorageReliabilityCounter -ErrorAction SilentlyContinue
  if ($r.Temperature) { [pscustomobject]@{ Name = $_.FriendlyName; T = $r.Temperature } } }) | ConvertTo-Json -Compress"#,
    )?);
    Ok(list
        .iter()
        .filter_map(|o| Some((format!("Disk: {}", o["Name"].as_str()?), o["T"].as_f64()? as f32)))
        .collect())
}

impl HardwareMonitor {
    fn sensor_log_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut on = self.log.is_some();
            if ui
                .checkbox(&mut on, "Log every reading to a file")
                .on_hover_text("One CSV row per second, with a column per sensor. Open it in a spreadsheet.")
                .changed()
            {
                self.log_error = None;
                self.log = None;
                if on {
                    match SensorLog::start(&self.sensors) {
                        Ok(l) => self.log = Some(l),
                        Err(e) => self.log_error = Some(format!("Could not start the log: {e:#}")),
                    }
                }
            }
            if let Some(l) = &self.log {
                ui.weak(format!("{} rows", l.rows));
                if ui.link("Show the file").clicked() {
                    wm_core::shell::reveal_in_explorer(&l.path);
                }
            }
        });
        if let Some(e) = &self.log_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
    }

    fn record(&mut self, name: String, value: f32, unit: &'static str) {
        self.sensors
            .entry(name)
            .or_insert_with(|| Sensor::new(value, unit))
            .add(value);
    }

    fn update_sensors(&mut self, s: &Sample) {
        self.record("CPU usage".into(), s.cpu, "%");
        if let Some(mhz) = s.cpu_mhz {
            self.record("CPU clock".into(), mhz, "MHz");
        }
        self.record(
            "Memory usage".into(),
            s.mem_used as f32 * 100.0 / s.mem_total.max(1) as f32,
            "%",
        );
        for (label, t) in &s.temps {
            self.record(label.clone(), *t, "°C");
        }
        for (label, t) in self.disk_temps.clone() {
            self.record(label, t, "°C");
        }
        for g in &s.gpus {
            let pairs = [
                ("temperature", g.temp, "°C"),
                ("load", g.util, "%"),
                ("power", g.power_w, "W"),
                ("fan", g.fan, "%"),
                ("fan speed", g.fan_rpm, "RPM"),
                ("power draw", g.power_pct, "%"),
                ("clock", g.clock_mhz, "MHz"),
            ];
            for (what, v, unit) in pairs {
                if let Some(v) = v {
                    self.record(format!("{} {what}", g.name), v, unit);
                }
            }
            if g.mem_total > 0 {
                self.record(
                    format!("{} memory", g.name),
                    g.mem_used as f32 * 100.0 / g.mem_total as f32,
                    "%",
                );
            }
        }
        if let Some(b) = s.battery {
            self.record("Battery charge".into(), b.percent as f32, "%");
        }
    }

    /// Hottest temperature above the alarm threshold, if any.
    fn check_alarm(&mut self) {
        if !self.alarm_on {
            self.alarm_text = None;
            return;
        }
        let hot = self
            .sensors
            .iter()
            .filter(|(_, s)| s.unit == "°C" && s.current >= self.alarm_c)
            .max_by(|a, b| a.1.current.total_cmp(&b.1.current))
            .map(|(name, s)| {
                format!(
                    "{name} is at {:.0} °C (alarm at {:.0} °C)",
                    s.current, self.alarm_c
                )
            });
        if hot.is_some()
            && self
                .last_alarm
                .is_none_or(|t| t.elapsed() > Duration::from_secs(60))
        {
            self.last_alarm = Some(Instant::now());
            unsafe {
                let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(
                    windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
                );
            }
        }
        self.alarm_text = hot;
    }
}

fn series(history: &VecDeque<Sample>, f: impl Fn(&Sample) -> f32) -> Vec<f32> {
    history.iter().map(f).collect()
}

/// Upper bound for auto-scaled charts (at least `floor`).
fn auto_max(values: &[f32], floor: f32) -> f32 {
    values.iter().copied().fold(floor, f32::max) * 1.1
}

fn card(ui: &mut egui::Ui, title: &str, value: String, values: &[f32], max: f32, color: Color32) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.strong(title);
            ui.label(value);
        });
        sparkline(ui, values, max, Vec2::new(340.0, 70.0), color);
    });
}

impl Tool for HardwareMonitor {
    fn id(&self) -> &'static str {
        "hwmon"
    }

    fn category(&self) -> Category {
        Category::Information
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let sampler = self
            .sampler
            .get_or_insert_with(|| Sampler::spawn(ui.ctx(), INTERVAL, init, sample));
        let fresh = sampler.drain();
        for s in fresh {
            if self.history.len() == HISTORY {
                self.history.pop_front();
            }
            // Statistics, alarm and log advance once per sample, not per frame.
            self.update_sensors(&s);
            self.check_alarm();
            if let Some(log) = &mut self.log
                && let Err(e) = log.write(&self.sensors)
            {
                self.log_error = Some(format!("Logging stopped: {e}"));
                self.log = None;
            }
            self.history.push_back(s);
        }
        let Some(now) = self.history.back().cloned() else {
            ui.spinner();
            ui.label(ctx.i18n.tr("common-loading"));
            return;
        };

        let accent = ui.visuals().selection.bg_fill;
        let warm = Color32::from_rgb(230, 140, 60);
        egui::Grid::new("hwmon_cards")
            .num_columns(2)
            .spacing([24.0, 16.0])
            .show(ui, |ui| {
                card(
                    ui,
                    "CPU",
                    match now.cpu_mhz {
                        Some(m) => format!("{:.0}% · {:.2} GHz", now.cpu, m / 1000.0),
                        None => format!("{:.0}%", now.cpu),
                    },
                    &series(&self.history, |s| s.cpu),
                    100.0,
                    accent,
                );
                let mem_pct = |s: &Sample| s.mem_used as f32 * 100.0 / s.mem_total.max(1) as f32;
                card(
                    ui,
                    "Memory",
                    format!(
                        "{} / {} ({:.0}%)",
                        format_bytes(now.mem_used),
                        format_bytes(now.mem_total),
                        mem_pct(&now)
                    ),
                    &series(&self.history, mem_pct),
                    100.0,
                    accent,
                );
                ui.end_row();

                let rx = series(&self.history, |s| s.net_rx as f32);
                let tx = series(&self.history, |s| s.net_tx as f32);
                card(
                    ui,
                    "Network ↓",
                    format!("{}/s", format_bytes(now.net_rx as u64)),
                    &rx,
                    auto_max(&rx, 1024.0),
                    accent,
                );
                card(
                    ui,
                    "Network ↑",
                    format!("{}/s", format_bytes(now.net_tx as u64)),
                    &tx,
                    auto_max(&tx, 1024.0),
                    warm,
                );
                ui.end_row();

                let dr = series(&self.history, |s| s.disk_read as f32);
                let dw = series(&self.history, |s| s.disk_write as f32);
                card(
                    ui,
                    "Disk read",
                    format!("{}/s", format_bytes(now.disk_read as u64)),
                    &dr,
                    auto_max(&dr, 1024.0 * 1024.0),
                    accent,
                );
                card(
                    ui,
                    "Disk write",
                    format!("{}/s", format_bytes(now.disk_write as u64)),
                    &dw,
                    auto_max(&dw, 1024.0 * 1024.0),
                    warm,
                );
                ui.end_row();
            });

        ui.add_space(12.0);
        if now.swap_total > 0 {
            ui.label(format!(
                "Page file: {} / {}",
                format_bytes(now.swap_used),
                format_bytes(now.swap_total)
            ));
        }

        egui::CollapsingHeader::new(format!("CPU cores ({})", now.per_core.len()))
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (i, usage) in now.per_core.iter().enumerate() {
                        ui.add(
                            egui::ProgressBar::new(usage / 100.0)
                                .desired_width(120.0)
                                .text(format!("#{i} {usage:.0}%")),
                        );
                    }
                });
            });

        if self.alarm_c == 0.0 {
            self.alarm_c = 85.0;
        }
        // SMART disk temperatures, once a minute (administrator only).
        if ctx.elevated
            && self.disk_task.is_none()
            && self
                .disk_checked
                .is_none_or(|t| t.elapsed() > Duration::from_secs(60))
        {
            self.disk_checked = Some(Instant::now());
            self.disk_task = Some(wm_core::task::BackgroundTask::spawn(ui.ctx(), |_| {
                disk_temperatures()
            }));
        }
        if let Some(t) = &mut self.disk_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.disk_temps = r.unwrap_or_default();
                self.disk_task = None;
            }
        }

        if let Some(b) = now.battery {
            ui.label(format!(
                "Battery: {}%{}",
                b.percent,
                if b.charging {
                    ", charging"
                } else if b.on_ac {
                    ", plugged in"
                } else {
                    ", on battery"
                }
            ));
        }
        for g in &now.gpus {
            egui::CollapsingHeader::new(&g.name)
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if let Some(u) = g.util {
                            ui.add(
                                egui::ProgressBar::new(u / 100.0)
                                    .desired_width(160.0)
                                    .text(format!("Load {u:.0}%")),
                            );
                        }
                        if g.mem_total > 0 {
                            ui.add(
                                egui::ProgressBar::new(g.mem_used as f32 / g.mem_total as f32)
                                    .desired_width(220.0)
                                    .text(format!(
                                        "Memory {} / {}",
                                        format_bytes(g.mem_used),
                                        format_bytes(g.mem_total)
                                    )),
                            );
                        }
                        if let Some(t) = g.temp {
                            ui.label(format!("{t:.0} °C"));
                        }
                        if let Some(p) = g.power_w {
                            ui.label(format!("{p:.0} W"));
                        }
                        if let Some(f) = g.fan {
                            ui.label(format!("fan {f:.0}%"));
                        }
                        if let Some(r) = g.fan_rpm {
                            ui.label(format!("fan {r:.0} RPM"));
                        }
                        if let Some(p) = g.power_pct {
                            ui.label(format!("power {p:.0}%"));
                        }
                    });
                });
        }

        egui::CollapsingHeader::new("Sensors").default_open(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.alarm_on, "Warn me when a temperature reaches");
                ui.add(egui::DragValue::new(&mut self.alarm_c).range(40.0..=120.0).suffix(" °C"));
                if ui.small_button("Reset statistics").clicked() {
                    self.sensors.clear();
                }
            });
            if let Some(text) = &self.alarm_text {
                ui.colored_label(ui.visuals().error_fg_color, format!("{}  {text}", wm_core::icons::WARNING));
            }
            self.sensor_log_controls(ui);
            if let Some(name) = self.selected.clone() {
                match self.sensors.get(&name) {
                    Some(s) => {
                        let values: Vec<f32> = s.history.iter().copied().collect();
                        let max = match s.unit {
                            "%" => 100.0,
                            _ => auto_max(&values, 1.0),
                        };
                        ui.horizontal(|ui| {
                            ui.strong(&name);
                            ui.label(fmt_value(s.current, s.unit));
                            ui.weak(format!("(chart top: {})", fmt_value(max, s.unit)));
                        });
                        sparkline(
                            ui,
                            &values,
                            max,
                            Vec2::new(ui.available_width().min(700.0), 80.0),
                            ui.visuals().selection.bg_fill,
                        );
                    }
                    None => self.selected = None,
                }
            } else {
                ui.weak("Click a sensor to chart it.");
            }
            egui::Grid::new("sensors").num_columns(5).striped(true).spacing([24.0, 4.0]).show(ui, |ui| {
                ui.strong("Sensor");
                ui.strong("Current");
                ui.strong("Minimum");
                ui.strong("Average");
                ui.strong("Maximum");
                ui.end_row();
                let mut clicked = None;
                for (name, s) in &self.sensors {
                    let sel = self.selected.as_deref() == Some(name.as_str());
                    if ui.selectable_label(sel, name).clicked() {
                        clicked = Some(name.clone());
                    }
                    ui.label(fmt_value(s.current, s.unit));
                    ui.weak(fmt_value(s.min, s.unit));
                    ui.weak(fmt_value(s.average(), s.unit));
                    ui.weak(fmt_value(s.max, s.unit));
                    ui.end_row();
                }
                if let Some(c) = clicked {
                    self.selected = (self.selected.as_ref() != Some(&c)).then_some(c);
                }
            });
            if now.temps.is_empty() && now.gpus.is_empty() && self.disk_temps.is_empty() {
                ui.weak(
                    "No temperature sensors are available without a kernel driver. Disk temperatures appear \
                     when running as administrator; NVIDIA graphics cards report theirs directly.",
                );
            }
            ui.weak("Voltages, fan speeds and CPU package power need a kernel driver, which this app doesn't install.");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_and_scale() {
        assert_eq!(rate(1000, 2.0), 500.0);
        assert_eq!(rate(1000, 0.0), 0.0);
        assert!((auto_max(&[10.0, 50.0], 1.0) - 55.0).abs() < 1e-4);
        assert!((auto_max(&[], 100.0) - 110.0).abs() < 1e-4);
    }

    #[test]
    fn sensor_statistics_and_log() {
        let mut s = Sensor::new(10.0, "°C");
        for v in [10.0, 20.0, 30.0] {
            s.add(v);
        }
        assert_eq!((s.min, s.max, s.current), (10.0, 30.0, 30.0));
        assert!((s.average() - 20.0).abs() < 1e-4);
        assert_eq!(s.history.len(), 3);
        for _ in 0..HISTORY + 5 {
            s.add(1.0);
        }
        assert_eq!(s.history.len(), HISTORY);
        assert_eq!(fmt_value(4567.8, "MHz"), "4568 MHz");
        assert_eq!(fmt_value(45.25, "°C"), "45.2 °C");
        assert_eq!(csv_field("a, b"), "\"a, b\"");
        assert_eq!(csv_field("x\"y"), "\"x\"\"y\"");

        let mut map = std::collections::BTreeMap::new();
        map.insert("GPU, temp".to_owned(), s.clone());
        let mut log = SensorLog::start(&map).unwrap();
        log.write(&map).unwrap();
        let path = log.path.clone();
        drop(log);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("Time,\"GPU, temp (°C)\""));
        assert!(lines.next().unwrap().ends_with(",1.0"));
    }

    #[test]
    fn sample_is_sane() {
        let mut state = init();
        std::thread::sleep(Duration::from_millis(250));
        let s = sample(&mut state);
        assert!((0.0..=100.0).contains(&s.cpu));
        assert!(!s.per_core.is_empty());
        assert!(s.mem_total > 0 && s.mem_used <= s.mem_total);
    }
}

#[cfg(test)]
mod gpu_tests {
    #[test]
    fn nvidia_gpu_reports_when_present() {
        // Only meaningful on PCs with an NVIDIA driver; elsewhere NVML is absent.
        if let Ok(nvml) = nvml_wrapper::Nvml::init() {
            let gpus = super::sample_gpus(&nvml);
            assert!(!gpus.is_empty());
            assert!(gpus[0].mem_total > 0, "{:?}", gpus[0]);
        }
        let _ = super::battery();
    }
}
