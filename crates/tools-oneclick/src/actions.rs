//! Maintenance actions that run without a window: used by the 1-Click tools and by
//! scheduled tasks (`win-manager.exe --run <action>`).

use anyhow::{Result, bail};
use wm_core::{
    AppCtx,
    process::{hidden_command, run_checked},
    safety::{UndoAction, restore_point},
    ui::format_bytes,
};
use wm_tools_cleaner::junk::{self, Kind, Rule, RuleScan};
use wm_tools_security::privacy::{self, Trace};

/// (id, description, needs administrator)
pub const ACTIONS: &[(&str, &str, bool)] = &[
    (
        "junk-clean",
        "Clean junk files (the default selection of the Junk File Cleaner)",
        false,
    ),
    (
        "oneclick-clean",
        "Run the 1-Click Cleaner with its saved selection",
        false,
    ),
    ("empty-recycle-bin", "Empty the Recycle Bin", false),
    (
        "desktop-unused",
        "Move unused desktop items aside (Desktop Cleaner options)",
        false,
    ),
    (
        "clear-traces",
        "Clear Run history, address bar history and recent files",
        false,
    ),
    (
        wm_tools_custom::custom_extras::THEME_ACTION,
        "Switch light or dark mode by the theme schedule (Custom System)",
        false,
    ),
    ("flush-dns", "Flush the DNS cache", true),
    ("restore-point", "Create a System Restore point", true),
    (
        wm_tools_security::autologon::OFF_ACTION,
        "Turn off automatic sign-in and delete its stored password",
        true,
    ),
];

/// Junk rules selected by default that the current process may clean.
pub fn default_junk_rules(elevated: bool) -> Vec<Rule> {
    junk::parse_rules(junk::RULES)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.default && r.kind == Kind::Files && (!r.admin || elevated))
        .collect()
}

pub fn scan_junk(rules: &[Rule]) -> Vec<(Rule, RuleScan)> {
    let opts = junk::Options::saved();
    rules
        .iter()
        .map(|r| (r.clone(), junk::scan_rule(r, &opts)))
        .collect()
}

pub fn clean_junk(ctx: &AppCtx<'_>, scans: &[(Rule, RuleScan)]) -> Result<String> {
    let report = junk::clean(scans, false);
    let desc = format!(
        "Junk cleanup: {} files deleted, {} freed",
        report.removed,
        format_bytes(report.freed)
    );
    ctx.journal.record("oneclick", desc.clone(), UndoAction::None)?;
    Ok(if report.failed > 0 {
        format!("{desc} ({} in use, skipped)", report.failed)
    } else {
        desc
    })
}

pub fn run(name: &str, ctx: &AppCtx<'_>) -> Result<String> {
    let Some((_, _, admin)) = ACTIONS.iter().find(|(id, _, _)| *id == name) else {
        bail!(
            "unknown action \"{name}\" (known: {})",
            ACTIONS.iter().map(|a| a.0).collect::<Vec<_>>().join(", ")
        );
    };
    if *admin && !ctx.elevated {
        bail!("\"{name}\" needs administrator rights");
    }
    if ctx.config.dry_run {
        return Ok(format!("dry run: {name} skipped"));
    }
    match name {
        "junk-clean" => clean_junk(ctx, &scan_junk(&default_junk_rules(ctx.elevated))),
        "oneclick-clean" => crate::oneclick_cleaner::run_saved(ctx),
        "empty-recycle-bin" => {
            let rule = junk::parse_rules(junk::RULES)?
                .into_iter()
                .find(|r| r.kind == Kind::RecycleBin);
            let Some(rule) = rule else {
                bail!("recycle bin rule missing")
            };
            let scan = junk::scan_rule(&rule, &junk::Options::default());
            let freed = scan.bytes;
            junk::clean(&[(rule, scan)], false);
            ctx.journal.record(
                "oneclick",
                format!("Emptied the Recycle Bin ({})", format_bytes(freed)),
                UndoAction::None,
            )?;
            Ok(format!("Emptied the Recycle Bin ({}).", format_bytes(freed)))
        }
        "clear-traces" => {
            let mut n = 0;
            for t in [Trace::RunHistory, Trace::TypedPaths, Trace::RecentFiles] {
                n += privacy::clear(ctx, t)?;
            }
            Ok(format!("Cleared {n} activity traces."))
        }
        "desktop-unused" => wm_tools_cleaner::desktop::move_unused(ctx),
        "theme-auto" => wm_tools_custom::custom_extras::auto_apply(),
        wm_tools_security::autologon::OFF_ACTION => wm_tools_security::autologon::disable(),
        "flush-dns" => {
            run_checked(hidden_command("ipconfig").arg("/flushdns"))?;
            ctx.journal
                .record("oneclick", "Flushed the DNS cache", UndoAction::None)?;
            Ok("Flushed the DNS cache.".into())
        }
        "restore-point" => {
            restore_point::create("Win Manager RS scheduled checkpoint")?;
            ctx.journal
                .record("oneclick", "Created a restore point", UndoAction::None)?;
            Ok("Created a restore point.".into())
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_admin_actions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config::default();
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        assert!(run("format-c", &ctx).is_err());
        assert!(run("restore-point", &ctx).is_err(), "needs admin");
        assert!(
            run("flush-dns", &ctx).is_err(),
            "flushing DNS needs admin on current Windows"
        );
        assert!(journal.entries().is_empty(), "refused actions record nothing");
    }

    #[test]
    fn dry_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            wm_core::safety::ChangeJournal::open(&dir.path().join("j.json"), &dir.path().join("b")).unwrap();
        let mut config = wm_core::config::Config {
            dry_run: true,
            ..Default::default()
        };
        let i18n = wm_core::i18n::I18n::new("en-US");
        let ctx = AppCtx {
            config: &mut config,
            journal: &journal,
            i18n: &i18n,
            elevated: false,
        };
        assert!(run("junk-clean", &ctx).unwrap().starts_with("dry run"));
        assert!(journal.entries().is_empty());
    }

    #[test]
    fn default_rules_exclude_admin_and_recycle_bin() {
        let rules = default_junk_rules(false);
        assert!(!rules.is_empty());
        assert!(
            rules
                .iter()
                .all(|r| !r.admin && r.kind == Kind::Files && r.default)
        );
    }
}
