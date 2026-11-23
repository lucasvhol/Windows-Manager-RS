//! Service Manager: list Win32 services, start/stop them and change their startup type.
//! Changes are journaled with the reverse command; core OS services are protected.

use anyhow::{Result, bail};
use egui_extras::{Column, TableBuilder};
use windows::Win32::System::Services::{
    CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, OpenSCManagerW, OpenServiceW,
    QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW, SC_ENUM_PROCESS_INFO, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_AUTO_START, SERVICE_BOOT_START,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION, SERVICE_DELAYED_AUTO_START_INFO,
    SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_DISABLED, SERVICE_PAUSED, SERVICE_QUERY_CONFIG,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATE_ALL, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_SYSTEM_START, SERVICE_WIN32,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use wm_core::{
    AppCtx, Category, Tool, egui,
    process::hidden_command,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, show_message},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Stopped,
    Starting,
    Stopping,
    Paused,
    Other,
}

impl RunState {
    pub fn label(self) -> &'static str {
        match self {
            RunState::Running => "Running",
            RunState::Stopped => "Stopped",
            RunState::Starting => "Starting",
            RunState::Stopping => "Stopping",
            RunState::Paused => "Paused",
            RunState::Other => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartType {
    Automatic,
    AutomaticDelayed,
    Manual,
    Disabled,
    Boot,
    System,
}

impl StartType {
    pub const EDITABLE: [StartType; 4] = [
        StartType::Automatic,
        StartType::AutomaticDelayed,
        StartType::Manual,
        StartType::Disabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StartType::Automatic => "Automatic",
            StartType::AutomaticDelayed => "Automatic (delayed)",
            StartType::Manual => "Manual",
            StartType::Disabled => "Disabled",
            StartType::Boot => "Boot",
            StartType::System => "System",
        }
    }

    /// Value for `sc config <name> start= <value>`.
    pub fn sc_value(self) -> Option<&'static str> {
        match self {
            StartType::Automatic => Some("auto"),
            StartType::AutomaticDelayed => Some("delayed-auto"),
            StartType::Manual => Some("demand"),
            StartType::Disabled => Some("disabled"),
            StartType::Boot | StartType::System => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub name: String,
    pub display: String,
    pub state: RunState,
    pub start: StartType,
    pub pid: u32,
    pub description: String,
    pub binary: String,
}

/// Services whose stopping or disabling breaks Windows (sign-in, networking, updates, security).
const PROTECTED: &[&str] = &[
    "BFE",
    "BrokerInfrastructure",
    "CoreMessagingRegistrar",
    "CryptSvc",
    "DcomLaunch",
    "Dhcp",
    "Dnscache",
    "EventLog",
    "gpsvc",
    "LSM",
    "mpssvc",
    "nsi",
    "PlugPlay",
    "Power",
    "ProfSvc",
    "RpcEptMapper",
    "RpcSs",
    "SamSs",
    "Schedule",
    "SecurityHealthService",
    "SENS",
    "SgrmBroker",
    "StateRepository",
    "SystemEventsBroker",
    "TimeBrokerSvc",
    "UserManager",
    "WinDefend",
    "Winmgmt",
    "WdNisSvc",
    "Audiosrv",
    "AudioEndpointBuilder",
    "Themes",
    "ShellHWDetection",
    "LanmanWorkstation",
    "wscsvc",
    "TrustedInstaller",
];

pub fn is_protected(name: &str) -> bool {
    PROTECTED.iter().any(|p| p.eq_ignore_ascii_case(name))
}

/// Owns a service-control handle.
struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

unsafe fn pwstr(p: PWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { p.to_string().unwrap_or_default() }
    }
}

fn query_config(scm: &Sc, name: &str) -> Option<(StartType, String, String)> {
    unsafe {
        let svc = Sc(OpenServiceW(scm.0, &HSTRING::from(name), SERVICE_QUERY_CONFIG).ok()?);

        let mut needed = 0u32;
        let _ = QueryServiceConfigW(svc.0, None, 0, &mut needed);
        // u64 buffer keeps the struct correctly aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let cfg = buf.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        QueryServiceConfigW(svc.0, Some(cfg), (buf.len() * 8) as u32, &mut needed).ok()?;
        let cfg = &*cfg;

        let delayed = {
            let mut b = [0u8; size_of::<SERVICE_DELAYED_AUTO_START_INFO>()];
            QueryServiceConfig2W(
                svc.0,
                SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                Some(&mut b),
                &mut needed,
            )
            .is_ok()
                && b[0] != 0
        };
        let start = match cfg.dwStartType {
            SERVICE_AUTO_START if delayed => StartType::AutomaticDelayed,
            SERVICE_AUTO_START => StartType::Automatic,
            SERVICE_DEMAND_START => StartType::Manual,
            SERVICE_DISABLED => StartType::Disabled,
            SERVICE_BOOT_START => StartType::Boot,
            SERVICE_SYSTEM_START => StartType::System,
            _ => StartType::Manual,
        };
        let binary = pwstr(cfg.lpBinaryPathName);

        let mut needed = 0u32;
        let _ = QueryServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, None, &mut needed);
        let mut dbuf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        let description = if QueryServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some(std::slice::from_raw_parts_mut(
                dbuf.as_mut_ptr().cast::<u8>(),
                dbuf.len() * 8,
            )),
            &mut needed,
        )
        .is_ok()
        {
            pwstr((*dbuf.as_ptr().cast::<SERVICE_DESCRIPTIONW>()).lpDescription)
        } else {
            String::new()
        };
        Some((start, binary, description))
    }
}

