//! The background monitor (`win-manager.exe --monitor`): checks My Task's live rules
//! once a second and carries out their actions, with an icon in the notification area
//! (open the app, pause rules, cancel a pending power action, stop the monitor).
//!
//! One monitor runs per user (a named mutex). The app stops it through a named event,
//! and finds out whether it runs by opening the mutex.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetSuspendState, SetThreadExecutionState,
};
use windows::Win32::System::Shutdown::{EWX_LOGOFF, ExitWindowsEx, LockWorkStation, SHUTDOWN_REASON};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP, OpenInputDesktop, SwitchDesktop,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE,
    SetEvent,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, HWND_BROADCAST, IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING, MSG,
    MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT, RegisterClassW,
    SC_MONITORPOWER, SetForegroundWindow, TPM_RETURNCMD, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE,
    WM_APP, WM_LBUTTONUP, WM_RBUTTONUP, WM_SYSCOMMAND, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR, w};
use wm_core::{
    drives::{self, DriveKind},
    process::{hidden_command, run_checked},
    rules::{self, Action, RuleState, Rules, Sample, Trigger},
    shell::shell_open,
    tool_settings,
};

const MUTEX: &str = r"Local\WinManagerRS.Monitor";
const STOP_EVENT: &str = r"Local\WinManagerRS.MonitorStop";
const LOG_ID: &str = "mytask-log";
const TRAY_MSG: u32 = WM_APP + 1;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WinManager Monitor";

// ---------------------------------------------------------------------------------
// Control from the app

pub fn is_running() -> bool {
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(MUTEX)) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

pub fn start() -> Result<()> {
    if is_running() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe).arg("--monitor").spawn()?;
    Ok(())
}

pub fn stop() -> Result<()> {
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(STOP_EVENT))?;
        SetEvent(h)?;
        let _ = CloseHandle(h);
    }
    Ok(())
}

pub fn starts_at_sign_in() -> bool {
    wm_core::registry::read_raw(RUN_KEY, RUN_VALUE).is_some()
}

pub fn set_start_at_sign_in(ctx: &wm_core::AppCtx<'_>, on: bool) -> Result<()> {
    let snap = wm_core::registry::snapshot(RUN_KEY, RUN_VALUE);
    if on {
        let exe = std::env::current_exe()?;
        wm_core::registry::write_raw(
            RUN_KEY,
            RUN_VALUE,
            &wm_core::registry::string(&format!("\"{}\" --monitor", exe.display())),
        )?;
    } else {
        wm_core::registry::delete_value(RUN_KEY, RUN_VALUE)?;
    }
    ctx.journal.record(
        "mytask",
        if on {
            "The rule monitor starts at sign-in"
        } else {
            "The rule monitor no longer starts at sign-in"
        },
        wm_core::safety::UndoAction::RestoreRegValues { values: vec![snap] },
    )?;
    Ok(())
}

/// What the monitor did recently (newest last), for My Task to show.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Log {
    pub lines: Vec<String>,
}

pub fn log() -> Log {
    tool_settings::load(LOG_ID)
}

fn log_line(text: String) {
    tracing::info!("monitor: {text}");
    let mut l = log();
    l.lines.push(format!(
        "{}  {text}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    let n = l.lines.len();
    if n > 50 {
        l.lines.drain(..n - 50);
    }
    let _ = tool_settings::save(LOG_ID, &l);
}

// ---------------------------------------------------------------------------------
// Sampling

fn idle_secs() -> u64 {
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut info).as_bool() {
            return 0;
        }
        (GetTickCount().wrapping_sub(info.dwTime) / 1000) as u64
    }
}

/// The input desktop can't be switched to while the PC is locked.
pub fn is_locked() -> bool {
    unsafe {
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) {
            Ok(d) => {
                let locked = SwitchDesktop(d).is_err();
                let _ = CloseDesktop(d);
                locked
            }
            Err(_) => true,
        }
    }
}

fn usb_drives() -> BTreeSet<char> {
    drives::list()
        .into_iter()
        .filter(|d| d.kind == DriveKind::Removable)
        .map(|d| d.letter)
        .collect()
}

