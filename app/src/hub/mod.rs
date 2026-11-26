//! The hub: category rail and tool tiles.

pub mod catalog;
pub mod widgets;

use wm_core::registry;

/// Static facts about this PC shown in the rail footer.
pub struct Summary {
    /// e.g. "Windows 11 Pro 64-bit 26H2"
    pub os: String,
    /// e.g. "Build 26300.10075"
    pub build: String,
}

/// Windows 11 still reports "Windows 10 …" as ProductName; builds ≥ 22000 are Windows 11.
pub fn product_name(raw: &str, build: u32) -> String {
    if build >= 22_000 && raw.starts_with("Windows 10") {
        raw.replacen("Windows 10", "Windows 11", 1)
    } else {
        raw.to_owned()
    }
}

impl Summary {
    pub fn collect() -> Self {
        let key = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        let s = |name: &str| {
            registry::read_raw(key, name)
                .map(|v| registry::decode_string(&v))
                .unwrap_or_default()
        };
        let build_no: u32 = s("CurrentBuild").parse().unwrap_or(0);
        let ubr = registry::read_raw(key, "UBR").and_then(|v| registry::decode_dword(&v));
        let bits = if cfg!(target_pointer_width = "64") {
            "64-bit"
        } else {
            "32-bit"
        };
        Summary {
            os: format!(
                "{} {bits} {}",
                product_name(&s("ProductName"), build_no),
                s("DisplayVersion")
            )
            .trim()
            .to_owned(),
            build: match ubr {
                Some(u) => format!("Build {build_no}.{u}"),
                None => format!("Build {build_no}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_names_and_summary() {
        assert_eq!(product_name("Windows 10 Pro", 26_300), "Windows 11 Pro");
        assert_eq!(product_name("Windows 10 Home", 19_045), "Windows 10 Home");
        let s = Summary::collect();
        assert!(s.os.starts_with("Windows"), "{}", s.os);
        assert!(s.build.starts_with("Build "));
    }

    #[test]
    fn catalog_has_icons_and_no_duplicate_tiles() {
        let mut seen = Vec::new();
        for (_, ids) in catalog::TILES {
            for id in *ids {
                assert!(!seen.contains(id), "{id} has two tiles");
                seen.push(*id);
                assert_ne!(
                    catalog::tool_icon(id),
                    wm_core::icons::APP_WINDOW,
                    "{id} needs an icon"
                );
            }
        }
    }

    #[test]
    fn every_tool_is_reachable_from_the_hub() {
        let mut reachable: Vec<&str> = vec!["journal", "settings", catalog::MISC_LINK];
        for (_, ids) in catalog::TILES {
            for id in *ids {
                reachable.push(id);
                reachable.extend(catalog::related(id));
            }
        }
        for t in crate::tools::all() {
            assert!(
                reachable.contains(&t.id()),
                "{} has no tile, dropdown entry or menu entry",
                t.id()
            );
        }
    }
}
