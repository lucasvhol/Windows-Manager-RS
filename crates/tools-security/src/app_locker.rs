//! App Locker: decide which programs may run, for every account, through Windows'
//! Software Restriction Policies. Rules block or allow a program by path (a file or a
//! folder), by name (any file with that name), by content (MD5 hash, so renaming or
//! moving it doesn't help) or by publisher (the certificate that signed it). Two modes:
//! block only what is listed, or allow only what is listed plus Windows and Program
//! Files. Quick blocks cover the system tools people most often lock away.
//!
//! Some security tools leave the policy key read-only even for administrators; the tool
//! offers to unlock it (journaled: undo puts the old permissions back). Rule keys whose
//! names start with "-" are inactive templates another program keeps; they're left alone.

use std::path::Path;

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use md5::{Digest, Md5};
use wm_core::{
    AppCtx, Category, Layout, Tool, acl, egui, icons, registry,
    registry::RawValue,
    safety::UndoAction,
    ui::{Confirm, Message, ToolButton, empty_state, frame, section, show_message},
};

pub const SRP: &str = r"HKLM\SOFTWARE\Policies\Microsoft\Windows\Safer\CodeIdentifiers";

/// The policy key the window works on. Debug builds can point it at a scratch key
/// (`WM_APPLOCKER_ROOT`) to try the window without touching the real policy.
fn root() -> String {
    #[cfg(debug_assertions)]
    if let Ok(r) = std::env::var("WM_APPLOCKER_ROOT") {
        return r;
    }
    SRP.to_owned()
}

const MARK: &str = "Win Manager RS";
const REG_EXPAND_SZ: u32 = 2;
const REG_QWORD: u32 = 11;
const ADMINISTRATORS: &str = "S-1-5-32-544";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Blocked,
    Allowed,
}

impl Level {
    fn subkey(self) -> &'static str {
        match self {
            Level::Blocked => "0",
            Level::Allowed => "262144",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Level::Blocked => "Blocked",
            Level::Allowed => "Allowed",
        }
    }
    /// The policy certificate store for publisher rules at this level.
    fn store(self) -> &'static str {
        match self {
            Level::Blocked => "Disallowed",
            Level::Allowed => "TrustedPublisher",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Path,
    Hash,
    Publisher,
}

impl RuleKind {
    pub fn label(self) -> &'static str {
        match self {
            RuleKind::Path => "Path or name",
            RuleKind::Hash => "File content",
            RuleKind::Publisher => "Publisher",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub level: Level,
    pub kind: RuleKind,
    /// The registry key that holds the rule (for publishers, the certificate's key).
    pub key: String,
    /// What it matches: a path or name, a file name and size, or a publisher name.
    pub target: String,
    pub description: String,
    pub ours: bool,
}

fn qword(v: u64) -> RawValue {
    RawValue {
        vtype: REG_QWORD,
        bytes: v.to_le_bytes().to_vec(),
    }
}

fn expand_sz(s: &str) -> RawValue {
    RawValue {
        vtype: REG_EXPAND_SZ,
        ..registry::string(s)
    }
}

fn now_filetime() -> u64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (unix.as_nanos() / 100) as u64 + 116_444_736_000_000_000
}

fn new_guid() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!(
        "{{{}-{}-{}-{}-{}}}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn subkeys(key: &str) -> Vec<String> {
    registry::open_read(key)
        .map(|k| k.enum_keys().flatten().collect())
        .unwrap_or_default()
}

fn read_str(key: &str, name: &str) -> String {
    registry::read_raw(key, name)
        .map(|v| registry::decode_string(&v))
        .unwrap_or_default()
}

fn read_qword(key: &str, name: &str) -> Option<u64> {
    let v = registry::read_raw(key, name)?;
    (v.bytes.len() >= 8).then(|| u64::from_le_bytes(v.bytes[..8].try_into().expect("8 bytes")))
}

// ---------------------------------------------------------------------------------
// Policy settings

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// True: everything runs except what's blocked. False: only what's allowed runs.
    pub default_allowed: bool,
    /// Administrators are not restricted.
    pub skip_admins: bool,
}

pub fn read_policy(root: &str) -> Policy {
    let dword = |n: &str| {
        registry::read_raw(root, n)
            .as_ref()
            .and_then(registry::decode_dword)
    };
    Policy {
        default_allowed: dword("DefaultLevel").is_none_or(|v| v != 0),
        skip_admins: dword("PolicyScope") == Some(1),
    }
}

fn policy_snapshots(root: &str) -> Vec<registry::ValueSnapshot> {
    [
        "DefaultLevel",
        "PolicyScope",
        "TransparentEnabled",
        "AuthenticodeEnabled",
    ]
    .iter()
    .map(|n| registry::snapshot(root, n))
    .collect()
}

/// Folders that must stay allowed in allow-only mode, or Windows stops working.
pub const ALWAYS_ALLOWED: [(&str, &str); 3] = [
    ("%SystemRoot%", "Windows"),
    ("%ProgramFiles%", "Programs (64-bit)"),
    ("%ProgramFiles(x86)%", "Programs (32-bit)"),
];

/// Writes the mode and scope; switching to allow-only first adds allow rules for
/// Windows, Program Files and this app's own folder. Returns the undo.
pub fn write_policy(root: &str, policy: Policy) -> Result<UndoAction> {
    let mut steps = Vec::new();
    if !policy.default_allowed {
        let mut needed: Vec<(String, String)> = ALWAYS_ALLOWED
            .iter()
            .map(|(p, d)| ((*p).to_owned(), (*d).to_owned()))
            .collect();
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
        {
            needed.push((dir.display().to_string(), "Win Manager RS itself".into()));
        }
        let existing = list_rules(root);
        for (path, desc) in needed {
            let have = existing.iter().any(|r| {
                r.level == Level::Allowed && r.kind == RuleKind::Path && r.target.eq_ignore_ascii_case(&path)
            });
            if !have {
                steps.push(add_path_rule(root, Level::Allowed, &path, &desc)?);
            }
        }
    }
    let snaps = policy_snapshots(root);
    registry::write_raw(
        root,
        "DefaultLevel",
        &registry::dword(if policy.default_allowed { 0x40000 } else { 0 }),
    )?;
    registry::write_raw(
        root,
        "PolicyScope",
        &registry::dword(u32::from(policy.skip_admins)),
    )?;
    registry::write_raw(root, "TransparentEnabled", &registry::dword(1))?;
    steps.push(UndoAction::RestoreRegValues { values: snaps });
    steps.reverse();
    Ok(UndoAction::Sequence { steps })
}

