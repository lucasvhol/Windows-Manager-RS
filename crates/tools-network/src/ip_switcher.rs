//! IP Switcher: save IPv4 address/DNS profiles and switch adapters between them,
//! plus one-click DNS presets. Changes use `netsh` positional syntax (locale-independent)
//! and are journaled with the commands that restore the previous configuration.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons, paths,
    process::{hidden_command, run_checked},
    registry,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Message, ToolButton, empty_state, frame, section, section_nav, show_message},
};

use crate::adapters::{self, Adapter, parse_mask, prefix_to_mask};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IpConfig {
    pub dhcp: bool,
    pub ip: Option<Ipv4Addr>,
    pub mask: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    /// Empty = DNS obtained automatically.
    pub dns: Vec<Ipv4Addr>,
}

impl IpConfig {
    pub fn of(adapter: &Adapter) -> IpConfig {
        let first = adapter.ipv4.first();
        IpConfig {
            dhcp: adapter.dhcp,
            ip: first.map(|(ip, _)| *ip),
            mask: first.map(|(_, p)| prefix_to_mask(*p)),
            gateway: adapter.gateways.iter().find_map(|g| match g {
                std::net::IpAddr::V4(v4) => Some(*v4),
                _ => None,
            }),
            dns: adapter.static_dns.clone(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !self.dhcp {
            if self.ip.is_none() || self.mask.is_none() {
                bail!("A static configuration needs an IP address and a subnet mask.");
            }
            if self.dns.is_empty() {
                bail!("A static configuration needs at least one DNS server.");
            }
        }
        Ok(())
    }
}

/// `netsh` argument lists that apply `cfg` to the adapter named `name`.
pub fn commands(name: &str, cfg: &IpConfig, include_address: bool) -> Vec<Vec<String>> {
    let s = |v: &str| v.to_owned();
    let mut cmds = Vec::new();
    if include_address {
        if cfg.dhcp {
            cmds.push(vec![
                s("interface"),
                s("ipv4"),
                s("set"),
                s("address"),
                s(name),
                s("dhcp"),
            ]);
        } else if let (Some(ip), Some(mask)) = (cfg.ip, cfg.mask) {
            let mut c = vec![
                s("interface"),
                s("ipv4"),
                s("set"),
                s("address"),
                s(name),
                s("static"),
                ip.to_string(),
                mask.to_string(),
            ];
            if let Some(gw) = cfg.gateway {
                c.push(gw.to_string());
            }
            cmds.push(c);
        }
    }
    match cfg.dns.split_first() {
        None => cmds.push(vec![
            s("interface"),
            s("ipv4"),
            s("set"),
            s("dnsservers"),
            s(name),
            s("dhcp"),
        ]),
        Some((first, rest)) => {
            cmds.push(vec![
                s("interface"),
                s("ipv4"),
                s("set"),
                s("dnsservers"),
                s(name),
                s("static"),
                first.to_string(),
                s("primary"),
                s("validate=no"),
            ]);
            for (i, d) in rest.iter().enumerate() {
                cmds.push(vec![
                    s("interface"),
                    s("ipv4"),
                    s("add"),
                    s("dnsservers"),
                    s(name),
                    d.to_string(),
                    format!("index={}", i + 2),
                    s("validate=no"),
                ]);
            }
        }
    }
    cmds
}

fn run_all(cmds: &[Vec<String>]) -> Result<()> {
    for c in cmds {
        run_checked(hidden_command("netsh").args(c)).with_context(|| format!("netsh {}", c.join(" ")))?;
    }
    Ok(())
}

fn undo_for(cmds: Vec<Vec<String>>) -> UndoAction {
    UndoAction::Sequence {
        steps: cmds
            .into_iter()
            .map(|args| UndoAction::RunCommand {
                program: "netsh".into(),
                args,
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(flatten)]
    pub config: IpConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    profile: Vec<Profile>,
}

fn store() -> Result<PathBuf> {
    Ok(paths::config_dir()?.join("ip-profiles.toml"))
}

pub fn load_profiles(path: &std::path::Path) -> Vec<Profile> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<Saved>(&t).ok())
        .map(|s| s.profile)
        .unwrap_or_default()
}

pub fn save_profiles(path: &std::path::Path, profiles: &[Profile]) -> Result<()> {
    std::fs::write(
        path,
        toml::to_string_pretty(&Saved {
            profile: profiles.to_vec(),
        })?,
    )?;
    Ok(())
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WinManager IP Switcher";

/// Whether IP Switcher opens when you sign in.
pub fn starts_with_windows() -> bool {
    registry::read_raw(RUN_KEY, RUN_VALUE).is_some()
}

pub fn set_start_with_windows(ctx: &AppCtx<'_>, on: bool) -> Result<()> {
    let snap = registry::snapshot(RUN_KEY, RUN_VALUE);
    if on {
        let exe = std::env::current_exe()?;
        registry::write_raw(
            RUN_KEY,
            RUN_VALUE,
            &registry::string(&format!("\"{}\" --open ipswitcher", exe.display())),
        )?;
    } else {
        registry::delete_value(RUN_KEY, RUN_VALUE)?;
    }
    ctx.journal.record(
        "ipswitcher",
        if on {
            "IP Switcher opens at sign-in"
        } else {
            "IP Switcher no longer opens at sign-in"
        },
        UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

/// "1 Gbps", "100 Mbps".
fn speed_text(bps: u64) -> String {
    match bps {
        0 => "unknown".into(),
        b if b >= 1_000_000_000 => format!("{:.1} Gbps", b as f64 / 1e9).replace(".0 ", " "),
        b => format!("{} Mbps", b / 1_000_000),
    }
}

pub const DNS_PRESETS: [(&str, [Ipv4Addr; 2]); 3] = [
    (
        "Cloudflare",
        [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(1, 0, 0, 1)],
    ),
    ("Google", [Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(8, 8, 4, 4)]),
    (
        "Quad9",
        [Ipv4Addr::new(9, 9, 9, 9), Ipv4Addr::new(149, 112, 112, 112)],
    ),
];

#[derive(Default)]
struct Form {
    name: String,
    dhcp: bool,
    ip: String,
    mask: String,
    gateway: String,
    dns1: String,
    dns2: String,
}

impl Form {
    fn from_config(name: &str, c: &IpConfig) -> Form {
        let show = |v: Option<Ipv4Addr>| v.map(|x| x.to_string()).unwrap_or_default();
        Form {
            name: name.to_owned(),
            dhcp: c.dhcp,
            ip: show(c.ip),
            mask: show(c.mask),
            gateway: show(c.gateway),
            dns1: show(c.dns.first().copied()),
            dns2: show(c.dns.get(1).copied()),
        }
    }

    fn to_config(&self) -> Result<IpConfig> {
        let opt = |s: &str, what: &str| -> Result<Option<Ipv4Addr>> {
            let s = s.trim();
            if s.is_empty() {
                Ok(None)
            } else {
                Ok(Some(
                    s.parse().with_context(|| format!("invalid {what} \"{s}\""))?,
                ))
            }
        };
        let mask = if self.mask.trim().is_empty() {
            None
        } else {
            Some(
                parse_mask(&self.mask)
                    .with_context(|| format!("invalid subnet mask \"{}\"", self.mask.trim()))?,
            )
        };
        let cfg = IpConfig {
            dhcp: self.dhcp,
            ip: if self.dhcp {
                None
            } else {
                opt(&self.ip, "IP address")?
            },
            mask: if self.dhcp { None } else { mask },
            gateway: if self.dhcp {
                None
            } else {
                opt(&self.gateway, "gateway")?
            },
            dns: [opt(&self.dns1, "DNS server")?, opt(&self.dns2, "DNS server")?]
                .into_iter()
                .flatten()
                .collect(),
        };
        cfg.validate()?;
        Ok(cfg)
    }
}

pub struct IpSwitcher {
    adapters: Option<Result<Vec<Adapter>, String>>,
    selected: Option<String>,
    profiles: Option<Vec<Profile>>,
    form: Form,
    task: Option<BackgroundTask<(String, UndoAction)>>,
    message: Message,
}

impl Default for IpSwitcher {
    fn default() -> Self {
        Self {
            adapters: None,
            selected: None,
            profiles: None,
            form: Form {
                dhcp: true,
                ..Default::default()
            },
            task: None,
            message: None,
        }
    }
}

impl IpSwitcher {
    fn apply(
        &mut self,
        ui: &egui::Ui,
        ctx: &AppCtx<'_>,
        adapter: &Adapter,
        cfg: IpConfig,
        address: bool,
        what: String,
    ) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let forward = commands(&adapter.name, &cfg, address);
        let undo = undo_for(commands(&adapter.name, &IpConfig::of(adapter), address));
        self.message = None;
        self.task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
            run_all(&forward)?;
            Ok((what, undo))
        }));
    }
}

impl Tool for IpSwitcher {
    fn id(&self) -> &'static str {
        "ipswitcher"
    }

