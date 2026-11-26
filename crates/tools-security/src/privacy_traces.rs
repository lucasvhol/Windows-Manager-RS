//! The Privacy Protector trace catalog: Windows, network, program and browser history
//! that can be cleared, each item made of registry values, keys or files.
//!
//! Registry values and keys are backed up and files go to the Recycle Bin, so clearing
//! can be undone from the Change Journal. Browser caches and commands (DNS, USB
//! history, clipboard) cannot be undone and say so.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use wm_core::{
    AppCtx, paths,
    process::{hidden_command, run_checked},
    registry,
    safety::UndoAction,
};

const TOOL: &str = "privacy";
const EXPLORER: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer";
const APPLETS: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Applets";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    Windows,
    Network,
    Programs,
    Browsers,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Windows,
        Section::Network,
        Section::Programs,
        Section::Browsers,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Windows => "Windows history",
            Section::Network => "Network history",
            Section::Programs => "Program history",
            Section::Browsers => "Browsers",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Every named value of a key (only names starting with `prefix`, if given).
    Values {
        key: String,
        prefix: Option<&'static str>,
    },
    /// One value.
    Value {
        key: String,
        name: &'static str,
    },
    /// A whole key with its subkeys.
    Key(String),
    /// The files directly inside a folder (only with extension `ext`, if given).
    FolderFiles {
        dir: PathBuf,
        ext: Option<&'static str>,
    },
    /// Particular files.
    Files(Vec<PathBuf>),
    /// A cache folder whose contents are deleted for good (files in use are skipped).
    Cache(PathBuf),
    /// A command; it has no count.
    Command {
        program: &'static str,
        args: Vec<String>,
    },
    Clipboard,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub section: Section,
    pub label: String,
    /// Ticked until the user changes the selection.
    pub default_on: bool,
    pub admin: bool,
    /// What else clearing it does, shown beside it.
    pub note: &'static str,
    /// A program that must be closed first (image name, such as msedge.exe).
    pub close: Option<&'static str>,
    pub targets: Vec<Target>,
}

impl Item {
    fn new(id: impl Into<String>, section: Section, label: impl Into<String>, targets: Vec<Target>) -> Self {
        Self {
            id: id.into(),
            section,
            label: label.into(),
            default_on: true,
            admin: false,
            note: "",
            close: None,
            targets,
        }
    }

    fn off(mut self) -> Self {
        self.default_on = false;
        self
    }

    fn admin(mut self) -> Self {
        self.admin = true;
        self
    }

    fn note(mut self, note: &'static str) -> Self {
        self.note = note;
        self
    }

    /// Whether everything it clears can be put back from the Change Journal.
    pub fn undoable(&self) -> bool {
        !self
            .targets
            .iter()
            .any(|t| matches!(t, Target::Cache(_) | Target::Command { .. } | Target::Clipboard))
    }

    /// Whether it has a meaningful count (commands and the clipboard don't).
    pub fn countable(&self) -> bool {
        self.targets
            .iter()
            .any(|t| !matches!(t, Target::Command { .. } | Target::Clipboard))
    }
}

fn values(key: impl Into<String>) -> Target {
    Target::Values {
        key: key.into(),
        prefix: None,
    }
}

fn subkeys(key: &str) -> Vec<String> {
    registry::open_read(key)
        .map(|k| k.enum_keys().flatten().map(|n| format!(r"{key}\{n}")).collect())
        .unwrap_or_default()
}

fn key_exists(key: &str) -> bool {
    registry::open_read(key).is_ok()
}

fn env_path(p: &str) -> PathBuf {
    PathBuf::from(paths::expand_env(p))
}

/// The command Device Security uses to forget USB drives that aren't connected.
const USB_HISTORY: &str = "Get-PnpDevice | Where-Object { -not $_.Present -and ($_.InstanceId -like 'USBSTOR*' -or $_.InstanceId -like 'SWD\\WPDBUSENUM*') } | ForEach-Object { pnputil /remove-device $_.InstanceId | Out-Null }";

