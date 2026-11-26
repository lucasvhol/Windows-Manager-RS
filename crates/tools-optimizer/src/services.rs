//! Service Manager: Win32 services and kernel/file-system drivers. Start, stop and
//! restart services, change startup types (singly, from a backup, from Windows'
//! defaults or from the Optimize questionnaire), delete third-party services and
//! inspect the files behind them. Every change is journaled with its reverse; core
//! OS services are protected.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use windows::Win32::System::Services::{
    CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, ENUM_SERVICE_TYPE, EnumServicesStatusExW,
    OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW,
    SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_AUTO_START,
    SERVICE_BOOT_START, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_DISABLED,
    SERVICE_DRIVER, SERVICE_PAUSED, SERVICE_QUERY_CONFIG, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATE_ALL, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_SYSTEM_START, SERVICE_WIN32,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui,
    fileinfo::{self, FileInfo, Signature},
    icons,
    paths::expand_env,
    process::{exe_from_command, hidden_command},
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, frame, show_message},
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

    /// Value for `sc config <name> start= <value>` (services only).
    pub fn sc_value(self) -> Option<&'static str> {
        match self {
            StartType::Automatic => Some("auto"),
            StartType::AutomaticDelayed => Some("delayed-auto"),
            StartType::Manual => Some("demand"),
            StartType::Disabled => Some("disabled"),
            StartType::Boot | StartType::System => None,
        }
    }

    pub fn from_sc_value(v: &str) -> Option<Self> {
        Self::EDITABLE.into_iter().find(|t| t.sc_value() == Some(v))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Service,
    Driver,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub name: String,
    pub display: String,
    pub kind: Kind,
    pub state: RunState,
    pub start: StartType,
    pub pid: u32,
    pub description: String,
    /// Command line (`ImagePath`) as stored by the Service Control Manager.
    pub binary: String,
    /// Account the service runs as (`LocalSystem`, `NT AUTHORITY\LocalService`, …).
    pub log_on: String,
    /// The file that implements it: the exe, the `ServiceDll` of a svchost service, or the driver.
    pub file: Option<PathBuf>,
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

/// Windows 10/11 client defaults for common services people change. Only services
/// whose default is the same on both are listed; anything else is restored from a backup.
pub const DEFAULTS: &[(&str, StartType)] = &[
    ("AJRouter", StartType::Manual),
    ("ALG", StartType::Manual),
    ("BITS", StartType::Manual),
    ("BTAGService", StartType::Manual),
    ("BthAvctpSvc", StartType::Manual),
    ("bthserv", StartType::Manual),
    ("diagnosticshub.standardcollector.service", StartType::Manual),
    ("DiagTrack", StartType::Automatic),
    ("dmwappushservice", StartType::Manual),
    ("Fax", StartType::Manual),
    ("FrameServer", StartType::Manual),
    ("icssvc", StartType::Manual),
    ("iphlpsvc", StartType::Automatic),
    ("lfsvc", StartType::Manual),
    ("MSiSCSI", StartType::Manual),
    ("NetTcpPortSharing", StartType::Disabled),
    ("PhoneSvc", StartType::Manual),
    ("RemoteAccess", StartType::Disabled),
    ("RemoteRegistry", StartType::Disabled),
    ("RetailDemo", StartType::Manual),
    ("SCardSvr", StartType::Manual),
    ("ScDeviceEnum", StartType::Manual),
    ("SCPolicySvc", StartType::Manual),
    ("seclogon", StartType::Manual),
    ("SessionEnv", StartType::Manual),
    ("SharedAccess", StartType::Manual),
    ("SNMPTRAP", StartType::Manual),
    ("Spooler", StartType::Automatic),
    ("SSDPSRV", StartType::Manual),
    ("SysMain", StartType::Automatic),
    ("TermService", StartType::Manual),
    ("TrkWks", StartType::Automatic),
    ("UmRdpService", StartType::Manual),
    ("upnphost", StartType::Manual),
    ("W32Time", StartType::Manual),
    ("WalletService", StartType::Manual),
    ("WbioSrvc", StartType::Manual),
    ("Wecsvc", StartType::Manual),
    ("WerSvc", StartType::Manual),
    ("WinRM", StartType::Manual),
    ("wisvc", StartType::Manual),
    ("WpcMonSvc", StartType::Manual),
    ("WSearch", StartType::AutomaticDelayed),
    ("wuauserv", StartType::Manual),
    ("XblAuthManager", StartType::Manual),
    ("XblGameSave", StartType::Manual),
    ("XboxGipSvc", StartType::Manual),
    ("XboxNetApiSvc", StartType::Manual),
];

pub fn default_start(name: &str) -> Option<StartType> {
    DEFAULTS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, t)| *t)
}

/// One Optimize question: answering "No" sets each listed service to the given type.
pub struct Question {
    pub text: &'static str,
    pub services: &'static [(&'static str, StartType)],
}