// ---------------------------------------------------------------------------------
// Path and hash rules

pub fn list_rules(root: &str) -> Vec<Rule> {
    let mut out = Vec::new();
    for level in [Level::Blocked, Level::Allowed] {
        for (kind, sub) in [(RuleKind::Path, "Paths"), (RuleKind::Hash, "Hashes")] {
            let base = format!(r"{root}\{}\{sub}", level.subkey());
            for name in subkeys(&base) {
                if !name.starts_with('{') {
                    continue; // inactive template ("-{…}") kept by another program
                }
                let key = format!(r"{base}\{name}");
                let description = read_str(&key, "Description");
                let target = match kind {
                    RuleKind::Path => read_str(&key, "ItemData"),
                    _ => {
                        let n = read_str(&key, "FriendlyName");
                        let size = read_qword(&key, "ItemSize").unwrap_or(0);
                        format!(
                            "{} ({})",
                            if n.is_empty() { "file" } else { &n },
                            wm_core::ui::format_bytes(size)
                        )
                    }
                };
                out.push(Rule {
                    level,
                    kind,
                    key,
                    target,
                    ours: description.starts_with(MARK),
                    description,
                });
            }
        }
    }
    out
}

/// Inactive rule templates left by another program (not enforced).
pub fn inactive_count(root: &str) -> usize {
    [Level::Blocked, Level::Allowed]
        .iter()
        .flat_map(|l| ["Paths", "Hashes"].map(|s| format!(r"{root}\{}\{s}", l.subkey())))
        .map(|k| subkeys(&k).iter().filter(|n| n.starts_with('-')).count())
        .sum()
}

/// A path rule: a file, a folder (everything in it), or a bare name like `game.exe`
/// (that name anywhere). Environment variables like `%ProgramFiles%` work.
pub fn valid_path_target(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty() && p.len() < 1024 && !p.contains(['"', '<', '>', '|', '\n', '\r'])
}

pub fn add_path_rule(root: &str, level: Level, path: &str, description: &str) -> Result<UndoAction> {
    let path = path.trim();
    if !valid_path_target(path) {
        bail!("\"{path}\" is not a valid path or program name");
    }
    let key = format!(r"{root}\{}\Paths\{}", level.subkey(), new_guid());
    registry::write_raw(&key, "ItemData", &expand_sz(path))?;
    registry::write_raw(&key, "SaferFlags", &registry::dword(0))?;
    registry::write_raw(
        &key,
        "Description",
        &registry::string(&format!("{MARK}: {description}")),
    )?;
    registry::write_raw(&key, "LastModified", &qword(now_filetime()))?;
    Ok(UndoAction::DeleteRegKey { key })
}

pub fn file_md5(path: &Path) -> Result<([u8; 16], u64)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Ok((h.finalize().into(), size))
}

pub fn add_hash_rule(root: &str, level: Level, file: &Path) -> Result<UndoAction> {
    let (hash, size) = file_md5(file)?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let key = format!(r"{root}\{}\Hashes\{}", level.subkey(), new_guid());
    registry::write_raw(&key, "ItemData", &registry::binary(&hash))?;
    registry::write_raw(&key, "HashAlg", &registry::dword(0x8003))?;
    registry::write_raw(&key, "ItemSize", &qword(size))?;
    registry::write_raw(&key, "SaferFlags", &registry::dword(0))?;
    registry::write_raw(&key, "FriendlyName", &registry::string(&name))?;
    registry::write_raw(
        &key,
        "Description",
        &registry::string(&format!("{MARK}: {name}, by content")),
    )?;
    registry::write_raw(&key, "LastModified", &qword(now_filetime()))?;
    Ok(UndoAction::DeleteRegKey { key })
}

fn delete_key(key: &str) -> Result<()> {
    let (hk, sub) = registry::split_path(key)?;
    hk.delete_subkey_all(sub)
        .with_context(|| format!("deleting {key}"))
}

// ---------------------------------------------------------------------------------
// Publisher rules (certificates in the policy stores)

mod cert {
    use anyhow::{Result, bail};
    use windows::Win32::Security::Cryptography::*;
    use windows::core::{HSTRING, PCWSTR};

    const ENCODING: CERT_QUERY_ENCODING_TYPE =
        CERT_QUERY_ENCODING_TYPE(X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0);

    pub struct Signer {
        pub name: String,
        pub thumbprint: String,
        ctx: *mut CERT_CONTEXT,
    }

    impl Drop for Signer {
        fn drop(&mut self) {
            unsafe {
                let _ = CertFreeCertificateContext(Some(self.ctx));
            }
        }
    }

