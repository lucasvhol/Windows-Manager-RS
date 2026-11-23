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

fn main() -> eframe::Result {
    let _guard = init_logging();
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