pub const QUESTIONS: &[Question] = &[
    Question {
        text: "Do you print from this PC (including Microsoft Print to PDF)?",
        services: &[("Spooler", StartType::Disabled)],
    },
    Question {
        text: "Do you send or receive faxes from this PC?",
        services: &[("Fax", StartType::Disabled)],
    },
    Question {
        text: "Do you use Bluetooth devices?",
        services: &[
            ("bthserv", StartType::Disabled),
            ("BTAGService", StartType::Disabled),
            ("BthAvctpSvc", StartType::Disabled),
        ],
    },
    Question {
        text: "Do you use Xbox apps, Game Pass or Xbox controllers?",
        services: &[
            ("XblAuthManager", StartType::Disabled),
            ("XblGameSave", StartType::Disabled),
            ("XboxNetApiSvc", StartType::Disabled),
            ("XboxGipSvc", StartType::Disabled),
        ],
    },
    Question {
        text: "Do you connect to this PC with Remote Desktop?",
        services: &[
            ("TermService", StartType::Disabled),
            ("SessionEnv", StartType::Disabled),
            ("UmRdpService", StartType::Disabled),
        ],
    },
    Question {
        text: "Do you sign in with a fingerprint or face (Windows Hello biometrics)?",
        services: &[("WbioSrvc", StartType::Disabled)],
    },
    Question {
        text: "Do you use smart cards?",
        services: &[
            ("SCardSvr", StartType::Disabled),
            ("ScDeviceEnum", StartType::Disabled),
            ("SCPolicySvc", StartType::Disabled),
        ],
    },
    Question {
        text: "Do apps on this PC use your location?",
        services: &[("lfsvc", StartType::Disabled)],
    },
    Question {
        text: "Do you make phone calls from this PC (Phone Link)?",
        services: &[("PhoneSvc", StartType::Disabled)],
    },
    Question {
        text: "Do you share this PC's internet connection (mobile hotspot)?",
        services: &[
            ("icssvc", StartType::Disabled),
            ("SharedAccess", StartType::Disabled),
        ],
    },
    Question {
        text: "Do you use Windows Search to find files by their contents?",
        services: &[("WSearch", StartType::Disabled)],
    },
    Question {
        text: "Send optional diagnostic data to Microsoft?",
        services: &[
            ("DiagTrack", StartType::Disabled),
            ("dmwappushservice", StartType::Disabled),
        ],
    },
    Question {
        text: "Should other computers be able to edit this PC's registry?",
        services: &[("RemoteRegistry", StartType::Disabled)],
    },
    Question {
        text: "Is this a store demonstration PC?",
        services: &[("RetailDemo", StartType::Disabled)],
    },
];

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

struct Config {
    start: StartType,
    binary: String,
    log_on: String,
    description: String,
}

fn query_config(scm: &Sc, name: &str) -> Option<Config> {
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
        Some(Config {
            start,
            binary: pwstr(cfg.lpBinaryPathName),
            log_on: pwstr(cfg.lpServiceStartName),
            description,
        })
    }
}

/// Turns an `ImagePath` into a real path: `\SystemRoot\…`, `System32\…` (relative to the
/// Windows folder), `\??\C:\…`, quoted or unquoted commands with arguments.
pub fn image_path_to_file(image: &str) -> Option<PathBuf> {
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let image = image.trim();
    let lower = image.to_ascii_lowercase();
    let raw = if let Some(rest) = lower.strip_prefix(r"\systemroot\") {
        format!(r"{windir}\{}", &image[image.len() - rest.len()..])
    } else if let Some(rest) = image.strip_prefix(r"\??\") {
        rest.to_owned()
    } else if lower.starts_with(r"system32\") || lower.starts_with(r"syswow64\") {
        format!(r"{windir}\{image}")
    } else {
        exe_from_command(image)?
    };
    let p = PathBuf::from(expand_env(&raw));
    p.is_absolute().then_some(p)
}

/// The DLL a shared-process (svchost) service lives in.
fn service_dll(name: &str) -> Option<PathBuf> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SYSTEM\CurrentControlSet\Services\{name}\Parameters"))
        .ok()?;
    let dll: String = key.get_value("ServiceDll").ok()?;
    Some(PathBuf::from(expand_env(&dll)))
}

fn implementing_file(name: &str, binary: &str) -> Option<PathBuf> {
    if binary.to_ascii_lowercase().contains("svchost.exe")
        && let Some(dll) = service_dll(name)
    {
        return Some(dll);
    }
    image_path_to_file(binary)
}

