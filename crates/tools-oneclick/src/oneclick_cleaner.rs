//! 1-Click Cleaner: like the original, a list of cleanups that run one after another
//! with one Start button, each reporting its result in its row. The choice and options
//! are remembered; Windows can shut down when the clean is done, and scheduled runs
//! (`--run oneclick-clean`) clean the saved selection.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    process::{hidden_command, run_checked},
    safety::UndoAction,
    task::BackgroundTask,
    tool_settings,
    ui::{CheckItem, Confirm, Message, checklist, format_bytes, frame, request_open_tool, show_message},
};
use wm_tools_cleaner::junk::{CleanReport, Rule, RuleScan};
use wm_tools_cleaner::registry_cleaner::{self, Issue};
use wm_tools_cleaner::{desktop, registry_defrag};
use wm_tools_optimizer::startup::{self, StartupItem};
use wm_tools_optimizer::tasks::{self, TaskRow};
use wm_tools_security::privacy::{self, Trace};

use crate::actions;

const ID: &str = "oneclickclean";
/// Seconds Windows waits before shutting down, so the user can still cancel.
const SHUTDOWN_DELAY: u32 = 60;
/// Seconds before the restart that finishes a registry defrag.
const DEFRAG_DELAY: u32 = 60;

/// The cleanups, in the original's order (those that need administrator rights and
/// carry real risk, such as removing services or devices, are not offered).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    Privacy,
    Startup,
    Tasks,
    Desktop,
    Junk,
    Registry,
    Defrag,
}

impl Item {
    pub const ALL: [Item; 7] = [
        Item::Privacy,
        Item::Startup,
        Item::Tasks,
        Item::Desktop,
        Item::Junk,
        Item::Registry,
        Item::Defrag,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Item::Privacy => "Clear privacy history",
            Item::Startup => "Clean up invalid startup items",
            Item::Tasks => "Clean up invalid scheduled tasks",
            Item::Desktop => "Organize desktop items",
            Item::Junk => "Junk File Cleaner",
            Item::Registry => "Registry Cleaner",
            Item::Defrag => "Registry Defrag (may need to restart)",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Item::Privacy => icons::EYE_SLASH,
            Item::Startup => icons::ROCKET,
            Item::Tasks => icons::CALENDAR_CHECK,
            Item::Desktop => icons::DESKTOP,
            Item::Junk => icons::BROOM,
            Item::Registry => icons::DATABASE,
            Item::Defrag => icons::STACK,
        }
    }

    /// The tool whose options the [Options] link opens (Privacy opens its own list).
    fn options_tool(self) -> Option<&'static str> {
        match self {
            Item::Desktop => Some("desktop"),
            Item::Junk => Some("junk"),
            Item::Registry => Some("regclean"),
            _ => None,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Files from before 0.3 had no switch: they cleaned the ticked traces.
    #[serde(default = "yes")]
    pub privacy: bool,
    /// One switch per entry of `Trace::ALL`.
    pub traces: [bool; 5],
    /// Off in files from before 0.3, so scheduled cleans don't start removing entries
    /// nobody chose.
    #[serde(default)]
    pub startup: bool,
    #[serde(default)]
    pub tasks: bool,
    pub desktop: bool,
    pub junk: bool,
    pub registry: bool,
    /// Compact the registry after a clean started from the window, then restart.
    /// Takes the place of `shutdown`; scheduled cleans never do it.
    pub defrag: bool,
    /// Shut Windows down after a clean started from the window.
    pub shutdown: bool,
    /// Unix time of the last clean.
    pub last_clean: Option<i64>,
    pub last_summary: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            privacy: false,
            traces: [true, true, true, false, false],
            startup: true,
            tasks: true,
            desktop: false,
            junk: true,
            registry: true,
            defrag: false,
            shutdown: false,
            last_clean: None,
            last_summary: String::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        tool_settings::load(ID)
    }

    pub fn is_on(&self, item: Item) -> bool {
        match item {
            Item::Privacy => self.privacy,
            Item::Startup => self.startup,
            Item::Tasks => self.tasks,
            Item::Desktop => self.desktop,
            Item::Junk => self.junk,
            Item::Registry => self.registry,
            Item::Defrag => self.defrag,
        }
    }

    fn switch(&mut self, item: Item) -> &mut bool {
        match item {
            Item::Privacy => &mut self.privacy,
            Item::Startup => &mut self.startup,
            Item::Tasks => &mut self.tasks,
            Item::Desktop => &mut self.desktop,
            Item::Junk => &mut self.junk,
            Item::Registry => &mut self.registry,
            Item::Defrag => &mut self.defrag,
        }
    }

    pub fn selected_traces(&self) -> Vec<Trace> {
        if !self.privacy {
            return Vec::new();
        }
        Trace::ALL
            .iter()
            .zip(self.traces)
            .filter(|(_, on)| *on)
            .map(|(t, _)| *t)
            .collect()
    }

    /// Items that run, given the rights the app has (Registry Defrag needs them).
    pub fn chosen(&self, elevated: bool) -> Vec<Item> {
        Item::ALL
            .into_iter()
            .filter(|i| self.is_on(*i) && (*i != Item::Defrag || elevated))
            .collect()
    }

    pub fn anything(&self) -> bool {
        Item::ALL.iter().any(|i| *i != Item::Defrag && self.is_on(*i))
    }

    fn mark_done(&mut self, summary: &str) {
        self.last_clean = Some(chrono::Local::now().timestamp());
        self.last_summary = summary.to_owned();
    }

    pub fn last_clean_text(&self) -> String {
        self.last_clean
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default()
    }
}

