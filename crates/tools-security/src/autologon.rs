//! Automatic sign-in, done the way Sysinternals Autologon does it: the password is
//! kept as the LSA secret `DefaultPassword` (encrypted, readable only by SYSTEM and
//! administrators), never as plain text in the registry, and Winlogon's
//! `AutoAdminLogon` turns it on. The password is checked with `LogonUser` first, so a
//! typo can't leave Windows failing to sign in at every start.

use anyhow::{Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS};
use windows::Win32::Security::Authentication::Identity::{
    LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaClose, LsaNtStatusToWinError, LsaOpenPolicy,
    LsaStorePrivateData, POLICY_CREATE_SECRET,
};
use windows::Win32::Security::{LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT, LogonUserW};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use wm_core::{
    AppCtx, egui, registry,
    safety::UndoAction,
    tweaks::ExtraSetting,
    ui::{Message, show_message},
};
use zeroize::Zeroize;

const WINLOGON: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon";
const PASSWORDLESS: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\PasswordLess\Device";
const PASSWORDLESS_VALUE: &str = "DevicePasswordLessBuildVersion";
/// The scheduled / undo action that turns automatic sign-in off and deletes the secret.
pub const OFF_ACTION: &str = "autologon-off";

/// Whether Windows signs in automatically, and as whom.
pub fn current() -> Option<String> {
    let read = |n: &str| registry::read_raw(WINLOGON, n).map(|v| registry::decode_string(&v));
    if read("AutoAdminLogon").as_deref() != Some("1") {
        return None;
    }
    let user = read("DefaultUserName").unwrap_or_default();
    let domain = read("DefaultDomainName").unwrap_or_default();
    Some(if domain.is_empty() {
        user
    } else {
        format!(r"{domain}\{user}")
    })
}

/// "Only allow Windows Hello sign-in" blocks automatic sign-in.
pub fn hello_only() -> bool {
    registry::read_raw(PASSWORDLESS, PASSWORDLESS_VALUE).and_then(|v| registry::decode_dword(&v)) == Some(2)
}

/// A plain-text password left in the registry by older tools or guides.
pub fn plaintext_password() -> bool {
    registry::read_raw(WINLOGON, "DefaultPassword").is_some()
}

fn lsa_string(s: &mut [u16]) -> LSA_UNICODE_STRING {
    let bytes = (s.len() * 2) as u16;
    LSA_UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: PWSTR(s.as_mut_ptr()),
    }
}

fn nt_check(status: NTSTATUS, what: &str) -> Result<()> {
    if status.0 < 0 {
        let code = unsafe { LsaNtStatusToWinError(status) };
        let e = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code));
        bail!("{what}: {e}");
    }
    Ok(())
}

/// Stores (`Some`) or deletes (`None`) the `DefaultPassword` LSA secret.
fn store_secret(password: Option<&str>) -> Result<()> {
    unsafe {
        let attrs = LSA_OBJECT_ATTRIBUTES::default();
        let mut policy = LSA_HANDLE::default();
        nt_check(
            LsaOpenPolicy(None, &attrs, POLICY_CREATE_SECRET as u32, &mut policy),
            "opening the local security policy",
        )?;
        let mut name: Vec<u16> = "DefaultPassword".encode_utf16().collect();
        let key = lsa_string(&mut name);
        let mut data: Vec<u16> = password.unwrap_or_default().encode_utf16().collect();
        let value = lsa_string(&mut data);
        let status = LsaStorePrivateData(policy, &key, password.map(|_| &value as *const _));
        data.zeroize();
        let _ = LsaClose(policy);
        // Deleting a secret that doesn't exist is fine.
        if password.is_none() && status.0 == 0xC000_0034_u32 as i32 {
            return Ok(());
        }
        nt_check(status, "saving the password")
    }
}