pub fn enumerate() -> Result<Vec<Service>> {
    unsafe {
        let scm = Sc(OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        )?);
        let (mut needed, mut count, mut resume) = (0u32, 0u32, 0u32);
        let _ = EnumServicesStatusExW(
            scm.0,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            None,
            &mut needed,
            &mut count,
            Some(&mut resume),
            PCWSTR::null(),
        );
        let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 64];
        resume = 0;
        EnumServicesStatusExW(
            scm.0,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            Some(std::slice::from_raw_parts_mut(
                buf.as_mut_ptr().cast::<u8>(),
                buf.len() * 8,
            )),
            &mut needed,
            &mut count,
            Some(&mut resume),
            PCWSTR::null(),
        )?;
        let entries = std::slice::from_raw_parts(
            buf.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
            count as usize,
        );
        let mut services: Vec<Service> = entries
            .iter()
            .map(|e| {
                let name = pwstr(e.lpServiceName);
                let st = &e.ServiceStatusProcess;
                let state = match st.dwCurrentState {
                    SERVICE_RUNNING => RunState::Running,
                    SERVICE_STOPPED => RunState::Stopped,
                    SERVICE_START_PENDING => RunState::Starting,
                    SERVICE_STOP_PENDING => RunState::Stopping,
                    SERVICE_PAUSED => RunState::Paused,
                    _ => RunState::Other,
                };
                let (start, binary, description) =
                    query_config(&scm, &name).unwrap_or((StartType::Manual, String::new(), String::new()));
                Service {
                    display: pwstr(e.lpDisplayName),
                    name,
                    state,
                    start,
                    pid: st.dwProcessId,
                    description,
                    binary,
                }
            })
            .collect();
        services.sort_by_key(|s| s.display.to_lowercase());
        Ok(services)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    SetStart(StartType),
}

/// Runs a service command synchronously (net start/stop wait for completion).
fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = hidden_command(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned()
            + " "
            + String::from_utf8_lossy(&out.stderr).trim();
        bail!("{}", text.trim());
    }
    Ok(())
}

/// Description and undo action of a completed change.
pub type Outcome = (String, UndoAction);