pub struct Sampler {
    sys: System,
    last: Instant,
    net: Option<u64>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let mut sys = System::new().expect("sysinfo");
        sys.refresh_cpu_usage();
        Self {
            sys,
            last: Instant::now(),
            net: None,
        }
    }

    pub fn sample(&mut self) -> Sample {
        let dt = self.last.elapsed().as_secs_f64().max(0.001);
        self.last = Instant::now();
        self.sys.refresh_cpu_usage();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_disk_usage(),
        );
        let mut processes = BTreeSet::new();
        let mut process_io: HashMap<String, f64> = HashMap::new();
        for p in self.sys.processes().values() {
            let name = p.name().to_string_lossy().to_lowercase();
            let d = p.disk_usage();
            *process_io.entry(name.clone()).or_default() += (d.read_bytes + d.written_bytes) as f64 / dt;
            processes.insert(name);
        }
        let net_now = wm_tools_network::total_traffic_bytes();
        let net_bps = self
            .net
            .map_or(0.0, |prev| net_now.saturating_sub(prev) as f64 / dt);
        self.net = Some(net_now);
        Sample {
            dt,
            cpu: self.sys.global_cpu_usage(),
            processes,
            process_io,
            idle_secs: idle_secs(),
            net_bps,
            usb: usb_drives(),
            locked: is_locked(),
        }
    }
}

// ---------------------------------------------------------------------------------
// Actions

fn mute() -> Result<()> {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    wm_core::shell::ensure_com();
    unsafe {
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = e.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;
        vol.SetMute(true, std::ptr::null())?;
    }
    Ok(())
}

/// Shows a reminder without stopping the monitor (each box runs on its own thread).
fn message_box(text: String) {
    std::thread::spawn(move || unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            MB_ICONINFORMATION, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
        };
        MessageBoxW(
            None,
            &HSTRING::from(text),
            w!("Win Manager RS"),
            MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
        );
    });
}

pub fn perform(action: &Action) -> Result<()> {
    match action {
        Action::Shutdown => {
            run_checked(hidden_command("shutdown").args(["/s", "/t", "0"]))?;
        }
        Action::Restart => {
            run_checked(hidden_command("shutdown").args(["/r", "/t", "0"]))?;
        }
        Action::Sleep | Action::Hibernate => unsafe {
            if !SetSuspendState(matches!(action, Action::Hibernate), false, false) {
                bail!("Windows refused to {}", action.describe());
            }
        },
        Action::SignOut => unsafe { ExitWindowsEx(EWX_LOGOFF, SHUTDOWN_REASON(0))? },
        Action::Lock => unsafe { LockWorkStation()? },
        Action::MonitorOff => unsafe {
            PostMessageW(
                Some(HWND_BROADCAST),
                WM_SYSCOMMAND,
                WPARAM(SC_MONITORPOWER as usize),
                LPARAM(2),
            )?;
        },
        Action::Mute => mute()?,
        Action::Run { program, args } => shell_open(program, args)?,
        Action::Remind { text } => message_box(text.clone()),
        Action::Maintenance { id } => {
            let exe = std::env::current_exe()?;
            hidden_command(&exe.display().to_string())
                .args(["--run", id])
                .spawn()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------
// The monitor itself

struct Tray {
    hwnd: HWND,
}

impl Tray {
    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    fn wide<const N: usize>(dst: &mut [u16; N], s: &str) {
        for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(N - 1).chain([0])) {
            *d = c;
        }
    }

    fn add(&self, tip: &str) -> bool {
        let mut d = self.data();
        d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        d.uCallbackMessage = TRAY_MSG;
        d.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() };
        Self::wide(&mut d.szTip, tip);
        unsafe { Shell_NotifyIconW(NIM_ADD, &d).as_bool() }
    }

    fn balloon(&self, title: &str, text: &str) {
        let mut d = self.data();
        d.uFlags = NIF_INFO;
        Self::wide(&mut d.szInfoTitle, title);
        Self::wide(&mut d.szInfo, text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Clicks on the tray icon, read by the main loop.
static TRAY_CLICK: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == TRAY_MSG {
        let event = (l.0 & 0xFFFF) as u32;
        if event == WM_LBUTTONUP || event == WM_RBUTTONUP {
            *TRAY_CLICK.lock().unwrap() = Some(event);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

const CMD_OPEN: usize = 1;
const CMD_PAUSE: usize = 2;
const CMD_CANCEL: usize = 3;
const CMD_STOP: usize = 4;

fn tray_menu(hwnd: HWND, paused: bool, pending: bool) -> usize {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN, w!("Open Win Manager"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_PAUSE,
            if paused {
                w!("Resume rules")
            } else {
                w!("Pause rules")
            },
        );
        if pending {
            let _ = AppendMenuW(menu, MF_STRING, CMD_CANCEL, w!("Cancel the pending action"));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_STOP, w!("Stop the rule monitor"));
        let mut pt = Default::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD, pt.x, pt.y, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(menu);
        cmd
    }
}

const APP_WINDOW_ID: &str = "app-window";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct AppWindow {
    hwnd: isize,
    pid: u32,
}

/// The app records its main window so the tray icon can bring back exactly that
/// window (other windows may share its title).
pub fn remember_app_window(hwnd: isize) {
    let _ = tool_settings::save(
        APP_WINDOW_ID,
        &AppWindow {
            hwnd,
            pid: std::process::id(),
        },
    );
}

fn app_window() -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};
    let w: AppWindow = tool_settings::load(APP_WINDOW_ID);
    let h = HWND(w.hwnd as *mut _);
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        (w.hwnd != 0 && IsWindow(Some(h)).as_bool() && pid == w.pid).then_some(h)
    }
}

/// Shows the app's main window if it's open (hidden in the notification area), or
/// starts the app.
pub fn open_app() {
    use windows::Win32::UI::WindowsAndMessaging::{SW_RESTORE, SW_SHOW, ShowWindow};
    if let Some(h) = app_window() {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = ShowWindow(h, SW_RESTORE);
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// Watches the app's main window: while `enabled` is set, minimising it hides it in the
/// notification area (starting the monitor for the icon). The app doesn't draw while
/// minimised, so this runs on its own thread.
pub fn watch_minimize(hwnd: isize, enabled: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow, IsWindowVisible, SW_HIDE, ShowWindow};
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let h = HWND(hwnd as *mut _);
            unsafe {
                if !IsWindow(Some(h)).as_bool() {
                    return;
                }
                if enabled.load(std::sync::atomic::Ordering::Relaxed)
                    && IsIconic(h).as_bool()
                    && IsWindowVisible(h).as_bool()
                    && start().is_ok()
                {
                    let _ = ShowWindow(h, SW_HIDE);
                }
            }
        }
    });
}