    fn category(&self) -> Category {
        Category::Network
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.adapters.is_none() {
            self.adapters = Some(adapters::list().map_err(|e| format!("{e:#}")));
        }
        if self.profiles.is_none() {
            self.profiles = Some(store().map(|p| load_profiles(&p)).unwrap_or_default());
        }
        if let Some(t) = &mut self.task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.task = None;
                self.message = Some(r.map(|(what, undo)| {
                    let _ = ctx.journal.record("ipswitcher", what.clone(), undo);
                    format!("{what}.")
                }));
                self.adapters = None;
            }
        }
        let adapters = match &self.adapters {
            Some(Ok(a)) => a.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        if self.selected.is_none() {
            self.selected = adapters
                .iter()
                .find(|a| a.up && !a.ipv4.is_empty())
                .map(|a| a.guid.clone());
        }

        let labels: Vec<String> = adapters
            .iter()
            .map(|a| format!("{}{}", a.name, if a.up { "" } else { " (off)" }))
            .collect();
        let nav: Vec<(&str, &str)> = adapters
            .iter()
            .zip(&labels)
            .map(|(a, l)| (if a.up { icons::PLUGS } else { icons::CLOUD_SLASH }, l.as_str()))
            .collect();
        let mut sel = self
            .selected
            .as_ref()
            .and_then(|g| adapters.iter().position(|a| &a.guid == g))
            .unwrap_or(0);
        frame::left(ui, 200.0, |ui| {
            ui.weak("Adapters");
            ui.add_space(4.0);
            section_nav(ui, &nav, &mut sel)
        });
        self.selected = adapters.get(sel).map(|a| a.guid.clone());
        let Some(adapter) = adapters.get(sel).cloned() else {
            frame::central(ui, |ui| {
                empty_state(ui, icons::PLUGS, "No network adapters", "");
            });
            return;
        };
        let busy = self.task.is_some();
        let can = ctx.elevated && !busy;

        let mut buttons = vec![
            Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh")),
            None,
            Some(ToolButton::new(icons::GLOBE, "Automatic DNS").enabled(can)),
        ];
        buttons.extend(
            DNS_PRESETS
                .iter()
                .map(|(name, _)| Some(ToolButton::new(icons::GLOBE, name).enabled(can))),
        );
        let mut autostart = None;
        let clicked = frame::toolbar(ui, &buttons, |ui| {
            let mut on = starts_with_windows();
            if ui
                .checkbox(&mut on, "Open IP Switcher when I sign in")
                .on_hover_text("Handy on a laptop that moves between networks.")
                .changed()
            {
                autostart = Some(on);
            }
        });
        match clicked {
            Some(0) => self.adapters = None,
            Some(2) => {
                let cfg = IpConfig {
                    dns: Vec::new(),
                    ..IpConfig::of(&adapter)
                };
                self.apply(
                    ui,
                    ctx,
                    &adapter,
                    cfg,
                    false,
                    format!("Set DNS of {} to automatic", adapter.name),
                );
            }
            Some(i) if i >= 3 => {
                let (name, servers) = DNS_PRESETS[i - 3];
                let cfg = IpConfig {
                    dns: servers.to_vec(),
                    ..IpConfig::of(&adapter)
                };
                self.apply(
                    ui,
                    ctx,
                    &adapter,
                    cfg,
                    false,
                    format!("Set DNS of {} to {name}", adapter.name),
                );
            }
            _ => {}
        }
        if let Some(on) = autostart {
            self.message = Some(if ctx.config.dry_run {
                Ok(ctx.i18n.tr("common-dry-run-skip"))
            } else {
                set_start_with_windows(ctx, on)
                    .map(|()| "Saved.".into())
                    .map_err(|e| format!("{e:#}"))
            });
        }
        let status = if adapter.up {
            format!("{}: connected at {}", adapter.name, speed_text(adapter.speed_bps))
        } else {
            format!("{}: disconnected", adapter.name)
        };
        frame::status(ui, &status, |ui| {
            if busy {
                ui.spinner();
            } else if !ctx.elevated {
                ui.weak("Applying a profile or DNS needs the app to run as administrator.");
            }
        });

        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            section(
                ui,
                &adapter.name,
                |_| {},
                |ui| {
                    egui::Grid::new("adapter_info")
                        .num_columns(2)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            let rows = [
                                ("Description", adapter.description.clone()),
                                ("MAC address", adapter.mac.clone()),
                                (
                                    "Address",
                                    if adapter.dhcp {
                                        "Automatic (DHCP)".to_owned()
                                    } else {
                                        "Manual".to_owned()
                                    },
                                ),
                                (
                                    "IPv4",
                                    adapter
                                        .ipv4
                                        .iter()
                                        .map(|(ip, p)| format!("{ip}/{p}"))
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                ),
                                (
                                    "Gateway",
                                    adapter
                                        .gateways
                                        .iter()
                                        .map(ToString::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                ),
                                (
                                    "DNS",
                                    format!(
                                        "{}{}",
                                        adapter
                                            .dns
                                            .iter()
                                            .map(ToString::to_string)
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                        if adapter.static_dns.is_empty() {
                                            " (automatic)"
                                        } else {
                                            " (manual)"
                                        }
                                    ),
                                ),
                            ];
                            for (k, v) in rows {
                                ui.weak(k);
                                ui.label(v);
                                ui.end_row();
                            }
                        });
                },
            );
            ui.add_space(8.0);

            let profiles = self.profiles.clone().unwrap_or_default();
            let mut delete = None;
            section(
                ui,
                "Profiles",
                |_| {},
                |ui| {
                    if profiles.is_empty() {
                        ui.weak("No profiles yet. Fill in the form below and save one.");
                    }
                    for (i, p) in profiles.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.strong(&p.name);
                            let summary = if p.config.dhcp {
                                "automatic address".to_owned()
                            } else {
                                format!(
                                    "{} / {}",
                                    p.config.ip.map(|x| x.to_string()).unwrap_or_default(),
                                    p.config.mask.map(|x| x.to_string()).unwrap_or_default()
                                )
                            };
                            ui.weak(summary);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Delete").clicked() {
                                    delete = Some(i);
                                }
                                if ui.small_button("Edit").clicked() {
                                    self.form = Form::from_config(&p.name, &p.config);
                                }
                                if ui.add_enabled(can, egui::Button::new("Apply").small()).clicked() {
                                    self.apply(
                                        ui,
                                        ctx,
                                        &adapter,
                                        p.config.clone(),
                                        true,
                                        format!("Applied IP profile \"{}\" to {}", p.name, adapter.name),
                                    );
                                }
                            });
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.name)
                                .hint_text("Profile name")
                                .desired_width(160.0),
                        );
                        ui.checkbox(&mut self.form.dhcp, "Obtain address automatically");
                        if ui.small_button("Fill from current").clicked() {
                            self.form = Form::from_config(&self.form.name, &IpConfig::of(&adapter));
                        }
                        if ui
                            .small_button("New")
                            .on_hover_text("Clear the form for a new profile")
                            .clicked()
                        {
                            self.form = Form {
                                dhcp: true,
                                ..Default::default()
                            };
                        }
                        if profiles.iter().any(|p| p.name == self.form.name.trim()) {
                            ui.weak("Saving replaces this profile.");
                        }
                    });
                    ui.add_enabled_ui(!self.form.dhcp, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.ip)
                                    .hint_text("IP address")
                                    .desired_width(120.0),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.mask)
                                    .hint_text("Mask or /24")
                                    .desired_width(120.0),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.gateway)
                                    .hint_text("Gateway")
                                    .desired_width(120.0),
                            );
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.dns1)
                                .hint_text("DNS 1 (empty = auto)")
                                .desired_width(140.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.form.dns2)
                                .hint_text("DNS 2")
                                .desired_width(140.0),
                        );
                        if ui
                            .button(format!("{}  Save profile", icons::FLOPPY_DISK))
                            .clicked()
                        {
                            self.message = Some(
                                self.form
                                    .to_config()
                                    .and_then(|config| {
                                        let name = self.form.name.trim().to_owned();
                                        if name.is_empty() {
                                            bail!("Enter a profile name.");
                                        }
                                        let mut list = self.profiles.clone().unwrap_or_default();
                                        list.retain(|p| p.name != name);
                                        list.push(Profile {
                                            name: name.clone(),
                                            config,
                                        });
                                        save_profiles(&store()?, &list)?;
                                        self.profiles = Some(list);
                                        Ok(format!("Saved profile \"{name}\"."))
                                    })
                                    .map_err(|e| format!("{e:#}")),
                            );
                        }
                    });
                },
            );
            if let Some(i) = delete {
                let mut list = profiles.clone();
                list.remove(i);
                let _ = store().and_then(|p| save_profiles(&p, &list));
                self.profiles = Some(list);
            }
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netsh_commands() {
        let dhcp = IpConfig {
            dhcp: true,
            ..Default::default()
        };
        assert_eq!(
            commands("Wi-Fi 2", &dhcp, true),
            vec![
                vec!["interface", "ipv4", "set", "address", "Wi-Fi 2", "dhcp"],
                vec!["interface", "ipv4", "set", "dnsservers", "Wi-Fi 2", "dhcp"],
            ]
        );
        let st = IpConfig {
            dhcp: false,
            ip: Some(Ipv4Addr::new(192, 168, 1, 50)),
            mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
            gateway: Some(Ipv4Addr::new(192, 168, 1, 1)),
            dns: vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(9, 9, 9, 9)],
        };
        let c = commands("Ethernet", &st, true);
        assert_eq!(
            c[0],
            [
                "interface",
                "ipv4",
                "set",
                "address",
                "Ethernet",
                "static",
                "192.168.1.50",
                "255.255.255.0",
                "192.168.1.1"
            ]
        );
        assert_eq!(c[1][5..], ["static", "1.1.1.1", "primary", "validate=no"]);
        assert_eq!(c[2][5..], ["9.9.9.9", "index=2", "validate=no"]);
        assert_eq!(commands("Ethernet", &st, false).len(), 2, "DNS-only change");
    }

    #[test]
    fn form_validation() {
        let mut f = Form {
            name: "Office".into(),
            dhcp: false,
            ip: "10.0.0.5".into(),
            mask: "/8".into(),
            ..Default::default()
        };
        assert!(f.to_config().is_err(), "static needs DNS");
        f.dns1 = "10.0.0.1".into();
        let c = f.to_config().unwrap();
        assert_eq!(c.mask, Some(Ipv4Addr::new(255, 0, 0, 0)));
        f.ip = "10.0.0.999".into();
        assert!(f.to_config().is_err());
        let auto = Form {
            dhcp: true,
            ..Default::default()
        };
        assert_eq!(
            auto.to_config().unwrap(),
            IpConfig {
                dhcp: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn profile_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.toml");
        let profiles = vec![
            Profile {
                name: "Home".into(),
                config: IpConfig {
                    dhcp: true,
                    ..Default::default()
                },
            },
            Profile {
                name: "Lab".into(),
                config: IpConfig {
                    dhcp: false,
                    ip: Some(Ipv4Addr::new(10, 1, 1, 2)),
                    mask: Some(Ipv4Addr::new(255, 255, 0, 0)),
                    gateway: None,
                    dns: vec![Ipv4Addr::new(10, 1, 1, 1)],
                },
            },
        ];
        save_profiles(&path, &profiles).unwrap();
        assert_eq!(load_profiles(&path), profiles);
    }

    #[test]
    fn start_with_windows_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        let before = starts_with_windows();
        set_start_with_windows(&ctx, !before).unwrap();
        assert_eq!(starts_with_windows(), !before);
        let e = journal.entries().into_iter().max_by_key(|e| e.id).unwrap();
        journal.undo(e.id).unwrap();
        assert_eq!(starts_with_windows(), before, "undo restores the user's setting");
        assert_eq!(speed_text(1_000_000_000), "1 Gbps");
        assert_eq!(speed_text(100_000_000), "100 Mbps");
    }
}
