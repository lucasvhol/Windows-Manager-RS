//! Wi-Fi Manager: saved Wi-Fi profiles (view key, delete with undo) and nearby networks.
//! Uses the native WLAN API, so it works on any Windows display language.
//!
//! Deleted profiles are backed up with the key still *encrypted* (as Windows stores it);
//! undo re-imports that copy with `netsh wlan add profile`, so no plaintext password is
//! ever written to disk.

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    WLAN_AVAILABLE_NETWORK_LIST, WLAN_INTERFACE_INFO_LIST, WLAN_PROFILE_GET_PLAINTEXT_KEY,
    WLAN_PROFILE_INFO_LIST, WlanCloseHandle, WlanDeleteProfile, WlanEnumInterfaces, WlanFreeMemory,
    WlanGetAvailableNetworkList, WlanGetProfile, WlanGetProfileList, WlanOpenHandle, WlanScan,
};
use windows::core::{GUID, HSTRING, PWSTR};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, paths,
    safety::UndoAction,
    ui::{Confirm, Message, ToolButton, empty_state, frame, section, show_message},
};

/// ERROR_SERVICE_NOT_ACTIVE: the WLAN AutoConfig service is not running (no Wi-Fi hardware).
const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;

fn wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// First `<tag>…</tag>` text in an XML string (profile XML is simple and well-formed).
pub fn xml_value(xml: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(
        xml[start..end]
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">"),
    )
}

struct Client(HANDLE);

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.0, None);
        }
    }
}

fn check(code: u32, what: &str) -> Result<()> {
    if code != 0 {
        bail!(
            "{what} failed: {}",
            windows::core::Error::from(windows::core::HRESULT::from_win32(code)).message()
        );
    }
    Ok(())
}

fn open() -> Result<Option<Client>> {
    let (mut version, mut h) = (0u32, HANDLE::default());
    let r = unsafe { WlanOpenHandle(2, None, &mut version, &mut h) };
    if r == ERROR_SERVICE_NOT_ACTIVE {
        return Ok(None);
    }
    check(r, "opening the WLAN service")?;
    Ok(Some(Client(h)))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    pub guid: GUID,
    pub description: String,
}

