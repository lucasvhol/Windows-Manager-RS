//! My Task's live rules: "when this happens, do that", checked by the background
//! monitor (`--monitor`) once a second. This module is the model and the decision
//! logic only (pure, so it is tested with made-up samples); the monitor gathers the
//! samples and carries out the actions.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

/// Saved with [`crate::tool_settings`] under this id; the monitor reloads it when it changes.
pub const SETTINGS_ID: &str = "mytask-rules";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Trigger {
    CpuAbove {
        percent: f32,
        secs: u32,
    },
    CpuBelow {
        percent: f32,
        secs: u32,
    },
    /// A program (image name, e.g. `game.exe`) starts.
    ProcessStarts {
        name: String,
    },
    ProcessExits {
        name: String,
    },
    /// A running program reads and writes less than `kbps` for `secs` (e.g. a download
    /// or installer has finished).
    ProcessIoBelow {
        name: String,
        kbps: f64,
        secs: u32,
    },
    /// No keyboard or mouse input for this long.
    Idle {
        minutes: u32,
    },
    NetAbove {
        kbps: f64,
        secs: u32,
    },
    NetBelow {
        kbps: f64,
        secs: u32,
    },
    /// After this many minutes of continuous use (a 5-minute break resets it).
    TakeRest {
        minutes: u32,
    },
    UsbInserted,
    UsbRemoved,
    Locked,
    Unlocked,
}

impl Trigger {
    pub fn describe(&self) -> String {
        match self {
            Trigger::CpuAbove { percent, secs } => format!("CPU above {percent}% for {secs} s"),
            Trigger::CpuBelow { percent, secs } => format!("CPU below {percent}% for {secs} s"),
            Trigger::ProcessStarts { name } => format!("{name} starts"),
            Trigger::ProcessExits { name } => format!("{name} exits"),
            Trigger::ProcessIoBelow { name, kbps, secs } => {
                format!("{name} reads and writes less than {kbps} KB/s for {secs} s")
            }
            Trigger::Idle { minutes } => format!("No input for {minutes} min"),
            Trigger::NetAbove { kbps, secs } => format!("Network above {kbps} KB/s for {secs} s"),
            Trigger::NetBelow { kbps, secs } => format!("Network below {kbps} KB/s for {secs} s"),
            Trigger::TakeRest { minutes } => format!("After {minutes} min of continuous use"),
            Trigger::UsbInserted => "A USB drive is plugged in".into(),
            Trigger::UsbRemoved => "A USB drive is removed".into(),
            Trigger::Locked => "The PC is locked".into(),
            Trigger::Unlocked => "The PC is unlocked".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Shutdown,
    Restart,
    Sleep,
    Hibernate,
    SignOut,
    Lock,
    MonitorOff,
    Mute,
    Run {
        program: String,
        args: String,
    },
    Remind {
        text: String,
    },
    /// One of the maintenance actions (`--run <id>`), e.g. `junk-clean`.
    Maintenance {
        id: String,
    },
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Shutdown => "shut down".into(),
            Action::Restart => "restart".into(),
            Action::Sleep => "sleep".into(),
            Action::Hibernate => "hibernate".into(),
            Action::SignOut => "sign out".into(),
            Action::Lock => "lock the PC".into(),
            Action::MonitorOff => "turn the monitor off".into(),
            Action::Mute => "mute the sound".into(),
            Action::Run { program, .. } => format!("run {program}"),
            Action::Remind { text } => format!("remind: \"{text}\""),
            Action::Maintenance { id } => format!("run {id}"),
        }
    }

