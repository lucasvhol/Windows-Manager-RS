//! Network Tools: ping, traceroute (native ICMP), DNS lookup, TCP port check and whois.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::net_scan;
use anyhow::{Context, Result, bail};
use windows::Win32::NetworkManagement::Dns::{
    DNS_QUERY_STANDARD, DNS_RECORDA, DNS_RECORDW, DNS_TYPE, DNS_TYPE_A, DNS_TYPE_AAAA, DNS_TYPE_CNAME,
    DNS_TYPE_MX, DNS_TYPE_NS, DNS_TYPE_TEXT, DnsFree, DnsFreeRecordList, DnsQuery_W,
};
use windows::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, IP_OPTION_INFORMATION, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
};
use windows::core::HSTRING;
use wm_core::{AppCtx, Category, Tool, egui, task::BackgroundTask};

const IP_SUCCESS: u32 = 0;
const IP_TTL_EXPIRED_TRANSIT: u32 = 11013;
const IP_REQ_TIMED_OUT: u32 = 11010;
pub const IP_SUCCESS_CODE: u32 = IP_SUCCESS;
pub const IP_TTL_EXPIRED: u32 = IP_TTL_EXPIRED_TRANSIT;

pub fn status_text(code: u32) -> &'static str {
    match code {
        IP_SUCCESS => "reply",
        11002 => "network unreachable",
        11003 => "host unreachable",
        11004 => "protocol unreachable",
        11005 => "port unreachable",
        IP_REQ_TIMED_OUT => "timed out",
        IP_TTL_EXPIRED_TRANSIT => "TTL expired",
        _ => "error",
    }
}

pub fn resolve_v4(host: &str) -> Result<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    (host, 0)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve {host}"))?
        .find_map(|a| match a.ip() {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        })
        .with_context(|| format!("{host} has no IPv4 address"))
}

