//! Hardware Monitor: live CPU, memory, disk and network charts plus temperature sensors.
//!
//! Temperatures come from ACPI thermal zones via sysinfo, NVIDIA cards via NVML and
//! other graphics adapters via Windows (see [`crate::gpu_counters`]); many desktop boards expose
//! none without a vendor kernel driver, in which case the section explains that.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::gpu_counters;
use egui_extras::{Column, TableBuilder};
use sysinfo::{Components, Disks, Networks, System};
use wm_core::{AppCtx, Category, Layout, Tool, egui, task::Sampler, ui::frame};

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
    latest: Option<Sample>,
    sensors: std::collections::BTreeMap<String, Sensor>,
    /// Paths of the tree groups the user folded.
    collapsed: std::collections::HashSet<String>,
    options_open: bool,
    cpu_name: Option<String>,
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

    #[cfg(test)]
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
        const GB: f32 = 1024.0 * 1024.0 * 1024.0;
        self.record("CPU Total".into(), s.cpu, "%");
        for (i, c) in s.per_core.iter().enumerate() {
            self.record(format!("CPU Core #{}", i + 1), *c, "%");
        }
        if let Some(mhz) = s.cpu_mhz {
            self.record("CPU Clock".into(), mhz, "MHz");
        }
        self.record(
            "Memory".into(),
            s.mem_used as f32 * 100.0 / s.mem_total.max(1) as f32,
            "%",
        );
        self.record("Used Memory".into(), s.mem_used as f32 / GB, "GB");
        self.record(
            "Available Memory".into(),
            s.mem_total.saturating_sub(s.mem_used) as f32 / GB,
            "GB",
        );
        if s.swap_total > 0 {
            self.record(
                "Page File".into(),
                s.swap_used as f32 * 100.0 / s.swap_total as f32,
                "%",
            );
        }
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
        self.record("Disk Read".into(), (s.disk_read / 1024.0 / 1024.0) as f32, "MB/s");
        self.record(
            "Disk Write".into(),
            (s.disk_write / 1024.0 / 1024.0) as f32,
            "MB/s",
        );
        self.record("Download".into(), (s.net_rx / 1024.0) as f32, "KB/s");
        self.record("Upload".into(), (s.net_tx / 1024.0) as f32, "KB/s");
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

/// One row of the sensor tree, like the original's: a group or a sensor (`key` into
/// the sensor map).
struct Node {
    label: String,
    icon: Option<&'static str>,
    key: Option<String>,
    children: Vec<Node>,
}

fn group(label: impl Into<String>, icon: Option<&'static str>, children: Vec<Node>) -> Node {
    Node {
        label: label.into(),
        icon,
        key: None,
        children,
    }
}

fn leaf(label: impl Into<String>, key: impl Into<String>) -> Node {
    Node {
        label: label.into(),
        icon: None,
        key: Some(key.into()),
        children: Vec::new(),
    }
}

/// A flattened, visible tree row: (depth, path, node).
type Row<'a> = (usize, String, &'a Node);

fn flatten<'a>(
    node: &'a Node,
    depth: usize,
    parent: &str,
    collapsed: &std::collections::HashSet<String>,
    out: &mut Vec<Row<'a>>,
) {
    let path = format!("{parent}/{}", node.label);
    out.push((depth, path.clone(), node));
    if !collapsed.contains(&path) {
        for c in &node.children {
            flatten(c, depth + 1, &path, collapsed, out);
        }
    }
}

fn cpu_name() -> String {
    wm_core::registry::read_raw(
        r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0",
        "ProcessorNameString",
    )
    .map(|v| wm_core::registry::decode_string(&v).trim().to_owned())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "Processor".into())
}

impl HardwareMonitor {
    /// Leaves for the sensors that exist, dropping groups left empty.
    fn leaves(&self, items: &[(&str, String)]) -> Vec<Node> {
        items
            .iter()
            .filter(|(_, key)| self.sensors.contains_key(key))
            .map(|(label, key)| leaf(*label, key.clone()))
            .collect()
    }