    fn name_of(ctx: *const CERT_CONTEXT) -> String {
        unsafe {
            let mut buf = [0u16; 512];
            let n = CertGetNameStringW(ctx, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, Some(&mut buf));
            String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)])
        }
    }

    fn thumbprint_of(ctx: *const CERT_CONTEXT) -> String {
        unsafe {
            let mut hash = [0u8; 20];
            let mut len = hash.len() as u32;
            if CertGetCertificateContextProperty(
                ctx,
                CERT_SHA1_HASH_PROP_ID,
                Some(hash.as_mut_ptr().cast()),
                &mut len,
            )
            .is_err()
            {
                return String::new();
            }
            hash[..len as usize].iter().map(|b| format!("{b:02X}")).collect()
        }
    }

    /// The certificate that signed `path` (embedded Authenticode signature).
    pub fn signer_of(path: &std::path::Path) -> Result<Signer> {
        unsafe {
            let wide = HSTRING::from(path.as_os_str());
            let mut store = HCERTSTORE::default();
            let mut msg: *mut core::ffi::c_void = std::ptr::null_mut();
            if CryptQueryObject(
                CERT_QUERY_OBJECT_FILE,
                wide.as_ptr().cast(),
                CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
                CERT_QUERY_FORMAT_FLAG_BINARY,
                0,
                None,
                None,
                None,
                Some(&mut store),
                Some(&mut msg),
                None,
            )
            .is_err()
            {
                bail!("{} has no digital signature", path.display());
            }
            let result = (|| -> Result<Signer> {
                let mut len = 0u32;
                CryptMsgGetParam(msg, CMSG_SIGNER_INFO_PARAM, 0, None, &mut len)?;
                let mut buf = vec![0u8; len as usize];
                CryptMsgGetParam(
                    msg,
                    CMSG_SIGNER_INFO_PARAM,
                    0,
                    Some(buf.as_mut_ptr().cast()),
                    &mut len,
                )?;
                let info = &*(buf.as_ptr() as *const CMSG_SIGNER_INFO);
                let find = CERT_INFO {
                    Issuer: info.Issuer,
                    SerialNumber: info.SerialNumber,
                    ..Default::default()
                };
                let ctx = CertFindCertificateInStore(
                    store,
                    ENCODING,
                    0,
                    CERT_FIND_SUBJECT_CERT,
                    Some((&raw const find).cast()),
                    None,
                );
                if ctx.is_null() {
                    bail!("the signing certificate isn't in the file");
                }
                Ok(Signer {
                    name: name_of(ctx),
                    thumbprint: thumbprint_of(ctx),
                    ctx,
                })
            })();
            let _ = CryptMsgClose(Some(msg));
            let _ = CertCloseStore(Some(store), 0);
            result
        }
    }

    fn open_policy_store(name: &str) -> Result<HCERTSTORE> {
        let w = HSTRING::from(name);
        unsafe {
            Ok(CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                CERT_QUERY_ENCODING_TYPE(0),
                None,
                CERT_OPEN_STORE_FLAGS(
                    CERT_SYSTEM_STORE_LOCAL_MACHINE_GROUP_POLICY_ID << CERT_SYSTEM_STORE_LOCATION_SHIFT,
                ),
                Some(PCWSTR(w.as_ptr()).0.cast()),
            )?)
        }
    }

    /// Adds the signer to a policy store, labelled with `friendly`.
    pub fn add(store: &str, signer: &Signer, friendly: &str) -> Result<()> {
        unsafe {
            let label = HSTRING::from(friendly);
            let blob = CRYPT_INTEGER_BLOB {
                cbData: ((label.len() + 1) * 2) as u32,
                pbData: label.as_ptr() as *mut u8,
            };
            let _ = CertSetCertificateContextProperty(
                signer.ctx,
                CERT_FRIENDLY_NAME_PROP_ID,
                0,
                Some((&raw const blob).cast()),
            );
            let s = open_policy_store(store)?;
            let r =
                CertAddCertificateContextToStore(Some(s), signer.ctx, CERT_STORE_ADD_REPLACE_EXISTING, None);
            let _ = CertCloseStore(Some(s), 0);
            r?;
            Ok(())
        }
    }

    /// (name, thumbprint, friendly name) of every certificate in a policy store.
    pub fn list(store: &str) -> Vec<(String, String, String)> {
        let Ok(s) = open_policy_store(store) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        unsafe {
            let mut prev: Option<*const CERT_CONTEXT> = None;
            loop {
                let ctx = CertEnumCertificatesInStore(s, prev);
                if ctx.is_null() {
                    break;
                }
                let mut buf = [0u16; 256];
                let mut len = (buf.len() * 2) as u32;
                let friendly = if CertGetCertificateContextProperty(
                    ctx,
                    CERT_FRIENDLY_NAME_PROP_ID,
                    Some(buf.as_mut_ptr().cast()),
                    &mut len,
                )
                .is_ok()
                {
                    String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)])
                } else {
                    String::new()
                };
                out.push((name_of(ctx), thumbprint_of(ctx), friendly));
                prev = Some(ctx);
            }
            let _ = CertCloseStore(Some(s), 0);
        }
        out
    }
}

/// Registry key of a certificate in a policy store.
fn cert_key(store: &str, thumbprint: &str) -> String {
    format!(r"HKLM\SOFTWARE\Policies\Microsoft\SystemCertificates\{store}\Certificates\{thumbprint}")
}

pub fn list_publishers() -> Vec<Rule> {
    let mut out = Vec::new();
    for level in [Level::Blocked, Level::Allowed] {
        for (name, thumb, friendly) in cert::list(level.store()) {
            out.push(Rule {
                level,
                kind: RuleKind::Publisher,
                key: cert_key(level.store(), &thumb),
                target: name,
                ours: friendly.starts_with(MARK),
                description: if friendly.is_empty() {
                    format!("Certificate {thumb}")
                } else {
                    friendly
                },
            });
        }
    }
    out
}

/// Blocks or allows every program signed by the publisher that signed `file`. Publisher
/// checks need Authenticode checking on in the policy, which this turns on.
pub fn add_publisher_rule(root: &str, level: Level, file: &Path) -> Result<(String, UndoAction)> {
    let signer = cert::signer_of(file)?;
    if level == Level::Blocked && signer.name.to_ascii_lowercase().contains("microsoft") {
        bail!(
            "{} is signed by {}. Blocking that publisher would stop Windows itself; block the program by path or content instead.",
            file.display(),
            signer.name
        );
    }
    let key = cert_key(level.store(), &signer.thumbprint);
    if registry::open_read(&key).is_ok() {
        bail!("{} already has a rule", signer.name);
    }
    let snaps = vec![registry::snapshot(root, "AuthenticodeEnabled")];
    cert::add(level.store(), &signer, &format!("{MARK}: {}", signer.name))?;
    registry::write_raw(root, "AuthenticodeEnabled", &registry::dword(1))?;
    Ok((
        signer.name.clone(),
        UndoAction::Sequence {
            steps: vec![
                UndoAction::DeleteRegKey { key },
                UndoAction::RestoreRegValues { values: snaps },
            ],
        },
    ))
}