    /// Ends the session or turns the PC off, so a countdown makes sense.
    pub fn is_power(&self) -> bool {
        matches!(
            self,
            Action::Shutdown | Action::Restart | Action::Sleep | Action::Hibernate | Action::SignOut
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub action: Action,
    /// Fire every time the trigger happens; otherwise the rule turns itself off after once.
    pub repeat: bool,
    /// Seconds of warning (cancellable) before power actions.
    pub countdown_secs: u32,
    /// Keep the PC from sleeping while this rule waits.
    pub keep_awake: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    pub rules: Vec<Rule>,
}

/// What the monitor measured in one tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// Seconds since the previous sample.
    pub dt: f64,
    pub cpu: f32,
    /// Lowercase image names of running processes.
    pub processes: BTreeSet<String>,
    /// Lowercase image name → disk bytes per second (summed over its processes).
    pub process_io: HashMap<String, f64>,
    pub idle_secs: u64,
    /// Bytes per second, received + sent.
    pub net_bps: f64,
    /// Letters of removable drives.
    pub usb: BTreeSet<char>,
    pub locked: bool,
}

/// Per-rule memory between ticks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleState {
    held: f64,
    /// Held conditions must turn false again before firing again.
    armed_after_fire: bool,
    used: f64,
}

fn held(state: &mut RuleState, condition: bool, dt: f64, secs: f64) -> bool {
    if !condition {
        state.held = 0.0;
        state.armed_after_fire = false;
        return false;
    }
    if state.armed_after_fire {
        return false;
    }
    state.held += dt;
    if state.held >= secs {
        state.held = 0.0;
        state.armed_after_fire = true;
        return true;
    }
    false
}