fn create_window() -> Result<HWND> {
    unsafe {
        let class = w!("WinManagerRSMonitor");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        Ok(CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Win Manager RS monitor"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )?)
    }
}

struct Pending {
    rule: String,
    action: Action,
    at: Instant,
}

/// Runs until stopped (tray menu, the app, or the session ending). Returns the exit code.
pub fn run() -> i32 {
    let (mutex, stop) = unsafe {
        let Ok(m) = CreateMutexW(None, true, &HSTRING::from(MUTEX)) else {
            return 1;
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(m);
            return 0; // already running
        }
        let Ok(e) = CreateEventW(None, true, false, &HSTRING::from(STOP_EVENT)) else {
            return 1;
        };
        (m, e)
    };
    let code = match create_window() {
        Ok(hwnd) => {
            let tray = Tray { hwnd };
            tray.add("Win Manager RS: watching My Task's rules");
            log_line("Rule monitor started.".into());
            monitor_loop(&tray, stop);
            log_line("Rule monitor stopped.".into());
            0
        }
        Err(e) => {
            tracing::error!("monitor window: {e:#}");
            1
        }
    };
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS);
        let _ = CloseHandle(stop);
        let _ = CloseHandle(mutex);
    }
    code
}

fn monitor_loop(tray: &Tray, stop: HANDLE) {
    let mut sampler = Sampler::new();
    let mut rules: Rules = tool_settings::load(rules::SETTINGS_ID);
    let mut states: HashMap<String, RuleState> = HashMap::new();
    let mut prev: Option<Sample> = None;
    let mut paused = false;
    let mut pending: Option<Pending> = None;
    let mut last_tick = Instant::now() - Duration::from_secs(1);
    let mut reload_at = Instant::now();
    loop {
        let waited = unsafe { MsgWaitForMultipleObjects(Some(&[stop]), false, 250, QS_ALLINPUT) };
        if waited == WAIT_OBJECT_0 {
            return;
        }
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Some(click) = TRAY_CLICK.lock().unwrap().take() {
            let cmd = if click == WM_LBUTTONUP {
                CMD_OPEN
            } else {
                tray_menu(tray.hwnd, paused, pending.is_some())
            };
            match cmd {
                CMD_OPEN => open_app(),
                CMD_PAUSE => {
                    paused = !paused;
                    log_line(if paused { "Rules paused." } else { "Rules resumed." }.into());
                }
                CMD_CANCEL => {
                    if let Some(p) = pending.take() {
                        log_line(format!("Cancelled \"{}\" ({}).", p.rule, p.action.describe()));
                    }
                }
                CMD_STOP => return,
                _ => {}
            }
        }
        if let Some(p) = &pending
            && Instant::now() >= p.at
        {
            let p = pending.take().unwrap();
            let r = perform(&p.action);
            log_line(match r {
                Ok(()) => format!("\"{}\": {}.", p.rule, p.action.describe()),
                Err(e) => format!("\"{}\": couldn't {}: {e:#}", p.rule, p.action.describe()),
            });
        }
        if last_tick.elapsed() < Duration::from_millis(1000) {
            continue;
        }
        last_tick = Instant::now();
        if reload_at.elapsed() >= Duration::from_secs(3) {
            rules = tool_settings::load(rules::SETTINGS_ID);
            reload_at = Instant::now();
        }
        let now = sampler.sample();
        let awake = !paused && rules.rules.iter().any(|r| r.enabled && r.keep_awake);
        unsafe {
            SetThreadExecutionState(if awake {
                ES_CONTINUOUS | ES_SYSTEM_REQUIRED
            } else {
                ES_CONTINUOUS
            });
        }
        let mut changed = false;
        if !paused {
            for rule in rules.rules.iter_mut().filter(|r| r.enabled) {
                let st = states.entry(rule.name.clone()).or_default();
                if !rules::step(&rule.trigger, st, prev.as_ref(), &now) {
                    continue;
                }
                if !rule.repeat {
                    rule.enabled = false;
                    changed = true;
                }
                if rule.action.is_power() && rule.countdown_secs > 0 {
                    tray.balloon(
                        &rule.name,
                        &format!(
                            "{} in {} seconds. Right-click the Win Manager icon to cancel.",
                            rule.action.describe(),
                            rule.countdown_secs
                        ),
                    );
                    log_line(format!(
                        "\"{}\" ({}): {} in {} s.",
                        rule.name,
                        rule.trigger.describe(),
                        rule.action.describe(),
                        rule.countdown_secs
                    ));
                    pending = Some(Pending {
                        rule: rule.name.clone(),
                        action: rule.action.clone(),
                        at: Instant::now() + Duration::from_secs(rule.countdown_secs as u64),
                    });
                    continue;
                }
                let r = perform(&rule.action);
                log_line(match r {
                    Ok(()) => format!(
                        "\"{}\" ({}): {}.",
                        rule.name,
                        rule.trigger.describe(),
                        rule.action.describe()
                    ),
                    Err(e) => format!("\"{}\": couldn't {}: {e:#}", rule.name, rule.action.describe()),
                });
            }
        }
        if changed {
            let _ = tool_settings::save(rules::SETTINGS_ID, &rules);
        }
        // Forget states of rules that were removed.
        states.retain(|name, _| rules.rules.iter().any(|r| &r.name == name));
        prev = Some(now);
    }
}

