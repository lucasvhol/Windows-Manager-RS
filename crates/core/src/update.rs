//! Update check against the project's GitHub Releases. Runs only when the user presses
//! Update. The repository is baked in at build time (`WM_RELEASE_REPO`, set by the
//! release workflow); local builds have none and say so. A release carries the installer
//! (`*-setup.exe`) and `SHA256SUMS.txt`; the download is checked against it before it runs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// `owner/name` of the GitHub repository releases come from, if this build has one.
pub fn repo() -> Option<&'static str> {
    option_env!("WM_RELEASE_REPO").filter(|r| valid_repo(r))
}

fn valid_repo(r: &str) -> bool {
    let mut parts = r.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `v1.2.3` / `1.2.3` / `1.2` → (1, 2, 3) / (1, 2, 0). Pre-release suffixes are ignored.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut n = core.split('.').map(|p| p.parse::<u64>());
    let major = n.next()?.ok()?;
    let minor = n.next().unwrap_or(Ok(0)).ok()?;
    let patch = n.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub notes: String,
    /// (file name, download URL) of the installer.
    pub installer: Option<(String, String)>,
    pub checksums: Option<String>,
}

/// Reads a GitHub "latest release" API answer.
pub fn parse_release(json: &str) -> Result<Release> {
    let v: serde_json::Value = serde_json::from_str(json).context("reading the release information")?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_owned();
    let assets: Vec<(String, String)> = v
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x.get("name")?.as_str()?.to_owned(),
                        x.get("browser_download_url")?.as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let version = s("tag_name");
    if parse_version(&version).is_none() {
        bail!("the latest release has no version number ({version:?})");
    }
    Ok(Release {
        version,
        page: s("html_url"),
        notes: s("body"),
        installer: assets
            .iter()
            .find(|(n, _)| n.to_ascii_lowercase().ends_with("-setup.exe"))
            .cloned(),
        checksums: assets
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("SHA256SUMS.txt"))
            .map(|(_, u)| u.clone()),
    })
}

/// The latest release, or None when this build has no release source.
pub fn latest() -> Result<Option<Release>> {
    let Some(repo) = repo() else {
        return Ok(None);
    };
    let json = crate::net::get_text(&format!("https://api.github.com/repos/{repo}/releases/latest"))?;
    parse_release(&json).map(Some)
}

/// The SHA-256 listed for `file` in a `sha256sum`-style list.
pub fn checksum_for(list: &str, file: &str) -> Option<String> {
    list.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name.eq_ignore_ascii_case(file) && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Downloads the installer into `dir` and checks it against the release's checksums.
pub fn download(release: &Release, dir: &Path) -> Result<PathBuf> {
    let Some((name, url)) = &release.installer else {
        bail!("this release has no installer to download");
    };
    let Some(sums) = &release.checksums else {
        bail!("this release has no checksum file, so the download can't be verified");
    };
    if name.contains(['\\', '/', ':']) {
        bail!("unexpected installer name {name:?}");
    }
    let list = crate::net::get_text(sums)?;
    let Some(want) = checksum_for(&list, name) else {
        bail!("{name} isn't in the release's checksum file");
    };
    let bytes = crate::net::get_bytes(url, 512 << 20)?;
    let got = sha256_hex(&bytes);
    if got != want {
        bail!("the download doesn't match its checksum (expected {want}, got {got}); it was not run");
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, &bytes).with_context(|| format!("saving {}", path.display()))?;
    Ok(path)
}

/// Starts the installer (it closes this app, updates it and starts it again).
pub fn run_installer(path: &Path) -> Result<()> {
    std::process::Command::new(path)
        .args(["/SILENT", "/CLOSEAPPLICATIONS", "/RESTARTAPPLICATIONS"])
        .spawn()
        .with_context(|| format!("starting {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.0.0-beta.1"), Some((1, 0, 0)));
        assert_eq!(parse_version("latest"), None);
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.10.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn repos() {
        assert!(valid_repo("someone/win-manager-rs"));
        assert!(!valid_repo("someone"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("a/b?x=1"));
    }

    #[test]
    fn release_json_and_checksums() {
        let json = r#"{"tag_name":"v0.2.0","html_url":"https://github.com/o/r/releases/tag/v0.2.0","body":"Fixes",
            "assets":[{"name":"WinManagerRS-0.2.0-setup.exe","browser_download_url":"https://x/setup.exe"},
                      {"name":"SHA256SUMS.txt","browser_download_url":"https://x/sums"}]}"#;
        let r = parse_release(json).unwrap();
        assert_eq!(r.version, "v0.2.0");
        assert_eq!(r.installer.as_ref().unwrap().0, "WinManagerRS-0.2.0-setup.exe");
        assert_eq!(r.checksums.as_deref(), Some("https://x/sums"));
        assert!(parse_release(r#"{"tag_name":"nightly"}"#).is_err());

        let h = sha256_hex(b"abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let list = format!("{h}  WinManagerRS-0.2.0-setup.exe\n{h} *other.zip\n");
        assert_eq!(
            checksum_for(&list, "WinManagerRS-0.2.0-setup.exe"),
            Some(h.clone())
        );
        assert_eq!(checksum_for(&list, "other.zip"), Some(h));
        assert_eq!(checksum_for(&list, "missing.exe"), None);
    }
}