fn windows_items() -> Vec<Item> {
    let recent = env_path(r"%APPDATA%\Microsoft\Windows\Recent");
    let user_assist: Vec<Target> = subkeys(&format!(r"{EXPLORER}\UserAssist"))
        .into_iter()
        .map(|k| values(format!(r"{k}\Count")))
        .collect();
    let feature_usage: Vec<Target> = subkeys(&format!(r"{EXPLORER}\FeatureUsage"))
        .into_iter()
        .map(values)
        .collect();
    let bags: Vec<Target> = [
        r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\Shell",
        r"HKCU\Software\Microsoft\Windows\Shell",
    ]
    .iter()
    .flat_map(|root| ["BagMRU", "Bags"].map(|b| Target::Key(format!(r"{root}\{b}"))))
    .collect();
    let tray = r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\TrayNotify";
    use Section::Windows as W;
    vec![
        Item::new(
            "run",
            W,
            "Run dialog history",
            vec![values(format!(r"{EXPLORER}\RunMRU"))],
        ),
        Item::new(
            "typed_paths",
            W,
            "File Explorer address bar history",
            vec![values(format!(r"{EXPLORER}\TypedPaths"))],
        ),
        Item::new(
            "search_box",
            W,
            "File Explorer search box history",
            vec![values(format!(r"{EXPLORER}\WordWheelQuery"))],
        ),
        Item::new(
            "recent_docs",
            W,
            "Recently used files",
            vec![
                Target::FolderFiles {
                    dir: recent.clone(),
                    ext: None,
                },
                Target::Key(format!(r"{EXPLORER}\RecentDocs")),
            ],
        ),
        Item::new(
            "jump_lists",
            W,
            "Jump List history (recent and pinned items)",
            vec![
                Target::FolderFiles {
                    dir: recent.join("AutomaticDestinations"),
                    ext: None,
                },
                Target::FolderFiles {
                    dir: recent.join("CustomDestinations"),
                    ext: None,
                },
            ],
        ),
        Item::new(
            "common_dialog",
            W,
            "Open and Save dialog history",
            subkeys(&format!(r"{EXPLORER}\ComDlg32"))
                .into_iter()
                .map(Target::Key)
                .collect(),
        ),
        Item::new(
            "regedit",
            W,
            "Last key opened in Registry Editor",
            vec![Target::Value {
                key: format!(r"{APPLETS}\Regedit"),
                name: "LastKey",
            }],
        ),
        Item::new(
            "mmc",
            W,
            "Management console (MMC) recent files",
            vec![values(
                r"HKCU\Software\Microsoft\Microsoft Management Console\Recent File List",
            )],
        ),
        Item::new(
            "wallpapers",
            W,
            "Desktop background history",
            vec![Target::Values {
                key: format!(r"{EXPLORER}\Wallpapers"),
                prefix: Some("BackgroundHistoryPath"),
            }],
        ),
        Item::new(
            "compat_store",
            W,
            "Program compatibility history",
            vec![values(
                r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Compatibility Assistant\Store",
            )],
        ),
        Item::new("executed", W, "Programs you have run (UserAssist)", user_assist)
            .off()
            .note("Also resets the most used apps in Start"),
        Item::new(
            "feature_usage",
            W,
            "Taskbar feature usage tracking",
            feature_usage,
        )
        .off(),
        Item::new("folder_history", W, "Folder history (shellbags)", bags)
            .off()
            .note("Also forgets each folder's view settings"),
        Item::new(
            "tray_icons",
            W,
            "Notification area icon history",
            vec![
                Target::Value {
                    key: tray.into(),
                    name: "IconStreams",
                },
                Target::Value {
                    key: tray.into(),
                    name: "PastIconsStream",
                },
            ],
        )
        .off()
        .note("Takes effect after Explorer restarts"),
        Item::new(
            "prefetch",
            W,
            "Windows Prefetch files",
            vec![Target::FolderFiles {
                dir: env_path(r"%WINDIR%\Prefetch"),
                ext: Some("pf"),
            }],
        )
        .off()
        .admin()
        .note("Programs start a little slower the next time"),
        Item::new(
            "usb",
            W,
            "History of USB drives not connected now",
            vec![Target::Command {
                program: "powershell",
                args: vec!["-NoProfile".into(), "-Command".into(), USB_HISTORY.into()],
            }],
        )
        .off()
        .admin(),
        Item::new(
            "clipboard",
            W,
            "Current clipboard contents",
            vec![Target::Clipboard],
        )
        .off(),
    ]
}

