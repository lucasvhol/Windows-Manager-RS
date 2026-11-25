//! Network Monitor: live TCP/UDP connections with their owning process, and
//! per-adapter throughput.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use egui_extras::{Column, TableBuilder};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetExtendedTcpTable, GetExtendedUdpTable, GetIfTable2, MIB_IF_TABLE2,
    MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID,
    TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use wm_core::{AppCtx, Category, Tool, egui, task::Sampler, ui::format_bytes};

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;

pub fn tcp_state(s: u32) -> &'static str {
    match s {
        1 => "Closed",
        2 => "Listening",
        3 => "SYN sent",
        4 => "SYN received",
        5 => "Established",
        6 => "FIN wait 1",
        7 => "FIN wait 2",
        8 => "Close wait",
        9 => "Closing",
        10 => "Last ACK",
        11 => "Time wait",
        12 => "Delete TCB",
        _ => "Unknown",
    }
}

/// Ports are stored in network byte order in the low 16 bits.
pub fn port(raw: u32) -> u16 {
    u16::from_be((raw & 0xFFFF) as u16)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conn {
    pub proto: &'static str,
    pub local: String,
    pub remote: String,
    pub state: &'static str,
    pub pid: u32,
    pub process: String,
}

fn endpoint(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

/// Calls a GetExtended*Table function with a growing buffer; returns (row count, buffer).
fn table(f: impl Fn(Option<*mut core::ffi::c_void>, *mut u32) -> u32) -> Option<(usize, Vec<u64>)> {
    let mut size = 0u32;
    let _ = f(None, &mut size);
    for _ in 0..4 {
        let mut buf = vec![0u64; (size as usize).div_ceil(8) + 16];
        size = (buf.len() * 8) as u32;
        match f(Some(buf.as_mut_ptr().cast()), &mut size) {
            0 => {
                let n = unsafe { *(buf.as_ptr() as *const u32) } as usize;
                return Some((n, buf));
            }
            122 => continue, // ERROR_INSUFFICIENT_BUFFER: the table grew
            _ => return None,
        }
    }
    None
}

/// Rows follow the 4-byte count, at the row type's alignment (4 for all these row types).
unsafe fn rows<T>(buf: &[u64], n: usize) -> &[T] {
    let base = buf.as_ptr() as *const u8;
    let offset = std::mem::align_of::<T>().max(4);
    unsafe { std::slice::from_raw_parts(base.add(offset) as *const T, n) }
}

pub fn connections() -> Vec<Conn> {
    let mut out = Vec::new();
    unsafe {
        if let Some((n, buf)) =
            table(|p, s| GetExtendedTcpTable(p, s, true, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0))
        {
            for r in rows::<MIB_TCPROW_OWNER_PID>(&buf, n) {
                out.push(Conn {
                    proto: "TCP",
                    local: endpoint(
                        IpAddr::V4(Ipv4Addr::from(r.dwLocalAddr.to_ne_bytes())),
                        port(r.dwLocalPort),
                    ),
                    remote: endpoint(
                        IpAddr::V4(Ipv4Addr::from(r.dwRemoteAddr.to_ne_bytes())),
                        port(r.dwRemotePort),
                    ),
                    state: tcp_state(r.dwState),
                    pid: r.dwOwningPid,
                    process: String::new(),
                });
            }
        }
        if let Some((n, buf)) =
            table(|p, s| GetExtendedTcpTable(p, s, true, AF_INET6, TCP_TABLE_OWNER_PID_ALL, 0))
        {
            for r in rows::<MIB_TCP6ROW_OWNER_PID>(&buf, n) {
                out.push(Conn {
                    proto: "TCPv6",
                    local: endpoint(IpAddr::V6(Ipv6Addr::from(r.ucLocalAddr)), port(r.dwLocalPort)),
                    remote: endpoint(IpAddr::V6(Ipv6Addr::from(r.ucRemoteAddr)), port(r.dwRemotePort)),
                    state: tcp_state(r.dwState),
                    pid: r.dwOwningPid,
                    process: String::new(),
                });
            }
        }
        if let Some((n, buf)) = table(|p, s| GetExtendedUdpTable(p, s, true, AF_INET, UDP_TABLE_OWNER_PID, 0))
        {
            for r in rows::<MIB_UDPROW_OWNER_PID>(&buf, n) {
                out.push(Conn {
                    proto: "UDP",
                    local: endpoint(
                        IpAddr::V4(Ipv4Addr::from(r.dwLocalAddr.to_ne_bytes())),
                        port(r.dwLocalPort),
                    ),
                    remote: "*".into(),
                    state: "",
                    pid: r.dwOwningPid,
                    process: String::new(),
                });
            }
        }
        if let Some((n, buf)) =
            table(|p, s| GetExtendedUdpTable(p, s, true, AF_INET6, UDP_TABLE_OWNER_PID, 0))
        {
            for r in rows::<MIB_UDP6ROW_OWNER_PID>(&buf, n) {
                out.push(Conn {
                    proto: "UDPv6",
                    local: endpoint(IpAddr::V6(Ipv6Addr::from(r.ucLocalAddr)), port(r.dwLocalPort)),
                    remote: "*".into(),
                    state: "",
                    pid: r.dwOwningPid,
                    process: String::new(),
                });
            }
        }
    }
    out
}

/// (alias, bytes received, bytes sent, link speed in bits/s) for physical, connected adapters.
pub fn interface_counters() -> Vec<(String, u64, u64, u64)> {
    let mut out = Vec::new();
    unsafe {
        let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut t).0 != 0 || t.is_null() {
            return out;
        }
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        for r in rows {
            let flags = r.InterfaceAndOperStatusFlags._bitfield;
            let hardware = flags & 0x01 != 0;
            let filter = flags & 0x02 != 0;
            if hardware && !filter && r.OperStatus == IfOperStatusUp {
                let end = r.Alias.iter().position(|&c| c == 0).unwrap_or(r.Alias.len());
                out.push((
                    String::from_utf16_lossy(&r.Alias[..end]),
                    r.InOctets,
                    r.OutOctets,
                    r.ReceiveLinkSpeed.max(r.TransmitLinkSpeed),
                ));
            }
        }
        FreeMibTable(t as *const _);
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub conns: Vec<Conn>,
    pub rates: Vec<Rate>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rate {
    pub adapter: String,
    /// Bytes per second.
    pub down: f64,
    pub up: f64,
    /// Link speed in bits per second.
    pub link: u64,
    /// Seconds the rate was measured over.
    pub secs: f64,
}

struct State {
    sys: System,
    last: HashMap<String, (u64, u64)>,
    at: Instant,
}

fn sample(s: &mut State) -> Snapshot {
    s.sys
        .refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut conns = connections();
    for c in &mut conns {
        c.process = match c.pid {
            0 => "System Idle".into(),
            4 => "System".into(),
            pid => s
                .sys
                .process(sysinfo::Pid::from_u32(pid))
                .map(|p| p.name().to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
    }
    let secs = s.at.elapsed().as_secs_f64().max(0.001);
    s.at = Instant::now();
    let mut rates = Vec::new();
    for (name, rx, tx, link) in interface_counters() {
        if let Some((prx, ptx)) = s.last.get(&name) {
            rates.push(Rate {
                adapter: name.clone(),
                down: rx.saturating_sub(*prx) as f64 / secs,
                up: tx.saturating_sub(*ptx) as f64 / secs,
                link,
                secs,
            });
        }
        s.last.insert(name, (rx, tx));
    }
    Snapshot { conns, rates }
}

/// Seconds of history in the chart (one sample per second).
pub const HISTORY: usize = 60;

/// Throughput of one adapter (or all of them) since the monitor opened or was reset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// (received, sent) bytes per second, oldest first.
    pub history: std::collections::VecDeque<(f64, f64)>,
    pub current: (f64, f64),
    pub max: (f64, f64),
    /// Bytes.
    pub total: (f64, f64),
    pub secs: f64,
    pub link: u64,
}

impl Stats {
    pub fn add(&mut self, r: &Rate) {
        self.current = (r.down, r.up);
        self.max = (self.max.0.max(r.down), self.max.1.max(r.up));
        self.total = (self.total.0 + r.down * r.secs, self.total.1 + r.up * r.secs);
        self.secs += r.secs;
        self.link = r.link;
        self.history.push_back((r.down, r.up));
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    /// Average bytes per second.
    pub fn average(&self) -> (f64, f64) {
        if self.secs <= 0.0 {
            (0.0, 0.0)
        } else {
            (self.total.0 / self.secs, self.total.1 / self.secs)
        }
    }
}

/// All adapters together (the link speed is the fastest one's).
pub fn combined(rates: &[Rate]) -> Option<Rate> {
    let first = rates.first()?;
    Some(Rate {
        adapter: String::new(),
        down: rates.iter().map(|r| r.down).sum(),
        up: rates.iter().map(|r| r.up).sum(),
        link: rates.iter().map(|r| r.link).max().unwrap_or(0),
        secs: first.secs,
    })
}

fn rate_text(bytes_per_sec: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec as u64))
}

/// "1 Gbps", "300 Mbps"…
pub fn link_text(bps: u64) -> String {
    match bps {
        0 => "unknown".into(),
        b if b >= 1_000_000_000 => format!("{:.1} Gbps", b as f64 / 1e9).replace(".0 ", " "),
        b if b >= 1_000_000 => format!("{} Mbps", b / 1_000_000),
        b => format!("{} kbps", b / 1000),
    }
}

/// A 60-second chart of received and sent rates, scaled to the highest point.
fn rate_chart(ui: &mut egui::Ui, s: &Stats, received: egui::Color32, sent: egui::Color32) {
    let size = egui::vec2(ui.available_width(), 150.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let v = ui.visuals();
    painter.rect_filled(rect, 3.0, v.extreme_bg_color);
    let grid = egui::Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color);
    for i in 1..4 {
        let y = rect.top() + rect.height() * i as f32 / 4.0;
        painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], grid);
    }
    let peak = s
        .history
        .iter()
        .map(|(d, u)| d.max(*u))
        .fold(0.0_f64, f64::max)
        .max(1024.0);
    let step = rect.width() / (HISTORY - 1) as f32;
    // Newest sample on the right edge.
    let offset = HISTORY - s.history.len();
    for (series, color) in [(0, received), (1, sent)] {
        let points: Vec<egui::Pos2> = s
            .history
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let val = if series == 0 { p.0 } else { p.1 };
                let y = rect.bottom() - (val / peak) as f32 * (rect.height() - 4.0) - 2.0;
                egui::pos2(rect.left() + (i + offset) as f32 * step, y)
            })
            .collect();
        if points.len() >= 2 {
            painter.add(egui::Shape::line(points, egui::Stroke::new(1.8, color)));
        }
    }
    let font = egui::FontId::proportional(11.0);
    painter.text(
        rect.left_top() + egui::vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        rate_text(peak),
        font.clone(),
        v.weak_text_color(),
    );
    painter.text(
        rect.left_bottom() + egui::vec2(4.0, -2.0),
        egui::Align2::LEFT_BOTTOM,
        "60 s ago",
        font.clone(),
        v.weak_text_color(),
    );
    painter.text(
        rect.right_bottom() + egui::vec2(-4.0, -2.0),
        egui::Align2::RIGHT_BOTTOM,
        "now",
        font,
        v.weak_text_color(),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Show {
    All,
    Established,
    Listening,
}

pub struct NetworkMonitor {
    sampler: Option<Sampler<Snapshot>>,
    snap: Snapshot,
    filter: String,
    show: Show,
    /// Per adapter name; "" is all adapters together.
    stats: std::collections::BTreeMap<String, Stats>,
    /// The adapter shown in the chart ("" = all).
    adapter: String,
    message: wm_core::ui::Message,
}

impl Default for NetworkMonitor {
    fn default() -> Self {
        Self {
            sampler: None,
            snap: Snapshot::default(),
            filter: String::new(),
            show: Show::Established,
            stats: Default::default(),
            adapter: String::new(),
            message: None,
        }
    }
}

impl NetworkMonitor {
    fn record(&mut self, snap: &Snapshot) {
        for r in &snap.rates {
            self.stats.entry(r.adapter.clone()).or_default().add(r);
        }
        if let Some(all) = combined(&snap.rates) {
            self.stats.entry(String::new()).or_default().add(&all);
        }
    }

    fn throughput_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let label = |a: &str| {
                if a.is_empty() {
                    "All adapters".to_owned()
                } else {
                    a.to_owned()
                }
            };
            egui::ComboBox::from_id_salt("netmon-adapter")
                .selected_text(label(&self.adapter))
                .show_ui(ui, |ui| {
                    for name in self.stats.keys() {
                        ui.selectable_value(&mut self.adapter, name.clone(), label(name));
                    }
                });
            if ui
                .button("Reset")
                .on_hover_text("Clears the chart and the statistics.")
                .clicked()
            {
                self.stats.clear();
            }
            if ui
                .button("Resource Monitor")
                .on_hover_text("Windows' Resource Monitor, with traffic per process.")
                .clicked()
            {
                self.message = wm_core::shell::shell_open("resmon.exe", "")
                    .err()
                    .map(|e| Err(format!("{e:#}")));
            }
        });
        wm_core::ui::show_message(ui, &self.message);
        let Some(s) = self.stats.get(&self.adapter).cloned() else {
            ui.weak("Measuring…");
            return;
        };
        let received = egui::Color32::from_rgb(64, 156, 255);
        let sent = egui::Color32::from_rgb(255, 140, 50);
        rate_chart(ui, &s, received, sent);
        let avg = s.average();
        egui::Grid::new("netmon-stats")
            .num_columns(5)
            .spacing([24.0, 4.0])
            .show(ui, |ui| {
                for h in ["", "Current", "Maximum", "Average", "Total"] {
                    ui.strong(h);
                }
                ui.end_row();
                for (name, color, cur, max, avg, total) in [
                    ("Received", received, s.current.0, s.max.0, avg.0, s.total.0),
                    ("Sent", sent, s.current.1, s.max.1, avg.1, s.total.1),
                ] {
                    ui.colored_label(color, format!("━ {name}"));
                    ui.label(rate_text(cur));
                    ui.label(rate_text(max));
                    ui.label(rate_text(avg));
                    ui.label(format_bytes(total as u64));
                    ui.end_row();
                }
            });
        let used = if s.link > 0 {
            format!(
                " · in use now: {:.1}%",
                (s.current.0.max(s.current.1) * 8.0) / s.link as f64 * 100.0
            )
        } else {
            String::new()
        };
        ui.weak(format!(
            "Link speed {}{used} · measured over {}",
            link_text(s.link),
            wm_core::ui::format_duration(s.secs as u64)
        ));
    }
}

