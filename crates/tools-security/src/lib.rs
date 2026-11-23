//! Security tools (Phase 5): privacy, settings restrictions, file permissions,
//! password encryption and read-only NTFS undelete.

pub mod crypto;
mod file_security;
pub mod ntfs;
mod privacy;
mod security_lock;
mod settings_security;
mod undelete;

use wm_core::Tool;

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(privacy::PrivacyProtector::default()),
        Box::new(settings_security::SettingsSecurity::default()),
        Box::new(file_security::FileSecurity::default()),
        Box::new(security_lock::SecurityLock::default()),
        Box::new(undelete::FileUndelete::default()),
    ]
}