#[derive(Clone, Default)]
pub struct ScanResult {
    pub junk: Vec<(Rule, RuleScan)>,
    pub issues: Vec<Issue>,
    /// Item count per entry of `Trace::ALL`.
    pub traces: [usize; 5],
    /// Invalid startup entries this account may remove.
    pub startup: Vec<StartupItem>,
    /// Invalid scheduled tasks.
    pub tasks: Vec<TaskRow>,
}

/// Looks at what each chosen item would clean.
pub fn scan(settings: &Settings, elevated: bool, progress: &dyn Fn(f32, &str)) -> ScanResult {
    let mut res = ScanResult::default();
    if settings.privacy {
        progress(0.05, "Counting activity traces…");
        res.traces = Trace::ALL.map(privacy::count);
    }
    if settings.startup {
        progress(0.15, "Checking startup items…");
        res.startup = startup::enumerate()
            .into_iter()
            .filter(|i| i.is_invalid() && (elevated || !i.source.needs_admin()))
            .collect();
    }
    if settings.tasks {
        progress(0.25, "Checking scheduled tasks…");
        res.tasks = tasks::enumerate()
            .unwrap_or_default()
            .into_iter()
            .filter(TaskRow::is_invalid)
            .collect();
    }
    if settings.junk {
        progress(0.35, "Scanning junk files…");
        res.junk = actions::scan_junk(&actions::default_junk_rules(elevated));
    }
    if settings.registry {
        progress(0.75, "Checking the registry…");
        res.issues = registry_cleaner::scan();
    }
    res
}

/// The steps that write to the change journal, run after the junk deletion (which
/// may run on a worker thread). Returns each item's result.
pub fn finish(
    ctx: &AppCtx<'_>,
    settings: &Settings,
    res: &ScanResult,
    junk: Result<Option<CleanReport>, String>,
) -> Vec<(Item, String)> {
    let mut out = vec![];
    let traces = settings.selected_traces();
    if !traces.is_empty() {
        let mut n = 0;
        let mut errors = vec![];
        for t in traces {
            match privacy::clear(ctx, t) {
                Ok(c) => n += c,
                Err(e) => errors.push(format!("{}: {e:#}", t.label())),
            }
        }
        let mut line = format!("{n} traces cleared");
        if !errors.is_empty() {
            line += &format!(" ({})", errors.join("; "));
        }
        out.push((Item::Privacy, line));
    }
    if settings.startup {
        let mut n = 0;
        let mut errors = vec![];
        for item in &res.startup {
            match startup::remove(ctx, item) {
                Ok(()) => n += 1,
                Err(e) => errors.push(format!("{}: {e:#}", item.name)),
            }
        }
        out.push((Item::Startup, counted(n, "invalid entries removed", &errors)));
    }
    if settings.tasks {
        let mut n = 0;
        let mut errors = vec![];
        for t in &res.tasks {
            match tasks::perform(t, tasks::Action::Delete)
                .and_then(|(desc, undo)| ctx.journal.record("oneclick", desc, undo).map(|_| ()))
            {
                Ok(()) => n += 1,
                Err(e) => errors.push(format!("{}: {e:#}", t.name)),
            }
        }
        out.push((Item::Tasks, counted(n, "invalid tasks removed", &errors)));
    }
    if settings.desktop {
        out.push((
            Item::Desktop,
            desktop::move_unused(ctx).unwrap_or_else(|e| format!("{e:#}")),
        ));
    }
    match junk {
        Ok(Some(rep)) => {
            let desc = format!(
                "Junk files: {} deleted, {} freed",
                rep.removed,
                format_bytes(rep.freed)
            );
            let _ = ctx.journal.record("oneclick", desc, UndoAction::None);
            let mut line = format!("{} deleted, {} freed", rep.removed, format_bytes(rep.freed));
            if rep.failed > 0 {
                line += &format!(" ({} in use, skipped)", rep.failed);
            }
            out.push((Item::Junk, line));
        }
        Ok(None) if settings.junk => out.push((Item::Junk, "Nothing to clean".into())),
        Ok(None) => {}
        Err(e) => out.push((Item::Junk, e)),
    }
    if settings.registry {
        let usable: Vec<Issue> = res
            .issues
            .iter()
            .filter(|i| !wm_core::registry::needs_admin(&i.key) || ctx.elevated)
            .cloned()
            .collect();
        let skipped = res.issues.len() - usable.len();
        out.push((
            Item::Registry,
            match registry_cleaner::fix(ctx, &usable) {
                Ok(n) if skipped > 0 => format!("{n} entries fixed ({skipped} need administrator rights)"),
                Ok(n) => format!("{n} entries fixed"),
                Err(e) => format!("{e:#}"),
            },
        ));
    }
    out
}

