//! Misc. utilities (Phase 7): file splitter, verified copy, registry tools and a
//! launcher for Windows' built-in administrative tools.

mod compat_manager;
mod download_windows;
pub mod file_splitter;
mod move_folders;
mod registry_extra;
mod registry_tools;
pub mod super_copy;
mod windows_utilities;

use wm_core::Tool;

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(file_splitter::FileSplitter::default()),
        Box::new(super_copy::SuperCopy::default()),
        Box::new(registry_tools::RegistryTools::default()),
        Box::new(windows_utilities::WindowsUtilities::default()),
        Box::new(download_windows::DownloadWindows::default()),
        Box::new(move_folders::MoveFolders::default()),
        Box::new(compat_manager::CompatManager::default()),
    ]
}