impl Tool for NetworkMonitor {
    fn id(&self) -> &'static str {
        "netmonitor"
    }

    fn category(&self) -> Category {
        Category::Network
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let sampler = self.sampler.get_or_insert_with(|| {
            Sampler::spawn(
                ui.ctx(),
                Duration::from_secs(1),
                || State {
                    sys: System::new().expect("sysinfo"),
                    last: HashMap::new(),
                    at: Instant::now(),
                },
                sample,
            )
        });
        // Every sample, so the chart has no gaps.
        let fresh = sampler.drain();
        for s in &fresh {
            self.record(s);
        }
        if let Some(s) = fresh.into_iter().last() {
            self.snap = s;
        }

        self.throughput_ui(ui);
        ui.add_space(6.0);
        ui.strong("Connections");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            for (s, label) in [
                (Show::Established, "Connected"),
                (Show::Listening, "Listening"),
                (Show::All, "All"),
            ] {
                ui.selectable_value(&mut self.show, s, label);
            }
        });
        let f = self.filter.to_lowercase();
        let mut visible: Vec<&Conn> = self
            .snap
            .conns
            .iter()
            .filter(|c| match self.show {
                Show::All => true,
                Show::Established => c.state == "Established",
                Show::Listening => c.state == "Listening" || c.proto.starts_with("UDP"),
            })
            .filter(|c| {
                f.is_empty()
                    || c.process.to_lowercase().contains(&f)
                    || c.local.contains(&f)
                    || c.remote.contains(&f)
                    || c.pid.to_string() == f
            })
            .collect();
        visible.sort_by(|a, b| {
            a.process
                .to_lowercase()
                .cmp(&b.process.to_lowercase())
                .then(a.local.cmp(&b.local))
        });
        ui.weak(format!(
            "{} of {} connections",
            visible.len(),
            self.snap.conns.len()
        ));

        TableBuilder::new(ui)
            .striped(true)
            .column(Column::initial(170.0).clip(true))
            .column(Column::initial(60.0))
            .column(Column::initial(60.0))
            .column(Column::initial(220.0).clip(true))
            .column(Column::initial(240.0).clip(true))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in [
                    "Process",
                    "PID",
                    "Protocol",
                    "Local address",
                    "Remote address",
                    "State",
                ] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let c = visible[row.index()];
                    row.col(|ui| {
                        ui.label(&c.process);
                    });
                    row.col(|ui| {
                        ui.weak(c.pid.to_string());
                    });
                    row.col(|ui| {
                        ui.weak(c.proto);
                    });
                    row.col(|ui| {
                        ui.label(&c.local);
                    });
                    row.col(|ui| {
                        ui.label(&c.remote);
                    });
                    row.col(|ui| {
                        ui.label(c.state);
                    });
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_and_states() {
        assert_eq!(port(u32::from(443u16.to_be())), 443);
        assert_eq!(tcp_state(5), "Established");
        assert_eq!(tcp_state(99), "Unknown");
    }

    #[test]
    fn sees_own_listening_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = listener.local_addr().unwrap().port();
        let conns = connections();
        let mine = conns
            .iter()
            .find(|c| c.proto == "TCP" && c.local == format!("127.0.0.1:{p}"))
            .expect("listener listed");
        assert_eq!(mine.state, "Listening");
        assert_eq!(mine.pid, std::process::id());
    }

    #[test]
    fn interface_counters_do_not_fail() {
        let _ = interface_counters();
    }

    #[test]
    fn statistics() {
        let r = |down, up| Rate {
            adapter: "Wi-Fi".into(),
            down,
            up,
            link: 100_000_000,
            secs: 1.0,
        };
        let mut s = Stats::default();
        s.add(&r(1000.0, 10.0));
        s.add(&r(3000.0, 30.0));
        assert_eq!(s.current, (3000.0, 30.0));
        assert_eq!(s.max, (3000.0, 30.0));
        assert_eq!(s.total, (4000.0, 40.0));
        assert_eq!(s.average(), (2000.0, 20.0));
        for _ in 0..100 {
            s.add(&r(1.0, 1.0));
        }
        assert_eq!(s.history.len(), HISTORY, "the chart keeps 60 seconds");
        let both = combined(&[
            r(1.0, 2.0),
            Rate {
                link: 1_000_000_000,
                ..r(3.0, 4.0)
            },
        ])
        .unwrap();
        assert_eq!((both.down, both.up, both.link), (4.0, 6.0, 1_000_000_000));
        assert!(combined(&[]).is_none());
        assert_eq!(link_text(1_000_000_000), "1 Gbps");
        assert_eq!(link_text(2_500_000_000), "2.5 Gbps");
        assert_eq!(link_text(300_000_000), "300 Mbps");
    }
}
