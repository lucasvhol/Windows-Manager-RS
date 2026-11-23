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
}

struct State {
    sys: System,
    nets: Option<Networks>,
    disks: Option<Disks>,
    comps: Option<Components>,
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
    }
}

#[derive(Default)]
pub struct HardwareMonitor {
    sampler: Option<Sampler<Sample>>,
    history: VecDeque<Sample>,
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

        egui::CollapsingHeader::new("Temperatures")
            .default_open(true)
            .show(ui, |ui| {
                if now.temps.is_empty() {
                    ui.weak(
                        "No temperature sensors exposed by the firmware. \
                     Some systems report ACPI thermal zones only when running as administrator.",
                    );
                }
                egui::Grid::new("temps")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for (label, t) in &now.temps {
                            ui.label(label);
                            ui.label(format!("{t:.1} °C"));
                            ui.end_row();
                        }
                    });
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