fn counted(n: usize, what: &str, errors: &[String]) -> String {
    if errors.is_empty() {
        format!("{n} {what}")
    } else {
        format!("{n} {what}; not removed: {}", errors.join("; "))
    }
}

fn summary(results: &[(Item, String)]) -> String {
    results
        .iter()
        .map(|(i, r)| format!("{}: {r}", i.label()))
        .collect::<Vec<_>>()
        .join(". ")
        + "."
}

/// Scheduled run (`--run oneclick-clean`): scans and cleans the saved selection.
/// It never shuts Windows down or compacts the registry.
pub fn run_saved(ctx: &AppCtx<'_>) -> Result<String> {
    let mut settings = Settings::load();
    if !settings.anything() {
        anyhow::bail!("nothing is selected in the 1-Click Cleaner");
    }
    let res = scan(&settings, ctx.elevated, &|_, _| {});
    let junk = settings
        .junk
        .then(|| wm_tools_cleaner::junk::clean(&res.junk, false));
    let text = summary(&finish(ctx, &settings, &res, Ok(junk)));
    settings.mark_done(&text);
    tool_settings::save(ID, &settings)?;
    Ok(text)
}

fn schedule_shutdown() -> Result<()> {
    let delay = SHUTDOWN_DELAY.to_string();
    run_checked(hidden_command("shutdown").args([
        "/s",
        "/t",
        &delay,
        "/c",
        "Win Manager RS finished cleaning. Windows will shut down in one minute.",
    ]))?;
    Ok(())
}

fn cancel_shutdown() -> Result<()> {
    run_checked(hidden_command("shutdown").arg("/a"))?;
    Ok(())
}

pub struct OneClickCleaner {
    settings: Settings,
    scan: Option<BackgroundTask<ScanResult>>,
    result: Option<ScanResult>,
    clean: Option<BackgroundTask<Option<CleanReport>>>,
    defrag: Option<BackgroundTask<registry_defrag::Scheduled>>,
    /// Each item's result from the last run in this window.
    results: Vec<(Item, String)>,
    privacy_options: bool,
    shutdown_pending: bool,
    confirm: Confirm<()>,
    message: Message,
}

impl Default for OneClickCleaner {
    fn default() -> Self {
        Self {
            settings: Settings::load(),
            scan: None,
            result: None,
            clean: None,
            defrag: None,
            results: Vec::new(),
            privacy_options: false,
            shutdown_pending: false,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl OneClickCleaner {
    fn save(&mut self) {
        if let Err(e) = tool_settings::save(ID, &self.settings) {
            self.message = Some(Err(format!("{e:#}")));
        }
    }

    fn busy(&self) -> bool {
        self.scan.is_some() || self.clean.is_some() || self.defrag.is_some()
    }

    fn start(&mut self, ui: &egui::Ui, ctx: &AppCtx<'_>) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        self.message = None;
        self.results.clear();
        let (settings, elevated) = (self.settings.clone(), ctx.elevated);
        self.scan = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
            Ok(scan(&settings, elevated, &|f, s| p.report(f, s)))
        }));
    }