    /// The sensor tree: this PC › CPU / Memory / each GPU / Drives / Network / Battery.
    fn tree(&self, now: &Sample) -> Node {
        fn nonempty(groups: Vec<Node>) -> Vec<Node> {
            groups
                .into_iter()
                .filter(|g| g.key.is_some() || !g.children.is_empty())
                .collect()
        }
        let mut cpu_load = vec![leaf("CPU Total", "CPU Total")];
        cpu_load.extend((1..=now.per_core.len()).map(|i| {
            let k = format!("CPU Core #{i}");
            leaf(k.clone(), k)
        }));
        let temps: Vec<Node> = now
            .temps
            .iter()
            .map(|(label, _)| leaf(label.clone(), label.clone()))
            .collect();
        let cpu = group(
            self.cpu_name.clone().unwrap_or_default(),
            Some(wm_core::icons::CPU),
            nonempty(vec![
                group("Load", None, cpu_load),
                group("Clocks", None, self.leaves(&[("CPU", "CPU Clock".into())])),
                group("Temperatures", None, temps),
            ]),
        );
        let memory = group(
            "Generic Memory",
            Some(wm_core::icons::STACK),
            nonempty(vec![
                group(
                    "Load",
                    None,
                    self.leaves(&[("Memory", "Memory".into()), ("Page File", "Page File".into())]),
                ),
                group(
                    "Data",
                    None,
                    self.leaves(&[
                        ("Used Memory", "Used Memory".into()),
                        ("Available Memory", "Available Memory".into()),
                    ]),
                ),
            ]),
        );
        let mut children = vec![cpu, memory];
        for g in &now.gpus {
            let k = |what: &str| format!("{} {what}", g.name);
            children.push(group(
                g.name.clone(),
                Some(wm_core::icons::MONITOR),
                nonempty(vec![
                    group(
                        "Load",
                        None,
                        self.leaves(&[("GPU Core", k("load")), ("GPU Memory", k("memory"))]),
                    ),
                    group(
                        "Temperatures",
                        None,
                        self.leaves(&[("GPU Core", k("temperature"))]),
                    ),
                    group("Clocks", None, self.leaves(&[("GPU Core", k("clock"))])),
                    group(
                        "Fans",
                        None,
                        self.leaves(&[("GPU Fan", k("fan")), ("GPU Fan Speed", k("fan speed"))]),
                    ),
                    group(
                        "Powers",
                        None,
                        self.leaves(&[("GPU Power", k("power")), ("GPU Power Limit", k("power draw"))]),
                    ),
                ]),
            ));
        }
        let disk_temps: Vec<Node> = self
            .disk_temps
            .iter()
            .map(|(label, _)| leaf(label.trim_start_matches("Disk: ").to_owned(), label.clone()))
            .collect();
        children.push(group(
            "Drives",
            Some(wm_core::icons::HARD_DRIVES),
            nonempty(vec![
                group(
                    "Throughput",
                    None,
                    self.leaves(&[
                        ("Read Rate", "Disk Read".into()),
                        ("Write Rate", "Disk Write".into()),
                    ]),
                ),
                group("Temperatures", None, disk_temps),
            ]),
        ));
        children.push(group(
            "Network",
            Some(wm_core::icons::NETWORK),
            nonempty(vec![group(
                "Throughput",
                None,
                self.leaves(&[
                    ("Download Speed", "Download".into()),
                    ("Upload Speed", "Upload".into()),
                ]),
            )]),
        ));
        if let Some(b) = now.battery {
            let state = if b.charging {
                "charging"
            } else if b.on_ac {
                "plugged in"
            } else {
                "on battery"
            };
            children.push(group(
                format!("Battery ({state})"),
                Some(wm_core::icons::LIGHTNING),
                self.leaves(&[("Charge Level", "Battery charge".into())]),
            ));
        }
        group(
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
            Some(wm_core::icons::DESKTOP),
            nonempty(children),
        )
    }