/// Checks the user name and password by signing in once (no profile is loaded).
fn verify(user: &str, domain: &str, password: &str) -> Result<()> {
    let mut token = HANDLE::default();
    let domain = if domain.is_empty() || user.contains('@') {
        None
    } else {
        Some(HSTRING::from(domain))
    };
    let r = unsafe {
        LogonUserW(
            &HSTRING::from(user),
            domain.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            &HSTRING::from(password),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if !token.is_invalid() {
        let _ = unsafe { CloseHandle(token) };
    }
    r.map_err(|e| {
        anyhow::anyhow!("Windows didn't accept that user name and password ({e}). Nothing was changed.")
    })
}

pub fn enable(ctx: &AppCtx<'_>, user: &str, domain: &str, password: &str) -> Result<String> {
    if user.trim().is_empty() {
        bail!("Enter the account's user name.");
    }
    verify(user, domain, password)?;
    let mut snaps: Vec<_> = [
        "AutoAdminLogon",
        "DefaultUserName",
        "DefaultDomainName",
        "DefaultPassword",
    ]
    .iter()
    .map(|n| registry::snapshot(WINLOGON, n))
    .collect();
    let hello = hello_only();
    if hello {
        snaps.push(registry::snapshot(PASSWORDLESS, PASSWORDLESS_VALUE));
    }
    store_secret(Some(password))?;
    registry::write_raw(WINLOGON, "DefaultUserName", &registry::string(user))?;
    registry::write_raw(WINLOGON, "DefaultDomainName", &registry::string(domain))?;
    registry::delete_value(WINLOGON, "DefaultPassword")?;
    if hello {
        registry::write_raw(PASSWORDLESS, PASSWORDLESS_VALUE, &registry::dword(0))?;
    }
    registry::write_raw(WINLOGON, "AutoAdminLogon", &registry::string("1"))?;
    let exe = std::env::current_exe()?.display().to_string();
    ctx.journal.record(
        "systemsecurity",
        format!("Turned on automatic sign-in as {user}"),
        UndoAction::Sequence {
            steps: vec![
                UndoAction::RunCommand {
                    program: exe,
                    args: vec!["--run".into(), OFF_ACTION.into()],
                },
                UndoAction::RestoreRegValues { values: snaps },
            ],
        },
    )?;
    let mut msg = format!("Windows will sign in as {user} at startup.");
    if hello {
        msg += " \"Only allow Windows Hello sign-in\" was turned off, as it blocks automatic sign-in.";
    }
    Ok(msg)
}

/// Turns automatic sign-in off and deletes the stored password.
pub fn disable() -> Result<String> {
    registry::write_raw(WINLOGON, "AutoAdminLogon", &registry::string("0"))?;
    registry::delete_value(WINLOGON, "DefaultPassword")?;
    store_secret(None)?;
    Ok("Automatic sign-in is off and the stored password was deleted.".into())
}

pub struct AutoSignInSetting {
    user: String,
    domain: String,
    password: String,
    message: Message,
}

impl Default for AutoSignInSetting {
    fn default() -> Self {
        let read = |n: &str| registry::read_raw(WINLOGON, n).map(|v| registry::decode_string(&v));
        let user = std::env::var("USERNAME").unwrap_or_default();
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        Self {
            user: read("DefaultUserName").filter(|u| !u.is_empty()).unwrap_or(user),
            domain: read("DefaultDomainName")
                .filter(|d| !d.is_empty())
                .unwrap_or(domain),
            password: String::new(),
            message: None,
        }
    }
}

impl Drop for AutoSignInSetting {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl ExtraSetting for AutoSignInSetting {
    fn group(&self) -> &str {
        "Sign-in screen"
    }

    fn title(&self) -> &str {
        "Sign in automatically"
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        ui.label(self.title());
        ui.indent("autologon", |ui| {
            let now = current();
            ui.weak(match &now {
                Some(who) => format!("On: Windows signs in as {who} at startup."),
                None => "Off. Anyone who turns on the PC gets into the account, so use it only where that is safe. The password is stored encrypted, not in the registry.".to_owned(),
            });
            if plaintext_password() {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "A plain-text password is stored in the registry. Turning automatic sign-in on or off removes it.",
                );
            }
            let can = ctx.elevated;
            ui.add_enabled_ui(can, |ui| {
                egui::Grid::new("autologon-grid").num_columns(2).show(ui, |ui| {
                    ui.label("User name");
                    ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(200.0))
                        .on_hover_text("For a Microsoft account, you can also enter its email address.");
                    ui.end_row();
                    ui.label("Domain or PC name");
                    ui.add(egui::TextEdit::singleline(&mut self.domain).desired_width(200.0));
                    ui.end_row();
                    ui.label("Password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(200.0),
                    );
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.button(if now.is_some() { "Update" } else { "Turn on" }).clicked() {
                        self.message = Some(if ctx.config.dry_run {
                            Ok(ctx.i18n.tr("common-dry-run-skip"))
                        } else {
                            enable(ctx, self.user.trim(), self.domain.trim(), &self.password)
                                .map_err(|e| format!("{e:#}"))
                        });
                        self.password.zeroize();
                        self.password.clear();
                    }
                    if ui
                        .add_enabled(now.is_some() || plaintext_password(), egui::Button::new("Turn off"))
                        .clicked()
                    {
                        self.message = Some(if ctx.config.dry_run {
                            Ok(ctx.i18n.tr("common-dry-run-skip"))
                        } else {
                            disable()
                                .and_then(|m| {
                                    ctx.journal.record(
                                        "systemsecurity",
                                        "Turned off automatic sign-in",
                                        UndoAction::None,
                                    )?;
                                    Ok(m)
                                })
                                .map_err(|e| format!("{e:#}"))
                        });
                    }
                    if !can {
                        ui.weak("· admin");
                    }
                });
            });
            show_message(ui, &self.message);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_state_does_not_fail() {
        let _ = current();
        let _ = hello_only();
        let _ = plaintext_password();
    }

    #[test]
    fn unknown_accounts_are_rejected() {
        // A made-up account, so no real account collects a failed sign-in (lockout).
        let pc = std::env::var("COMPUTERNAME").unwrap_or_default();
        assert!(verify("wm-no-such-user-7f3a", &pc, "x").is_err());
    }
}
