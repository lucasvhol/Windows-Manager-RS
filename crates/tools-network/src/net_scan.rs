//! Network Tools helpers: reverse DNS, MAC lookup, IP range scan, traceroute with three
//! round trips per hop, IP geolocation, the router page and Windows' WLAN report.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use windows::Win32::NetworkManagement::Dns::{
    DNS_QUERY_STANDARD, DNS_RECORDA, DNS_RECORDW, DNS_TYPE_PTR, DnsFree, DnsFreeRecordList, DnsQuery_W,
};
use windows::Win32::NetworkManagement::IpHelper::SendARP;
use windows::core::HSTRING;

use crate::adapters;
use crate::net_tools::{IP_SUCCESS_CODE, IP_TTL_EXPIRED, echo, resolve_v4};

/// The host name registered for an address (PTR record), if any.
pub fn reverse_dns(ip: Ipv4Addr) -> Option<String> {
    let o = ip.octets();
    let name = format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0]);
    unsafe {
        let mut results: *mut DNS_RECORDA = std::ptr::null_mut();
        let err = DnsQuery_W(
            &HSTRING::from(name),
            DNS_TYPE_PTR,
            DNS_QUERY_STANDARD,
            None,
            &mut results,
            None,
        );
        if err.0 != 0 || results.is_null() {
            return None;
        }
        let mut out = None;
        let mut p = results as *const DNS_RECORDW;
        while !p.is_null() {
            let r = &*p;
            if r.wType == DNS_TYPE_PTR.0 {
                out = r.Data.PTR.pNameHost.to_string().ok();
                break;
            }
            p = r.pNext;
        }
        DnsFree(Some(results as *const _), DnsFreeRecordList);
        out.filter(|n| !n.is_empty())
    }
}