    fn poll(&mut self, ui: &egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(t) = &mut self.scan {
            t.poll();
            if let Some(r) = t.take_result() {
                self.scan = None;
                match r {
                    Ok(r) => {
                        // Only the deletion runs in the background; the journal entries are
                        // written on the UI thread through the app's single journal instance.
                        let junk = if self.settings.junk {
                            r.junk.clone()
                        } else {
                            Vec::new()
                        };
                        self.result = Some(r);
                        self.clean = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                            Ok((!junk.is_empty()).then(|| wm_tools_cleaner::junk::clean(&junk, false)))
                        }));
                    }
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        if let Some(t) = &mut self.clean {
            t.poll();
            if let Some(r) = t.take_result() {
                self.clean = None;
                let res = self.result.take().unwrap_or_default();
                self.results = finish(ctx, &self.settings, &res, r);
                let text = summary(&self.results);
                self.settings.mark_done(&text);
                self.save();
                if self.settings.defrag && ctx.elevated {
                    self.results.push((Item::Defrag, "Compacting…".into()));
                    self.defrag = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                        registry_defrag::compact(&registry_defrag::hives())
                    }));
                } else if self.settings.shutdown {
                    match schedule_shutdown() {
                        Ok(()) => self.shutdown_pending = true,
                        Err(e) => self.message = Some(Err(format!("Shutting down: {e:#}"))),
                    }
                }
            }
        }
        if let Some(t) = &mut self.defrag {
            t.poll();
            if let Some(r) = t.take_result() {
                self.defrag = None;
                let line = match r {
                    Ok(s) => registry_defrag::finish_compact(ctx, &s, DEFRAG_DELAY).unwrap_or_else(|e| e),
                    Err(e) => e,
                };
                if let Some(last) = self.results.iter_mut().find(|(i, _)| *i == Item::Defrag) {
                    last.1 = line;
                }
            }
        }
    }

    /// The Result column: progress while running, then each item's outcome.
    fn result_of(&self, item: Item) -> String {
        if let Some((_, r)) = self.results.iter().find(|(i, _)| *i == item) {
            return r.clone();
        }
        if !self.settings.is_on(item) {
            return String::new();
        }
        if self.scan.is_some() {
            "Scanning…".into()
        } else if self.clean.is_some() {
            "Cleaning…".into()
        } else {
            String::new()
        }
    }

    fn privacy_window(&mut self, ui: &egui::Ui) {
        if !self.privacy_options {
            return;
        }
        let modal = egui::Modal::new(egui::Id::unique("oneclick-privacy")).show(ui.ctx(), |ui| {
            ui.set_min_width(320.0);
            ui.strong("Clear privacy history");
            ui.add_space(6.0);
            for (i, t) in Trace::ALL.iter().enumerate() {
                ui.checkbox(&mut self.settings.traces[i], t.label());
            }
            ui.add_space(8.0);
            ui.weak("Cleared traces (except the clipboard) can be restored from the Change Journal.");
            ui.add_space(8.0);
            if ui.button("OK").clicked() {
                self.privacy_options = false;
            }
        });
        if modal.should_close() {
            self.privacy_options = false;
        }
    }
}

