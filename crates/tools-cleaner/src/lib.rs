//! Cleaner tools (Phase 3): disk usage, junk, duplicates, desktop, uninstallers,
//! component store and a conservative registry cleaner.

mod apps;
mod component_store;
mod desktop;
mod disk_analyzer;
mod duplicates;
pub mod junk;
pub mod registry_cleaner;
mod uninstaller;

use wm_core::Tool;

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(disk_analyzer::DiskAnalyzer::default()),
        Box::new(junk::JunkCleaner::default()),
        Box::new(duplicates::DuplicateFinder::default()),
        Box::new(desktop::DesktopCleaner::default()),
        Box::new(uninstaller::SmartUninstaller::default()),
        Box::new(apps::AppsUninstaller::default()),
        Box::new(component_store::ComponentStoreCleaner::default()),
        Box::new(registry_cleaner::RegistryCleaner::default()),
    ]
}
