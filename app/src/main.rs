// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hub;
mod shell;
mod tools;
mod updater;

use wm_core::{APP_NAME, egui, paths};

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = paths::logs_dir().ok()?;
    let appender = tracing_appender::rolling::daily(dir, "win-manager.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    use tracing_subscriber::{filter::Targets, prelude::*};
    // Our own crates at INFO; noisy GPU/windowing dependencies only at WARN.
    let filter = Targets::new()
        .with_default(tracing::Level::WARN)
        .with_target("win_manager", tracing::Level::INFO)
        .with_target("wm_core", tracing::Level::INFO);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false),
        )
        .with(filter)
        .init();
    Some(guard)
}

/// `win-manager.exe --run <action>`: performs a maintenance action without a window
/// (used by scheduled tasks created in My Task). Returns the process exit code.
fn run_headless(action: &str) -> i32 {
    use wm_core::{AppCtx, config::Config, elevation, i18n::I18n, safety::ChangeJournal};
    let mut config = Config::default_path()
        .map(|p| Config::load_or_default(&p))
        .unwrap_or_default();
    let journal = match ChangeJournal::open_default() {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("cannot open journal: {e:#}");
            return 2;
        }
    };
    let i18n = I18n::new(&config.language);
    let ctx = AppCtx {
        config: &mut config,
        journal: &journal,
        i18n: &i18n,
        elevated: elevation::is_elevated(),
    };
    match wm_tools_oneclick::actions::run(action, &ctx) {
        Ok(msg) => {
            tracing::info!("--run {action}: {msg}");
            0
        }
        Err(e) => {
            tracing::error!("--run {action} failed: {e:#}");
            1
        }
    }
}

fn main() -> eframe::Result {
    let _guard = init_logging();
    let args: Vec<String> = std::env::args().collect();
    // My Task's rule monitor: runs headless (with a notification-area icon) until stopped.
    if args.iter().any(|a| a == "--monitor") {
        let code = wm_tools_oneclick::monitor::run();
        drop(_guard);
        std::process::exit(code);
    }
    // Called by the uninstaller: remove what this app registered with Windows.
    if args.iter().any(|a| a == "--uninstall-cleanup") {
        for line in wm_tools_oneclick::monitor::uninstall_cleanup() {
            tracing::info!("uninstall cleanup: {line}");
        }
        drop(_guard);
        std::process::exit(0);
    }
    // Desktop shortcuts made by Registry Tools: open Registry Editor at a key and exit.
    if let Some(i) = args.iter().position(|a| a == "--regjump") {
        let key = args.get(i + 1).map(String::as_str).unwrap_or_default();
        let code = match wm_core::registry::open_in_regedit(key) {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("--regjump {key} failed: {e:#}");
                1
            }
        };
        drop(_guard);
        std::process::exit(code);
    }
    if let Some(i) = args.iter().position(|a| a == "--run") {
        let action = args.get(i + 1).map(String::as_str).unwrap_or_default();
        let code = run_headless(action);
        drop(_guard); // flush the log before exiting
        std::process::exit(code);
    }
    tracing::info!("starting {APP_NAME} {}", env!("CARGO_PKG_VERSION"));

    // Like the original, the window runs as administrator: ask UAC once at launch.
    // Cancelling the prompt keeps this unelevated instance (tools that need admin say
    // so); the relaunched instance never asks again.
    if !wm_core::elevation::is_elevated() && !args.iter().any(|a| a == wm_core::elevation::RELAUNCHED) {
        let wants = wm_core::config::Config::default_path()
            .map(|p| wm_core::config::Config::load_or_default(&p).run_as_admin)
            .unwrap_or(true);
        if wants {
            match wm_core::elevation::relaunch_as_admin() {
                Ok(()) => {
                    drop(_guard);
                    std::process::exit(0);
                }
                Err(e) => tracing::warn!("staying unelevated: {e:#}"),
            }
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 480.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/app.png")).unwrap_or_default(),
            ),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(shell::Shell::new(cc)))),
    )
}
