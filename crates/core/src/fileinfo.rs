//! File details used by several list tools: Authenticode signature (embedded or
//! catalog), signer, company, description and version.
//!
//! Everything is read in one Windows PowerShell call per batch, because
//! `Get-AuthenticodeSignature` also checks catalog-signed Windows files, which a
//! plain `WinVerifyTrust` on the file does not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Signature {
    /// Signed and the signature is valid.
    Valid,
    /// Not signed at all.
    Unsigned,
    /// Signed, but the signature is invalid, expired or untrusted.
    Untrusted,
    /// Not checked (yet), or the file could not be read.
    #[default]
    Unknown,
}

impl Signature {
    pub fn label(self) -> &'static str {
        match self {
            Signature::Valid => "Signed",
            Signature::Unsigned => "Unsigned",
            Signature::Untrusted => "Untrusted",
            Signature::Unknown => "",
        }
    }

    fn from_status(s: &str) -> Self {
        match s {
            "Valid" => Signature::Valid,
            "NotSigned" => Signature::Unsigned,
            "" | "UnknownError" | "Incompatible" => Signature::Unknown,
            _ => Signature::Untrusted,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileInfo {
    pub signature: Signature,
    pub signer: String,
    pub company: String,
    pub description: String,
    pub version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Raw {
    path: String,
    status: String,
    #[serde(default)]
    signer: Option<String>,
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

const SCRIPT: &str = r#"[Console]::OutputEncoding = [Text.Encoding]::UTF8
$r = foreach ($f in (Get-Content -LiteralPath $args[0] -Encoding UTF8)) {
  try {
    $s = Get-AuthenticodeSignature -LiteralPath $f -ErrorAction Stop
    $v = (Get-Item -LiteralPath $f -ErrorAction Stop).VersionInfo
    $n = if ($s.SignerCertificate) { $s.SignerCertificate.GetNameInfo('SimpleName', $false) } else { '' }
    [pscustomobject]@{ Path = [string]$f; Status = [string]$s.Status; Signer = $n; Company = [string]$v.CompanyName; Description = [string]$v.FileDescription; Version = [string]$v.FileVersion }
  } catch { [Console]::Error.WriteLine("$f`: $_") }
}
ConvertTo-Json -Compress -InputObject @($r)"#;

fn parse(json: &str) -> Result<HashMap<PathBuf, FileInfo>> {
    let json = json.trim().trim_start_matches('\u{feff}');
    if json.is_empty() {
        return Ok(HashMap::new());
    }
    let raw: Vec<Raw> = serde_json::from_str(json).context("reading file details")?;
    Ok(raw
        .into_iter()
        .map(|r| {
            let trim = |s: Option<String>| s.unwrap_or_default().trim().to_owned();
            (
                PathBuf::from(r.path),
                FileInfo {
                    signature: Signature::from_status(&r.status),
                    signer: trim(r.signer),
                    company: trim(r.company),
                    description: trim(r.description),
                    version: trim(r.version),
                },
            )
        })
        .collect())
}

/// Reads details for `paths` (blocking; call from a background task).
/// Files that can't be read are missing from the result.
pub fn describe(paths: &[PathBuf]) -> Result<HashMap<PathBuf, FileInfo>> {
    let existing: Vec<&Path> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|p| p.is_file())
        .collect();
    if existing.is_empty() {
        return Ok(HashMap::new());
    }
    let list = tempfile::Builder::new()
        .prefix("wm-fileinfo-")
        .suffix(".txt")
        .tempfile()?
        .into_temp_path();
    let text: String = existing.iter().map(|p| format!("{}\r\n", p.display())).collect();
    std::fs::write(&list, text)?;
    let script = tempfile::Builder::new()
        .prefix("wm-fileinfo-")
        .suffix(".ps1")
        .tempfile()?
        .into_temp_path();
    std::fs::write(&script, SCRIPT)?;
    let out = crate::process::run_checked(crate::process::hidden_command("powershell").args([
        "-NoProfile".as_ref(),
        "-ExecutionPolicy".as_ref(),
        "Bypass".as_ref(),
        "-File".as_ref(),
        script.as_os_str(),
        list.as_os_str(),
    ]))?;
    let found = parse(&String::from_utf8_lossy(&out.stdout))?;
    if found.is_empty() {
        anyhow::bail!("no file details: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_output() {
        let json = r#"[{"Path":"C:\\a.exe","Status":"Valid","Signer":"Contoso","Company":"Contoso Ltd ","Description":"A","Version":"1.0"},
                       {"Path":"C:\\b.exe","Status":"NotSigned","Signer":"","Company":null,"Description":"","Version":""},
                       {"Path":"C:\\c.exe","Status":"HashMismatch","Signer":"X","Company":"","Description":"","Version":""}]"#;
        let m = parse(json).unwrap();
        assert_eq!(m[Path::new(r"C:\a.exe")].signature, Signature::Valid);
        assert_eq!(m[Path::new(r"C:\a.exe")].company, "Contoso Ltd");
        assert_eq!(m[Path::new(r"C:\b.exe")].signature, Signature::Unsigned);
        assert_eq!(m[Path::new(r"C:\c.exe")].signature, Signature::Untrusted);
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn describes_a_windows_file() {
        let notepad = PathBuf::from(std::env::var("WINDIR").unwrap()).join(r"System32\notepad.exe");
        let missing = PathBuf::from(r"C:\definitely\missing.exe");
        let m = describe(&[notepad.clone(), missing.clone()]).unwrap();
        let info = m.get(&notepad).unwrap_or_else(|| panic!("notepad not in {m:?}"));
        // Windows files are catalog-signed by Microsoft.
        assert_eq!(info.signature, Signature::Valid, "{info:?}");
        assert!(info.company.contains("Microsoft"), "{info:?}");
        assert!(!m.contains_key(&missing));
    }
}