impl Tool for OneClickCleaner {
    fn id(&self) -> &'static str {
        ID
    }

    fn category(&self) -> Category {
        Category::OneClick
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        self.poll(ui, ctx);
        if self.confirm.show(ui.ctx(), ctx.i18n).is_some() {
            self.start(ui, ctx);
        }
        let before = self.settings.clone();
        self.privacy_window(ui);

        let busy = self.busy();
        let n = self.settings.chosen(ctx.elevated).len();
        let mut start = false;
        frame::footer(
            ui,
            |ui| {
                ui.menu_button(format!("Task  {}", icons::CARET_DOWN), |ui| {
                    if ui.button("Schedule with My Task…").clicked() {
                        request_open_tool(ui.ctx(), "mytask");
                        ui.close();
                    }
                });
            },
            |ui| {
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                start = ui
                    .add_enabled(
                        !busy && self.settings.anything(),
                        egui::Button::new(format!("Start ({n})")),
                    )
                    .clicked();
                if busy {
                    ui.spinner();
                }
            },
        );
        if start {
            let question = if self.settings.defrag && ctx.elevated {
                "Run the selected cleanups now? The registry is then compacted and Windows restarts."
            } else if self.settings.shutdown {
                "Run the selected cleanups now? Windows shuts down when they are done."
            } else {
                "Run the selected cleanups now? Junk files are deleted permanently; the rest can be undone from the Change Journal."
            };
            self.confirm.ask(question, ());
        }

        let fill = ui.visuals().panel_fill;
        let mut cancel_shutdown_now = false;
        egui::Panel::bottom("oneclick-other")
            .resizable(false)
            .frame(egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut all = Item::ALL.iter().all(|i| self.settings.is_on(*i));
                    let partial = !all && Item::ALL.iter().any(|i| self.settings.is_on(*i));
                    if ui
                        .add(egui::Checkbox::new(&mut all, "Select All").indeterminate(partial))
                        .changed()
                    {
                        for i in Item::ALL {
                            *self.settings.switch(i) = all && (i != Item::Defrag || ctx.elevated);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(self.settings.last_clean_text());
                        ui.label("Last clean date:");
                    });
                });
                ui.separator();
                ui.label("Other Options");
                ui.indent("oneclick-other-options", |ui| {
                    ui.checkbox(&mut self.settings.shutdown, "Shut down after cleaning is completed")
                        .on_hover_text(format!(
                            "Windows shuts down {SHUTDOWN_DELAY} seconds after the clean, and you can still cancel. Scheduled cleans never shut down."
                        ));
                    if self.shutdown_pending {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                format!("Windows shuts down in {SHUTDOWN_DELAY} seconds."),
                            );
                            cancel_shutdown_now = ui.button("Cancel shutdown").clicked();
                        });
                    }
                });
            });
        if cancel_shutdown_now {
            self.shutdown_pending = false;
            self.message = Some(
                cancel_shutdown()
                    .map(|()| "Shutdown cancelled.".into())
                    .map_err(|e| format!("Cancelling the shutdown: {e:#}")),
            );
        }

        let results: Vec<String> = Item::ALL.iter().map(|i| self.result_of(*i)).collect();
        let elevated = ctx.elevated;
        let mut options = None;
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            let s = &mut self.settings;
            let mut switches = [
                s.privacy, s.startup, s.tasks, s.desktop, s.junk, s.registry, s.defrag,
            ];
            let mut rows: Vec<CheckItem<'_>> = switches
                .iter_mut()
                .zip(Item::ALL)
                .zip(&results)
                .map(|((on, item), result)| CheckItem {
                    on,
                    icon: item.icon(),
                    label: item.label().to_owned(),
                    options: item == Item::Privacy || item.options_tool().is_some(),
                    result: result.clone(),
                    enabled: !busy && (item != Item::Defrag || elevated),
                    disabled_hint: if item == Item::Defrag && !elevated {
                        "Needs the app to run as administrator.".into()
                    } else {
                        String::new()
                    },
                })
                .collect();
            options = checklist(ui, "Select the items to run", "Result", &mut rows);
            drop(rows);
            for (item, on) in Item::ALL.into_iter().zip(switches) {
                *s.switch(item) = on;
            }
        });
        match options.map(|i| Item::ALL[i]) {
            Some(Item::Privacy) => self.privacy_options = true,
            Some(item) => {
                if let Some(tool) = item.options_tool() {
                    request_open_tool(ui.ctx(), tool);
                }
            }
            None => {}
        }
        if self.settings != before {
            self.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_tolerate_old_files() {
        let mut s = Settings {
            privacy: true,
            traces: [false, true, false, true, false],
            ..Default::default()
        };
        s.mark_done("Registry Cleaner: 2 entries fixed.");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.selected_traces(), vec![Trace::TypedPaths, Trace::JumpLists]);
        assert!(!back.last_clean_text().is_empty());

        // A file from before 0.3 keeps cleaning its traces, and doesn't start removing
        // startup items or tasks on its own.
        let old: Settings =
            serde_json::from_str(r#"{"junk":false,"traces":[true,false,false,false,false]}"#).unwrap();
        assert!(!old.junk && old.registry && !old.shutdown);
        assert!(old.privacy && !old.startup && !old.tasks);
        assert_eq!(old.selected_traces(), vec![Trace::RunHistory]);
        assert_eq!(old.last_clean_text(), "");
    }

    #[test]
    fn nothing_selected_means_nothing_to_do() {
        let none = Settings {
            privacy: false,
            startup: false,
            tasks: false,
            desktop: false,
            junk: false,
            registry: false,
            defrag: true,
            ..Default::default()
        };
        assert!(!none.anything(), "defrag alone does nothing on a scheduled run");
        assert!(none.chosen(false).is_empty());
        assert_eq!(none.chosen(true), vec![Item::Defrag]);
        assert!(Settings::default().anything());
    }

    #[test]
    fn finish_with_nothing_selected_touches_nothing() {
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
        let s = Settings {
            privacy: false,
            startup: false,
            tasks: false,
            desktop: false,
            junk: false,
            registry: false,
            ..Default::default()
        };
        let results = finish(&ctx, &s, &ScanResult::default(), Ok(None));
        assert!(results.is_empty());
        assert!(journal.entries().is_empty());
    }
}
