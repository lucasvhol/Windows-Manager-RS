//! Information tools (Phase 1): read-mostly views of the system.

mod device_manager;
mod disk_bench;
mod event_viewer;
mod gpu_counters;
mod hardware_monitor;
mod process_manager;
mod system_info;

use wm_core::Tool;

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(system_info::SystemInfo::default()),
        Box::new(process_manager::ProcessManager::default()),
        Box::new(hardware_monitor::HardwareMonitor::default()),
        Box::new(event_viewer::EventViewer::default()),
        Box::new(device_manager::DeviceManager::default()),
    ]
}