/// Whether the rule's trigger happened between `prev` and `now`.
pub fn step(trigger: &Trigger, state: &mut RuleState, prev: Option<&Sample>, now: &Sample) -> bool {
    let dt = now.dt;
    match trigger {
        Trigger::CpuAbove { percent, secs } => held(state, now.cpu > *percent, dt, *secs as f64),
        Trigger::CpuBelow { percent, secs } => held(state, now.cpu < *percent, dt, *secs as f64),
        Trigger::ProcessStarts { name } => {
            let n = name.to_lowercase();
            prev.is_some_and(|p| !p.processes.contains(&n)) && now.processes.contains(&n)
        }
        Trigger::ProcessExits { name } => {
            let n = name.to_lowercase();
            prev.is_some_and(|p| p.processes.contains(&n)) && !now.processes.contains(&n)
        }
        Trigger::ProcessIoBelow { name, kbps, secs } => {
            let n = name.to_lowercase();
            let quiet =
                now.processes.contains(&n) && now.process_io.get(&n).copied().unwrap_or(0.0) < kbps * 1024.0;
            held(state, quiet, dt, *secs as f64)
        }
        Trigger::Idle { minutes } => {
            let idle = now.idle_secs >= *minutes as u64 * 60;
            // Fires once per idle stretch.
            held(state, idle, dt, 0.0)
        }
        Trigger::NetAbove { kbps, secs } => held(state, now.net_bps > kbps * 1024.0, dt, *secs as f64),
        Trigger::NetBelow { kbps, secs } => held(state, now.net_bps < kbps * 1024.0, dt, *secs as f64),
        Trigger::TakeRest { minutes } => {
            if now.idle_secs >= 300 {
                state.used = 0.0;
                return false;
            }
            state.used += dt;
            if state.used >= *minutes as f64 * 60.0 {
                state.used = 0.0;
                return true;
            }
            false
        }
        Trigger::UsbInserted => prev.is_some_and(|p| now.usb.iter().any(|d| !p.usb.contains(d))),
        Trigger::UsbRemoved => prev.is_some_and(|p| p.usb.iter().any(|d| !now.usb.contains(d))),
        Trigger::Locked => prev.is_some_and(|p| !p.locked) && now.locked,
        Trigger::Unlocked => prev.is_some_and(|p| p.locked) && !now.locked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(f: impl FnOnce(&mut Sample)) -> Sample {
        let mut x = Sample {
            dt: 1.0,
            ..Sample::default()
        };
        f(&mut x);
        x
    }

    fn run(trigger: &Trigger, samples: &[Sample]) -> Vec<bool> {
        let mut st = RuleState::default();
        let mut prev: Option<&Sample> = None;
        samples
            .iter()
            .map(|now| {
                let fired = step(trigger, &mut st, prev, now);
                prev = Some(now);
                fired
            })
            .collect()
    }

    #[test]
    fn held_conditions_fire_once_per_stretch() {
        let t = Trigger::CpuAbove {
            percent: 80.0,
            secs: 3,
        };
        let hot = s(|x| x.cpu = 95.0);
        let cool = s(|x| x.cpu = 10.0);
        let r = run(
            &t,
            &[
                hot.clone(),
                hot.clone(),
                hot.clone(),
                hot.clone(),
                hot.clone(),
                cool,
                hot.clone(),
                hot.clone(),
                hot,
            ],
        );
        assert_eq!(r, [false, false, true, false, false, false, false, false, true]);
    }

    #[test]
    fn process_and_io_triggers() {
        let with = s(|x| {
            x.processes.insert("setup.exe".into());
            x.process_io.insert("setup.exe".into(), 50_000.0);
        });
        let quiet = s(|x| {
            x.processes.insert("setup.exe".into());
        });
        let without = s(|_| {});
        let starts = Trigger::ProcessStarts {
            name: "Setup.exe".into(),
        };
        assert_eq!(
            run(&starts, &[without.clone(), with.clone(), with.clone()]),
            [false, true, false]
        );
        let exits = Trigger::ProcessExits {
            name: "setup.exe".into(),
        };
        assert_eq!(run(&exits, &[with.clone(), without.clone()]), [false, true]);
        let io = Trigger::ProcessIoBelow {
            name: "setup.exe".into(),
            kbps: 10.0,
            secs: 2,
        };
        assert_eq!(
            run(&io, &[with, quiet.clone(), quiet.clone(), without]),
            [false, false, true, false]
        );
    }

    #[test]
    fn idle_rest_usb_and_lock() {
        let idle = Trigger::Idle { minutes: 1 };
        let a = s(|x| x.idle_secs = 30);
        let b = s(|x| x.idle_secs = 61);
        assert_eq!(
            run(&idle, &[a.clone(), b.clone(), b.clone(), a, b]),
            [false, true, false, false, true]
        );

        let rest = Trigger::TakeRest { minutes: 1 };
        let busy = Sample {
            dt: 30.0,
            ..Sample::default()
        };
        let away = Sample {
            dt: 30.0,
            idle_secs: 400,
            ..Sample::default()
        };
        assert_eq!(
            run(&rest, &[busy.clone(), away, busy.clone(), busy]),
            [false, false, false, true]
        );

        let plugged = s(|x| {
            x.usb.insert('E');
        });
        let none = s(|_| {});
        assert_eq!(
            run(&Trigger::UsbInserted, &[none.clone(), plugged.clone()]),
            [false, true]
        );
        assert_eq!(run(&Trigger::UsbRemoved, &[plugged, none]), [false, true]);

        let locked = s(|x| x.locked = true);
        let open = s(|_| {});
        assert_eq!(
            run(&Trigger::Locked, &[open.clone(), locked.clone()]),
            [false, true]
        );
        assert_eq!(run(&Trigger::Unlocked, &[locked, open]), [false, true]);
    }

    #[test]
    fn network_thresholds_and_texts() {
        let t = Trigger::NetBelow { kbps: 100.0, secs: 2 };
        let slow = s(|x| x.net_bps = 10.0 * 1024.0);
        let fast = s(|x| x.net_bps = 900.0 * 1024.0);
        assert_eq!(run(&t, &[fast, slow.clone(), slow]), [false, false, true]);
        assert!(Action::Shutdown.is_power() && !Action::Lock.is_power());
        assert_eq!(Trigger::UsbInserted.describe(), "A USB drive is plugged in");
        let rules = Rules {
            rules: vec![Rule {
                name: "x".into(),
                enabled: true,
                trigger: t,
                action: Action::Remind { text: "hi".into() },
                repeat: true,
                countdown_secs: 0,
                keep_awake: false,
            }],
        };
        let json = serde_json::to_string(&rules).unwrap();
        assert_eq!(serde_json::from_str::<Rules>(&json).unwrap(), rules);
    }
}