// ---------------------------------------------------------------------------------
// Quick blocks

/// System tools commonly locked away: (title, file under %SystemRoot%\System32).
pub const QUICK: [(&str, &str); 10] = [
    ("Registry Editor", r"%SystemRoot%\regedit.exe"),
    ("Task Manager", r"%SystemRoot%\System32\taskmgr.exe"),
    ("Command Prompt", r"%SystemRoot%\System32\cmd.exe"),
    (
        "Windows PowerShell",
        r"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe",
    ),
    (
        "PowerShell ISE",
        r"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell_ise.exe",
    ),
    (
        "System Configuration (msconfig)",
        r"%SystemRoot%\System32\msconfig.exe",
    ),
    ("Management consoles (MMC)", r"%SystemRoot%\System32\mmc.exe"),
    (
        "Windows Script Host (wscript)",
        r"%SystemRoot%\System32\wscript.exe",
    ),
    (
        "Windows Script Host (cscript)",
        r"%SystemRoot%\System32\cscript.exe",
    ),
    (
        "Windows Security tray icon",
        r"%SystemRoot%\System32\SecurityHealthSystray.exe",
    ),
];

fn quick_rule<'a>(rules: &'a [Rule], path: &str) -> Option<&'a Rule> {
    rules.iter().find(|r| {
        r.level == Level::Blocked && r.kind == RuleKind::Path && r.target.eq_ignore_ascii_case(path)
    })
}

/// Ready-made sets of quick blocks: (name, what it's for, titles from [`QUICK`]).
pub const TEMPLATES: [(&str, &str, &[&str]); 3] = [
    (
        "Child account",
        "Blocks the tools that change or get around settings",
        &[
            "Registry Editor",
            "Task Manager",
            "Command Prompt",
            "Windows PowerShell",
            "PowerShell ISE",
            "System Configuration (msconfig)",
            "Management consoles (MMC)",
        ],
    ),
    (
        "No scripts",
        "Blocks the script engines malware often uses",
        &[
            "Windows PowerShell",
            "PowerShell ISE",
            "Windows Script Host (wscript)",
            "Windows Script Host (cscript)",
        ],
    ),
    (
        "Shared PC",
        "Blocks system tools but keeps Task Manager",
        &[
            "Registry Editor",
            "Command Prompt",
            "System Configuration (msconfig)",
            "Management consoles (MMC)",
        ],
    ),
];

/// Adds the template's quick blocks that aren't there yet; returns the undo (None when
/// everything was already blocked).
pub fn apply_template(root: &str, titles: &[&str]) -> Result<Option<UndoAction>> {
    let rules = list_rules(root);
    let mut steps = Vec::new();
    for (title, path) in QUICK.iter().filter(|(t, _)| titles.contains(t)) {
        if quick_rule(&rules, path).is_none() {
            steps.push(add_path_rule(root, Level::Blocked, path, title)?);
        }
    }
    Ok((!steps.is_empty()).then_some(UndoAction::Sequence { steps }))
}

/// Saves the whole policy (mode and rules) to a .reg file. Publisher rules live in the
/// certificate stores and aren't included.
pub fn back_up(root: &str, file: &Path) -> Result<()> {
    wm_core::process::run_checked(
        wm_core::process::hidden_command("reg")
            .args(["export", root])
            .arg(file)
            .arg("/y"),
    )?;
    Ok(())
}

