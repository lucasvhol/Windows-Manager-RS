//! Favorites checker: reads bookmarks from Edge, Chrome and Brave (every profile) and
//! the Windows Favorites folder (.url files), then checks which links still work.
//! Only http(s) links are requested; nothing is sent anywhere else.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use wm_core::paths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A browser's Bookmarks file.
    Browser(PathBuf),
    /// An Internet shortcut in the Favorites folder.
    UrlFile(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub browser: String,
    pub folder: String,
    pub title: String,
    pub url: String,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok,
    Moved(String),
    /// HTTP error such as 404 or 410.
    Broken(u16),
    /// No answer: unknown host, refused, timed out.
    Unreachable(String),
    /// Not a web link (javascript:, file:, edge://…).
    Skipped,
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Ok => "Works".into(),
            Status::Moved(to) => format!("Moved to {to}"),
            Status::Broken(code) => format!("Broken (HTTP {code})"),
            Status::Unreachable(e) => format!("Unreachable: {e}"),
            Status::Skipped => "Not a web link".into(),
        }
    }

    pub fn is_problem(&self) -> bool {
        matches!(self, Status::Broken(_) | Status::Unreachable(_))
    }
}

/// Chromium-based browsers: (name, user data folder).
const BROWSERS: [(&str, &str); 3] = [
    ("Edge", r"%LOCALAPPDATA%\Microsoft\Edge\User Data"),
    ("Chrome", r"%LOCALAPPDATA%\Google\Chrome\User Data"),
    ("Brave", r"%LOCALAPPDATA%\BraveSoftware\Brave-Browser\User Data"),
];

fn walk_json(node: &serde_json::Value, folder: &str, browser: &str, file: &Path, out: &mut Vec<Bookmark>) {
    let name = node.get("name").and_then(|n| n.as_str()).unwrap_or_default();
    match node.get("type").and_then(|t| t.as_str()) {
        Some("url") => {
            if let Some(url) = node.get("url").and_then(|u| u.as_str()) {
                out.push(Bookmark {
                    browser: browser.to_owned(),
                    folder: folder.to_owned(),
                    title: name.to_owned(),
                    url: url.to_owned(),
                    source: Source::Browser(file.to_owned()),
                });
            }
        }
        Some("folder") => {
            let sub = if folder.is_empty() {
                name.to_owned()
            } else {
                format!("{folder} / {name}")
            };
            for c in node
                .get("children")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                walk_json(c, &sub, browser, file, out);
            }
        }
        _ => {}
    }
}

/// Bookmarks in one Chromium `Bookmarks` file.
pub fn parse_chromium(text: &str, browser: &str, file: &Path) -> Vec<Bookmark> {
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(roots) = v.get("roots").and_then(|r| r.as_object())
    {
        for root in roots.values() {
            walk_json(root, "", browser, file, &mut out);
        }
    }
    out
}

/// The URL in an Internet shortcut (.url, INI format).
pub fn url_file_target(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("URL="))
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
}

pub fn all_bookmarks() -> Vec<Bookmark> {
    let mut out = Vec::new();
    for (browser, dir) in BROWSERS {
        let root = PathBuf::from(paths::expand_env(dir));
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut profiles: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("Bookmarks").is_file())
            .collect();
        profiles.sort();
        let many = profiles.len() > 1;
        for p in profiles {
            let file = p.join("Bookmarks");
            let label = if many {
                format!(
                    "{browser} ({})",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )
            } else {
                browser.to_owned()
            };
            if let Ok(text) = std::fs::read_to_string(&file) {
                out.extend(parse_chromium(&text, &label, &file));
            }
        }
    }
    let favorites = PathBuf::from(paths::expand_env(r"%USERPROFILE%\Favorites"));
    for e in walkdir::WalkDir::new(&favorites).into_iter().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("url"))
            && let Some(url) = std::fs::read(p)
                .ok()
                .and_then(|b| url_file_target(&String::from_utf8_lossy(&b)))
        {
            let folder = p
                .parent()
                .and_then(|d| d.strip_prefix(&favorites).ok())
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            out.push(Bookmark {
                browser: "Favorites folder".into(),
                folder,
                title: p.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
                url,
                source: Source::UrlFile(p.to_path_buf()),
            });
        }
    }
    out
}

