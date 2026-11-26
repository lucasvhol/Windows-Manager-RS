//! Shared foundation for every tool in the suite: the [`Tool`] trait, configuration,
//! localisation, elevation helpers, background tasks and the safety layer
//! (change journal + restore points) that all system-modifying code must go through.

pub mod acl;
pub mod command;
pub mod config;
pub mod drives;
pub mod elevation;
pub mod explorer_verb;
pub mod file_icon;
pub mod fileinfo;
pub mod i18n;
pub mod icons;
pub mod keyboard;
pub mod machine;
pub mod net;
pub mod password;
pub mod paths;
pub mod process;
pub mod registry;
pub mod rules;
pub mod safety;
pub mod scheduler;
pub mod shell;
pub mod task;
pub mod tool;
pub mod tool_settings;
pub mod tweaks;
pub mod ui;
pub mod update;
pub mod user_assist;

pub use eframe::egui;
pub use tool::{AppCtx, Category, Layout, Tool};

/// Display name of the application. Change here to rebrand.
pub const APP_NAME: &str = "Win Manager RS";
