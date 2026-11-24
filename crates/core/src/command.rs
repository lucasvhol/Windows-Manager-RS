//! Runs console tools (sfc, DISM, chkdsk…) and streams their output line by line.
//! SFC writes UTF-16LE to pipes while most tools write 8-bit text; both are handled.

use std::io::Read;
use std::process::Stdio;

use crate::process::hidden_command;
use anyhow::{Context, Result};

/// Incremental decoder that splits output into lines. `\r` (progress redraws) produces
/// a status update; `\n` completes a line.
#[derive(Default)]
pub struct LineSplitter {
    utf16: Option<bool>,
    pending: Vec<u8>,
    current: String,
    /// A `\r` was seen; the next character decides between CRLF and a progress redraw.
    after_cr: bool,
}

pub enum Event {
    Status(String),
    Line(String),
}

impl LineSplitter {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let utf16 = *self.utf16.get_or_insert_with(|| {
            // Mostly-zero odd bytes means UTF-16LE text.
            let odd = self
                .pending
                .iter()
                .skip(1)
                .step_by(2)
                .take(64)
                .collect::<Vec<_>>();
            !odd.is_empty() && odd.iter().filter(|b| ***b == 0).count() * 10 >= odd.len() * 7
        });
        if utf16 {
            let usable = self.pending.len() & !1;
            let units: Vec<u16> = self.pending[..usable]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            self.pending.drain(..usable);
            String::from_utf16_lossy(&units).replace('\u{feff}', "")
        } else {
            let s = String::from_utf8_lossy(&self.pending).into_owned();
            self.pending.clear();
            s
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        let text = self.decode(bytes);
        let mut events = Vec::new();
        for c in text.chars() {
            if std::mem::take(&mut self.after_cr) && c != '\n' {
                // A lone CR: the tool is redrawing a progress line.
                let s = std::mem::take(&mut self.current).trim().to_owned();
                if !s.is_empty() {
                    events.push(Event::Status(s));
                }
            }
            match c {
                '\n' => {
                    let line = std::mem::take(&mut self.current);
                    events.push(Event::Line(line.trim_end().to_owned()));
                }
                '\r' => self.after_cr = true,
                _ => self.current.push(c),
            }
        }
        events
    }

    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.current);
        (!rest.trim().is_empty()).then(|| rest.trim_end().to_owned())
    }
}

/// Runs a command, calling `on_status` for progress text; returns (exit code, output lines).
pub fn run_streaming(
    program: &str,
    args: &[&str],
    mut on_status: impl FnMut(&str),
) -> Result<(i32, Vec<String>)> {
    let mut child = hidden_command(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let err_thread = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stderr.read_to_end(&mut s);
        s
    });
    let mut stdout = child.stdout.take().context("stdout")?;
    let mut splitter = LineSplitter::default();
    let mut lines = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for e in splitter.push(&buf[..n]) {
            match e {
                Event::Status(s) => on_status(&s),
                Event::Line(l) => {
                    if !l.trim().is_empty() {
                        on_status(&l);
                    }
                    lines.push(l);
                }
            }
        }
    }
    lines.extend(splitter.finish());
    let err = err_thread.join().unwrap_or_default();
    lines.extend(
        String::from_utf8_lossy(&err)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned),
    );
    let code = child.wait()?.code().unwrap_or(-1);
    // Collapse runs of blank lines.
    lines.dedup_by(|a, b| a.trim().is_empty() && b.trim().is_empty());
    Ok((code, lines))
}

/// The last percentage in a progress line (`[====  45.2% ]` → 45.2), for progress bars.
pub fn percent_in(s: &str) -> Option<f32> {
    let end = s.rfind('%')?;
    let head = s[..end].trim_end();
    let start = head
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .map_or(0, |i| i + 1);
    head[start..].replace(',', ".").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages() {
        assert_eq!(percent_in("[====  45.2% ]"), Some(45.2));
        assert_eq!(percent_in("[==========100.0%==========]"), Some(100.0));
        assert_eq!(percent_in("Verification 7% complete."), Some(7.0));
        assert_eq!(percent_in("Fortschritt 12,5 %"), Some(12.5));
        assert_eq!(percent_in("no number"), None);
    }

    fn lines_of(events: Vec<Event>) -> Vec<String> {
        events
            .into_iter()
            .filter_map(|e| if let Event::Line(l) = e { Some(l) } else { None })
            .collect()
    }

    #[test]
    fn utf8_with_progress_redraws() {
        let mut s = LineSplitter::default();
        let ev = s.push(b"Start\r\n[==  10% ]\r[=== 20% ]\rDone\r\nTail");
        let statuses: Vec<String> = ev
            .iter()
            .filter_map(|e| {
                if let Event::Status(t) = e {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(statuses.contains(&"[=== 20% ]".to_owned()));
        assert_eq!(s.finish().as_deref(), Some("Tail"));
    }

    #[test]
    fn utf16_split_across_chunks() {
        let text = "Beginning verification phase.\r\nVerification 100% complete.\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut s = LineSplitter::default();
        let (a, b) = bytes.split_at(33); // odd split inside a code unit
        let mut all = lines_of(s.push(a));
        all.extend(lines_of(s.push(b)));
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(
            all.iter().any(|l| l == "Beginning verification phase."),
            "{all:?}"
        );
        assert!(all.iter().any(|l| l == "Verification 100% complete."), "{all:?}");
    }

    #[test]
    fn runs_a_real_command() {
        let (code, lines) = run_streaming("cmd", &["/c", "echo hello&& echo world"], |_| {}).unwrap();
        assert_eq!(code, 0);
        assert!(
            lines.iter().any(|l| l == "hello") && lines.iter().any(|l| l == "world"),
            "{lines:?}"
        );
    }
}
