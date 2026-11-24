// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod shell;
mod tools;

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
    if let Some(i) = args.iter().position(|a| a == "--run") {
        let action = args.get(i + 1).map(String::as_str).unwrap_or_default();
        let code = run_headless(action);
        drop(_guard); // flush the log before exiting
        std::process::exit(code);
    }
    tracing::info!("starting {APP_NAME} {}", env!("CARGO_PKG_VERSION"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(shell::Shell::new(cc)))),
    )
}
