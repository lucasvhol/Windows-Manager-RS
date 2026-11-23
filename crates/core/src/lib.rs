//! Shared foundation for every tool in the suite: the [`Tool`] trait, configuration,
//! localisation, elevation helpers, background tasks and the safety layer
//! (change journal + restore points) that all system-modifying code must go through.

pub mod config;
pub mod elevation;
pub mod i18n;
pub mod paths;
pub mod process;
pub mod registry;
pub mod safety;
pub mod task;
pub mod tool;
pub mod tweaks;
pub mod ui;

pub use eframe::egui;
pub use tool::{AppCtx, Category, Tool};

/// Display name of the application. Change here to rebrand.
pub const APP_NAME: &str = "Win Manager RS";