/// Rules whose trigger needs a value the monitor measures (for the UI's hints).
pub fn trigger_kinds() -> [&'static str; 13] {
    [
        "CPU above",
        "CPU below",
        "Program starts",
        "Program exits",
        "Program's disk activity low",
        "No input",
        "Network above",
        "Network below",
        "Take a break",
        "USB drive plugged in",
        "USB drive removed",
        "PC locked",
        "PC unlocked",
    ]
}

/// A default trigger for each kind in [`trigger_kinds`].
pub fn default_trigger(kind: usize) -> Trigger {
    match kind {
        0 => Trigger::CpuAbove {
            percent: 90.0,
            secs: 60,
        },
        1 => Trigger::CpuBelow {
            percent: 10.0,
            secs: 300,
        },
        2 => Trigger::ProcessStarts {
            name: "notepad.exe".into(),
        },
        3 => Trigger::ProcessExits {
            name: "notepad.exe".into(),
        },
        4 => Trigger::ProcessIoBelow {
            name: "setup.exe".into(),
            kbps: 10.0,
            secs: 120,
        },
        5 => Trigger::Idle { minutes: 15 },
        6 => Trigger::NetAbove {
            kbps: 1000.0,
            secs: 30,
        },
        7 => Trigger::NetBelow {
            kbps: 20.0,
            secs: 300,
        },
        8 => Trigger::TakeRest { minutes: 50 },
        9 => Trigger::UsbInserted,
        10 => Trigger::UsbRemoved,
        11 => Trigger::Locked,
        _ => Trigger::Unlocked,
    }
}

