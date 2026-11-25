//! 1-Click tools and automation (Phase 7): 1-Click Cleaner and Fixer, Repair Center,
//! Optimization Wizard and My Task (scheduled maintenance via `--run <action>`).

pub mod actions;

mod fixer;
pub mod monitor;
mod my_task;
mod my_task_rules;
mod oneclick_cleaner;
mod repair_center;
mod wizard;

use wm_core::Tool;

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(oneclick_cleaner::OneClickCleaner::default()),
        Box::new(fixer::OneClickFixer::default()),
        Box::new(wizard::Wizard::default()),
        Box::new(repair_center::RepairCenter::default()),
        Box::new(my_task::MyTask::default()),
    ]
}
