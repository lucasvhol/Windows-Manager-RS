//! The keyboard Scancode Map (HKLM, read at sign-in… in practice at restart): remaps or
//! turns off physical keys for every account. Several tools change different keys in
//! it, so each one replaces only its own entries and keeps the rest.

use anyhow::Result;

use crate::registry::{self, ValueSnapshot};

pub const KEYBOARD_LAYOUT: &str = r"HKLM\SYSTEM\CurrentControlSet\Control\Keyboard Layout";
pub const SCANCODE_MAP: &str = "Scancode Map";

/// Left and right Windows keys (extended scancodes).
pub const LEFT_WIN: u16 = 0xE05B;
pub const RIGHT_WIN: u16 = 0xE05C;

/// Entries of a Scancode Map: (key it becomes, key pressed). 0 as "becomes" turns the
/// key off.
pub fn parse_map(bytes: &[u8]) -> Vec<(u16, u16)> {
    if bytes.len() < 16 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    bytes[12..]
        .as_chunks::<4>()
        .0
        .iter()
        .take(count.saturating_sub(1))
        .map(|c| (u16::from_le_bytes([c[0], c[1]]), u16::from_le_bytes([c[2], c[3]])))
        .filter(|&(_, from)| from != 0)
        .collect()
}

pub fn build_map(entries: &[(u16, u16)]) -> Vec<u8> {
    let mut out = vec![0u8; 8];
    out.extend(((entries.len() + 1) as u32).to_le_bytes());
    for (to, from) in entries {
        out.extend(to.to_le_bytes());
        out.extend(from.to_le_bytes());
    }
    out.extend([0u8; 4]);
    out
}

/// The current entries (empty when there is no map).
pub fn current_map() -> Vec<(u16, u16)> {
    registry::read_raw(KEYBOARD_LAYOUT, SCANCODE_MAP)
        .map(|v| parse_map(&v.bytes))
        .unwrap_or_default()
}

/// `entries` with every remap of a key in `keys` replaced by `new`.
pub fn merge(entries: &[(u16, u16)], keys: &[u16], new: &[(u16, u16)]) -> Vec<(u16, u16)> {
    entries
        .iter()
        .copied()
        .filter(|(_, from)| !keys.contains(from))
        .chain(new.iter().copied())
        .collect()
}

/// Replaces the entries for `keys` with `new` (which may be empty), keeping every other
/// remap; deletes the map when nothing is left. Returns the value as it was before, for
/// the journal. Needs administrator rights.
pub fn replace_keys(keys: &[u16], new: &[(u16, u16)]) -> Result<ValueSnapshot> {
    let snap = registry::snapshot(KEYBOARD_LAYOUT, SCANCODE_MAP);
    let entries = merge(&current_map(), keys, new);
    if entries.is_empty() {
        registry::delete_value(KEYBOARD_LAYOUT, SCANCODE_MAP)?;
    } else {
        registry::write_raw(
            KEYBOARD_LAYOUT,
            SCANCODE_MAP,
            &registry::binary(&build_map(&entries)),
        )?;
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_round_trip_and_merge() {
        let bytes = build_map(&[(0x1D, 0xE038), (0x01, 0x3A)]);
        assert_eq!(parse_map(&bytes), vec![(0x1D, 0xE038), (0x01, 0x3A)]);
        assert!(parse_map(&[0; 4]).is_empty());
        let caps_only = vec![(0x01, 0x3A)];
        let with_win = merge(
            &caps_only,
            &[LEFT_WIN, RIGHT_WIN],
            &[(0, LEFT_WIN), (0, RIGHT_WIN)],
        );
        assert_eq!(with_win, vec![(0x01, 0x3A), (0, LEFT_WIN), (0, RIGHT_WIN)]);
        assert_eq!(merge(&with_win, &[LEFT_WIN, RIGHT_WIN], &[]), caps_only);
        assert_eq!(
            merge(&with_win, &[0x3A], &[]),
            vec![(0, LEFT_WIN), (0, RIGHT_WIN)]
        );
    }
}
