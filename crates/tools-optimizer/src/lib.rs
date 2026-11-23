//! Optimizer tools (Phase 2): startup items, services, scheduled tasks, boot
//! configuration and system speed tweaks. Every change is journaled for undo.

mod boot;
mod services;
mod speed;
mod startup;
mod tasks;

use wm_core::Tool;

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(startup::StartupManager::default()),
        Box::new(services::ServiceManager::default()),
        Box::new(tasks::TaskSchedulerManager::default()),
        Box::new(boot::BootConfiguration::default()),
        Box::new(speed::SystemSpeed::default()),
    ]
}