fn interfaces(c: &Client) -> Result<Vec<Interface>> {
    let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
    check(
        unsafe { WlanEnumInterfaces(c.0, None, &mut list) },
        "listing Wi-Fi adapters",
    )?;
    let out = unsafe {
        let l = &*list;
        std::slice::from_raw_parts(l.InterfaceInfo.as_ptr(), l.dwNumberOfItems as usize)
            .iter()
            .map(|i| Interface {
                guid: i.InterfaceGuid,
                description: wide(&i.strInterfaceDescription),
            })
            .collect()
    };
    unsafe { WlanFreeMemory(list as *const _) };
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Profile {
    pub name: String,
    pub ssid: String,
    pub authentication: String,
    pub auto_connect: bool,
}

fn profile_xml(c: &Client, iface: &GUID, name: &str, plaintext: bool) -> Result<String> {
    let mut xml = PWSTR::null();
    let mut flags = if plaintext {
        WLAN_PROFILE_GET_PLAINTEXT_KEY
    } else {
        0
    };
    check(
        unsafe {
            WlanGetProfile(
                c.0,
                iface,
                &HSTRING::from(name),
                None,
                &mut xml,
                Some(&mut flags),
                None,
            )
        },
        "reading the profile",
    )?;
    let text = unsafe { xml.to_string() }.unwrap_or_default();
    unsafe { WlanFreeMemory(xml.0 as *const _) };
    Ok(text)
}

fn profiles(c: &Client, iface: &GUID) -> Result<Vec<Profile>> {
    let mut list: *mut WLAN_PROFILE_INFO_LIST = std::ptr::null_mut();
    check(
        unsafe { WlanGetProfileList(c.0, iface, None, &mut list) },
        "listing profiles",
    )?;
    let names: Vec<String> = unsafe {
        let l = &*list;
        std::slice::from_raw_parts(l.ProfileInfo.as_ptr(), l.dwNumberOfItems as usize)
            .iter()
            .map(|p| wide(&p.strProfileName))
            .collect()
    };
    unsafe { WlanFreeMemory(list as *const _) };
    Ok(names
        .into_iter()
        .map(|name| {
            let xml = profile_xml(c, iface, &name, false).unwrap_or_default();
            Profile {
                ssid: xml_value(&xml, "name").unwrap_or_else(|| name.clone()),
                authentication: xml_value(&xml, "authentication").unwrap_or_default(),
                auto_connect: xml_value(&xml, "connectionMode").as_deref() == Some("auto"),
                name,
            }
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub ssid: String,
    pub signal: u32,
    pub secured: bool,
    pub saved: bool,
}

fn networks(c: &Client, iface: &GUID) -> Result<Vec<Network>> {
    unsafe {
        let _ = WlanScan(c.0, iface, None, None, None); // asynchronous; results appear on the next refresh
    }
    let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
    check(
        unsafe { WlanGetAvailableNetworkList(c.0, iface, 0, None, &mut list) },
        "listing networks",
    )?;
    let mut out: Vec<Network> = unsafe {
        let l = &*list;
        std::slice::from_raw_parts(l.Network.as_ptr(), l.dwNumberOfItems as usize)
            .iter()
            .map(|n| Network {
                ssid: String::from_utf8_lossy(
                    &n.dot11Ssid.ucSSID[..(n.dot11Ssid.uSSIDLength as usize).min(32)],
                )
                .into_owned(),
                signal: n.wlanSignalQuality,
                secured: n.bSecurityEnabled.as_bool(),
                saved: n.strProfileName[0] != 0,
            })
            .collect()
    };
    unsafe { WlanFreeMemory(list as *const _) };
    // The list has one entry per profile/SSID combination; keep the strongest per SSID.
    out.sort_by_key(|n| std::cmp::Reverse(n.signal));
    let mut seen = std::collections::HashSet::new();
    out.retain(|n| !n.ssid.is_empty() && seen.insert(n.ssid.clone()));
    Ok(out)
}

fn delete(c: &Client, iface: &GUID, name: &str) -> Result<UndoAction> {
    let xml = profile_xml(c, iface, name, false).context("backing up the profile")?;
    let safe: String = name
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
        .collect();
    let file = paths::backups_dir()?.join(format!(
        "{}-wifi-{safe}.xml",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(&file, xml)?;
    check(
        unsafe { WlanDeleteProfile(c.0, iface, &HSTRING::from(name), None) },
        "deleting the profile",
    )?;
    Ok(UndoAction::RunCommand {
        program: "netsh".into(),
        args: vec![
            "wlan".into(),
            "add".into(),
            "profile".into(),
            file.display().to_string(),
        ],
    })
}

#[derive(Default)]
struct State {
    interfaces: Vec<Interface>,
    profiles: Vec<Profile>,
    networks: Vec<Network>,
}

#[derive(Default)]
pub struct WifiManager {
    state: Option<Result<Option<State>, String>>,
    iface: usize,
    revealed: std::collections::HashMap<String, String>,
    confirm: Confirm<String>,
    message: Message,
}

impl WifiManager {
    fn load(&self) -> Result<Option<State>> {
        let Some(c) = open()? else { return Ok(None) };
        let interfaces = interfaces(&c)?;
        let Some(i) = interfaces.get(self.iface.min(interfaces.len().saturating_sub(1))) else {
            return Ok(Some(State::default()));
        };
        Ok(Some(State {
            profiles: profiles(&c, &i.guid)?,
            networks: networks(&c, &i.guid).unwrap_or_default(),
            interfaces,
        }))
    }
}

impl Tool for WifiManager {
    fn id(&self) -> &'static str {
        "wifi"
    }

    fn category(&self) -> Category {
        Category::Network
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if frame::toolbar(
            ui,
            &[Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh"))],
            |_| {},
        ) == Some(0)
        {
            self.state = None;
            self.message = None;
        }
        if self.state.is_none() {
            self.state = Some(self.load().map_err(|e| format!("{e:#}")));
        }
        let unavailable = match &self.state {
            Some(Ok(Some(s))) if s.interfaces.is_empty() => Some((
                "No Wi-Fi adapter found".to_owned(),
                "Plug in a Wi-Fi adapter, then press Refresh.".to_owned(),
            )),
            Some(Ok(Some(_))) => None,
            Some(Ok(None)) | None => Some((
                "No Wi-Fi on this PC".to_owned(),
                "The WLAN AutoConfig service is not running; this PC probably has no Wi-Fi adapter."
                    .to_owned(),
            )),
            Some(Err(e)) => Some(("Wi-Fi is unavailable".to_owned(), e.clone())),
        };
        if let Some((title, hint)) = unavailable {
            frame::status(ui, "", |_| {});
            frame::central(ui, |ui| empty_state(ui, icons::WIFI_HIGH, &title, &hint));
            return;
        }
        let Some(Ok(Some(state))) = &self.state else {
            return;
        };
        let iface = state.interfaces[self.iface.min(state.interfaces.len() - 1)].clone();
        let profiles = state.profiles.clone();
        let networks = state.networks.clone();

        if let Some(name) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                open()
                    .and_then(|c| c.context("WLAN service unavailable"))
                    .and_then(|c| delete(&c, &iface.guid, &name))
                    .and_then(|undo| {
                        ctx.journal
                            .record("wifi", format!("Deleted Wi-Fi profile \"{name}\""), undo)
                    })
                    .map(|_| format!("Deleted \"{name}\"."))
                    .map_err(|e| format!("{e:#}"))
            });
            self.state = None;
        }

        frame::status(
            ui,
            &format!(
                "{}: {} saved networks, {} in range",
                iface.description,
                profiles.len(),
                networks.len()
            ),
            |_| {},
        );
        let mut forget = None;
        let mut reveal = None;
        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            section(
                ui,
                &format!("Saved networks ({})", profiles.len()),
                |_| {},
                |ui| {
                    egui::Grid::new("wifi_profiles")
                        .num_columns(4)
                        .striped(true)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            for p in &profiles {
                                ui.label(&p.name);
                                ui.weak(format!(
                                    "{}{}",
                                    p.authentication,
                                    if p.auto_connect {
                                        " · connects automatically"
                                    } else {
                                        ""
                                    }
                                ));
                                match self.revealed.get(&p.name) {
                                    Some(key) => {
                                        ui.add(
                                            egui::Label::new(egui::RichText::new(key).monospace())
                                                .selectable(true),
                                        );
                                    }
                                    None => {
                                        if ui.small_button("Show password").clicked() {
                                            reveal = Some(p.name.clone());
                                        }
                                    }
                                }
                                if ui.small_button("Forget").clicked() {
                                    forget = Some(p.name.clone());
                                }
                                ui.end_row();
                            }
                        });
                },
            );
            ui.add_space(8.0);
            section(
                ui,
                &format!("Networks in range ({})", networks.len()),
                |_| {},
                |ui| {
                    egui::Grid::new("wifi_networks")
                        .num_columns(3)
                        .striped(true)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            for n in &networks {
                                ui.label(&n.ssid);
                                ui.add(
                                    egui::ProgressBar::new(n.signal as f32 / 100.0)
                                        .desired_width(120.0)
                                        .text(format!("{}%", n.signal)),
                                );
                                ui.weak(format!(
                                    "{}{}",
                                    if n.secured { "secured" } else { "open" },
                                    if n.saved { " · saved" } else { "" }
                                ));
                                ui.end_row();
                            }
                        });
                },
            );
        });
        if let Some(name) = reveal {
            let key = open()
                .ok()
                .flatten()
                .and_then(|c| profile_xml(&c, &iface.guid, &name, true).ok())
                .map(|xml| {
                    match (
                        xml_value(&xml, "keyMaterial"),
                        xml_value(&xml, "protected").as_deref(),
                    ) {
                        (Some(k), Some("false")) => k,
                        (Some(_), _) => "(restart as administrator to show)".into(),
                        (None, _) => "(open network or enterprise login)".into(),
                    }
                })
                .unwrap_or_else(|| "(unavailable)".into());
            self.revealed.insert(name, key);
        }
        if let Some(name) = forget {
            self.confirm.ask(
                format!("Forget the Wi-Fi network \"{name}\"? A backup is kept so it can be restored from the Change Journal."),
                name,
            );
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
    fn profile_xml_fields() {
        let xml = "<WLANProfile><name>Home</name><SSIDConfig><SSID><name>Home &amp; Co</name></SSID></SSIDConfig>\
                   <connectionMode>auto</connectionMode><MSM><security><authEncryption><authentication>WPA2PSK</authentication>\
                   </authEncryption><sharedKey><keyType>passPhrase</keyType><protected>false</protected>\
                   <keyMaterial>s3cret</keyMaterial></sharedKey></security></MSM></WLANProfile>";
        assert_eq!(xml_value(xml, "name").as_deref(), Some("Home"));
        assert_eq!(xml_value(xml, "authentication").as_deref(), Some("WPA2PSK"));
        assert_eq!(xml_value(xml, "keyMaterial").as_deref(), Some("s3cret"));
        assert_eq!(xml_value(xml, "missing"), None);
    }

    #[test]
    fn wlan_service_query_does_not_fail() {
        // Works with or without Wi-Fi hardware: either no service, or a (possibly empty) list.
        if let Some(c) = open().unwrap() {
            let _ = interfaces(&c).unwrap();
        }
    }
}
