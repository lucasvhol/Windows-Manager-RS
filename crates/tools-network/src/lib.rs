//! Network tools (Phase 6): IP profiles and DNS, Wi-Fi profiles, hosts file,
//! diagnostics (ping, traceroute, DNS, ports, whois) and a connection monitor.

pub mod adapters;
mod hosts;
mod ip_switcher;
mod net_monitor;
mod net_tools;
mod wifi;

use wm_core::{Category, Tool, tweaks::TweakTool};

/// System Network tweak definitions.
pub const NETWORK_TWEAKS: &str = include_str!("../tweaks/network.toml");

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ip_switcher::IpSwitcher::default()),
        Box::new(wifi::WifiManager::default()),
        Box::new(hosts::UrlManager::default()),
        Box::new(net_tools::NetworkTools::default()),
        Box::new(net_monitor::NetworkMonitor::default()),
        Box::new(TweakTool::new(
            Category::Network,
            "systemnetwork",
            "",
            NETWORK_TWEAKS,
        )),
    ]
}
