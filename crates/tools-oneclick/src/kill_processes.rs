//! 1-Click Kill Processes: end programs you don't need right now to free memory and CPU
//! (before a game, a render or a meeting), in three tiers:
//! - Standard: background programs without a window (tray apps, updaters, helpers of
//!   closed programs).
//! - Enhanced: also programs with windows, which are asked to close (unsaved work
//!   prompts appear in them).
//! - Maximum: also Windows' optional helpers (widgets, search, phone link…) and Explorer,
//!   which is restarted.
//!
//! Only processes in your own session are touched; Windows' system and shell processes,
//! terminals and shells, this app, and your protected list are always kept. Helper
//! processes of programs that stay open are kept too, so open apps don't break.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use wm_core::{
    AppCtx, Category, Tool, egui,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    tool_settings,
    ui::{Confirm, Message, format_bytes, show_message},
};

const ID: &str = "killprocs";

/// Never ended: Windows itself, the shell's plumbing, security, terminals and shells.
const ALWAYS_KEEP: &[&str] = &[
    "system",
    "registry",
    "idle",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "lsaiso.exe",
    "fontdrvhost.exe",
    "dwm.exe",
    "memory compression",
    "svchost.exe",
    "sihost.exe",
    "ctfmon.exe",
    "taskhostw.exe",
    "runtimebroker.exe",
    "conhost.exe",
    "dllhost.exe",
    "audiodg.exe",
    "smartscreen.exe",
    "securityhealthsystray.exe",
    "securityhealthservice.exe",
    "msmpeng.exe",
    "nissrv.exe",
    "textinputhost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "searchindexer.exe",
    "spoolsv.exe",
    "wudfhost.exe",
    "userinit.exe",
    "lockapp.exe",
    "logonui.exe",
    "applicationframehost.exe",
    "windowsterminal.exe",
    "openconsole.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "wsl.exe",
    "wslhost.exe",
    "taskmgr.exe",
    "mmc.exe",
    "consent.exe",
    "useroobebroker.exe",
    "credentialuibroker.exe",
    // Shell and background hosts Windows starts on demand.
    "shellhost.exe",
    "backgroundtaskhost.exe",
    "searchprotocolhost.exe",
    "searchfilterhost.exe",
    "wmiprvse.exe",
    "unsecapp.exe",
    "systemsettingsbroker.exe",
    "settingsynchost.exe",
    "gameinputsvc.exe",
    "monotificationux.exe",
    "storedesktopextension.exe",
];

/// Anything started (directly or further down) by these is command-line work in
/// progress and is kept.
const CONSOLES: &[&str] = &[
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "windowsterminal.exe",
    "openconsole.exe",
    "conhost.exe",
    "wsl.exe",
    "wslhost.exe",
];

/// Windows' optional helpers, ended only at the Maximum tier (Explorer is restarted).
const MAXIMUM_ONLY: &[&str] = &[
    "explorer.exe",
    "searchhost.exe",
    "widgets.exe",
    "widgetservice.exe",
    "phoneexperiencehost.exe",
    "yourphone.exe",
    "gamebar.exe",
    "gamebarftserver.exe",
    "crossdeviceresume.exe",
    "lockapp.exe",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, PartialOrd, Ord)]