/// One ICMP echo; returns (status, responder, round-trip ms).
pub fn echo(target: Ipv4Addr, ttl: u8, timeout_ms: u32) -> Result<(u32, Ipv4Addr, u32)> {
    unsafe {
        let h = IcmpCreateFile()?;
        let payload = *b"win-manager-rs-ping-payload-0123";
        let options = IP_OPTION_INFORMATION {
            Ttl: ttl,
            ..Default::default()
        };
        let mut reply = vec![0u8; size_of::<ICMP_ECHO_REPLY>() + payload.len() + 64];
        let n = IcmpSendEcho(
            h,
            u32::from_ne_bytes(target.octets()),
            payload.as_ptr().cast(),
            payload.len() as u16,
            Some(&options),
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            timeout_ms,
        );
        let _ = IcmpCloseHandle(h);
        if n == 0 {
            // No reply structure: treat as a timeout (GetLastError is IP_REQ_TIMED_OUT or similar).
            return Ok((IP_REQ_TIMED_OUT, Ipv4Addr::UNSPECIFIED, timeout_ms));
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        Ok((r.Status, Ipv4Addr::from(r.Address.to_ne_bytes()), r.RoundTripTime))
    }
}

pub fn ping(host: &str, count: u32) -> Result<Vec<String>> {
    let ip = resolve_v4(host)?;
    let mut out = vec![format!("Pinging {host} [{ip}]")];
    let mut times = Vec::new();
    for _ in 0..count {
        let (status, from, ms) = echo(ip, 128, 2000)?;
        if status == IP_SUCCESS {
            times.push(ms);
            out.push(format!("Reply from {from}: time={ms} ms"));
        } else {
            out.push(format!(
                "{} ({})",
                status_text(status),
                if from.is_unspecified() { ip } else { from }
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    let lost = count as usize - times.len();
    out.push(format!(
        "Sent {count}, received {}, lost {lost} ({}%){}",
        times.len(),
        lost * 100 / count.max(1) as usize,
        if times.is_empty() {
            String::new()
        } else {
            format!(
                ". Min {} ms, max {} ms, avg {} ms",
                times.iter().min().unwrap(),
                times.iter().max().unwrap(),
                times.iter().sum::<u32>() / times.len() as u32
            )
        }
    ));
    Ok(out)
}

pub const DNS_TYPES: [(&str, DNS_TYPE); 6] = [
    ("A", DNS_TYPE_A),
    ("AAAA", DNS_TYPE_AAAA),
    ("CNAME", DNS_TYPE_CNAME),
    ("MX", DNS_TYPE_MX),
    ("NS", DNS_TYPE_NS),
    ("TXT", DNS_TYPE_TEXT),
];

pub fn dns_lookup(name: &str, kind: DNS_TYPE) -> Result<Vec<String>> {
    unsafe {
        let mut results: *mut DNS_RECORDA = std::ptr::null_mut();
        let err = DnsQuery_W(
            &HSTRING::from(name),
            kind,
            DNS_QUERY_STANDARD,
            None,
            &mut results,
            None,
        );
        if err.0 != 0 {
            bail!("{}", windows::core::Error::from(err.to_hresult()).message());
        }
        let mut out = Vec::new();
        // DnsQuery_W fills DNS_RECORDW structures (the binding declares the A variant).
        let mut p = results as *const DNS_RECORDW;
        while !p.is_null() {
            let r = &*p;
            let ty = DNS_TYPE(r.wType);
            let line = if ty == DNS_TYPE_A {
                Some(Ipv4Addr::from(r.Data.A.IpAddress.to_ne_bytes()).to_string())
            } else if ty == DNS_TYPE_AAAA {
                Some(Ipv6Addr::from(r.Data.AAAA.Ip6Address.IP6Byte).to_string())
            } else if ty == DNS_TYPE_CNAME || ty == DNS_TYPE_NS {
                r.Data
                    .CNAME
                    .pNameHost
                    .to_string()
                    .ok()
                    .map(|n| format!("{} {n}", if ty == DNS_TYPE_NS { "NS" } else { "CNAME" }))
            } else if ty == DNS_TYPE_MX {
                r.Data
                    .MX
                    .pNameExchange
                    .to_string()
                    .ok()
                    .map(|n| format!("{} {n}", r.Data.MX.wPreference))
            } else if ty == DNS_TYPE_TEXT {
                let txt = &r.Data.TXT;
                let strings =
                    std::slice::from_raw_parts(txt.pStringArray.as_ptr(), txt.dwStringCount as usize);
                Some(
                    strings
                        .iter()
                        .filter_map(|s| s.to_string().ok())
                        .collect::<Vec<_>>()
                        .join(""),
                )
            } else {
                None
            };
            out.extend(line);
            p = r.pNext;
        }
        DnsFree(Some(results as *const _), DnsFreeRecordList);
        if out.is_empty() {
            out.push("(no records of this type)".into());
        }
        Ok(out)
    }
}

/// Parses "80, 443, 8000-8010" (at most 1024 ports).
pub fn parse_ports(s: &str) -> Result<Vec<u16>> {
    let mut ports = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u16, u16) = (a.trim().parse()?, b.trim().parse()?);
                if a > b {
                    bail!("invalid range {part}");
                }
                ports.extend(a..=b);
            }
            None => ports.push(part.parse().with_context(|| format!("invalid port {part}"))?),
        }
    }
    ports.sort_unstable();
    ports.dedup();
    if ports.is_empty() || ports.len() > 1024 || ports.contains(&0) {
        bail!("enter 1–1024 ports between 1 and 65535");
    }
    Ok(ports)
}

pub fn check_ports(host: &str, ports: &[u16]) -> Result<Vec<(u16, bool)>> {
    let ip = (host, 0)
        .to_socket_addrs()?
        .next()
        .context("could not resolve host")?
        .ip();
    let mut results: Vec<(u16, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = ports
            .chunks(32)
            .map(|chunk| {
                s.spawn(move || {
                    chunk
                        .iter()
                        .map(|&p| {
                            (
                                p,
                                TcpStream::connect_timeout(
                                    &SocketAddr::new(ip, p),
                                    Duration::from_millis(800),
                                )
                                .is_ok(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    results.sort();
    Ok(results)
}

fn whois_query(server: &str, query: &str) -> Result<String> {
    let mut stream = TcpStream::connect((server, 43)).with_context(|| format!("connecting to {server}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(format!("{query}\r\n").as_bytes())?;
    let mut buf = Vec::new();
    stream.take(256 * 1024).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Asks IANA which server is authoritative, then queries that server.
pub fn whois(query: &str) -> Result<String> {
    let iana = whois_query("whois.iana.org", query)?;
    let refer = iana
        .lines()
        .find_map(|l| l.strip_prefix("refer:").map(|s| s.trim().to_owned()));
    match refer {
        Some(server) if !server.is_empty() => Ok(format!("[{server}]\n{}", whois_query(&server, query)?)),
        _ => Ok(iana),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ping,
    Trace,
    Dns,
    Ports,
    Whois,
    Scan,
    Geo,
}

/// Ports most often worth checking.
const COMMON_PORTS: &str =
    "21-23, 25, 53, 80, 110, 135, 139, 143, 443, 445, 993, 995, 1433, 3306, 3389, 5900, 8080";

pub struct NetworkTools {
    mode: Mode,
    host: String,
    dns_type: usize,
    ports: String,
    /// IP scanner range.
    range: String,
    /// Look up hop locations in traceroute (sends hop addresses to ipinfo.io).
    locate: bool,
    output: Vec<String>,
    task: Option<BackgroundTask<Vec<String>>>,
    message: wm_core::ui::Message,
}

impl Default for NetworkTools {
    fn default() -> Self {
        Self {
            mode: Mode::Ping,
            host: "example.com".into(),
            dns_type: 0,
            ports: "22, 80, 443, 3389".into(),
            range: String::new(),
            locate: false,
            output: Vec::new(),
            task: None,
            message: None,
        }
    }
}

impl Tool for NetworkTools {
    fn id(&self) -> &'static str {
        "nettools"
    }

    fn category(&self) -> Category {
        Category::Network
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.horizontal(|ui| {
            for (m, label) in [
                (Mode::Ping, "Ping"),
                (Mode::Trace, "Traceroute"),
                (Mode::Dns, "DNS lookup"),
                (Mode::Ports, "Port check"),
                (Mode::Scan, "IP scanner"),
                (Mode::Geo, "Geolocation"),
                (Mode::Whois, "Whois"),
            ] {
                ui.selectable_value(&mut self.mode, m, label);
            }
            ui.separator();
            if ui
                .button("Router page")
                .on_hover_text("Opens your router's settings page (the default gateway) in the browser.")
                .clicked()
            {
                self.message = Some(match net_scan::default_gateway() {
                    Some(gw) => wm_core::shell::shell_open(&format!("http://{gw}/"), "")
                        .map(|()| format!("Opened http://{gw}/"))
                        .map_err(|e| format!("{e:#}")),
                    None => Err("No default gateway: this PC isn't connected to a router.".into()),
                });
            }
            let report = net_scan::wlan_report_path();
            if ui
                .add_enabled(ctx.elevated || report.exists(), egui::Button::new("Wi-Fi report"))
                .on_hover_text(
                    "Windows' report of Wi-Fi connections, errors and disconnects over the last three days.",
                )
                .on_disabled_hover_text("Creating the report needs the app to run as administrator.")
                .clicked()
            {
                let made = if ctx.elevated {
                    net_scan::make_wlan_report()
                } else {
                    Ok(report)
                };
                self.message = Some(
                    made.and_then(|p| wm_core::shell::shell_open(&p.display().to_string(), ""))
                        .map(|()| {
                            if ctx.elevated {
                                "Opened a new Wi-Fi report.".into()
                            } else {
                                "Opened the last Wi-Fi report (run as administrator for a new one).".into()
                            }
                        })
                        .map_err(|e| format!("{e:#}")),
                );
            }
        });
        ui.separator();
        wm_core::ui::show_message(ui, &self.message);
        let busy = self.task.is_some();
        if self.mode == Mode::Scan && self.range.is_empty() {
            self.range = net_scan::local_range().unwrap_or_else(|| "192.168.1.0/24".into());
        }
        ui.horizontal(|ui| {
            match self.mode {
                Mode::Scan => {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.range)
                            .hint_text("192.168.1.1-254 or 192.168.1.0/24")
                            .desired_width(240.0),
                    );
                }
                Mode::Geo => {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.host)
                            .hint_text("IP or host; empty = your public address")
                            .desired_width(240.0),
                    );
                }
                _ => {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.host)
                            .hint_text("Host name or IP")
                            .desired_width(240.0),
                    );
                }
            }
            match self.mode {
                Mode::Trace => {
                    ui.checkbox(&mut self.locate, "Show hop locations").on_hover_text(
                        "Looks up each public hop on ipinfo.io, which sends those addresses to that service.",
                    );
                }
                Mode::Ports if ui.small_button("Common ports").clicked() => {
                    self.ports = COMMON_PORTS.into();
                }
                _ => {}
            }
            match self.mode {
                Mode::Dns => {
                    egui::ComboBox::from_id_salt("dns_type")
                        .selected_text(DNS_TYPES[self.dns_type].0)
                        .show_ui(ui, |ui| {
                            for (i, (name, _)) in DNS_TYPES.iter().enumerate() {
                                ui.selectable_value(&mut self.dns_type, i, *name);
                            }
                        });
                }
                Mode::Ports => {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ports)
                            .hint_text("80, 443, 8000-8010")
                            .desired_width(180.0),
                    );
                }
                _ => {}
            }
            let ready = match self.mode {
                Mode::Scan => !self.range.trim().is_empty(),
                Mode::Geo => true,
                _ => !self.host.trim().is_empty(),
            };
            if ui.add_enabled(!busy && ready, egui::Button::new("Run")).clicked() {
                let host = self.host.trim().to_owned();
                let (mode, kind, ports) = (self.mode, DNS_TYPES[self.dns_type].1, self.ports.clone());
                let (range, locate) = (self.range.clone(), self.locate);
                self.output.clear();
                self.task = Some(BackgroundTask::spawn(ui.ctx(), move |p| match mode {
                    Mode::Ping => ping(&host, 4),
                    Mode::Trace => {
                        let mut lines = Vec::new();
                        net_scan::traceroute3(&host, 30, locate, |l| {
                            p.report(0.0, l.clone());
                            lines.push(l);
                        })?;
                        Ok(lines)
                    }
                    Mode::Scan => {
                        let r = net_scan::parse_range(&range)?;
                        let hosts = net_scan::scan(r, &|f| p.report(f, ""), &|| p.is_cancelled());
                        Ok(net_scan::host_lines(&hosts, r))
                    }
                    Mode::Geo => net_scan::geolocate(&host),
                    Mode::Dns => dns_lookup(&host, kind),
                    Mode::Ports => {
                        let ports = parse_ports(&ports)?;
                        let results = check_ports(&host, &ports)?;
                        let open = results.iter().filter(|(_, o)| *o).count();
                        let mut lines = vec![format!("{open} of {} ports open on {host}", results.len())];
                        lines.extend(results.into_iter().filter(|(_, o)| *o || ports.len() <= 32).map(
                            |(port, o)| {
                                format!("{port:>5}  {}", if o { "open" } else { "closed or filtered" })
                            },
                        ));
                        Ok(lines)
                    }
                    Mode::Whois => Ok(whois(&host)?.lines().map(str::to_owned).collect()),
                }));
            }
            if let Some(t) = &self.task {
                if self.mode == Mode::Scan {
                    ui.add(
                        egui::ProgressBar::new(t.progress)
                            .desired_width(160.0)
                            .show_percentage(),
                    );
                } else {
                    ui.spinner();
                }
                if ui.small_button("Stop").clicked() {
                    t.cancel();
                }
            }
        });
        if self.mode == Mode::Geo {
            ui.weak("Sends the address to ipinfo.io when you press Run.");
        }
        if let Some(t) = &mut self.task {
            t.poll();
            if !t.status.is_empty() && self.output.last() != Some(&t.status) {
                self.output.push(t.status.clone());
            }
            if let Some(r) = t.take_result() {
                self.task = None;
                match r {
                    Ok(lines) => self.output = lines,
                    Err(e) => self.output.push(format!("Error: {e}")),
                }
            }
        }
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let mut text = self.output.join("\n");
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .interactive(true),
                );
            });
        let _ = ctx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_parsing() {
        assert_eq!(parse_ports("443, 80,80").unwrap(), vec![80, 443]);
        assert_eq!(parse_ports("20-22").unwrap(), vec![20, 21, 22]);
        assert!(parse_ports("22-20").is_err());
        assert!(parse_ports("0").is_err());
        assert!(parse_ports("1-2000").is_err());
        assert!(parse_ports("http").is_err());
    }

    #[test]
    fn ping_localhost() {
        let lines = ping("127.0.0.1", 2).unwrap();
        assert!(
            lines.iter().any(|l| l.starts_with("Reply from 127.0.0.1")),
            "{lines:?}"
        );
        assert!(lines.last().unwrap().contains("received 2"));
    }

    #[test]
    fn dns_localhost_and_ports() {
        let a = dns_lookup("localhost", DNS_TYPE_A).unwrap();
        assert!(a.iter().any(|l| l == "127.0.0.1"), "{a:?}");
        // Bind a listener so one port is known to be open.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let r = check_ports("127.0.0.1", &[port]).unwrap();
        assert_eq!(r, vec![(port, true)]);
    }
}
