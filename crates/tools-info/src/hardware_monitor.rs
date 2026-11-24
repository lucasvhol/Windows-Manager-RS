//! Hardware Monitor: live CPU, memory, disk and network charts plus temperature sensors.
//!
//! Temperatures come from ACPI thermal zones via sysinfo; many desktop boards expose
//! none without a vendor kernel driver, in which case the section explains that.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
    Sample {
        cpu: s.sys.global_cpu_usage(),
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
        gpus: s.nvml.as_ref().map(sample_gpus).unwrap_or_default(),
        battery: battery(),
    }
}

#[derive(Default)]
pub struct HardwareMonitor {
    sampler: Option<Sampler<Sample>>,
    history: VecDeque<Sample>,
    /// Sensor name -> (current, maximum seen, unit).
    sensors: std::collections::BTreeMap<String, (f32, f32, &'static str)>,
    alarm_on: bool,
    alarm_c: f32,
    last_alarm: Option<Instant>,
    alarm_text: Option<String>,
    disk_task: Option<wm_core::task::BackgroundTask<Vec<(String, f32)>>>,
    disk_temps: Vec<(String, f32)>,
    disk_checked: Option<Instant>,
}

/// SMART disk temperatures (needs administrator rights; slow, so polled once a minute).
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
    fn record(&mut self, name: String, value: f32, unit: &'static str) {
        let e = self.sensors.entry(name).or_insert((value, value, unit));
        e.0 = value;
        e.1 = e.1.max(value);
    }

    fn update_sensors(&mut self, s: &Sample) {
        self.record("CPU usage".into(), s.cpu, "%");
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
            .filter(|(_, (cur, _, unit))| *unit == "°C" && *cur >= self.alarm_c)
            .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
            .map(|(name, (cur, _, _))| format!("{name} is at {cur:.0} °C (alarm at {:.0} °C)", self.alarm_c));
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
        for s in sampler.drain() {
            if self.history.len() == HISTORY {
                self.history.pop_front();
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
                    format!("{:.0}%", now.cpu),
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
        self.update_sensors(&now);
        self.check_alarm();

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
                    });
                });
        }

        egui::CollapsingHeader::new("Sensors").default_open(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.alarm_on, "Warn me when a temperature reaches");
                ui.add(egui::DragValue::new(&mut self.alarm_c).range(40.0..=120.0).suffix(" °C"));
                if ui.small_button("Reset maximums").clicked() {
                    self.sensors.clear();
                }
            });
            if let Some(text) = &self.alarm_text {
                ui.colored_label(ui.visuals().error_fg_color, format!("{}  {text}", wm_core::icons::WARNING));
            }
            egui::Grid::new("sensors").num_columns(3).striped(true).spacing([24.0, 4.0]).show(ui, |ui| {
                ui.strong("Sensor");
                ui.strong("Current");
                ui.strong("Maximum");
                ui.end_row();
                for (name, (cur, max, unit)) in &self.sensors {
                    ui.label(name);
                    ui.label(format!("{cur:.1} {unit}"));
                    ui.weak(format!("{max:.1} {unit}"));
                    ui.end_row();
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