pub enum Tier {
    #[default]
    Standard,
    Enhanced,
    Maximum,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Standard => "Standard",
            Tier::Enhanced => "Enhanced",
            Tier::Maximum => "Maximum",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Tier::Standard => "Background programs without a window: tray apps, updaters, launchers.",
            Tier::Enhanced => {
                "Also open programs; they're asked to close, so they can offer to save your work."
            }
            Tier::Maximum => {
                "Also Windows' optional helpers (widgets, search, Phone Link) and Explorer, which restarts."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub session: u32,
    pub has_window: bool,
    pub memory: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub pid: u32,
    pub name: String,
    pub memory: u64,
    /// Ask it to close (it has a window) rather than ending it at once.
    pub polite: bool,
}

/// Which processes a tier would end.
pub fn plan(procs: &[Proc], tier: Tier, protected: &[String], own_pid: u32, own_session: u32) -> Vec<Target> {
    let lower = |s: &str| s.to_ascii_lowercase();
    let protected: HashSet<String> = protected.iter().map(|p| lower(p.trim())).collect();
    let keep_name = |n: &str| {
        let n = lower(n);
        ALWAYS_KEEP.contains(&n.as_str())
            || protected.contains(&n)
            || n == "win-manager.exe"
            || (tier < Tier::Maximum && MAXIMUM_ONLY.contains(&n.as_str()))
    };
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    // Programs that stay: kept names, and windowed programs below Enhanced.
    let stays = |p: &Proc| {
        p.pid == own_pid
            || p.session != own_session
            || p.pid <= 4
            || keep_name(&p.name)
            || (p.has_window && tier == Tier::Standard)
    };
    // A helper whose parent (or grandparent) stays belongs to an open program.
    let parent_stays = |p: &Proc| {
        let mut cur = p.parent;
        for _ in 0..2 {
            let Some(pp) = cur.and_then(|id| by_pid.get(&id)) else {
                return false;
            };
            // Explorer and services start nearly everything; they don't make a child an app helper.
            let n = lower(&pp.name);
            if n == "explorer.exe" || n == "services.exe" || n == "svchost.exe" || n == "userinit.exe" {
                return false;
            }
            if stays(pp) {
                return true;
            }
            cur = pp.parent;
        }
        false
    };
    // Started under a terminal or shell, however deep (guarding against pid reuse loops).
    let in_console = |p: &Proc| {
        let mut cur = p.parent;
        for _ in 0..32 {
            let Some(pp) = cur.and_then(|id| by_pid.get(&id)) else {
                return false;
            };
            if CONSOLES.contains(&lower(&pp.name).as_str()) {
                return true;
            }
            cur = pp.parent;
        }
        false
    };
    let mut out: Vec<Target> = procs
        .iter()
        .filter(|p| !in_console(p))
        .filter(|p| !stays(p) && !parent_stays(p))
        .map(|p| Target {
            pid: p.pid,
            name: p.name.clone(),
            memory: p.memory,
            polite: p.has_window,
        })
        .collect();
    out.sort_by_key(|t| std::cmp::Reverse(t.memory));
    out
}

fn windowed_pids() -> HashSet<u32> {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> windows::core::BOOL {
        unsafe {
            let set = &mut *(l.0 as *mut HashSet<u32>);
            let tool = GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
            let owned = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid());
            if IsWindowVisible(h).as_bool() && !tool && !owned && GetWindowTextLengthW(h) > 0 {
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, Some(&mut pid));
                set.insert(pid);
            }
        }
        true.into()
    }
    let mut set: HashSet<u32> = HashSet::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut set as *mut _ as isize));
    }
    set
}

fn session_of(pid: u32) -> u32 {
    let mut s = u32::MAX;
    unsafe {
        let _ = ProcessIdToSessionId(pid, &mut s);
    }
    s
}

pub fn snapshot() -> Vec<Proc> {
    let mut sys = System::new().expect("sysinfo");
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let windows = windowed_pids();
    sys.processes()
        .values()
        .map(|p| {
            let pid = p.pid().as_u32();
            Proc {
                pid,
                parent: p.parent().map(|x| x.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                session: session_of(pid),
                has_window: windows.contains(&pid),
                memory: p.memory(),
            }
        })
        .collect()
}

pub fn own_session() -> u32 {
    session_of(std::process::id())
}

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub ended: Vec<String>,
    pub failed: Vec<String>,
    pub freed: u64,
}

/// Ends the targets: windowed programs are asked to close, the rest are ended. Explorer
/// is restarted afterwards when it was ended.
pub fn end(targets: &[Target]) -> Outcome {
    let mut o = Outcome::default();
    let mut explorer = false;
    for t in targets {
        let is_explorer = t.name.eq_ignore_ascii_case("explorer.exe");
        explorer |= is_explorer;
        let pid = t.pid.to_string();
        let args: Vec<&str> = if t.polite && !is_explorer {
            vec!["/PID", &pid]
        } else {
            vec!["/F", "/PID", &pid]
        };
        match run_checked(hidden_command("taskkill").args(&args)) {
            Ok(_) => {
                o.ended.push(t.name.clone());
                o.freed += t.memory;
            }
            Err(e) => o.failed.push(format!("{} ({}): {e:#}", t.name, t.pid)),
        }
    }
    if explorer {
        std::thread::sleep(std::time::Duration::from_millis(800));
        let _ = std::process::Command::new("explorer.exe").spawn();
    }
    o
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub tier: Tier,
    pub protected: Vec<String>,
    pub log: Vec<String>,
}