/// Replaces the policy with a backup made by [`back_up`]; returns the undo (the policy as
/// it was, exported to the journal's backups first).
pub fn restore(ctx: &AppCtx<'_>, root: &str, file: &Path) -> Result<UndoAction> {
    let text = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let text = if text.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = text[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&text).into_owned()
    };
    let long = registry::regedit_path(root);
    let wanted = long.trim_start_matches(r"Computer\").to_ascii_lowercase();
    let keys: Vec<String> = text
        .lines()
        .filter(|l| l.starts_with('['))
        .map(|l| {
            l.trim_start_matches(['[', '-'])
                .trim_end_matches(']')
                .to_ascii_lowercase()
        })
        .collect();
    let inside = |k: &String| *k == wanted || k.starts_with(&format!(r"{wanted}\"));
    if keys.is_empty() || !keys.iter().all(inside) {
        bail!(
            "{} isn't an App Locker backup (it has keys outside the policy)",
            file.display()
        );
    }
    // The rule folders are replaced; the key itself (and its permissions) stays.
    let before = ctx.journal.backup_reg_key(root)?;
    let levels = [Level::Blocked, Level::Allowed].map(|l| format!(r"{root}\{}", l.subkey()));
    for k in &levels {
        if registry::open_read(k).is_ok() {
            delete_key(k)?;
        }
    }
    wm_core::process::run_checked(wm_core::process::hidden_command("reg").arg("import").arg(file))?;
    if matches!(before, UndoAction::DeleteRegKey { .. }) {
        return Ok(before); // there was no policy before
    }
    let mut steps: Vec<UndoAction> = levels
        .iter()
        .filter(|k| registry::open_read(k).is_ok())
        .map(|k| UndoAction::DeleteRegKey { key: k.clone() })
        .collect();
    steps.push(before);
    Ok(UndoAction::Sequence { steps })
}

// ---------------------------------------------------------------------------------
// Unlocking a read-only policy key

/// True when the key exists but administrators can't write it.
pub fn locked(root: &str) -> bool {
    registry::open_read(root).is_ok() && !registry::writable(root)
}

pub fn unlock(root: &str) -> Result<UndoAction> {
    let before = acl::reg_dacl_sddl(root)?;
    acl::set_reg_dacl_sddl(root, &acl::with_full_control(&before, ADMINISTRATORS))?;
    Ok(UndoAction::RestoreRegAcl {
        key: root.to_owned(),
        sddl: before,
    })
}

// ---------------------------------------------------------------------------------
// UI

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddHow {
    Path,
    Name,
    Hash,
    Publisher,
}

impl AddHow {
    const ALL: [AddHow; 4] = [AddHow::Path, AddHow::Name, AddHow::Hash, AddHow::Publisher];
    fn label(self) -> &'static str {
        match self {
            AddHow::Path => "This file (by path)",
            AddHow::Name => "Any file with this name",
            AddHow::Hash => "This file wherever it is (by content)",
            AddHow::Publisher => "Everything from its publisher (signature)",
        }
    }
}

enum Pending {
    Remove(Rule),
    Mode(Policy),
    Restore(std::path::PathBuf),
}

pub struct AppLocker {
    rules: Option<Vec<Rule>>,
    /// Registry key of the selected rule.
    selected: Option<String>,
    how: AddHow,
    level: Level,
    typed: String,
    confirm: Confirm<Pending>,
    message: Message,
}

impl Default for AppLocker {
    fn default() -> Self {
        Self {
            rules: None,
            selected: None,
            how: AddHow::Path,
            level: Level::Blocked,
            typed: String::new(),
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl AppLocker {
    fn done(&mut self, ctx: &AppCtx<'_>, what: String, r: Result<UndoAction>) {
        self.message = Some(match r {
            Ok(undo) => match ctx.journal.record("applocker", what.clone(), undo) {
                Ok(_) => Ok(format!("{what}. Takes effect for programs started from now on.")),
                Err(e) => Err(format!("{what}, but it couldn't be journaled: {e:#}")),
            },
            Err(e) => Err(format!("{e:#}")),
        });
        self.rules = None;
    }

    fn add(&mut self, ctx: &AppCtx<'_>, file: Option<&Path>) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let verb = if self.level == Level::Blocked {
            "Blocked"
        } else {
            "Allowed"
        };
        match (self.how, file) {
            (AddHow::Path, Some(f)) => {
                let p = f.display().to_string();
                let r = add_path_rule(&root(), self.level, &p, &p);
                self.done(ctx, format!("{verb} {p}"), r);
            }
            (AddHow::Name, _) => {
                let name = file
                    .and_then(|f| f.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.typed.trim().to_owned());
                if name.contains(['\\', '/']) {
                    self.message = Some(Err("Type a file name only, like game.exe.".into()));
                    return;
                }
                let r = add_path_rule(&root(), self.level, &name, &format!("any file named {name}"));
                self.done(ctx, format!("{verb} every file named {name}"), r);
            }
            (AddHow::Hash, Some(f)) => {
                let r = add_hash_rule(&root(), self.level, f);
                self.done(ctx, format!("{verb} {} by its content", f.display()), r);
            }
            (AddHow::Publisher, Some(f)) => match add_publisher_rule(&root(), self.level, f) {
                Ok((name, undo)) => {
                    self.done(ctx, format!("{verb} every program signed by {name}"), Ok(undo))
                }
                Err(e) => self.done(ctx, String::new(), Err(e)),
            },
            _ => {}
        }
    }
}

impl Tool for AppLocker {
    fn id(&self) -> &'static str {
        "applocker"
    }

    fn category(&self) -> Category {
        Category::Security
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if let Some(p) = self.confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                match p {
                    Pending::Remove(rule) => {
                        let r = ctx.journal.backup_reg_key(&rule.key).and_then(|undo| {
                            delete_key(&rule.key)?;
                            Ok(undo)
                        });
                        self.selected = None;
                        self.done(ctx, format!("Removed the rule for {}", rule.target), r);
                    }
                    Pending::Mode(policy) => {
                        let r = write_policy(&root(), policy);
                        self.done(ctx, "Changed the App Locker mode".into(), r);
                    }
                    Pending::Restore(file) => {
                        let r = restore(ctx, &root(), &file);
                        self.done(ctx, format!("Restored App Locker from {}", file.display()), r);
                    }
                }
            }
        }

        let usable = ctx.elevated && !locked(&root());
        let rules = if usable {
            self.rules
                .get_or_insert_with(|| {
                    let mut r = list_rules(&root());
                    r.extend(list_publishers());
                    r
                })
                .clone()
        } else {
            Vec::new()
        };
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| rules.iter().find(|r| &r.key == k))
            .cloned();
        match frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::PLUS, "Add program").enabled(usable)),
                Some(
                    ToolButton::new(icons::FOLDERS, "Add folder").enabled(usable && self.how == AddHow::Path),
                ),
                Some(ToolButton::new(icons::TRASH, "Remove").enabled(selected.is_some())),
                None,
                Some(ToolButton::new(icons::FLOPPY_DISK, "Back up").enabled(usable)),
                Some(ToolButton::new(icons::CLOCK_COUNTER_CLOCKWISE, "Restore").enabled(usable)),
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(usable)),
            ],
            |_| {},
        ) {
            Some(0) => {
                if let Some(f) = rfd::FileDialog::new()
                    .add_filter(
                        "Programs",
                        &["exe", "com", "bat", "cmd", "msi", "ps1", "vbs", "js"],
                    )
                    .pick_file()
                {
                    self.add(ctx, Some(&f));
                }
            }
            Some(1) => {
                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                    self.add(ctx, Some(&d));
                }
            }
            Some(2) => {
                if let Some(r) = selected.clone() {
                    let extra = if r.ours {
                        ""
                    } else {
                        " It wasn't made by Win Manager RS."
                    };
                    self.confirm.ask(
                        format!("Remove the rule for {}?{extra} It can be undone.", r.target),
                        Pending::Remove(r),
                    );
                }
            }
            Some(4) => {
                if let Some(f) = rfd::FileDialog::new()
                    .add_filter("Registry file", &["reg"])
                    .set_file_name("App Locker backup.reg")
                    .save_file()
                {
                    self.message = Some(
                        back_up(&root(), &f)
                            .map(|()| format!("Saved to {}. Publisher rules aren't included.", f.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(5) => {
                if let Some(f) = rfd::FileDialog::new()
                    .add_filter("Registry file", &["reg"])
                    .pick_file()
                {
                    self.confirm.ask(
                        format!(
                            "Replace the current App Locker rules with the ones in {}? It can be undone.",
                            f.display()
                        ),
                        Pending::Restore(f),
                    );
                }
            }
            Some(6) => {
                self.rules = None;
                self.message = None;
            }
            _ => {}
        }
        if usable {
            frame::options(ui, |ui| {
                ui.label("New rule:");
                egui::ComboBox::from_id_salt("al-level")
                    .selected_text(self.level.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.level, Level::Blocked, "Block");
                        ui.selectable_value(&mut self.level, Level::Allowed, "Allow");
                    });
                egui::ComboBox::from_id_salt("al-how")
                    .selected_text(self.how.label())
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for h in AddHow::ALL {
                            ui.selectable_value(&mut self.how, h, h.label());
                        }
                    });
                if self.how == AddHow::Name {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.typed)
                            .hint_text("game.exe")
                            .desired_width(160.0),
                    );
                    if ui
                        .add_enabled(!self.typed.trim().is_empty(), egui::Button::new("Add"))
                        .clicked()
                    {
                        self.add(ctx, None);
                        self.typed.clear();
                    }
                } else {
                    ui.weak("then Add program or Add folder");
                }
            });
        }
        let inactive = if usable { inactive_count(&root()) } else { 0 };
        let status = format!(
            "{} rules, {} blocking",
            rules.len(),
            rules.iter().filter(|r| r.level == Level::Blocked).count()
        );
        frame::status(ui, &status, |ui| {
            if inactive > 0 {
                ui.weak(format!(
                    "{inactive} inactive templates from another program are left alone"
                ));
            } else {
                ui.weak("Windows enforces the rules each time a program starts, for every account.");
            }
        });
        if !usable {
            frame::central(ui, |ui| {
                show_message(ui, &self.message);
                if !ctx.elevated {
                    empty_state(
                        ui,
                        icons::SHIELD,
                        "Administrator rights needed",
                        "App Locker decides which programs may run, for every account. Restart the app as administrator.",
                    );
                    return;
                }
                section(
                    ui,
                    "The policy key is read-only",
                    |_| {},
                    |ui| {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "Another program left the policy key read-only, even for administrators.",
                        );
                        ui.add_space(4.0);
                        if ui
                        .button("Unlock it")
                        .on_hover_text(
                            "Gives Administrators full control of the key. Undo restores the old permissions.",
                        )
                        .clicked()
                    {
                        let r = unlock(&root());
                        self.done(ctx, "Unlocked the program-restriction policy key".into(), r);
                    }
                    },
                );
            });
            return;
        }

        frame::left(ui, 300.0, |ui| self.settings_pane(ui, ctx, &rules));
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            if rules.is_empty() {
                empty_state(
                    ui,
                    icons::SHIELD_CHECK,
                    "No rules yet",
                    "Choose Block or Allow and how to match above, then Add program or Add folder.",
                );
                return;
            }
            let mut clicked = None;
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            TableBuilder::new(ui)
                .striped(true)
                .sense(egui::Sense::click())
                .column(Column::initial(70.0))
                .column(Column::initial(90.0))
                .column(Column::initial(330.0).clip(true))
                .column(Column::remainder().clip(true))
                .header(22.0, |mut h| {
                    for t in ["", "Type", "Matches", "Note"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, rules.len(), |mut row| {
                        let r = &rules[row.index()];
                        row.set_selected(self.selected.as_ref() == Some(&r.key));
                        row.col(|ui| {
                            if r.level == Level::Blocked {
                                ui.colored_label(ui.visuals().warn_fg_color, r.level.label());
                            } else {
                                ui.label(r.level.label());
                            }
                        });
                        row.col(|ui| {
                            ui.weak(r.kind.label());
                        });
                        row.col(|ui| {
                            ui.label(&r.target).on_hover_text(&r.target);
                        });
                        row.col(|ui| {
                            ui.weak(
                                r.description
                                    .strip_prefix(&format!("{MARK}: "))
                                    .unwrap_or(&r.description),
                            );
                        });
                        if row.response().clicked() {
                            clicked = Some(r.key.clone());
                        }
                    });
                });
            if clicked.is_some() {
                self.selected = clicked;
            }
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