pub fn trigger_kind(t: &Trigger) -> usize {
    match t {
        Trigger::CpuAbove { .. } => 0,
        Trigger::CpuBelow { .. } => 1,
        Trigger::ProcessStarts { .. } => 2,
        Trigger::ProcessExits { .. } => 3,
        Trigger::ProcessIoBelow { .. } => 4,
        Trigger::Idle { .. } => 5,
        Trigger::NetAbove { .. } => 6,
        Trigger::NetBelow { .. } => 7,
        Trigger::TakeRest { .. } => 8,
        Trigger::UsbInserted => 9,
        Trigger::UsbRemoved => 10,
        Trigger::Locked => 11,
        Trigger::Unlocked => 12,
    }
}

/// Used by the uninstaller (`--uninstall-cleanup`): stops the rule monitor and removes
/// everything this app registered with Windows for the current account (sign-in entry,
/// scheduled tasks, Explorer right-click entries). Settings and the Change Journal stay.
/// Returns a line per step.
pub fn uninstall_cleanup() -> Vec<String> {
    use wm_core::process::{hidden_command, run_checked};
    let mut log = Vec::new();
    if is_running() {
        log.push(format!(
            "stop the rule monitor: {}",
            stop().map_or_else(|e| format!("{e:#}"), |()| "ok".into())
        ));
    }
    if starts_at_sign_in() {
        let r = wm_core::registry::delete_value(RUN_KEY, RUN_VALUE);
        log.push(format!(
            "remove the sign-in entry: {}",
            r.map_or_else(|e| format!("{e:#}"), |()| "ok".into())
        ));
    }
    // Scheduled tasks live in their own folder; delete each, then the folder.
    let folder = wm_core::scheduler::FOLDER;
    if let Ok(out) = hidden_command("schtasks")
        .args(["/query", "/fo", "csv", "/nh"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let prefix = format!(r"{folder}\");
        let tasks: Vec<String> = text
            .lines()
            .filter_map(|l| l.split(',').next())
            .map(|n| n.trim_matches('"').to_owned())
            .filter(|n| n.starts_with(&prefix))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for t in tasks {
            let r = run_checked(hidden_command("schtasks").args(["/delete", "/tn", &t, "/f"]));
            log.push(format!(
                "delete task {t}: {}",
                r.map_or_else(|e| format!("{e:#}"), |_| "ok".into())
            ));
        }
    }
    for class in ["Directory", "Drive", r"Directory\Background", "*"] {
        for id in [
            "WinManager.DiskAnalyzer",
            "WinManager.FileSplitter",
            "WinManager.FilesAndFolders",
        ] {
            let key = format!(r"HKCU\Software\Classes\{class}\shell\{id}");
            if wm_core::registry::open_read(&key).is_ok()
                && let Ok((root, sub)) = wm_core::registry::split_path(&key)
            {
                let r = root.delete_subkey_all(sub);
                log.push(format!(
                    "remove {key}: {}",
                    r.map_or_else(|e| e.to_string(), |()| "ok".into())
                ));
            }
        }
    }
    log
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_works() {
        let mut s = Sampler::new();
        std::thread::sleep(Duration::from_millis(300));
        let x = s.sample();
        assert!(
            x.processes
                .iter()
                .any(|p| p.starts_with("cargo") || p.contains("wm_tools_oneclick") || p.contains("svchost"))
        );
        assert!((0.0..=100.0).contains(&x.cpu));
        assert!(!x.locked, "tests run on an unlocked desktop");
        for k in 0..trigger_kinds().len() {
            assert_eq!(trigger_kind(&default_trigger(k)), k);
        }
    }
}