fn network_items() -> Vec<Item> {
    let ie = r"HKCU\Software\Microsoft\Internet Explorer";
    let rdp = r"HKCU\Software\Microsoft\Terminal Server Client";
    use Section::Network as N;
    vec![
        Item::new(
            "typed_urls",
            N,
            "Typed web addresses (Internet Explorer and File Explorer)",
            vec![
                values(format!(r"{ie}\TypedURLs")),
                values(format!(r"{ie}\TypedURLsTime")),
            ],
        ),
        Item::new(
            "map_drive",
            N,
            "Map Network Drive history",
            vec![values(format!(r"{EXPLORER}\Map Network Drive MRU"))],
        ),
        Item::new(
            "rdp",
            N,
            "Remote Desktop connection history",
            vec![
                values(format!(r"{rdp}\Default")),
                Target::Key(format!(r"{rdp}\Servers")),
            ],
        )
        .off()
        .note("Also forgets the user name saved for each computer"),
        Item::new(
            "dns",
            N,
            "DNS cache",
            vec![Target::Command {
                program: "ipconfig",
                args: vec!["/flushdns".into()],
            }],
        )
        .off(),
    ]
}

/// Office keeps File MRU / Place MRU per version and app, and per signed-in account.
fn office_targets() -> Vec<Target> {
    let mut out = Vec::new();
    for version in subkeys(r"HKCU\Software\Microsoft\Office") {
        for app in subkeys(&version) {
            for list in ["File MRU", "Place MRU"] {
                let k = format!(r"{app}\{list}");
                if key_exists(&k) {
                    out.push(values(k));
                }
            }
            for user in subkeys(&format!(r"{app}\User MRU")) {
                for list in ["File MRU", "Place MRU"] {
                    let k = format!(r"{user}\{list}");
                    if key_exists(&k) {
                        out.push(values(k));
                    }
                }
            }
        }
    }
    out
}

fn adobe_targets() -> Vec<Target> {
    let mut out = Vec::new();
    for product in ["Acrobat Reader", "Adobe Acrobat"] {
        for version in subkeys(&format!(r"HKCU\Software\Adobe\{product}")) {
            for list in ["cRecentFiles", "cRecentFolders"] {
                let k = format!(r"{version}\AVGeneral\{list}");
                if key_exists(&k) {
                    out.push(Target::Key(k));
                }
            }
        }
    }
    out
}

/// Program items, listed only when the program has left history behind.
fn program_items() -> Vec<Item> {
    use Section::Programs as P;
    let winrar = r"HKCU\Software\WinRAR";
    let wmp = r"HKCU\Software\Microsoft\MediaPlayer\Player";
    let all = vec![
        Item::new(
            "paint",
            P,
            "Paint (classic) recent files",
            vec![values(format!(r"{APPLETS}\Paint\Recent File List"))],
        ),
        Item::new(
            "wordpad",
            P,
            "WordPad recent files",
            vec![values(format!(r"{APPLETS}\Wordpad\Recent File List"))],
        ),
        Item::new(
            "wmp",
            P,
            "Windows Media Player recent files and URLs",
            vec![
                values(format!(r"{wmp}\RecentFileList")),
                values(format!(r"{wmp}\RecentURLList")),
            ],
        ),
        Item::new(
            "office",
            P,
            "Microsoft Office recent files and places",
            office_targets(),
        ),
        Item::new(
            "adobe",
            P,
            "Adobe Acrobat and Reader recent files",
            adobe_targets(),
        ),
        Item::new(
            "winrar",
            P,
            "WinRAR archive and folder history",
            vec![
                values(format!(r"{winrar}\ArcHistory")),
                values(format!(r"{winrar}\DialogEditHistory\ArcName")),
                values(format!(r"{winrar}\DialogEditHistory\ExtrPath")),
            ],
        ),
        Item::new(
            "7zip",
            P,
            "7-Zip folder and copy history",
            vec![
                Target::Value {
                    key: r"HKCU\Software\7-Zip\FM".into(),
                    name: "FolderHistory",
                },
                Target::Value {
                    key: r"HKCU\Software\7-Zip\FM".into(),
                    name: "CopyHistory",
                },
                Target::Value {
                    key: r"HKCU\Software\7-Zip\Compression".into(),
                    name: "ArcHistory",
                },
                Target::Value {
                    key: r"HKCU\Software\7-Zip\Extraction".into(),
                    name: "PathHistory",
                },
            ],
        ),
    ];
    all.into_iter().filter(|i| count_item(i) > 0).collect()
}