fn cmd(program: &str, args: &[&str]) -> UndoAction {
    UndoAction::RunCommand {
        program: program.into(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
    }
}

pub fn perform(service: &Service, action: &Action) -> Result<Outcome> {
    let name = service.name.as_str();
    let protected = is_protected(name);
    match action {
        Action::Start => {
            run("net", &["start", name])?;
            Ok((
                format!("Started service {}", service.display),
                cmd("net", &["stop", name]),
            ))
        }
        Action::Stop => {
            if protected {
                bail!(
                    "{} is a core Windows service and cannot be stopped here.",
                    service.display
                );
            }
            run("net", &["stop", name])?;
            Ok((
                format!("Stopped service {}", service.display),
                cmd("net", &["start", name]),
            ))
        }
        Action::Restart => {
            if protected {
                bail!(
                    "{} is a core Windows service and cannot be restarted here.",
                    service.display
                );
            }
            run("net", &["stop", name])?;
            run("net", &["start", name])?;
            Ok((format!("Restarted service {}", service.display), UndoAction::None))
        }
        Action::SetStart(new) => {
            if protected && matches!(new, StartType::Disabled | StartType::Manual) {
                bail!(
                    "{} is a core Windows service; its startup type is protected.",
                    service.display
                );
            }
            let (Some(new_v), Some(old_v)) = (new.sc_value(), service.start.sc_value()) else {
                bail!("The startup type of {} cannot be changed.", service.display);
            };
            run("sc", &["config", name, "start=", new_v])?;
            Ok((
                format!(
                    "Changed {} startup from {} to {}",
                    service.display,
                    service.start.label(),
                    new.label()
                ),
                cmd("sc", &["config", name, "start=", old_v]),
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Show {
    All,
    Running,
    Automatic,
    Disabled,
}

pub struct ServiceManager {
    load: Option<BackgroundTask<Vec<Service>>>,
    services: Option<Result<Vec<Service>, String>>,
    action: Option<BackgroundTask<Outcome>>,
    filter: String,
    show: Show,
    selected: Option<String>,
    confirm: Confirm<(Service, Action)>,
    message: Message,
}

impl Default for ServiceManager {
    fn default() -> Self {
        Self {
            load: None,
            services: None,
            action: None,
            filter: String::new(),
            show: Show::All,
            selected: None,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl ServiceManager {
    fn start_action(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>, service: Service, action: Action) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        self.message = None;
        self.action = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
            perform(&service, &action)
        }));
    }
}

impl Tool for ServiceManager {
    fn id(&self) -> &'static str {
        "services"
    }

    fn category(&self) -> Category {
        Category::Optimizer
    }

    fn requires_admin(&self) -> bool {
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.load.is_none() && self.services.is_none() {
            self.load = Some(BackgroundTask::spawn(ui.ctx(), |_| enumerate()));
        }
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                self.services = Some(r);
                self.load = None;
            }
        }
        if let Some(t) = &mut self.action {
            t.poll();
            if let Some(r) = t.take_result() {
                self.action = None;
                self.message = Some(r.map(|(desc, undo)| {
                    let _ = ctx.journal.record("services", desc.clone(), undo);
                    desc + "."
                }));
                self.services = None;
            }
        }
        if let Some((svc, action)) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.start_action(ui, ctx, svc, action);
        }

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
            for (s, label) in [
                (Show::All, "All"),
                (Show::Running, "Running"),
                (Show::Automatic, "Automatic"),
                (Show::Disabled, "Disabled"),
            ] {
                ui.selectable_value(&mut self.show, s, label);
            }
            if ui
                .add_enabled(
                    self.load.is_none(),
                    egui::Button::new(ctx.i18n.tr("common-refresh")),
                )
                .clicked()
            {
                self.services = None;
            }
            if self.load.is_some() || self.action.is_some() {
                ui.spinner();
            }
        });
        show_message(ui, &self.message);

        let services = match &self.services {
            Some(Ok(s)) => s.clone(),
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        let f = self.filter.to_lowercase();
        let visible: Vec<&Service> = services
            .iter()
            .filter(|s| match self.show {
                Show::All => true,
                Show::Running => s.state == RunState::Running,
                Show::Automatic => matches!(s.start, StartType::Automatic | StartType::AutomaticDelayed),
                Show::Disabled => s.start == StartType::Disabled,
            })
            .filter(|s| {
                f.is_empty()
                    || s.name.to_lowercase().contains(&f)
                    || s.display.to_lowercase().contains(&f)
                    || s.description.to_lowercase().contains(&f)
            })
            .collect();
        ui.weak(format!(
            "{} services shown, {} running",
            visible.len(),
            services.iter().filter(|s| s.state == RunState::Running).count()
        ));

        let selected = self
            .selected
            .as_ref()
            .and_then(|n| services.iter().find(|s| &s.name == n))
            .cloned();
        let mut detail_open = selected.is_some();
        egui::Panel::bottom("service_detail").resizable(true).default_size(170.0).show_collapsible(
            ui,
            &mut detail_open,
            |ui| {
                let Some(s) = &selected else { return };
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong(&s.display);
                    ui.weak(format!("({})", s.name));
                    if is_protected(&s.name) {
                        ui.weak("· protected core service");
                    }
                });
                if !s.description.is_empty() {
                    ui.label(&s.description);
                }
                ui.add(egui::Label::new(egui::RichText::new(&s.binary).monospace().small()).selectable(true));
                let busy = self.action.is_some();
                ui.add_enabled_ui(ctx.elevated && !busy, |ui| {
                    ui.horizontal(|ui| {
                        let running = s.state == RunState::Running;
                        if ui.add_enabled(!running, egui::Button::new("Start")).clicked() {
                            self.start_action(ui, ctx, s.clone(), Action::Start);
                        }
                        if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                            self.confirm.ask(format!("Stop {}?", s.display), (s.clone(), Action::Stop));
                        }
                        if ui.add_enabled(running, egui::Button::new("Restart")).clicked() {
                            self.confirm.ask(format!("Restart {}?", s.display), (s.clone(), Action::Restart));
                        }
                        ui.separator();
                        ui.label("Startup type:");
                        let editable = s.start.sc_value().is_some();
                        ui.add_enabled_ui(editable, |ui| {
                            egui::ComboBox::from_id_salt("start_type").selected_text(s.start.label()).show_ui(
                                ui,
                                |ui| {
                                    for t in StartType::EDITABLE {
                                        if ui.selectable_label(s.start == t, t.label()).clicked() && t != s.start {
                                            if t == StartType::Disabled {
                                                self.confirm.ask(
                                                    format!(
                                                        "Disable {}? Features that depend on it will stop working \
                                                         until it is re-enabled.",
                                                        s.display
                                                    ),
                                                    (s.clone(), Action::SetStart(t)),
                                                );
                                            } else {
                                                self.start_action(ui, ctx, s.clone(), Action::SetStart(t));
                                            }
                                        }
                                    }
                                },
                            );
                        });
                    });
                });
            },
        );

        let mut clicked = None;
        let (green, weak) = (
            egui::Color32::from_rgb(90, 180, 110),
            ui.visuals().weak_text_color(),
        );
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(260.0).clip(true))
            .column(Column::initial(80.0))
            .column(Column::initial(140.0))
            .column(Column::initial(150.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in [
                    "Display name",
                    "Status",
                    "Startup type",
                    "Service name",
                    "Description",
                ] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let s = visible[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(s.name.as_str()));
                    row.col(|ui| {
                        ui.label(&s.display);
                    });
                    row.col(|ui| {
                        let color = if s.state == RunState::Running { green } else { weak };
                        ui.colored_label(color, s.state.label());
                    });
                    row.col(|ui| {
                        ui.label(s.start.label());
                    });
                    row.col(|ui| {
                        ui.weak(&s.name);
                    });
                    row.col(|ui| {
                        ui.weak(&s.description);
                    });
                    if row.response().clicked() {
                        clicked = Some(s.name.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_type_mapping_and_protection() {
        assert_eq!(StartType::AutomaticDelayed.sc_value(), Some("delayed-auto"));
        assert_eq!(StartType::Boot.sc_value(), None);
        assert!(is_protected("rpcss"));
        assert!(!is_protected("Spooler"));
    }

    #[test]
    fn protected_services_are_refused_without_running_commands() {
        let svc = Service {
            name: "RpcSs".into(),
            display: "RPC".into(),
            state: RunState::Running,
            start: StartType::Automatic,
            pid: 0,
            description: String::new(),
            binary: String::new(),
        };
        assert!(perform(&svc, &Action::Stop).is_err());
        assert!(perform(&svc, &Action::SetStart(StartType::Disabled)).is_err());
    }

    #[test]
    fn enumerates_real_services() {
        let services = enumerate().unwrap();
        assert!(services.len() > 50, "got {}", services.len());
        let rpc = services
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case("RpcSs"))
            .expect("RpcSs present");
        assert_eq!(rpc.state, RunState::Running);
        assert_eq!(rpc.start, StartType::Automatic);
        assert!(!rpc.binary.is_empty());
        assert!(services.iter().any(|s| !s.description.is_empty()));
    }
}
