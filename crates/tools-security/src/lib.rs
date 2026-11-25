//! Security tools (Phase 5): privacy, settings restrictions, file permissions,
//! password encryption and read-only NTFS undelete.

pub mod autologon;
pub mod crypto;
mod device_security;
mod file_security;
pub mod ntfs;
pub mod privacy;
pub mod privacy_traces;
mod security_lock;
mod settings_security;
mod system_protection;
mod undelete;

use wm_core::{Category, Tool, tweaks::TweakTool};

/// Privacy tweak definitions, also used by the Optimization Wizard.
pub const PRIVACY_TWEAKS: &str = include_str!("../tweaks/privacy.toml");
pub const LOCKDOWN_TWEAKS: &str = include_str!("../tweaks/lockdown.toml");
pub const SYSTEM_SECURITY_TWEAKS: &str = include_str!("../tweaks/systemsecurity.toml");
pub const OTHER_SECURITY_TWEAKS: &str = include_str!("../tweaks/othersecurity.toml");
pub const DEVICE_SECURITY_TWEAKS: &str = include_str!("../tweaks/devicesecurity.toml");

pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(privacy::PrivacyProtector::default()),
        Box::new(settings_security::SettingsSecurity::default()),
        Box::new(file_security::FileSecurity::default()),
        Box::new(security_lock::SecurityLock::default()),
        Box::new(undelete::FileUndelete::default()),
        Box::new(device_security::DeviceSecurity::default()),
        Box::new(system_protection::SystemProtection::default()),
        Box::new(
            TweakTool::new(Category::Security, "systemsecurity", "", SYSTEM_SECURITY_TWEAKS)
                .with_extra(Box::new(autologon::AutoSignInSetting::default())),
        ),
        Box::new(TweakTool::new(
            Category::Security,
            "othersecurity",
            "",
            OTHER_SECURITY_TWEAKS,
        )),
    ]
}

#[cfg(test)]
mod tweak_file_tests {
    #[test]
    fn security_tweak_files_parse() {
        for text in [
            super::SYSTEM_SECURITY_TWEAKS,
            super::OTHER_SECURITY_TWEAKS,
            super::LOCKDOWN_TWEAKS,
            super::PRIVACY_TWEAKS,
        ] {
            for t in wm_core::tweaks::parse(text).unwrap() {
                let _ = t.state();
            }
        }
    }
}
