//! Mounted drive letters with type, label and space, for drive pickers and usage bars.

use std::path::PathBuf;

use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::core::PCWSTR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    RamDisk,
    Unknown,
}

impl DriveKind {
    pub fn label(self) -> &'static str {
        match self {
            DriveKind::Fixed => "Local disk",
            DriveKind::Removable => "Removable",
            DriveKind::Network => "Network",
            DriveKind::Optical => "Optical",
            DriveKind::RamDisk => "RAM disk",
            DriveKind::Unknown => "Drive",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DriveInfo {
    /// `C:\`
    pub root: PathBuf,
    pub letter: char,
    pub label: String,
    pub file_system: String,
    pub kind: DriveKind,
    pub total: u64,
    pub free: u64,
}

impl DriveInfo {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// `Local disk (C:)`, or the volume label when there is one.
    pub fn display_name(&self) -> String {
        let name = if self.label.is_empty() {
            self.kind.label()
        } else {
            &self.label
        };
        format!("{name} ({}:)", self.letter)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// The kind of drive holding `path` (`Unknown` for UNC paths or a missing drive).
pub fn kind_of(path: &std::path::Path) -> DriveKind {
    let s = path.to_string_lossy();
    let mut chars = s.chars();
    let (Some(letter), Some(':')) = (chars.next(), chars.next()) else {
        return DriveKind::Unknown;
    };
    let w = wide(&format!("{letter}:\\"));
    match unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) } {
        2 => DriveKind::Removable,
        3 => DriveKind::Fixed,
        4 => DriveKind::Network,
        5 => DriveKind::Optical,
        6 => DriveKind::RamDisk,
        _ => DriveKind::Unknown,
    }
}

/// Every drive letter that is present and ready (empty card readers and optical drives
/// without a disc are left out, since they report no size).
pub fn list() -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i as u8);
        let root = format!("{letter}:\\");
        let w = wide(&root);
        let kind = match unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) } {
            2 => DriveKind::Removable,
            3 => DriveKind::Fixed,
            4 => DriveKind::Network,
            5 => DriveKind::Optical,
            6 => DriveKind::RamDisk,
            _ => DriveKind::Unknown,
        };
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), Some(&mut total), None) }
            .is_err()
        {
            continue;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        let _ = unsafe {
            GetVolumeInformationW(
                PCWSTR(w.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                Some(&mut fs),
            )
        };
        out.push(DriveInfo {
            root: PathBuf::from(root),
            letter,
            label: from_wide(&label),
            file_system: from_wide(&fs),
            kind,
            total,
            free,
        });
    }
    out
}

/// Whether the disk behind a drive letter is solid state (no seek penalty), or `None`
/// when Windows cannot tell (for example some USB bridges and virtual disks). Opening
/// the volume for a property query needs no administrator rights.
pub fn is_ssd(letter: char) -> Option<bool> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

    let path = wide(&format!(r"\\.\{letter}:"));
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut desc = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(std::ptr::from_ref(&query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(std::ptr::from_mut(&mut desc).cast()),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            Some(&mut returned),
            None,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    ok.ok()?;
    (returned as usize >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()).then_some(!desc.IncursSeekPenalty)
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_drive_is_listed() {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let letter = sys.chars().next().unwrap().to_ascii_uppercase();
        let d = super::list()
            .into_iter()
            .find(|d| d.letter == letter)
            .expect("system drive");
        assert!(d.total > 0 && d.free <= d.total);
        assert!(d.display_name().ends_with(&format!("({letter}:)")));
    }
}