/// The scheduled / rule action: the Standard tier with the saved protected list.
pub fn run_standard(ctx: &AppCtx<'_>) -> Result<String> {
    let mut s: Settings = tool_settings::load(ID);
    let targets = plan(
        &snapshot(),
        Tier::Standard,
        &s.protected,
        std::process::id(),
        own_session(),
    );
    let o = end(&targets);
    let line = summary(&o, Tier::Standard);
    record(ctx, &mut s, &line);
    Ok(line)
}

fn summary(o: &Outcome, tier: Tier) -> String {
    let mut line = format!(
        "{} tier: ended {} program(s), about {} freed",
        tier.label(),
        o.ended.len(),
        format_bytes(o.freed)
    );
    if !o.failed.is_empty() {
        line += &format!("; {} couldn't be ended", o.failed.len());
    }
    line
}

fn record(ctx: &AppCtx<'_>, s: &mut Settings, line: &str) {
    s.log.push(format!(
        "{}  {line}",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    ));
    let n = s.log.len();
    if n > 20 {
        s.log.drain(..n - 20);
    }
    let _ = tool_settings::save(ID, s);
    let _ = ctx.journal.record(ID, line.to_owned(), UndoAction::None);
}

pub struct KillProcesses {
    settings: Settings,
    preview: Option<Vec<(Target, bool)>>,
    new_protected: String,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for KillProcesses {
    fn default() -> Self {
        Self {
            settings: tool_settings::load(ID),
            preview: None,
            new_protected: String::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl KillProcesses {
    fn refresh(&mut self) {
        let targets = plan(
            &snapshot(),
            self.settings.tier,
            &self.settings.protected,
            std::process::id(),
            own_session(),
        );
        self.preview = Some(targets.into_iter().map(|t| (t, true)).collect());
    }

    fn save(&mut self) {
        let _ = tool_settings::save(ID, &self.settings);
    }
}

impl Tool for KillProcesses {
    fn id(&self) -> &'static str {
        ID
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let chosen: Vec<Target> = self
                    .preview
                    .iter()
                    .flatten()
                    .filter(|(_, on)| *on)
                    .map(|(t, _)| t.clone())
                    .collect();
                let o = end(&chosen);
                let line = summary(&o, self.settings.tier);
                record(ctx, &mut self.settings, &line);
                self.message = Some(if o.failed.is_empty() {
                    Ok(format!("{line}."))
                } else {
                    Err(format!("{line}: {}", o.failed.join("; ")))
                });
            }
            self.preview = None;
        }
        if self.preview.is_none() {
            self.refresh();
        }

        ui.label("Ends programs you don't need right now, to free memory and CPU before a game, a big job or a meeting.");
        ui.add_space(4.0);
        let before = self.settings.tier;
        for t in [Tier::Standard, Tier::Enhanced, Tier::Maximum] {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.settings.tier, t, t.label());
                ui.weak(t.description());
            });
        }
        if self.settings.tier != before {
            self.save();
            self.preview = None;
        }
        show_message(ui, &self.message);
        ui.separator();

        let list = self.preview.clone().unwrap_or_default();
        let chosen: Vec<&Target> = list.iter().filter(|(_, on)| *on).map(|(t, _)| t).collect();
        let memory: u64 = chosen.iter().map(|t| t.memory).sum();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !chosen.is_empty(),
                    egui::Button::new(
                        egui::RichText::new(format!("End {} programs", chosen.len())).size(16.0),
                    ),
                )
                .clicked()
            {
                let polite = chosen.iter().filter(|t| t.polite).count();
                let mut q = format!("End {} programs (about {})?", chosen.len(), format_bytes(memory));
                if polite > 0 {
                    q += &format!(" {polite} open program(s) are asked to close and may ask to save.");
                }
                if chosen.iter().any(|t| t.name.eq_ignore_ascii_case("explorer.exe")) {
                    q += " Explorer restarts.";
                }
                self.confirm.ask(q, ());
            }
            if ui.button(ctx.i18n.tr("common-refresh")).clicked() {
                self.preview = None;
            }
            ui.weak(format!("{} would be freed", format_bytes(memory)));
        });
        let mut toggles = Vec::new();
        let mut protect = None;
        egui::ScrollArea::vertical()
            .max_height(280.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if list.is_empty() {
                    ui.weak("Nothing to end at this tier.");
                }
                egui::Grid::new("kill-grid")
                    .striped(true)
                    .num_columns(4)
                    .show(ui, |ui| {
                        for (i, (t, on)) in list.iter().enumerate() {
                            let mut v = *on;
                            if ui.checkbox(&mut v, &t.name).changed() {
                                toggles.push((i, v));
                            }
                            ui.weak(format_bytes(t.memory));
                            ui.weak(if t.polite { "asked to close" } else { "ended" });
                            if ui
                                .small_button("Protect")
                                .on_hover_text("Never end this program")
                                .clicked()
                            {
                                protect = Some(t.name.clone());
                            }
                            ui.end_row();
                        }
                    });
            });
        if let Some(p) = &mut self.preview {
            for (i, v) in toggles {
                p[i].1 = v;
            }
        }
        if let Some(n) = protect {
            if !self.settings.protected.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                self.settings.protected.push(n);
            }
            self.save();
            self.preview = None;
        }

        ui.add_space(6.0);
        egui::CollapsingHeader::new(format!("Protected programs ({})", self.settings.protected.len())).show(ui, |ui| {
            ui.weak("Never ended, at any tier. Windows' own processes, terminals and this app are always protected.");
            let mut remove = None;
            for (i, p) in self.settings.protected.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(p);
                    if ui.small_button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_protected).hint_text("program.exe").desired_width(160.0));
                if ui.button("Add").clicked() && !self.new_protected.trim().is_empty() {
                    self.settings.protected.push(self.new_protected.trim().to_owned());
                    self.new_protected.clear();
                    self.save();
                    self.preview = None;
                }
            });
            if let Some(i) = remove {
                self.settings.protected.remove(i);
                self.save();
                self.preview = None;
            }
        });
        egui::CollapsingHeader::new("History").show(ui, |ui| {
            if self.settings.log.is_empty() {
                ui.weak("Nothing ended yet.");
            }
            for l in self.settings.log.iter().rev() {
                ui.weak(l);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, name: &str, window: bool) -> Proc {
        Proc {
            pid,
            parent: Some(parent),
            name: name.into(),
            session: 1,
            has_window: window,
            memory: pid as u64 * 1000,
        }
    }

    fn names(t: &[Target]) -> Vec<&str> {
        let mut v: Vec<&str> = t.iter().map(|t| t.name.as_str()).collect();
        v.sort();
        v
    }

    #[test]
    fn tiers_escalate_and_keep_what_matters() {
        let procs = vec![
            p(10, 1, "explorer.exe", true),
            p(11, 10, "updater.exe", false),
            p(12, 10, "game.exe", true),
            p(13, 12, "game-helper.exe", false),
            p(14, 1, "svchost.exe", false),
            p(15, 10, "searchhost.exe", false),
            p(16, 10, "win-manager.exe", true),
            p(17, 10, "keepme.exe", false),
            Proc {
                session: 0,
                ..p(18, 1, "service-thing.exe", false)
            },
            p(19, 10, "cmd.exe", true),
            p(20, 19, "cargo.exe", false),
            p(21, 20, "rustc.exe", false),
        ];
        let protected = vec!["KeepMe.exe".to_owned()];
        let std = plan(&procs, Tier::Standard, &protected, 999, 1);
        assert_eq!(names(&std), ["updater.exe"], "helpers of open programs stay");
        let enh = plan(&procs, Tier::Enhanced, &protected, 999, 1);
        assert_eq!(names(&enh), ["game-helper.exe", "game.exe", "updater.exe"]);
        assert!(enh.iter().find(|t| t.name == "game.exe").unwrap().polite);
        let max = plan(&procs, Tier::Maximum, &protected, 999, 1);
        assert_eq!(
            names(&max),
            [
                "explorer.exe",
                "game-helper.exe",
                "game.exe",
                "searchhost.exe",
                "updater.exe"
            ]
        );
        assert!(
            max.windows(2).all(|w| w[0].memory >= w[1].memory),
            "largest first"
        );
    }

    #[test]
    fn ends_a_real_background_process() {
        let mut child = hidden_command("ping")
            .args(["-n", "60", "127.0.0.1"])
            .spawn()
            .unwrap();
        let t = Target {
            pid: child.id(),
            name: "PING.EXE".into(),
            memory: 0,
            polite: false,
        };
        let o = end(&[t]);
        assert_eq!(o.ended.len(), 1, "{:?}", o.failed);
        let status = child.wait().unwrap();
        assert!(!status.success());
        // The live snapshot works and sees this test.
        assert!(snapshot().iter().any(|p| p.pid == std::process::id()));
    }
}