/// The MAC address of a host on the local network (ARP), as aa:bb:cc:dd:ee:ff.
pub fn mac_of(ip: Ipv4Addr) -> Option<String> {
    let mut mac = [0u8; 8];
    let mut len = mac.len() as u32;
    let r = unsafe {
        SendARP(
            u32::from_ne_bytes(ip.octets()),
            0,
            mac.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (r == 0 && len >= 6).then(|| {
        mac[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// Private, loopback, link-local and similar addresses that have no public location.
pub fn is_local(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.octets()[0] >= 224
        // 100.64.0.0/10, carrier-grade NAT.
        || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
}

// ---------------------------------------------------------------------------------
// IP ranges

pub const MAX_HOSTS: u32 = 1024;

/// Parses "192.168.1.0/24", "192.168.1.10-50" or "192.168.1.10-192.168.1.50" into an
/// inclusive range of host addresses (network and broadcast left out for CIDR).
pub fn parse_range(s: &str) -> Result<(Ipv4Addr, Ipv4Addr)> {
    let s = s.trim();
    let (a, b) = if let Some((ip, bits)) = s.split_once('/') {
        let ip: Ipv4Addr = ip.trim().parse().context("not an IPv4 address")?;
        let bits: u32 = bits.trim().parse().context("bad prefix length")?;
        if !(8..=32).contains(&bits) {
            bail!("use a prefix between /8 and /32");
        }
        let mask = if bits == 32 { u32::MAX } else { !(u32::MAX >> bits) };
        let net = u32::from(ip) & mask;
        let last = net | !mask;
        if bits >= 31 {
            (net, last)
        } else {
            (net + 1, last - 1)
        }
    } else if let Some((first, end)) = s.split_once('-') {
        let first: Ipv4Addr = first.trim().parse().context("not an IPv4 address")?;
        let end = end.trim();
        let last: Ipv4Addr = match end.parse::<Ipv4Addr>() {
            Ok(ip) => ip,
            Err(_) => {
                let n: u8 = end.parse().context("the end must be an address or a number")?;
                let o = first.octets();
                Ipv4Addr::new(o[0], o[1], o[2], n)
            }
        };
        (u32::from(first), u32::from(last))
    } else {
        let ip: Ipv4Addr = s
            .parse()
            .context("enter a range such as 192.168.1.1-254 or 192.168.1.0/24")?;
        (u32::from(ip), u32::from(ip))
    };
    if b < a {
        bail!("the range ends before it starts");
    }
    if b - a + 1 > MAX_HOSTS {
        bail!("at most {MAX_HOSTS} addresses at a time");
    }
    Ok((Ipv4Addr::from(a), Ipv4Addr::from(b)))
}

/// The local network of the first connected adapter with a gateway, as a range string
/// (limited to the /24 around this PC's address on larger networks).
pub fn local_range() -> Option<String> {
    let list = adapters::list().ok()?;
    let a = list
        .iter()
        .find(|a| a.up && !a.gateways.is_empty() && !a.ipv4.is_empty())?;
    let (ip, prefix) = a.ipv4[0];
    Some(format!("{ip}/{}", prefix.max(24)))
}

pub fn default_gateway() -> Option<Ipv4Addr> {
    adapters::list().ok()?.into_iter().filter(|a| a.up).find_map(|a| {
        a.gateways.into_iter().find_map(|g| match g {
            std::net::IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4),
            _ => None,
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub ip: Ipv4Addr,
    /// Ping round trip, or `None` when only ARP answered (ping blocked).
    pub ms: Option<u32>,
    pub mac: Option<String>,
    pub name: Option<String>,
}

/// Finds the hosts in a range that answer ping or ARP. `progress` gets the fraction done.
pub fn scan(
    range: (Ipv4Addr, Ipv4Addr),
    progress: &(dyn Fn(f32) + Sync),
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Vec<Host> {
    let (a, b) = (u32::from(range.0), u32::from(range.1));
    let ips: Vec<Ipv4Addr> = (a..=b).map(Ipv4Addr::from).collect();
    let total = ips.len().max(1) as f32;
    let done = std::sync::atomic::AtomicUsize::new(0);
    let threads = 128.min(ips.len().max(1));
    let chunk = ips.len().div_ceil(threads).max(1);
    let mut hosts: Vec<Host> = std::thread::scope(|s| {
        let handles: Vec<_> = ips
            .chunks(chunk)
            .map(|part| {
                let done = &done;
                s.spawn(move || {
                    let mut found = Vec::new();
                    for &ip in part {
                        if cancelled() {
                            break;
                        }
                        let ms = match echo(ip, 128, 700) {
                            Ok((code, _, ms)) if code == IP_SUCCESS_CODE => Some(ms),
                            _ => None,
                        };
                        let mac = mac_of(ip);
                        if ms.is_some() || mac.is_some() {
                            found.push(Host {
                                ip,
                                ms,
                                mac,
                                name: reverse_dns(ip),
                            });
                        }
                        let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        progress(n as f32 / total);
                    }
                    found
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    hosts.sort_by_key(|h| u32::from(h.ip));
    hosts
}

pub fn host_lines(hosts: &[Host], range: (Ipv4Addr, Ipv4Addr)) -> Vec<String> {
    let mut lines = vec![format!(
        "{} hosts found between {} and {}",
        hosts.len(),
        range.0,
        range.1
    )];
    lines.push(format!(
        "{:<16} {:>8}  {:<17}  {}",
        "Address", "Ping", "MAC address", "Name"
    ));
    for h in hosts {
        lines.push(format!(
            "{:<16} {:>8}  {:<17}  {}",
            h.ip,
            h.ms.map_or("no reply".to_owned(), |m| format!("{m} ms")),
            h.mac.as_deref().unwrap_or("-"),
            h.name.as_deref().unwrap_or("")
        ));
    }
    lines
}

// ---------------------------------------------------------------------------------
// Traceroute with three probes per hop

pub fn traceroute3(host: &str, max_hops: u8, locate: bool, mut each: impl FnMut(String)) -> Result<()> {
    let ip = resolve_v4(host)?;
    each(format!("Tracing route to {host} [{ip}], at most {max_hops} hops"));
    each(format!(
        "{:>3}  {:>7} {:>7} {:>7}  {}",
        "Hop", "RTT 1", "RTT 2", "RTT 3", "Address"
    ));
    for ttl in 1..=max_hops {
        let mut times = Vec::new();
        let mut from = None;
        let mut reached = false;
        for _ in 0..3 {
            let (status, addr, ms) = echo(ip, ttl, 1500)?;
            if status == IP_SUCCESS_CODE || status == IP_TTL_EXPIRED {
                times.push(format!("{ms} ms"));
                from = Some(addr);
                reached |= status == IP_SUCCESS_CODE;
            } else {
                times.push("*".into());
            }
        }
        let who = match from {
            Some(addr) => {
                let mut s = match reverse_dns(addr) {
                    Some(name) => format!("{name} [{addr}]"),
                    None => addr.to_string(),
                };
                if locate
                    && !is_local(addr)
                    && let Ok(g) = geolocate_ip(addr)
                {
                    let place = g.place();
                    if !place.is_empty() {
                        s += &format!("  ({place})");
                    }
                }
                s
            }
            None => "request timed out".into(),
        };
        each(format!(
            "{ttl:>3}  {:>7} {:>7} {:>7}  {who}",
            times[0], times[1], times[2]
        ));
        if reached {
            each("Trace complete.".into());
            return Ok(());
        }
    }
    each("Destination not reached within the hop limit.".into());
    Ok(())
}

// ---------------------------------------------------------------------------------
// Geolocation (ipinfo.io; only on request)

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Geo {
    pub ip: String,
    pub hostname: String,
    pub city: String,
    pub region: String,
    pub country: String,
    pub org: String,
    pub loc: String,
    pub timezone: String,
}

impl Geo {
    pub fn place(&self) -> String {
        [&self.city, &self.region, &self.country]
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn from_json(text: &str) -> Result<Self> {
        let v: serde_json::Value = serde_json::from_str(text).context("unexpected answer")?;
        if let Some(e) = v.get("error") {
            bail!(
                "{}",
                e.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("lookup failed")
            );
        }
        let f = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_owned();
        Ok(Self {
            ip: f("ip"),
            hostname: f("hostname"),
            city: f("city"),
            region: f("region"),
            country: f("country"),
            org: f("org"),
            loc: f("loc"),
            timezone: f("timezone"),
        })
    }

    pub fn lines(&self) -> Vec<String> {
        [
            ("IP address", &self.ip),
            ("Host name", &self.hostname),
            ("City", &self.city),
            ("Region", &self.region),
            ("Country", &self.country),
            ("Network", &self.org),
            ("Coordinates", &self.loc),
            ("Time zone", &self.timezone),
        ]
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k:<12} {v}"))
        .collect()
    }
}

pub fn geolocate_ip(ip: Ipv4Addr) -> Result<Geo> {
    if is_local(ip) {
        bail!("{ip} is a private or local address, which has no public location");
    }
    Geo::from_json(&wm_core::net::get_text(&format!("https://ipinfo.io/{ip}/json"))?)
}

/// Looks up a host or IP; an empty query means this connection's public address.
pub fn geolocate(query: &str) -> Result<Vec<String>> {
    let geo = if query.trim().is_empty() {
        Geo::from_json(&wm_core::net::get_text("https://ipinfo.io/json")?)?
    } else {
        geolocate_ip(resolve_v4(query.trim())?)?
    };
    let mut lines = geo.lines();
    lines.push("Location data from ipinfo.io; it is approximate.".into());
    Ok(lines)
}

// ---------------------------------------------------------------------------------
// WLAN report

pub fn wlan_report_path() -> PathBuf {
    PathBuf::from(wm_core::paths::expand_env(
        r"%ProgramData%\Microsoft\Windows\WlanReport\wlan-report-latest.html",
    ))
}

/// Has Windows write a fresh Wi-Fi report (needs administrator rights) and returns it.
pub fn make_wlan_report() -> Result<PathBuf> {
    wm_core::process::run_checked(wm_core::process::hidden_command("netsh").args([
        "wlan",
        "show",
        "wlanreport",
    ]))?;
    let p = wlan_report_path();
    anyhow::ensure!(p.exists(), "Windows didn't write the report");
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(
            parse_range("192.168.1.0/24").unwrap(),
            (ip("192.168.1.1"), ip("192.168.1.254"))
        );
        assert_eq!(
            parse_range("192.168.1.77/24").unwrap(),
            (ip("192.168.1.1"), ip("192.168.1.254"))
        );
        assert_eq!(
            parse_range("10.0.0.5-20").unwrap(),
            (ip("10.0.0.5"), ip("10.0.0.20"))
        );
        assert_eq!(
            parse_range("10.0.0.250-10.0.1.3").unwrap(),
            (ip("10.0.0.250"), ip("10.0.1.3"))
        );
        assert_eq!(parse_range("10.0.0.9").unwrap(), (ip("10.0.0.9"), ip("10.0.0.9")));
        assert!(parse_range("10.0.0.0/16").is_err(), "too many hosts");
        assert!(parse_range("10.0.0.9-3").is_err());
        assert!(parse_range("printer").is_err());
    }

    #[test]
    fn local_addresses() {
        for s in [
            "10.1.2.3",
            "192.168.0.1",
            "172.16.5.5",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "224.0.0.1",
        ] {
            assert!(is_local(s.parse().unwrap()), "{s}");
        }
        assert!(!is_local("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn geo_json() {
        let g = Geo::from_json(
            r#"{"ip":"8.8.8.8","hostname":"dns.google","city":"Mountain View","region":"California","country":"US","loc":"37.4,-122.1","org":"AS15169 Google LLC","timezone":"America/Los_Angeles"}"#,
        )
        .unwrap();
        assert_eq!(g.place(), "Mountain View, California, US");
        assert!(g.lines().iter().any(|l| l.contains("dns.google")));
        assert!(Geo::from_json(r#"{"error":{"message":"Wrong ip"}}"#).is_err());
        assert!(
            geolocate_ip("192.168.1.1".parse().unwrap()).is_err(),
            "never sent out"
        );
    }

    #[test]
    fn scanning_localhost() {
        let hosts = scan(
            ("127.0.0.1".parse().unwrap(), "127.0.0.1".parse().unwrap()),
            &|_| {},
            &|| false,
        );
        assert_eq!(hosts.len(), 1);
        assert!(hosts[0].ms.is_some());
        assert!(
            host_lines(
                &hosts,
                ("127.0.0.1".parse().unwrap(), "127.0.0.1".parse().unwrap())
            )[0]
            .starts_with("1 hosts")
        );
    }

    #[test]
    fn traceroute_to_localhost_finishes() {
        let mut lines = Vec::new();
        traceroute3("127.0.0.1", 3, false, |l| lines.push(l)).unwrap();
        assert!(lines.iter().any(|l| l == "Trace complete."), "{lines:?}");
    }
}