/// Chromium-based browsers: (name, image, user data folder).
const BROWSERS: [(&str, &str, &str); 3] = [
    (
        "Microsoft Edge",
        "msedge.exe",
        r"%LOCALAPPDATA%\Microsoft\Edge\User Data",
    ),
    (
        "Google Chrome",
        "chrome.exe",
        r"%LOCALAPPDATA%\Google\Chrome\User Data",
    ),
    (
        "Brave",
        "brave.exe",
        r"%LOCALAPPDATA%\BraveSoftware\Brave-Browser\User Data",
    ),
];

fn profiles(user_data: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(user_data)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n == "Default" || n.starts_with("Profile "))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn browser_items() -> Vec<Item> {
    let mut out = Vec::new();
    for (name, image, dir) in BROWSERS {
        let profiles = profiles(&env_path(dir));
        if profiles.is_empty() {
            continue;
        }
        let files = |names: &[&str]| -> Vec<Target> {
            vec![Target::Files(
                profiles
                    .iter()
                    .flat_map(|p| names.iter().map(move |n| p.join(n)))
                    .collect(),
            )]
        };
        let slug: String = name.chars().filter(|c| c.is_alphanumeric()).collect();
        let item = |what: &str, label: &str, targets: Vec<Target>| {
            let mut i = Item::new(
                format!("{slug}_{what}"),
                Section::Browsers,
                format!("{name}: {label}"),
                targets,
            )
            .off();
            i.close = Some(image);
            i
        };
        out.push(item(
            "history",
            "browsing and download history",
            files(&[
                "History",
                "History-journal",
                "Visited Links",
                "Top Sites",
                "Top Sites-journal",
                "Shortcuts",
                "Shortcuts-journal",
            ]),
        ));
        out.push(
            item(
                "cookies",
                "cookies",
                files(&[r"Network\Cookies", r"Network\Cookies-journal"]),
            )
            .note("Signs you out of websites"),
        );
        out.push(item(
            "cache",
            "cache",
            profiles
                .iter()
                .flat_map(|p| ["Cache", "Code Cache", "GPUCache"].map(|c| Target::Cache(p.join(c))))
                .collect(),
        ));
        out.push(
            item(
                "forms",
                "saved form entries",
                files(&["Web Data", "Web Data-journal"]),
            )
            .note("Also removes saved addresses, cards and custom search engines"),
        );
        out.push(
            item(
                "passwords",
                "saved passwords",
                files(&[
                    "Login Data",
                    "Login Data-journal",
                    "Login Data For Account",
                    "Login Data For Account-journal",
                ]),
            )
            .note("Passwords synced to your account may come back"),
        );
    }
    out
}

/// Everything that can be cleared on this PC. It reads the registry and disk, so
/// build it off the UI thread.
pub fn catalog() -> Vec<Item> {
    let mut items = windows_items();
    items.extend(network_items());
    items.extend(program_items());
    items.extend(browser_items());
    items
}

// ---------------------------------------------------------------------------------
// Counting

fn value_names(key: &str, prefix: Option<&str>) -> Vec<String> {
    registry::open_read(key)
        .map(|k| {
            k.enum_values()
                .flatten()
                .map(|(n, _)| n)
                .filter(|n| !n.is_empty() && prefix.is_none_or(|p| n.starts_with(p)))
                .collect()
        })
        .unwrap_or_default()
}

/// Values and subkeys in a key tree.
fn key_entries(key: &str) -> usize {
    let Ok(k) = registry::open_read(key) else { return 0 };
    let Ok(info) = k.query_info() else { return 0 };
    (info.values as usize + info.sub_keys as usize).max(1)
}

