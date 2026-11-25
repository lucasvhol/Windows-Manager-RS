//! Scheduled tasks of this app: they live in Task Scheduler's `\Win Manager RS\`
//! folder and run this executable with `--run <action>`, so they work while the app
//! is closed.

use anyhow::{Context, Result};

use crate::{
    AppCtx,
    process::{hidden_command, run_checked},
    safety::UndoAction,
};

pub const FOLDER: &str = r"\Win Manager RS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Daily,
    Weekly(usize),
    AtLogon,
}

pub const DAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

/// `schtasks /create` arguments for a new task.
pub fn create_args(
    name: &str,
    exe: &str,
    action: &str,
    schedule: Schedule,
    time: &str,
    highest: bool,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "/create".into(),
        "/tn".into(),
        format!(r"{FOLDER}\{name}"),
        "/tr".into(),
        format!("\"{exe}\" --run {action}"),
    ];
    match schedule {
        Schedule::Daily => a.extend(["/sc".into(), "DAILY".into(), "/st".into(), time.into()]),
        Schedule::Weekly(d) => a.extend([
            "/sc".into(),
            "WEEKLY".into(),
            "/d".into(),
            DAYS[d % 7].into(),
            "/st".into(),
            time.into(),
        ]),
        Schedule::AtLogon => a.extend(["/sc".into(), "ONLOGON".into()]),
    }
    if highest {
        a.extend(["/rl".into(), "HIGHEST".into()]);
    }
    a.push("/f".into());
    a
}

pub fn valid_time(t: &str) -> bool {
    let Some((h, m)) = t.split_once(':') else {
        return false;
    };
    h.len() == 2
        && m.len() == 2
        && h.parse::<u8>().is_ok_and(|h| h < 24)
        && m.parse::<u8>().is_ok_and(|m| m < 60)
}

/// Creates (or replaces) a task in our folder that runs `--run <action>`, journaled
/// with an undo that deletes it. Inputs are assumed valid.
pub fn schedule(
    ctx: &AppCtx<'_>,
    name: &str,
    action: &str,
    when: Schedule,
    time: &str,
    highest: bool,
) -> Result<String> {
    if ctx.config.dry_run {
        return Ok(ctx.i18n.tr("common-dry-run-skip"));
    }
    let exe = std::env::current_exe()?.display().to_string();
    let args = create_args(name, &exe, action, when, time, highest);
    run_checked(hidden_command("schtasks").args(&args)).context("creating the task")?;
    let path = format!(r"{FOLDER}\{name}");
    ctx.journal.record(
        "mytask",
        format!("Scheduled \"{name}\" ({action})"),
        UndoAction::RunCommand {
            program: "schtasks".into(),
            args: vec!["/delete".into(), "/tn".into(), path, "/f".into()],
        },
    )?;
    Ok(format!("Scheduled \"{name}\"."))
}

/// Deletes one of our tasks if it exists (journaled only when one was removed;
/// the undo cannot recreate it, so the entry is for the record).
pub fn unschedule(ctx: &AppCtx<'_>, name: &str) -> Result<bool> {
    if ctx.config.dry_run {
        return Ok(false);
    }
    let path = format!(r"{FOLDER}\{name}");
    let exists = hidden_command("schtasks")
        .args(["/query", "/tn", &path])
        .output()
        .is_ok_and(|o| o.status.success());
    if !exists {
        return Ok(false);
    }
    run_checked(hidden_command("schtasks").args(["/delete", "/tn", &path, "/f"]))
        .context("deleting the task")?;
    ctx.journal
        .record("mytask", format!("Removed the task \"{name}\""), UndoAction::None)?;
    Ok(true)
}
