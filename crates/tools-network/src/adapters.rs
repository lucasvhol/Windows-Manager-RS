//! Network adapter enumeration (GetAdaptersAddresses) shared by the network tools.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Result, bail};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::SOCKET_ADDRESS;
use wm_core::registry;

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_TUNNEL: u32 = 131;
const IP_ADAPTER_DHCP_ENABLED: u32 = 0x4;

#[derive(Debug, Clone, PartialEq)]
pub struct Adapter {
    /// `{GUID}` used in the registry.
    pub guid: String,
    /// Name shown in Windows (and used by `netsh`).
    pub name: String,
    pub description: String,
    pub mac: String,
    pub up: bool,
    pub dhcp: bool,
    /// IPv4 addresses with prefix length.
    pub ipv4: Vec<(Ipv4Addr, u8)>,
    pub ipv6: Vec<Ipv6Addr>,
    pub gateways: Vec<IpAddr>,
    pub dns: Vec<IpAddr>,
    /// DNS servers set manually (empty = obtained automatically).
    pub static_dns: Vec<Ipv4Addr>,
    pub speed_bps: u64,
}

/// Decodes a WinSock SOCKET_ADDRESS (AF_INET or AF_INET6).
unsafe fn socket_ip(sa: &SOCKET_ADDRESS) -> Option<IpAddr> {
    if sa.lpSockaddr.is_null() {
        return None;
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(sa.lpSockaddr as *const u8, sa.iSockaddrLength as usize) };
    let family = u16::from_le_bytes(bytes.get(0..2)?.try_into().ok()?);
    match family {
        2 => Some(IpAddr::V4(Ipv4Addr::from(
            <[u8; 4]>::try_from(bytes.get(4..8)?).ok()?,
        ))),
        23 => Some(IpAddr::V6(Ipv6Addr::from(
            <[u8; 16]>::try_from(bytes.get(8..24)?).ok()?,
        ))),
        _ => None,
    }
}

/// Splits a registry DNS list ("1.1.1.1,8.8.8.8" or space separated).
pub fn parse_dns_list(s: &str) -> Vec<Ipv4Addr> {
    s.split([',', ' '])
        .filter_map(|p| p.trim().parse().ok())
        .collect()
}

fn static_dns(guid: &str) -> Vec<Ipv4Addr> {
    let key = format!(r"HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{guid}");
    registry::read_raw(&key, "NameServer")
        .map(|v| parse_dns_list(&registry::decode_string(&v)))
        .unwrap_or_default()
}

pub fn list() -> Result<Vec<Adapter>> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut size: u32 = 32 * 1024;
    let mut buf: Vec<u64>;
    loop {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        let r = unsafe {
            GetAdaptersAddresses(
                0,
                flags,
                None,
                Some(buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut size,
            )
        };
        match r {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW: size updated
            e => bail!("GetAdaptersAddresses failed ({e})"),
        }
    }
    let mut out = Vec::new();
    let mut p = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !p.is_null() {
        let a = unsafe { &*p };
        p = a.Next;
        if a.IfType == IF_TYPE_SOFTWARE_LOOPBACK || a.IfType == IF_TYPE_TUNNEL {
            continue;
        }
        let guid = unsafe { a.AdapterName.to_string() }.unwrap_or_default();
        let mut adapter = Adapter {
            name: unsafe { a.FriendlyName.to_string() }.unwrap_or_default(),
            description: unsafe { a.Description.to_string() }.unwrap_or_default(),
            mac: a.PhysicalAddress[..(a.PhysicalAddressLength as usize).min(8)]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join("-"),
            up: a.OperStatus == IfOperStatusUp,
            dhcp: unsafe { a.Anonymous2.Flags } & IP_ADAPTER_DHCP_ENABLED != 0,
            ipv4: Vec::new(),
            ipv6: Vec::new(),
            gateways: Vec::new(),
            dns: Vec::new(),
            static_dns: static_dns(&guid),
            speed_bps: a.ReceiveLinkSpeed.max(a.TransmitLinkSpeed),
            guid,
        };
        let mut u = a.FirstUnicastAddress;
        while !u.is_null() {
            let ua = unsafe { &*u };
            match unsafe { socket_ip(&ua.Address) } {
                Some(IpAddr::V4(v4)) => adapter.ipv4.push((v4, ua.OnLinkPrefixLength)),
                Some(IpAddr::V6(v6)) => adapter.ipv6.push(v6),
                None => {}
            }
            u = ua.Next;
        }
        let mut g = a.FirstGatewayAddress;
        while !g.is_null() {
            let ga = unsafe { &*g };
            adapter.gateways.extend(unsafe { socket_ip(&ga.Address) });
            g = ga.Next;
        }
        let mut d = a.FirstDnsServerAddress;
        while !d.is_null() {
            let da = unsafe { &*d };
            adapter.dns.extend(unsafe { socket_ip(&da.Address) });
            d = da.Next;
        }
        out.push(adapter);
    }
    Ok(out)
}

pub fn prefix_to_mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    };
    Ipv4Addr::from(bits)
}

/// Accepts a dotted mask ("255.255.255.0") or a prefix ("24" / "/24").
pub fn parse_mask(s: &str) -> Option<Ipv4Addr> {
    let s = s.trim().trim_start_matches('/');
    if let Ok(prefix) = s.parse::<u8>() {
        return (prefix <= 32).then(|| prefix_to_mask(prefix));
    }
    let mask: Ipv4Addr = s.parse().ok()?;
    let bits = u32::from(mask);
    // A valid mask is a run of ones followed by zeros.
    (bits.leading_ones() + bits.trailing_zeros() == 32).then_some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks() {
        assert_eq!(prefix_to_mask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(prefix_to_mask(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(prefix_to_mask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(parse_mask("/20"), Some(Ipv4Addr::new(255, 255, 240, 0)));
        assert_eq!(parse_mask("255.255.0.0"), Some(Ipv4Addr::new(255, 255, 0, 0)));
        assert_eq!(parse_mask("255.0.255.0"), None, "non-contiguous");
        assert_eq!(parse_mask("33"), None);
    }

    #[test]
    fn dns_lists() {
        assert_eq!(
            parse_dns_list("1.1.1.1,8.8.8.8"),
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
        );
        assert_eq!(parse_dns_list("9.9.9.9 bad"), vec![Ipv4Addr::new(9, 9, 9, 9)]);
        assert!(parse_dns_list("").is_empty());
    }

    #[test]
    fn real_adapters_listed() {
        let adapters = list().unwrap();
        assert!(!adapters.is_empty());
        assert!(
            adapters
                .iter()
                .all(|a| !a.name.is_empty() && a.guid.starts_with('{'))
        );
        assert!(
            adapters.iter().any(|a| a.up && !a.ipv4.is_empty()),
            "expected a connected IPv4 adapter"
        );
    }
}