    fn options_window(&mut self, ui: &egui::Ui) {
        if !self.options_open {
            return;
        }
        let modal = egui::Modal::new(egui::Id::unique("hwmon-options")).show(ui.ctx(), |ui| {
            ui.set_max_width(460.0);
            ui.strong("Options");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.alarm_on, "Warn me when a temperature reaches");
                ui.add(
                    egui::DragValue::new(&mut self.alarm_c)
                        .range(40.0..=120.0)
                        .suffix(" °C"),
                );
            });
            self.sensor_log_controls(ui);
            if ui.button("Reset the Maximum column").clicked() {
                self.sensors.clear();
            }
            ui.add_space(6.0);
            ui.weak(
                "Temperatures come from ACPI thermal zones, NVIDIA cards and, when running as administrator, \
                 the disks. Voltages, fan speeds and CPU package power need a kernel driver, which this app doesn't install.",
            );
            ui.add_space(8.0);
            if ui.button("OK").clicked() {
                self.options_open = false;
            }
        });
        if modal.should_close() {
            self.options_open = false;
        }
    }

    /// SMART disk temperatures, once a minute (administrator only).
    fn poll_disk_temperatures(&mut self, ui: &egui::Ui, elevated: bool) {
        if elevated
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
    }
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
        for s in sampler.drain() {
            // Statistics, alarm and log advance once per sample, not per frame.
            self.update_sensors(&s);
            self.check_alarm();
            if let Some(log) = &mut self.log
                && let Err(e) = log.write(&self.sensors)
            {
                self.log_error = Some(format!("Logging stopped: {e}"));
                self.log = None;
            }
            self.latest = Some(s);
        }
        self.poll_disk_temperatures(ui, ctx.elevated);
        if self.alarm_c == 0.0 {
            self.alarm_c = 85.0;
        }
        if self.cpu_name.is_none() {
            self.cpu_name = Some(cpu_name());
        }
        self.options_window(ui);

        let all_open = self.collapsed.is_empty();
        let alarm = self.alarm_text.clone();
        let mut toggle_all = false;
        frame::footer(
            ui,
            |ui| {
                let (icon, tip) = if all_open {
                    (wm_core::icons::ARROW_UP, "Collapse all")
                } else {
                    (wm_core::icons::ARROW_DOWN, "Expand all")
                };
                toggle_all = ui.button(icon).on_hover_text(tip).clicked();
                if let Some(text) = alarm {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("{}  {text}", wm_core::icons::WARNING),
                    );
                }
            },
            |ui| {
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Options").clicked() {
                    self.options_open = true;
                }
            },
        );

        let Some(now) = self.latest.clone() else {
            let loading = ctx.i18n.tr("common-loading");
            frame::central(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(loading);
                });
            });
            return;
        };
        let tree = self.tree(&now);
        if toggle_all {
            if all_open {
                // Fold everything below the CPU, memory, GPU… level.
                let mut rows = Vec::new();
                flatten(&tree, 0, "", &Default::default(), &mut rows);
                self.collapsed = rows
                    .iter()
                    .filter(|(d, _, n)| *d >= 2 && !n.children.is_empty())
                    .map(|(_, p, _)| p.clone())
                    .collect();
            } else {
                self.collapsed.clear();
            }
        }
        let mut rows = Vec::new();
        flatten(&tree, 0, "", &self.collapsed, &mut rows);
        let mut toggle = None;
        frame::central_fixed(ui, |ui| {
            if let Some(e) = &self.log_error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            let accent = ui.visuals().selection.bg_fill;
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::remainder().at_least(260.0).clip(true))
                .column(Column::exact(130.0))
                .column(Column::exact(130.0))
                .header(22.0, |mut h| {
                    for t in ["Sensor", "Current", "Maximum"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, rows.len(), |mut row| {
                        let (depth, path, node) = &rows[row.index()];
                        let sensor = node.key.as_ref().and_then(|k| self.sensors.get(k));
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.add_space(*depth as f32 * 18.0);
                                if node.children.is_empty() {
                                    ui.add_space(18.0);
                                } else {
                                    let folded = self.collapsed.contains(path);
                                    let caret = if folded { "▸" } else { "▾" };
                                    if ui.add(egui::Button::new(caret).frame(false)).clicked() {
                                        toggle = Some(path.clone());
                                    }
                                }
                                if let Some(icon) = node.icon {
                                    ui.label(egui::RichText::new(icon).color(accent));
                                }
                                if node.key.is_none() && *depth <= 1 {
                                    ui.strong(&node.label);
                                } else {
                                    ui.label(&node.label);
                                }
                            });
                        });
                        row.col(|ui| {
                            if let Some(s) = sensor {
                                ui.label(fmt_value(s.current, s.unit));
                            }
                        });
                        row.col(|ui| {
                            if let Some(s) = sensor {
                                ui.label(fmt_value(s.max, s.unit));
                            }
                        });
                    });
                });
        });
        if let Some(p) = toggle
            && !self.collapsed.remove(&p)
        {
            self.collapsed.insert(p);
        }
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_and_scale() {
        assert_eq!(rate(1000, 2.0), 500.0);
        assert_eq!(rate(1000, 0.0), 0.0);
    }

    #[test]
    fn sensor_tree_follows_the_original() {
        let mut m = HardwareMonitor::default();
        let s = Sample {
            cpu: 10.0,
            per_core: vec![5.0, 15.0],
            mem_used: 1 << 30,
            mem_total: 4 << 30,
            ..Default::default()
        };
        m.update_sensors(&s);
        let tree = m.tree(&s);
        let mut rows = Vec::new();
        flatten(&tree, 0, "", &Default::default(), &mut rows);
        let labels: Vec<&str> = rows.iter().map(|(_, _, n)| n.label.as_str()).collect();
        for want in [
            "Load",
            "CPU Total",
            "CPU Core #1",
            "CPU Core #2",
            "Used Memory",
            "Download Speed",
        ] {
            assert!(labels.contains(&want), "{want} missing from {labels:?}");
        }
        // No temperature sensors: no Temperatures group under the CPU.
        assert!(!labels.contains(&"Temperatures"), "{labels:?}");
        // Folding a group hides its children.
        let cpu_path = rows.iter().find(|(d, _, _)| *d == 1).unwrap().1.clone();
        let mut folded = Vec::new();
        flatten(&tree, 0, "", &[cpu_path].into_iter().collect(), &mut folded);
        assert!(folded.len() < rows.len());
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