fn folder_files(dir: &Path, ext: Option<&str>) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && !p
                            .file_name()
                            .is_some_and(|n| n.eq_ignore_ascii_case("desktop.ini"))
                        && ext.is_none_or(|x| {
                            p.extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| e.eq_ignore_ascii_case(x))
                        })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cache_files(dir: &Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .count()
}

pub fn count_target(t: &Target) -> usize {
    match t {
        Target::Values { key, prefix } => value_names(key, *prefix).len(),
        Target::Value { key, name } => usize::from(registry::read_raw(key, name).is_some()),
        Target::Key(key) => key_entries(key),
        Target::FolderFiles { dir, ext } => folder_files(dir, *ext).len(),
        Target::Files(files) => files.iter().filter(|p| p.is_file()).count(),
        Target::Cache(dir) => cache_files(dir),
        Target::Command { .. } | Target::Clipboard => 0,
    }
}

pub fn count_item(item: &Item) -> usize {
    item.targets.iter().map(count_target).sum()
}

// ---------------------------------------------------------------------------------
// Clearing

pub fn process_running(image: &str) -> bool {
    hidden_command("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_ascii_lowercase()
                .contains(&format!("\"{}\"", image.to_ascii_lowercase()))
        })
        .unwrap_or(false)
}

fn delete_key_tree(key: &str) -> Result<()> {
    let (root, sub) = registry::split_path(key)?;
    match root.delete_subkey_all(sub) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("deleting {key}"))),
    }
}

fn empty_cache(dir: &Path) -> usize {
    let mut removed = 0;
    for e in walkdir::WalkDir::new(dir)
        .contents_first(true)
        .min_depth(1)
        .into_iter()
        .flatten()
    {
        if e.file_type().is_file() {
            if std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        } else {
            let _ = std::fs::remove_dir(e.path());
        }
    }
    removed
}

fn empty_clipboard() -> Result<()> {
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};
    unsafe {
        OpenClipboard(None)?;
        let r = EmptyClipboard();
        let _ = CloseClipboard();
        r?;
    }
    Ok(())
}

/// What clearing an item has done so far, for the journal.
#[derive(Default)]
struct Done {
    count: usize,
    ran: bool,
    values: Vec<registry::ValueSnapshot>,
    steps: Vec<UndoAction>,
}

fn clear_targets(ctx: &AppCtx<'_>, item: &Item, done: &mut Done) -> Result<()> {
    for t in &item.targets {
        match t {
            Target::Values { key, prefix } => {
                for name in value_names(key, *prefix) {
                    let snap = registry::snapshot(key, &name);
                    registry::delete_value(key, &name)?;
                    done.values.push(snap);
                    done.count += 1;
                }
            }
            Target::Value { key, name } => {
                if registry::read_raw(key, name).is_some() {
                    let snap = registry::snapshot(key, name);
                    registry::delete_value(key, name)?;
                    done.values.push(snap);
                    done.count += 1;
                }
            }
            Target::Key(key) => {
                if key_exists(key) {
                    let entries = key_entries(key);
                    done.steps.push(ctx.journal.backup_reg_key(key)?);
                    delete_key_tree(key)?;
                    done.count += entries;
                }
            }
            Target::FolderFiles { .. } | Target::Files(_) => {
                let files = match t {
                    Target::FolderFiles { dir, ext } => folder_files(dir, *ext),
                    Target::Files(list) => list.iter().filter(|p| p.is_file()).cloned().collect(),
                    _ => unreachable!(),
                };
                if !files.is_empty() {
                    trash::delete_all(&files).context("moving to the Recycle Bin")?;
                    done.count += files.len();
                    done.steps
                        .push(UndoAction::RestoreFromRecycleBin { paths: files });
                }
            }
            Target::Cache(dir) => done.count += empty_cache(dir),
            Target::Command { program, args } => {
                run_checked(hidden_command(program).args(args))?;
                done.ran = true;
            }
            Target::Clipboard => {
                empty_clipboard()?;
                done.ran = true;
            }
        }
    }
    Ok(())
}