/// Same scheme, host and path (ignoring a trailing slash and http→https upgrades).
fn same_place(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

pub fn check(url: &str) -> Status {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Status::Skipped;
    }
    match wm_core::net::check_link(url) {
        Ok(c) if (200..400).contains(&c.status) => {
            if same_place(url, &c.final_url) {
                Status::Ok
            } else {
                Status::Moved(c.final_url)
            }
        }
        // Sites that refuse scripts (401/403/429) are alive; only clear failures count.
        Ok(c) if matches!(c.status, 401 | 403 | 429) => Status::Ok,
        Ok(c) if c.status >= 400 => Status::Broken(c.status),
        Ok(_) => Status::Ok,
        Err(e) => {
            let text = format!("{e:#}");
            let reason = if text.contains("resolve") || text.contains("dns") || text.contains("No such host")
            {
                "the site's name no longer exists"
            } else if text.contains("timed out") || text.contains("Timeout") {
                "timed out"
            } else if text.contains("refused") {
                "connection refused"
            } else {
                "no answer"
            };
            Status::Unreachable(reason.into())
        }
    }
}

/// Checks every bookmark with a few at a time; `progress` gets (done, total).
pub fn check_all(
    marks: &[Bookmark],
    progress: &(dyn Fn(usize, usize) + Sync),
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Vec<Status> {
    let results: Mutex<Vec<Option<Status>>> = Mutex::new(vec![None; marks.len()]);
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..16.min(marks.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= marks.len() || cancelled() {
                        break;
                    }
                    let st = check(&marks[i].url);
                    results.lock().unwrap()[i] = Some(st);
                    progress(done.fetch_add(1, Ordering::Relaxed) + 1, marks.len());
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|s| s.unwrap_or(Status::Skipped))
        .collect()
}

/// CSV of the results, for keeping or sharing.
pub fn to_csv(marks: &[Bookmark], status: &[Status]) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut out = String::from("Browser,Folder,Title,URL,Status\r\n");
    for (m, s) in marks.iter().zip(status) {
        out += &format!(
            "{},{},{},{},{}\r\n",
            q(&m.browser),
            q(&m.folder),
            q(&m.title),
            q(&m.url),
            q(&s.label())
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_bookmarks_parse() {
        let json = r#"{"roots":{"bookmark_bar":{"type":"folder","name":"Bookmarks bar","children":[
            {"type":"url","name":"Example","url":"https://example.com/"},
            {"type":"folder","name":"Work","children":[{"type":"url","name":"Docs","url":"https://docs.example.com"}]}]},
            "other":{"type":"folder","name":"Other","children":[]}}}"#;
        let b = parse_chromium(json, "Edge", Path::new("x"));
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].folder, "Bookmarks bar / Work");
        assert!(parse_chromium("not json", "Edge", Path::new("x")).is_empty());
    }

    #[test]
    fn url_files_and_csv() {
        assert_eq!(
            url_file_target("[InternetShortcut]\r\nURL=https://example.com/a\r\n").as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(url_file_target("[InternetShortcut]\r\n"), None);
        let m = Bookmark {
            browser: "Edge".into(),
            folder: String::new(),
            title: "Say \"hi\"".into(),
            url: "https://x".into(),
            source: Source::Browser("b".into()),
        };
        assert!(to_csv(&[m], &[Status::Broken(404)]).contains("\"Say \"\"hi\"\"\""));
        assert!(same_place("http://www.example.com/", "https://example.com"));
        assert!(!same_place("https://example.com/a", "https://example.com/b"));
    }

    #[test]
    fn checks_skip_non_web_links_and_report_dead_hosts() {
        assert_eq!(check("javascript:alert(1)"), Status::Skipped);
        assert_eq!(check("edge://settings"), Status::Skipped);
        // A reserved name that never resolves (RFC 6761).
        assert!(check("https://no-such-host.invalid/").is_problem());
        let _ = all_bookmarks();
    }
}