impl AppLocker {
    /// Mode, quick blocks and templates, in the left pane.
    fn settings_pane(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>, rules: &[Rule]) {
        let policy = read_policy(&root());
        let mut want = policy;
        section(
            ui,
            "Mode",
            |_| {},
            |ui| {
                ui.radio_value(
                    &mut want.default_allowed,
                    true,
                    "Block only the programs I list (everything else runs)",
                );
                ui.radio_value(
                    &mut want.default_allowed,
                    false,
                    "Allow only the programs I list, plus Windows and Program Files",
                );
                ui.checkbox(&mut want.skip_admins, "Don't restrict administrators")
                    .on_hover_text(
                        "Administrators (including this app when run as administrator) can run anything.",
                    );
            },
        );
        if want != policy {
            let text = if !want.default_allowed && policy.default_allowed {
                "Switch to allow-only? Programs outside Windows, Program Files and the allowed list will refuse to start, \
                 including ones in Downloads or on USB drives. Allow rules for Windows, Program Files and this app are added first."
            } else {
                "Change the App Locker mode?"
            };
            self.confirm.ask(text, Pending::Mode(want));
        }
        ui.add_space(8.0);
        section(
            ui,
            "Quick blocks",
            |_| {},
            |ui| {
                if !policy.skip_admins {
                    ui.weak("Win Manager RS uses Command Prompt and PowerShell for some of its own tools; while those are blocked for administrators, those parts stop working.");
                    ui.add_space(4.0);
                }
                for (title, path) in QUICK {
                    let existing = quick_rule(rules, path).cloned();
                    let mut on = existing.is_some();
                    if ui.checkbox(&mut on, title).changed() {
                        if on {
                            let r = add_path_rule(&root(), Level::Blocked, path, title);
                            self.done(ctx, format!("Blocked {title}"), r);
                        } else if let Some(rule) = existing {
                            let r = ctx.journal.backup_reg_key(&rule.key).and_then(|undo| {
                                delete_key(&rule.key)?;
                                Ok(undo)
                            });
                            self.done(ctx, format!("Unblocked {title}"), r);
                        }
                    }
                }
            },
        );
        ui.add_space(8.0);
        section(
            ui,
            "Templates",
            |_| {},
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (name, what, titles) in TEMPLATES {
                        if ui
                            .button(name)
                            .on_hover_text(format!("{what}: {}", titles.join(", ")))
                            .clicked()
                        {
                            if ctx.config.dry_run {
                                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
                            } else {
                                match apply_template(&root(), titles) {
                                    Ok(Some(undo)) => {
                                        self.done(ctx, format!("Applied the {name} template"), Ok(undo))
                                    }
                                    Ok(None) => {
                                        self.message =
                                            Some(Ok(format!("Everything in {name} is already blocked.")))
                                    }
                                    Err(e) => self.done(ctx, String::new(), Err(e)),
                                }
                            }
                        }
                    }
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> String {
        format!(
            r"HKCU\Software\win-manager-rs-test-applocker-{}",
            std::process::id()
        )
    }

    #[test]
    fn guids_and_targets() {
        let g = new_guid();
        assert_eq!(g.len(), 38);
        assert!(g.starts_with('{') && g.ends_with('}'));
        assert_ne!(new_guid(), g);
        assert!(valid_path_target("game.exe"));
        assert!(valid_path_target(r"%ProgramFiles%\Game\"));
        assert!(!valid_path_target(""));
        assert!(!valid_path_target("a|b"));
    }

    #[test]
    fn rules_policy_and_undo_in_a_test_key() {
        let root = root();
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let run = || -> Result<()> {
            // A template another program keeps must be ignored.
            registry::write_raw(
                &format!(r"{root}\0\Paths\-{{30A9864A-875A-4740-ADD6-7C13767E0555}}"),
                "ItemData",
                &registry::string(r"C:\Program Files\"),
            )?;
            let exe = dir.path().join("tool.exe");
            std::fs::write(&exe, b"MZ not really a program")?;
            let u1 = add_path_rule(&root, Level::Blocked, "game.exe", "any file named game.exe")?;
            let u2 = add_hash_rule(&root, Level::Blocked, &exe)?;
            let rules = list_rules(&root);
            assert_eq!(rules.len(), 2);
            assert_eq!(inactive_count(&root), 1);
            assert!(rules.iter().all(|r| r.ours && r.level == Level::Blocked));
            assert!(
                rules
                    .iter()
                    .any(|r| r.kind == RuleKind::Path && r.target == "game.exe")
            );
            let hash_key = &rules.iter().find(|r| r.kind == RuleKind::Hash).unwrap().key;
            let (md5, size) = file_md5(&exe)?;
            assert_eq!(registry::read_raw(hash_key, "ItemData").unwrap().bytes, md5);
            assert_eq!(read_qword(hash_key, "ItemSize"), Some(size));
            assert_eq!(
                registry::read_raw(&rules[0].key, "ItemData").unwrap().vtype,
                REG_EXPAND_SZ
            );

            // Allow-only mode adds the folders Windows needs, once.
            let u3 = write_policy(
                &root,
                Policy {
                    default_allowed: false,
                    skip_admins: true,
                },
            )?;
            assert_eq!(
                read_policy(&root),
                Policy {
                    default_allowed: false,
                    skip_admins: true
                }
            );
            let allowed = list_rules(&root)
                .into_iter()
                .filter(|r| r.level == Level::Allowed)
                .count();
            assert_eq!(
                allowed,
                ALWAYS_ALLOWED.len() + 1,
                "Windows, Program Files and this app"
            );
            let _ = write_policy(
                &root,
                Policy {
                    default_allowed: false,
                    skip_admins: true,
                },
            )?;
            let again = list_rules(&root)
                .into_iter()
                .filter(|r| r.level == Level::Allowed)
                .count();
            assert_eq!(again, allowed, "not added twice");

            for (i, u) in [u3, u2, u1].into_iter().enumerate() {
                let e = journal.record("t", format!("step {i}"), u)?;
                journal.undo(e.id)?;
            }
            assert!(read_policy(&root).default_allowed, "mode undone");
            let left: Vec<_> = list_rules(&root)
                .into_iter()
                .filter(|r| r.level == Level::Blocked)
                .collect();
            assert!(left.is_empty(), "block rules undone: {left:?}");
            assert!(add_path_rule(&root, Level::Blocked, "a|b", "x").is_err());
            Ok(())
        };
        let r = run();
        let (hk, sub) = registry::split_path(&root).unwrap();
        let _ = hk.delete_subkey_all(sub);
        r.unwrap();
    }

    #[test]
    fn templates_backup_and_restore() {
        let root = format!("{}-backup", root());
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: true,
        };
        let run = || -> Result<()> {
            for (_, _, titles) in TEMPLATES {
                assert!(
                    titles.iter().all(|t| QUICK.iter().any(|(q, _)| q == t)),
                    "{t:?}",
                    t = titles
                );
            }
            let scripts = TEMPLATES[1].2;
            let u = apply_template(&root, scripts)?.expect("adds rules");
            assert_eq!(list_rules(&root).len(), scripts.len());
            assert!(apply_template(&root, scripts)?.is_none(), "nothing added twice");
            let file = dir.path().join("backup.reg");
            back_up(&root, &file)?;
            let e = journal.record("t", "template", u)?;
            journal.undo(e.id)?;
            assert!(list_rules(&root).is_empty());
            add_path_rule(&root, Level::Blocked, "other.exe", "other")?;
            let undo = restore(&ctx, &root, &file)?;
            let mut got: Vec<String> = list_rules(&root).into_iter().map(|r| r.target).collect();
            got.sort();
            let mut want: Vec<String> = QUICK
                .iter()
                .filter(|(t, _)| scripts.contains(t))
                .map(|(_, p)| (*p).to_owned())
                .collect();
            want.sort();
            assert_eq!(got, want, "the backup replaces the rules");
            let e = journal.record("t", "restore", undo)?;
            journal.undo(e.id)?;
            let after: Vec<String> = list_rules(&root).into_iter().map(|r| r.target).collect();
            assert_eq!(
                after,
                vec!["other.exe".to_owned()],
                "undo brings the old rules back"
            );
            let bad = dir.path().join("bad.reg");
            std::fs::write(
                &bad,
                "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\Elsewhere]\r\n",
            )?;
            assert!(
                restore(&ctx, &root, &bad).is_err(),
                "files with other keys are refused"
            );
            Ok(())
        };
        let r = run();
        let (hk, sub) = registry::split_path(&root).unwrap();
        let _ = hk.delete_subkey_all(sub);
        r.unwrap();
    }

    #[test]
    fn microsoft_publisher_cannot_be_blocked() {
        let root = format!("{}-publisher", root());
        let notepad = Path::new(r"C:\Windows\System32\notepad.exe");
        let e = add_publisher_rule(&root, Level::Blocked, notepad).unwrap_err();
        let text = format!("{e:#}");
        // notepad is catalog-signed on some builds (no embedded signature): either
        // refusal is fine, but nothing may be written.
        assert!(
            text.contains("Microsoft") || text.contains("no digital signature"),
            "{text}"
        );
        assert!(registry::open_read(&root).is_err());
    }
}

/// Live checks against the real policy (administrator). Run with
/// `WM_AL_PROBE=<unsigned copy of an exe> WM_AL_SIGNED=<self-signed copy>`
/// `cargo test -p wm-tools-security live_app_locker -- --ignored`.
#[cfg(test)]
mod live {
    use super::*;

    /// Started from a fresh cmd each time: a process caches the policy it has seen.
    fn runs(exe: &Path) -> bool {
        let out = std::process::Command::new("cmd")
            .arg("/c")
            .arg(exe)
            .output()
            .expect("cmd");
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        let blocked = text.contains("blocked by group policy");
        if blocked {
            eprintln!("  blocked: {}", text.trim());
        }
        !blocked
    }

    /// Waits up to 5 s for the program to become runnable (or blocked).
    fn settles(exe: &Path, want_runs: bool) -> bool {
        (0..20).any(|_| {
            let ok = runs(exe) == want_runs;
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            ok
        })
    }

    #[test]
    #[ignore]
    fn live_app_locker() {
        let probe = std::path::PathBuf::from(std::env::var("WM_AL_PROBE").unwrap());
        let signed = std::path::PathBuf::from(std::env::var("WM_AL_SIGNED").unwrap());
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let acl_before = acl::reg_dacl_sddl(SRP).unwrap();
        let rules_before = list_rules(SRP);
        let policy_before = policy_snapshots(SRP);
        let mut results = Vec::new();
        let mut unlock_undo = None;
        if locked(SRP) {
            unlock_undo = Some(unlock(SRP).unwrap());
        }
        assert!(!locked(SRP), "unlocked");
        results.push(("probe runs before", runs(&probe)));
        // By path, by content, by publisher: each blocks, and undo lifts it.
        type Add<'a> = Box<dyn Fn() -> Result<UndoAction> + 'a>;
        let attempts: Vec<(&str, &Path, Add)> = vec![
            (
                "path",
                &probe,
                Box::new(|| add_path_rule(SRP, Level::Blocked, &probe.display().to_string(), "live test")),
            ),
            (
                "content",
                &probe,
                Box::new(|| add_hash_rule(SRP, Level::Blocked, &probe)),
            ),
            (
                "publisher",
                &signed,
                Box::new(|| add_publisher_rule(SRP, Level::Blocked, &signed).map(|(_, u)| u)),
            ),
        ];
        for (what, exe, add) in attempts {
            match add() {
                Ok(u) => {
                    results.push((what, settles(exe, false)));
                    let e = journal.record("t", what, u).unwrap();
                    let undo_result = journal.undo(e.id);
                    if let Err(err) = &undo_result {
                        eprintln!("{what} undo: {err:#}");
                    }
                    let undone = undo_result.is_ok();
                    eprintln!(
                        "{what}: rules now {:?}",
                        list_rules(SRP)
                            .iter()
                            .map(|r| r.target.clone())
                            .collect::<Vec<_>>()
                    );
                    results.push((what, undone && settles(exe, true)));
                }
                Err(e) => {
                    eprintln!("{what}: {e:#}");
                    results.push((what, false));
                }
            }
        }
        for s in &policy_before {
            let _ = registry::restore(s);
        }
        if let Some(u) = unlock_undo {
            let e = journal.record("t", "unlock", u).unwrap();
            journal.undo(e.id).unwrap();
        }
        let acl_after = acl::reg_dacl_sddl(SRP).unwrap();
        let rules_after = list_rules(SRP);
        for r in &results {
            eprintln!("{r:?}");
        }
        assert_eq!(acl_after, acl_before, "permissions restored exactly");
        assert_eq!(rules_after, rules_before, "no rules left behind");
        assert!(results.iter().all(|(_, ok)| *ok), "{results:?}");
    }
}