pub fn enumerate(kind: Kind) -> Result<Vec<Service>> {
    let types: ENUM_SERVICE_TYPE = match kind {
        Kind::Service => SERVICE_WIN32,
        Kind::Driver => SERVICE_DRIVER,
    };
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
            types,
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
            types,
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
                let cfg = query_config(&scm, &name).unwrap_or(Config {
                    start: StartType::Manual,
                    binary: String::new(),
                    log_on: String::new(),
                    description: String::new(),
                });
                Service {
                    display: pwstr(e.lpDisplayName),
                    file: implementing_file(&name, &cfg.binary),
                    name,
                    kind,
                    state,
                    start: cfg.start,
                    pid: st.dwProcessId,
                    description: cfg.description,
                    binary: cfg.binary,
                    log_on: cfg.log_on,
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

fn check_start_change(service: &Service, new: StartType) -> Result<(&'static str, &'static str)> {
    if is_protected(&service.name) && matches!(new, StartType::Disabled | StartType::Manual) {
        bail!(
            "{} is a core Windows service; its startup type is protected.",
            service.display
        );
    }
    match (new.sc_value(), service.start.sc_value()) {
        (Some(n), Some(o)) if service.kind == Kind::Service => Ok((n, o)),
        _ => bail!("The startup type of {} cannot be changed.", service.display),
    }
}

pub fn perform(service: &Service, action: &Action) -> Result<Outcome> {
    let name = service.name.as_str();
    let protected = is_protected(name);
    if service.kind == Kind::Driver {
        bail!("Drivers are shown read-only.");
    }
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
            let (new_v, old_v) = check_start_change(service, *new)?;
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

/// Applies several startup-type changes as one journal entry. Changes that are refused
/// or fail are skipped and listed in the description; the undo covers the ones applied.
pub fn apply_batch(label: &str, changes: &[(Service, StartType)]) -> Result<Outcome> {
    let mut undo = Vec::new();
    let mut failed = Vec::new();
    for (s, new) in changes {
        let r = check_start_change(s, *new).and_then(|(n, o)| {
            run("sc", &["config", &s.name, "start=", n])?;
            Ok(o)
        });
        match r {
            Ok(old) => undo.push(cmd("sc", &["config", &s.name, "start=", old])),
            Err(e) => failed.push(format!("{}: {e:#}", s.name)),
        }
    }
    if undo.is_empty() && !failed.is_empty() {
        bail!("No startup types changed. {}", failed.join("; "));
    }
    let mut desc = format!("{label}: changed {} service startup types", undo.len());
    if !failed.is_empty() {
        desc += &format!(" ({} skipped: {})", failed.len(), failed.join("; "));
    }
    Ok((desc, UndoAction::Sequence { steps: undo }))
}

/// Stops (if needed) and deletes a service. Its registry key must already be backed up.
pub fn delete_service(service: &Service) -> Result<()> {
    if service.kind != Kind::Service || is_protected(&service.name) {
        bail!("{} cannot be deleted.", service.display);
    }
    if service.state != RunState::Stopped {
        let _ = run("net", &["stop", &service.name]);
    }
    run("sc", &["delete", &service.name])
}

/// Startup types of all editable services, as written to a backup file.
pub fn backup_json(services: &[Service]) -> serde_json::Value {
    let map: BTreeMap<&str, &str> = services
        .iter()
        .filter_map(|s| Some((s.name.as_str(), s.start.sc_value()?)))
        .collect();
    serde_json::json!({
        "format": "win-manager-rs/service-startup-types",
        "version": 1,
        "created": chrono::Local::now().to_rfc3339(),
        "services": map,
    })
}

pub fn parse_backup(text: &str) -> Result<HashMap<String, StartType>> {
    let v: serde_json::Value = serde_json::from_str(text).context("not a JSON file")?;
    let map = v
        .get("services")
        .and_then(|s| s.as_object())
        .context("not a service startup-type backup")?;
    Ok(map
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), StartType::from_sc_value(v.as_str()?)?)))
        .collect())
}

/// The changes needed to bring `services` to `target` (only existing, editable, different ones).
pub fn plan_changes(
    services: &[Service],
    target: impl Fn(&str) -> Option<StartType>,
) -> Vec<(Service, StartType)> {
    services
        .iter()
        .filter(|s| s.kind == Kind::Service && s.start.sc_value().is_some())
        .filter_map(|s| {
            let t = target(&s.name)?;
            (t != s.start).then(|| (s.clone(), t))
        })
        .collect()
}

fn baseline_path() -> Result<PathBuf> {
    Ok(wm_core::paths::backups_dir()?.join("services-baseline.json"))
}

