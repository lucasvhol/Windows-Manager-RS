//! Network tools (Phase 6): IP profiles and DNS, Wi-Fi profiles, hosts file,
//! diagnostics (ping, traceroute, DNS, ports, whois) and a connection monitor.

pub mod adapters;
mod hosts;
mod ip_switcher;
mod net_monitor;
mod net_tools;
mod wifi;

use wm_core::Tool;

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ip_switcher::IpSwitcher::default()),
        Box::new(wifi::WifiManager::default()),
        Box::new(hosts::UrlManager::default()),
        Box::new(net_tools::NetworkTools::default()),
        Box::new(net_monitor::NetworkMonitor::default()),
    ]
}
