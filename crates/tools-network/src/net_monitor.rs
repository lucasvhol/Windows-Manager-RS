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

/// (alias, bytes received, bytes sent) for physical, connected adapters.
pub fn interface_counters() -> Vec<(String, u64, u64)> {
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
                out.push((String::from_utf16_lossy(&r.Alias[..end]), r.InOctets, r.OutOctets));
            }
        }
        FreeMibTable(t as *const _);
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub conns: Vec<Conn>,
    /// (adapter, download B/s, upload B/s)
    pub rates: Vec<(String, f64, f64)>,
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
    for (name, rx, tx) in interface_counters() {
        if let Some((prx, ptx)) = s.last.get(&name) {
            rates.push((
                name.clone(),
                rx.saturating_sub(*prx) as f64 / secs,
                tx.saturating_sub(*ptx) as f64 / secs,
            ));
        }
        s.last.insert(name, (rx, tx));
    }
    Snapshot { conns, rates }
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
}

impl Default for NetworkMonitor {
    fn default() -> Self {
        Self {
            sampler: None,
            snap: Snapshot::default(),
            filter: String::new(),
            show: Show::Established,
        }
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
                Duration::from_secs(2),
                || State {
                    sys: System::new().expect("sysinfo"),
                    last: HashMap::new(),
                    at: Instant::now(),
                },
                sample,
            )
        });
        if let Some(s) = sampler.latest() {
            self.snap = s;
        }

        ui.horizontal_wrapped(|ui| {
            for (name, down, up) in &self.snap.rates {
                ui.strong(name);
                ui.label(format!(
                    "↓ {}/s  ↑ {}/s",
                    format_bytes(*down as u64),
                    format_bytes(*up as u64)
                ));
                ui.separator();
            }
        });
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
}