/// Saves the first-run snapshot once, so the original startup types can always be restored.
fn ensure_baseline(services: &[Service]) {
    if let Ok(p) = baseline_path()
        && !p.exists()
        && let Ok(text) = serde_json::to_string_pretty(&backup_json(services))
    {
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(&p, text);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Show {
    All,
    Running,
    Automatic,
    Disabled,
    NonMicrosoft,
}

impl Show {
    const ALL: [Show; 5] = [
        Show::All,
        Show::Running,
        Show::Automatic,
        Show::Disabled,
        Show::NonMicrosoft,
    ];

    fn label(self) -> &'static str {
        match self {
            Show::All => "All",
            Show::Running => "Running",
            Show::Automatic => "Automatic",
            Show::Disabled => "Disabled",
            Show::NonMicrosoft => "Non-Microsoft",
        }
    }
}

enum Ask {
    One(Service, Action),
    Batch(String, Vec<(Service, StartType)>),
    Delete(Service, bool),
}

/// Result of a background change.
struct Done {
    outcome: Outcome,
    /// Pre-change registry export (delete), recorded together with the outcome.
    extra_undo: Option<UndoAction>,
    /// File to move to the Recycle Bin after a delete.
    recycle: Option<PathBuf>,
}

struct Questionnaire {
    /// `true` = "Yes, I use it" (no change).
    answers: Vec<bool>,
}

pub struct ServiceManager {
    kind: Kind,
    load: Option<BackgroundTask<Vec<Service>>>,
    services: Option<Result<Vec<Service>, String>>,
    drivers: Option<Result<Vec<Service>, String>>,
    action: Option<BackgroundTask<Done>>,
    info_task: Option<BackgroundTask<HashMap<PathBuf, FileInfo>>>,
    info: HashMap<PathBuf, FileInfo>,
    asked: HashSet<PathBuf>,
    filter: String,
    show: Show,
    selected: Option<String>,
    delete_file: bool,
    confirm: Confirm<Ask>,
    questionnaire: Option<Questionnaire>,
    message: Message,
    /// Folded groups: true = Non-Microsoft, false = Microsoft.
    folded: HashSet<bool>,
    /// File creation dates, read once per file.
    created: HashMap<PathBuf, String>,
}

impl Default for ServiceManager {
    fn default() -> Self {
        Self {
            kind: Kind::Service,
            load: None,
            services: None,
            drivers: None,
            action: None,
            info_task: None,
            info: HashMap::new(),
            asked: HashSet::new(),
            filter: String::new(),
            show: Show::All,
            selected: None,
            delete_file: false,
            confirm: Confirm::default(),
            questionnaire: None,
            message: None,
            folded: HashSet::new(),
            created: HashMap::new(),
        }
    }
}

/// `NT AUTHORITY\LocalService` → `LocalService` (any case).
fn short_account(account: &str) -> &str {
    let prefix = r"nt authority\";
    match account.get(..prefix.len()) {
        Some(p) if p.eq_ignore_ascii_case(prefix) => &account[prefix.len()..],
        _ => account,
    }
}

fn list_names(changes: &[(Service, StartType)]) -> String {
    let mut lines: Vec<String> = changes
        .iter()
        .take(15)
        .map(|(s, t)| format!("{}: {} → {}", s.display, s.start.label(), t.label()))
        .collect();
    if changes.len() > 15 {
        lines.push(format!("… and {} more", changes.len() - 15));
    }
    lines.join("\n")
}

impl ServiceManager {
    fn list(&self) -> &Option<Result<Vec<Service>, String>> {
        match self.kind {
            Kind::Service => &self.services,
            Kind::Driver => &self.drivers,
        }
    }

    fn list_mut(&mut self) -> &mut Option<Result<Vec<Service>, String>> {
        match self.kind {
            Kind::Service => &mut self.services,
            Kind::Driver => &mut self.drivers,
        }
    }

    fn info_of(&self, s: &Service) -> Option<&FileInfo> {
        self.info.get(s.file.as_ref()?)
    }

    /// `None` while the signature is still being checked.
    fn is_microsoft(&self, s: &Service) -> Option<bool> {
        let file = s.file.as_ref()?;
        if !self.asked.contains(file) || (self.info_task.is_some() && !self.info.contains_key(file)) {
            return None;
        }
        Some(
            self.info
                .get(file)
                .is_some_and(|i| i.signature == Signature::Valid && i.signer.contains("Microsoft")),
        )
    }

    fn request_info(&mut self, ctx: &egui::Context, services: &[Service]) {
        if let Some(t) = &mut self.info_task {
            t.poll();
            if let Some(r) = t.take_result() {
                if let Ok(m) = r {
                    self.info.extend(m);
                }
                self.info_task = None;
            }
            return;
        }
        let new: Vec<PathBuf> = services
            .iter()
            .filter_map(|s| s.file.clone())
            .filter(|p| !self.asked.contains(p) && p.exists())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !new.is_empty() {
            self.asked.extend(new.iter().cloned());
            self.info_task = Some(BackgroundTask::spawn(ctx, move |_| fileinfo::describe(&new)));
        }
    }

    fn spawn(
        &mut self,
        ui: &egui::Ui,
        ctx: &AppCtx<'_>,
        work: impl FnOnce() -> Result<Done> + Send + 'static,
    ) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        self.message = None;
        self.action = Some(BackgroundTask::spawn(ui.ctx(), move |_| work()));
    }

    fn run_ask(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>, ask: Ask) {
        match ask {
            Ask::One(s, a) => self.spawn(ui, ctx, move || {
                Ok(Done {
                    outcome: perform(&s, &a)?,
                    extra_undo: None,
                    recycle: None,
                })
            }),
            Ask::Batch(label, changes) => self.spawn(ui, ctx, move || {
                Ok(Done {
                    outcome: apply_batch(&label, &changes)?,
                    extra_undo: None,
                    recycle: None,
                })
            }),
            Ask::Delete(s, with_file) => {
                if ctx.config.dry_run {
                    self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                    return;
                }
                let key = format!(r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\{}", s.name);
                let backup = match ctx.journal.backup_reg_key(&key) {
                    Ok(b) => b,
                    Err(e) => {
                        self.message = Some(Err(format!("Could not back up the service first: {e:#}")));
                        return;
                    }
                };
                let recycle = with_file.then(|| s.file.clone()).flatten();
                self.spawn(ui, ctx, move || {
                    delete_service(&s)?;
                    Ok(Done {
                        outcome: (
                            format!(
                                "Deleted service {} ({}). Undo re-imports its settings; restart Windows afterwards",
                                s.display, s.name
                            ),
                            UndoAction::None,
                        ),
                        extra_undo: Some(backup),
                        recycle,
                    })
                });
            }
        }
    }

    fn poll(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                if self.kind == Kind::Service
                    && let Ok(list) = &r
                {
                    ensure_baseline(list);
                }
                *self.list_mut() = Some(r);
                self.load = None;
            }
        }
        if let Some(t) = &mut self.action {
            t.poll();
            if let Some(r) = t.take_result() {
                self.action = None;
                self.message = Some(r.map(|d| {
                    let (desc, undo) = d.outcome;
                    let undo = d.extra_undo.unwrap_or(undo);
                    let _ = ctx.journal.record("services", desc.clone(), undo);
                    let mut text = desc + ".";
                    if let Some(file) = d.recycle {
                        match ctx.journal.recycle(
                            "services",
                            format!("Recycled {}", file.display()),
                            vec![file],
                        ) {
                            Ok(_) => text += " Its file was moved to the Recycle Bin.",
                            Err(e) => text += &format!(" The file could not be recycled: {e:#}"),
                        }
                    }
                    text
                }));
                self.services = None;
            }
        }
        if let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.run_ask(ui, ctx, ask);
        }
    }

    fn backup(&mut self, services: &[Service]) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Service backup", &["json"])
            .set_directory(wm_core::paths::backups_dir().unwrap_or_default())
            .set_file_name(format!(
                "services-{}.json",
                chrono::Local::now().format("%Y%m%d-%H%M")
            ))
            .save_file()
        else {
            return;
        };
        self.message = Some(
            serde_json::to_string_pretty(&backup_json(services))
                .map_err(anyhow::Error::from)
                .and_then(|t| Ok(std::fs::write(&path, t)?))
                .map(|()| {
                    format!(
                        "Saved the startup types of {} services to {}.",
                        services.len(),
                        path.display()
                    )
                })
                .map_err(|e| format!("{e:#}")),
        );
    }

    fn restore(&mut self, services: &[Service]) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Service backup", &["json"])
            .set_directory(wm_core::paths::backups_dir().unwrap_or_default())
            .pick_file()
        else {
            return;
        };
        let map = match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| parse_backup(&t))
        {
            Ok(m) => m,
            Err(e) => {
                self.message = Some(Err(format!("{}: {e:#}", path.display())));
                return;
            }
        };
        let changes = plan_changes(services, |n| map.get(n).copied());
        if changes.is_empty() {
            self.message = Some(Ok("Every service already matches the backup.".into()));
            return;
        }
        self.confirm.ask(
            format!(
                "Restore {} startup types from {}?\n\n{}",
                changes.len(),
                path.file_name().unwrap_or_default().to_string_lossy(),
                list_names(&changes)
            ),
            Ask::Batch("Restored service backup".into(), changes),
        );
    }

    fn questionnaire_ui(&mut self, ui: &egui::Ui, services: &[Service]) {
        let Some(q) = &mut self.questionnaire else { return };
        let mut apply = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("services_optimize")).show(ui.ctx(), |ui| {
            ui.set_width(560.0);
            ui.heading(format!("{}  Optimize services", icons::MAGIC_WAND));
            ui.label("Answer \"No\" for features you never use; their services will be disabled. Everything can be undone from the Change Journal or with Restore defaults.");
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                egui::Grid::new("questions").num_columns(2).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
                    for (i, question) in QUESTIONS.iter().enumerate() {
                        ui.label(question.text);
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut q.answers[i], true, "Yes");
                            ui.radio_value(&mut q.answers[i], false, "No");
                        });
                        ui.end_row();
                    }
                });
            });
            let wanted: HashMap<&str, StartType> = QUESTIONS
                .iter()
                .zip(&q.answers)
                .filter(|(_, yes)| !**yes)
                .flat_map(|(qq, _)| qq.services.iter().copied())
                .collect();
            let changes = plan_changes(services, |n| {
                wanted.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, t)| *t)
            });
            ui.add_space(6.0);
            ui.weak(format!("{} services will change.", changes.len()));
            ui.horizontal(|ui| {
                if ui.add_enabled(!changes.is_empty(), egui::Button::new("Apply")).clicked() {
                    apply = Some(changes);
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if let Some(changes) = apply {
            self.questionnaire = None;
            self.confirm.ask(
                format!(
                    "Apply these {} changes?\n\n{}",
                    changes.len(),
                    list_names(&changes)
                ),
                Ask::Batch("Optimize services".into(), changes),
            );
        } else if close {
            self.questionnaire = None;
        }
    }

    fn detail(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, s: &Service) {
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
        egui::Grid::new("svc_detail")
            .num_columns(2)
            .spacing([10.0, 2.0])
            .show(ui, |ui| {
                ui.weak("Command line");
                ui.add(egui::Label::new(egui::RichText::new(&s.binary).monospace().small()).selectable(true));
                ui.end_row();
                if let Some(f) = &s.file {
                    ui.weak("File");
                    ui.add(
                        egui::Label::new(egui::RichText::new(f.display().to_string()).monospace().small())
                            .selectable(true),
                    );
                    ui.end_row();
                }
                if let Some(i) = self.info_of(s) {
                    ui.weak("Publisher");
                    ui.label(format!("{}  ·  {}  {}", i.company, i.signature.label(), i.signer));
                    ui.end_row();
                }
                if !s.log_on.is_empty() {
                    ui.weak("Log on as");
                    ui.label(&s.log_on);
                    ui.end_row();
                }
                if let Some(d) = default_start(&s.name) {
                    ui.weak("Windows default");
                    ui.label(d.label());
                    ui.end_row();
                }
            });
        if s.kind == Kind::Driver {
            return;
        }
        let busy = self.action.is_some();
        ui.add_enabled_ui(ctx.elevated && !busy, |ui| {
            ui.horizontal(|ui| {
                ui.label("Startup type:");
                ui.add_enabled_ui(s.start.sc_value().is_some(), |ui| {
                    egui::ComboBox::from_id_salt("start_type").selected_text(s.start.label()).show_ui(ui, |ui| {
                        for t in StartType::EDITABLE {
                            if ui.selectable_label(s.start == t, t.label()).clicked() && t != s.start {
                                if t == StartType::Disabled {
                                    self.confirm.ask(
                                        format!(
                                            "Disable {}? Features that depend on it will stop working until it is re-enabled.",
                                            s.display
                                        ),
                                        Ask::One(s.clone(), Action::SetStart(t)),
                                    );
                                } else {
                                    self.run_ask(ui, ctx, Ask::One(s.clone(), Action::SetStart(t)));
                                }
                            }
                        }
                    });
                });
                ui.separator();
                ui.checkbox(&mut self.delete_file, "Delete also removes its file");
            });
        });
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
        if self.load.is_none() && self.list().is_none() {
            let kind = self.kind;
            self.load = Some(BackgroundTask::spawn(ui.ctx(), move |_| enumerate(kind)));
        }
        self.poll(ui, ctx);

        let loading = self.load.is_some();
        let list = match self.list() {
            Some(Ok(s)) => s.clone(),
            Some(Err(e)) => {
                let e = e.clone();
                frame::central(ui, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                });
                return;
            }
            None => {
                frame::central(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("Reading services…");
                    });
                });
                return;
            }
        };
        self.request_info(ui.ctx(), &list);
        self.questionnaire_ui(ui, &list);

        let selected = self
            .selected
            .as_ref()
            .and_then(|n| list.iter().find(|s| &s.name == n))
            .cloned();
        let svc = self.kind == Kind::Service;
        let can = ctx.elevated && self.action.is_none() && svc;
        let sel_ok = selected.is_some() && can;
        let running = selected.as_ref().is_some_and(|s| s.state == RunState::Running);
        let deletable = selected
            .as_ref()
            .is_some_and(|s| svc && !is_protected(&s.name) && self.is_microsoft(s) == Some(false));
        // The original's toolbar order.
        let buttons = [
            Some(ToolButton::new(icons::MAGIC_WAND, "Optimize").enabled(can)),
            Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(!loading)),
            None,
            Some(
                ToolButton::new(icons::INFO, "File Properties").enabled(
                    selected
                        .as_ref()
                        .is_some_and(|s| s.file.as_ref().is_some_and(|f| f.exists())),
                ),
            ),
            Some(
                ToolButton::new(icons::FOLDER_OPEN, "Open File Location")
                    .enabled(selected.as_ref().is_some_and(|s| s.file.is_some())),
            ),
            Some(ToolButton::new(icons::GLOBE, "Query").enabled(selected.is_some())),
            None,
            Some(ToolButton::new(icons::PLAY, "Start").enabled(sel_ok && !running)),
            Some(ToolButton::new(icons::STOP, "Stop").enabled(sel_ok && running)),
            Some(ToolButton::new(icons::ARROW_COUNTER_CLOCKWISE, "Restart").enabled(sel_ok && running)),
            None,
            Some(ToolButton::new(icons::FLOPPY_DISK, "Backup").enabled(svc)),
            Some(ToolButton::new(icons::UPLOAD, "Restore").enabled(can)),
            Some(ToolButton::new(icons::CLOCK_COUNTER_CLOCKWISE, "Defaults").enabled(can)),
            Some(ToolButton::new(icons::X, "Delete").enabled(can && deletable)),
            None,
            Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
        ];
        match frame::toolbar(ui, &buttons, |_| {}) {
            Some(0) => {
                self.questionnaire = Some(Questionnaire {
                    answers: vec![true; QUESTIONS.len()],
                })
            }
            Some(1) => {
                *self.list_mut() = None;
                self.message = None;
            }
            Some(3) => {
                if let Some(f) = selected.as_ref().and_then(|s| s.file.as_ref()) {
                    self.message = wm_core::shell::show_properties(f)
                        .err()
                        .map(|e| Err(format!("{e:#}")));
                }
            }
            Some(4) => {
                if let Some(f) = selected.as_ref().and_then(|s| s.file.as_ref()) {
                    wm_core::shell::reveal_in_explorer(f);
                }
            }
            Some(5) => {
                if let Some(s) = &selected {
                    let q: String = format!("{} {}", s.name, if svc { "service" } else { "driver" })
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '.' {
                                c
                            } else {
                                '+'
                            }
                        })
                        .collect();
                    let _ = std::process::Command::new("explorer")
                        .arg(format!("https://www.bing.com/search?q={q}"))
                        .spawn();
                }
            }
            Some(7) => {
                if let Some(s) = &selected {
                    self.run_ask(ui, ctx, Ask::One(s.clone(), Action::Start));
                }
            }
            Some(8) => {
                if let Some(s) = &selected {
                    self.confirm
                        .ask(format!("Stop {}?", s.display), Ask::One(s.clone(), Action::Stop));
                }
            }
            Some(9) => {
                if let Some(s) = &selected {
                    self.confirm.ask(
                        format!("Restart {}?", s.display),
                        Ask::One(s.clone(), Action::Restart),
                    );
                }
            }
            Some(11) => self.backup(&list),
            Some(12) => self.restore(&list),
            Some(13) => {
                let changes = plan_changes(&list, default_start);
                if changes.is_empty() {
                    self.message = Some(Ok(format!(
                        "All {} services with a known Windows default already use it. Use Restore with services-baseline.json for the rest.",
                        DEFAULTS.len()
                    )));
                } else {
                    self.confirm.ask(
                        format!(
                            "Restore the Windows default startup type of {} services?\n\n{}\n\nOther services can be restored from services-baseline.json, the snapshot taken the first time this tool ran.",
                            changes.len(),
                            list_names(&changes)
                        ),
                        Ask::Batch("Restored default service startup types".into(), changes),
                    );
                }
            }
            Some(14) => {
                if let Some(s) = &selected {
                    let file = match (&s.file, self.delete_file) {
                        (Some(f), true) => {
                            format!("\n\nIts file will be moved to the Recycle Bin:\n{}", f.display())
                        }
                        _ => String::new(),
                    };
                    self.confirm.ask(
                        format!(
                            "Delete the service {} ({})? Its settings are backed up first and can be re-imported from the Change Journal.{file}",
                            s.display, s.name
                        ),
                        Ask::Delete(s.clone(), self.delete_file),
                    );
                }
            }
            Some(16) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }

        // Services / Drivers, as the original's toggle buttons, then the filters.
        let mut kind = self.kind;
        let search_hint = format!("{}  {}", icons::MAGNIFYING_GLASS, ctx.i18n.tr("common-filter"));
        frame::options(ui, |ui| {
            ui.add_enabled_ui(!loading, |ui| {
                ui.selectable_value(&mut kind, Kind::Service, format!("{}  Services", icons::GEAR));
                ui.selectable_value(&mut kind, Kind::Driver, format!("{}  Drivers", icons::CPU));
            });
            ui.separator();
            egui::ComboBox::from_id_salt("svc-show")
                .selected_text(self.show.label())
                .show_ui(ui, |ui| {
                    for s in Show::ALL {
                        ui.selectable_value(&mut self.show, s, s.label());
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(search_hint)
                    .desired_width(220.0),
            );
        });
        if kind != self.kind {
            self.kind = kind;
            self.selected = None;
        }

        let f = self.filter.to_lowercase();
        let visible: Vec<&Service> = list
            .iter()
            .filter(|s| match self.show {
                Show::All => true,
                Show::Running => s.state == RunState::Running,
                Show::Automatic => matches!(
                    s.start,
                    StartType::Automatic | StartType::AutomaticDelayed | StartType::Boot | StartType::System
                ),
                Show::Disabled => s.start == StartType::Disabled,
                Show::NonMicrosoft => self.is_microsoft(s) == Some(false),
            })
            .filter(|s| {
                f.is_empty()
                    || s.name.to_lowercase().contains(&f)
                    || s.display.to_lowercase().contains(&f)
                    || s.description.to_lowercase().contains(&f)
            })
            .collect();
        let busy = self.load.is_some() || self.action.is_some();
        let checking = self.info_task.is_some();
        frame::status(
            ui,
            &format!(
                "{} Total: {}",
                if svc { "Services" } else { "Drivers" },
                visible.len()
            ),
            |ui| {
                if busy {
                    ui.spinner();
                }
                if checking {
                    ui.weak("checking signatures…");
                }
            },
        );

        let mut detail_open = selected.is_some();
        egui::Panel::bottom("service_detail")
            .resizable(true)
            .default_size(150.0)
            .show_collapsible(ui, &mut detail_open, |ui| {
                if let Some(s) = &selected {
                    self.detail(ui, ctx, s);
                }
            });

        frame::central_fixed(ui, |ui| self.table(ui, &visible));
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

/// A row of the grouped list: a Non-Microsoft / Microsoft header, or a service.
enum Row<'a> {
    Group(bool, usize),
    Item(&'a Service),
}

impl ServiceManager {
    /// The binary's creation date ("1/11/2026"), cached per file.
    fn created_of(&mut self, s: &Service) -> String {
        let Some(f) = &s.file else { return String::new() };
        self.created
            .entry(f.clone())
            .or_insert_with(|| {
                std::fs::metadata(f)
                    .and_then(|m| m.created())
                    .map(|t| {
                        chrono::DateTime::<chrono::Local>::from(t)
                            .format("%-d/%-m/%Y")
                            .to_string()
                    })
                    .unwrap_or_default()
            })
            .clone()
    }

    fn table(&mut self, ui: &mut egui::Ui, visible: &[&Service]) {
        show_message(ui, &self.message);
        // Like the original: Non-Microsoft first, then Microsoft (still unchecked
        // files count as Microsoft until their signature is read).
        let mut rows: Vec<Row<'_>> = Vec::new();
        for non_ms in [true, false] {
            let group: Vec<&Service> = visible
                .iter()
                .copied()
                .filter(|s| (self.is_microsoft(s) == Some(false)) == non_ms)
                .collect();
            if group.is_empty() {
                continue;
            }
            rows.push(Row::Group(non_ms, group.len()));
            if !self.folded.contains(&non_ms) {
                rows.extend(group.into_iter().map(Row::Item));
            }
        }
        let created: Vec<String> = rows
            .iter()
            .map(|r| match r {
                Row::Item(s) => self.created_of(s),
                Row::Group(..) => String::new(),
            })
            .collect();
        let mut clicked = None;
        let mut fold = None;
        let (green, weak) = (
            egui::Color32::from_rgb(90, 180, 110),
            ui.visuals().weak_text_color(),
        );
        let accent = ui.visuals().selection.bg_fill;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(190.0).clip(true))
            .column(Column::initial(230.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::initial(120.0))
            .column(Column::initial(110.0).clip(true))
            .column(Column::initial(95.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in [
                    "Name",
                    "Title",
                    "Status",
                    "Startup Type",
                    "Log On As",
                    "Creation Date",
                    "Command Line",
                ] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let i = row.index();
                    match &rows[i] {
                        Row::Group(non_ms, n) => {
                            let open = !self.folded.contains(non_ms);
                            row.col(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(if open { "▾" } else { "▸" });
                                    ui.colored_label(
                                        accent,
                                        if *non_ms { "Non-Microsoft" } else { "Microsoft" },
                                    );
                                    ui.weak(format!("({n})"));
                                });
                            });
                            for _ in 0..6 {
                                row.col(|_| {});
                            }
                            if row.response().clicked() {
                                fold = Some(*non_ms);
                            }
                        }
                        Row::Item(s) => {
                            row.set_selected(self.selected.as_deref() == Some(s.name.as_str()));
                            row.col(|ui| {
                                ui.horizontal(|ui| {
                                    ui.add_space(18.0);
                                    ui.label(&s.name);
                                });
                            });
                            row.col(|ui| {
                                ui.label(&s.display).on_hover_text(&s.description);
                            });
                            row.col(|ui| {
                                let color = if s.state == RunState::Running { green } else { weak };
                                ui.colored_label(color, s.state.label());
                            });
                            row.col(|ui| {
                                ui.label(s.start.label());
                            });
                            row.col(|ui| {
                                ui.label(short_account(&s.log_on));
                            });
                            row.col(|ui| {
                                ui.weak(&created[i]);
                            });
                            row.col(|ui| {
                                ui.weak(&s.binary).on_hover_text(&s.binary);
                            });
                            if row.response().clicked() {
                                clicked = Some(s.name.clone());
                            }
                        }
                    }
                });
            });
        if let Some(g) = fold
            && !self.folded.remove(&g)
        {
            self.folded.insert(g);
        }
        if clicked.is_some() {
            self.selected = clicked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, start: StartType) -> Service {
        Service {
            name: name.into(),
            display: name.into(),
            kind: Kind::Service,
            state: RunState::Running,
            start,
            pid: 0,
            description: String::new(),
            binary: String::new(),
            log_on: String::new(),
            file: None,
        }
    }

    #[test]
    fn start_type_mapping_and_protection() {
        assert_eq!(StartType::AutomaticDelayed.sc_value(), Some("delayed-auto"));
        assert_eq!(
            StartType::from_sc_value("delayed-auto"),
            Some(StartType::AutomaticDelayed)
        );
        assert_eq!(StartType::Boot.sc_value(), None);
        assert!(is_protected("rpcss"));
        assert!(!is_protected("Spooler"));
    }

    #[test]
    fn protected_services_are_refused_without_running_commands() {
        let rpc = svc("RpcSs", StartType::Automatic);
        assert!(perform(&rpc, &Action::Stop).is_err());
        assert!(perform(&rpc, &Action::SetStart(StartType::Disabled)).is_err());
        assert!(delete_service(&rpc).is_err());
        let r = apply_batch("t", &[(rpc, StartType::Disabled)]);
        assert!(r.is_err(), "a batch of only refused changes fails");
        let mut drv = svc("disk", StartType::Boot);
        drv.kind = Kind::Driver;
        assert!(perform(&drv, &Action::Stop).is_err());
        assert!(delete_service(&drv).is_err());
    }

    #[test]
    fn image_paths_resolve() {
        let win = std::env::var("SystemRoot").unwrap();
        assert_eq!(
            image_path_to_file(r"\SystemRoot\System32\drivers\disk.sys"),
            Some(PathBuf::from(format!(r"{win}\System32\drivers\disk.sys")))
        );
        assert_eq!(
            image_path_to_file(r"System32\drivers\x.sys"),
            Some(PathBuf::from(format!(r"{win}\System32\drivers\x.sys")))
        );
        assert_eq!(
            image_path_to_file(r"\??\C:\a\b.sys"),
            Some(PathBuf::from(r"C:\a\b.sys"))
        );
        assert_eq!(
            image_path_to_file(r#""C:\Program Files\X\svc.exe" -k run"#),
            Some(PathBuf::from(r"C:\Program Files\X\svc.exe"))
        );
    }

    #[test]
    fn backup_round_trip_and_plans() {
        let list = vec![
            svc("A", StartType::Automatic),
            svc("B", StartType::Disabled),
            svc("Drv", StartType::Boot),
        ];
        let text = backup_json(&list).to_string();
        let map = parse_backup(&text).unwrap();
        assert_eq!(map.len(), 2, "boot/system types are not backed up");
        assert_eq!(map["B"], StartType::Disabled);
        assert!(parse_backup("{}").is_err());

        let mut target = map.clone();
        target.insert("A".into(), StartType::Manual);
        let plan = plan_changes(&list, |n| target.get(n).copied());
        assert_eq!(plan.len(), 1);
        assert_eq!((plan[0].0.name.as_str(), plan[0].1), ("A", StartType::Manual));
    }

    #[test]
    fn defaults_and_questions_are_consistent() {
        let mut seen = HashSet::new();
        for (n, t) in DEFAULTS {
            assert!(seen.insert(n.to_ascii_lowercase()), "duplicate default {n}");
            assert!(t.sc_value().is_some());
            assert!(!is_protected(n), "{n} is protected");
        }
        // Every service the questionnaire changes has a default to go back to.
        for q in QUESTIONS {
            for (n, _) in q.services {
                assert!(default_start(n).is_some(), "{n} has no default");
            }
        }
    }

    #[test]
    fn enumerates_real_services_and_drivers() {
        let services = enumerate(Kind::Service).unwrap();
        assert!(services.len() > 50, "got {}", services.len());
        let rpc = services
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case("RpcSs"))
            .expect("RpcSs present");
        assert_eq!(rpc.state, RunState::Running);
        assert_eq!(rpc.start, StartType::Automatic);
        assert!(!rpc.binary.is_empty());
        assert!(
            rpc.file.as_ref().is_some_and(|f| f.exists()),
            "svchost DLL resolved: {:?}",
            rpc.file
        );
        assert!(!rpc.log_on.is_empty());
        assert!(services.iter().any(|s| !s.description.is_empty()));

        let drivers = enumerate(Kind::Driver).unwrap();
        assert!(drivers.len() > 50, "got {}", drivers.len());
        assert!(drivers.iter().all(|d| d.kind == Kind::Driver));
        let resolved = drivers
            .iter()
            .filter(|d| d.file.as_ref().is_some_and(|f| f.exists()))
            .count();
        assert!(resolved * 2 > drivers.len(), "only {resolved} driver files found");
    }
}