/// Clears one item and journals what was cleared, even when a later part fails.
/// Returns how many entries went.
pub fn clear_item(ctx: &AppCtx<'_>, item: &Item) -> Result<usize> {
    if let Some(image) = item.close
        && process_running(image)
    {
        bail!(
            "close {} first (check the notification area too)",
            image.trim_end_matches(".exe")
        );
    }
    if item.admin && !ctx.elevated {
        bail!("needs the app to run as administrator");
    }
    let mut done = Done::default();
    let result = clear_targets(ctx, item, &mut done);
    if done.count > 0 || done.ran {
        let mut steps = done.steps;
        if !done.values.is_empty() {
            steps.insert(0, UndoAction::RestoreRegValues { values: done.values });
        }
        let undo = match steps.len() {
            0 => UndoAction::None,
            1 => steps.remove(0),
            _ => UndoAction::Sequence { steps },
        };
        let what = if item.countable() {
            format!("Cleared {} ({})", item.label, done.count)
        } else {
            format!("Cleared {}", item.label)
        };
        ctx.journal.record(TOOL, what, undo)?;
    }
    result.map(|()| done.count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_builds_and_counts() {
        let items = catalog();
        let mut ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "item ids must be unique");
        assert!(items.iter().any(|i| i.id == "run"));
        for i in &items {
            let _ = count_item(i);
            assert!(!i.label.is_empty());
        }
        let dns = items.iter().find(|i| i.id == "dns").unwrap();
        assert!(!dns.undoable() && !dns.countable());
        assert!(items.iter().find(|i| i.id == "run").unwrap().undoable());
    }

    #[test]
    fn values_with_prefix_are_counted_and_cleared_only() {
        // Each test has its own root key, so parallel tests can't delete each other's.
        let key = r"HKCU\Software\WinManagerTestPrivacyValues";
        registry::write_raw(key, "BackgroundHistoryPath0", &registry::string("a")).unwrap();
        registry::write_raw(key, "BackgroundHistoryPath1", &registry::string("b")).unwrap();
        registry::write_raw(key, "Keep", &registry::dword(1)).unwrap();
        let t = Target::Values {
            key: key.into(),
            prefix: Some("BackgroundHistoryPath"),
        };
        assert_eq!(count_target(&t), 2);
        assert_eq!(count_target(&Target::Key(key.into())), 3);
        delete_key_tree(key).unwrap();
        assert_eq!(count_target(&t), 0);
    }

    #[test]
    fn folder_files_filters_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.PF"), b"x").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("desktop.ini"), b"x").unwrap();
        assert_eq!(folder_files(dir.path(), Some("pf")).len(), 1);
        assert_eq!(folder_files(dir.path(), None).len(), 2);
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("c"), b"x").unwrap();
        assert_eq!(cache_files(dir.path()), 4);
        assert_eq!(empty_cache(dir.path()), 4);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn clearing_is_journaled_and_undone() {
        let root = r"HKCU\Software\WinManagerTestPrivacyClear";
        let vals = format!(r"{root}\Values");
        let tree = format!(r"{root}\Tree");
        registry::write_raw(&vals, "MRU0", &registry::string("secret")).unwrap();
        registry::write_raw(&format!(r"{tree}\Sub"), "x", &registry::dword(7)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trace.txt");
        std::fs::write(&file, b"x").unwrap();
        let item = Item::new(
            "test",
            Section::Windows,
            "Test trace",
            vec![
                values(vals.clone()),
                Target::Key(tree.clone()),
                Target::Files(vec![file.clone()]),
            ],
        );
        assert_eq!(count_item(&item), 3);

        let jdir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&jdir.path().join("j.json"), &jdir.path().join("b"))
                .unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        assert_eq!(clear_item(&ctx, &item).unwrap(), 3);
        assert_eq!(count_item(&item), 0);
        assert!(!file.exists());
        let entries = journal.entries();
        assert_eq!(entries.len(), 1, "one journal entry per item");

        if let Err(e) = journal.undo(entries[0].id) {
            delete_key_tree(root).unwrap();
            if wm_core::safety::no_recycle_bin_here(&e) {
                return;
            }
            panic!("{e:#}");
        }
        assert!(registry::read_raw(&vals, "MRU0").is_some());
        assert!(registry::read_raw(&format!(r"{tree}\Sub"), "x").is_some());
        assert!(file.exists());
        delete_key_tree(root).unwrap();
    }

    #[test]
    fn missing_process_is_not_running() {
        assert!(!process_running("wm-no-such-process.exe"));
    }
}
