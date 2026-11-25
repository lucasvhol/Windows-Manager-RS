//! URL Manager: edit the hosts file to block or redirect web addresses.
//! Unrecognised lines (comments, blank lines) are preserved exactly. Saving backs up
//! the previous file (journaled undo) and flushes the DNS cache.

use std::net::IpAddr;
use std::path::PathBuf;

use crate::favorites;
use anyhow::{Context, Result, bail};
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    process::{hidden_command, run_checked},
    task::BackgroundTask,
    ui::{Message, show_message},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Entry {
        enabled: bool,
        ip: String,
        hosts: Vec<String>,
        comment: Option<String>,
    },
    Other(String),
}

fn parse_entry(text: &str, enabled: bool) -> Option<Line> {
    let (body, comment) = match text.split_once('#') {
        Some((b, c)) => (b, Some(c.trim().to_owned()).filter(|c| !c.is_empty())),
        None => (text, None),
    };
    let mut tokens = body.split_whitespace();
    let ip = tokens.next()?;
    ip.parse::<IpAddr>().ok()?;
    let hosts: Vec<String> = tokens.map(str::to_owned).collect();
    (!hosts.is_empty()).then(|| Line::Entry {
        enabled,
        ip: ip.to_owned(),
        hosts,
        comment,
    })
}

pub fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .map(|raw| {
            let t = raw.trim();
            let parsed = match t.strip_prefix('#') {
                Some(rest) => parse_entry(rest.trim(), false),
                None => parse_entry(t, true),
            };
            parsed.unwrap_or_else(|| Line::Other(raw.to_owned()))
        })
        .collect()
}

pub fn render(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        match l {
            Line::Other(s) => out.push_str(s),
            Line::Entry {
                enabled,
                ip,
                hosts,
                comment,
            } => {
                if !enabled {
                    out.push_str("# ");
                }
                out.push_str(&format!("{ip}\t{}", hosts.join(" ")));
                if let Some(c) = comment {
                    out.push_str(&format!("\t# {c}"));
                }
            }
        }
        out.push_str("\r\n");
    }
    out
}

pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && !label.starts_with('-')
        })
}

pub fn hosts_path() -> PathBuf {
    PathBuf::from(paths::expand_env(r"%WINDIR%\System32\drivers\etc\hosts"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Hosts,
    Favorites,
}

pub struct UrlManager {
    tab: Tab,
    marks: Option<Vec<favorites::Bookmark>>,
    status: Vec<Option<favorites::Status>>,
    check: Option<BackgroundTask<Vec<favorites::Status>>>,
    problems_only: bool,
    lines: Option<Result<Vec<Line>, String>>,
    dirty: bool,
    host: String,
    ip: String,
    filter: String,
    message: Message,
}

impl Default for UrlManager {
    fn default() -> Self {
        Self {
            tab: Tab::Hosts,
            marks: None,
            status: Vec::new(),
            check: None,
            problems_only: false,
            lines: None,
            dirty: false,
            host: String::new(),
            ip: "0.0.0.0".into(),
            filter: String::new(),
            message: None,
        }
    }
}

impl UrlManager {
    fn save(&mut self, ctx: &AppCtx<'_>) -> Result<String> {
        let Some(Ok(lines)) = &self.lines else {
            bail!("nothing loaded")
        };
        if ctx.config.dry_run {
            return Ok(ctx.i18n.tr("common-dry-run-skip"));
        }
        let path = hosts_path();
        let undo = ctx.journal.backup_file(&path)?;
        std::fs::write(&path, render(lines))
            .context("writing the hosts file (administrator rights required)")?;
        ctx.journal.record("hosts", "Edited the hosts file", undo)?;
        let _ = run_checked(hidden_command("ipconfig").arg("/flushdns"));
        self.dirty = false;
        Ok("Saved. The DNS cache was flushed; browsers may need a restart to notice.".into())
    }
}

impl Tool for UrlManager {
    fn id(&self) -> &'static str {
        "hosts"
    }

    fn category(&self) -> Category {
        Category::Network
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Hosts, "Block or redirect sites");
            ui.selectable_value(&mut self.tab, Tab::Favorites, "Check favorites");
        });
        ui.separator();
        match self.tab {
            Tab::Hosts => self.hosts_ui(ui, ctx),
            Tab::Favorites => self.favorites_ui(ui, ctx),
        }
    }
}

impl UrlManager {
    fn favorites_ui(&mut self, ui: &mut egui::Ui, ctx: &AppCtx<'_>) {
        if let Some(t) = &mut self.check {
            t.poll();
            if let Some(r) = t.take_result() {
                self.check = None;
                match r {
                    Ok(s) => self.status = s.into_iter().map(Some).collect(),
                    Err(e) => self.message = Some(Err(e)),
                }
            }
        }
        let marks = self.marks.get_or_insert_with(favorites::all_bookmarks).clone();
        ui.label("Finds bookmarks in Edge, Chrome, Brave and the Windows Favorites folder whose sites have moved or no longer exist.");
        ui.horizontal(|ui| {
            let busy = self.check.is_some();
            if ui
                .add_enabled(
                    !busy && !marks.is_empty(),
                    egui::Button::new(format!("Check {} links", marks.len())),
                )
                .on_hover_text("Visits every web link once (16 at a time).")
                .clicked()
            {
                let list = marks.clone();
                self.status = vec![None; list.len()];
                self.check = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
                    Ok(favorites::check_all(
                        &list,
                        &|d, n| p.report(d as f32 / n.max(1) as f32, format!("{d} of {n}")),
                        &|| p.is_cancelled(),
                    ))
                }));
            }
            if let Some(t) = &self.check {
                ui.add(
                    egui::ProgressBar::new(t.progress)
                        .desired_width(200.0)
                        .text(t.status.clone()),
                );
                if ui.button(ctx.i18n.tr("common-cancel")).clicked() {
                    t.cancel();
                }
            }
            if ui
                .add_enabled(!busy, egui::Button::new(ctx.i18n.tr("common-refresh")))
                .clicked()
            {
                self.marks = None;
                self.status.clear();
            }
            ui.checkbox(&mut self.problems_only, "Only problems");
            let done: Vec<favorites::Status> = self.status.iter().flatten().cloned().collect();
            if done.len() == marks.len()
                && !marks.is_empty()
                && ui.button("Save report…").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("favorites-check.csv")
                    .save_file()
            {
                self.message = Some(
                    std::fs::write(&p, favorites::to_csv(&marks, &done))
                        .map(|()| format!("Saved {}.", p.display()))
                        .map_err(|e| format!("{e:#}")),
                );
            }
        });
        let problems = self.status.iter().flatten().filter(|s| s.is_problem()).count();
        if !self.status.is_empty() {
            ui.weak(format!("{problems} broken or unreachable so far."));
        }
        show_message(ui, &self.message);
        let mut recycle = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("fav-grid")
                .striped(true)
                .num_columns(4)
                .show(ui, |ui| {
                    for h in ["Where", "Title", "Link", "Status"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for (i, m) in marks.iter().enumerate() {
                        let st = self.status.get(i).cloned().flatten();
                        if self.problems_only && !st.as_ref().is_some_and(|s| s.is_problem()) {
                            continue;
                        }
                        ui.weak(if m.folder.is_empty() {
                            m.browser.clone()
                        } else {
                            format!("{} · {}", m.browser, m.folder)
                        });
                        ui.label(m.title.chars().take(50).collect::<String>());
                        ui.hyperlink_to(m.url.chars().take(60).collect::<String>(), &m.url);
                        ui.horizontal(|ui| {
                            match &st {
                                None => {
                                    ui.weak("—");
                                }
                                Some(s) if s.is_problem() => {
                                    ui.colored_label(ui.visuals().error_fg_color, s.label());
                                }
                                Some(s) => {
                                    ui.label(s.label());
                                }
                            }
                            if let favorites::Source::UrlFile(p) = &m.source
                                && st.as_ref().is_some_and(|s| s.is_problem())
                                && ui
                                    .small_button("Delete")
                                    .on_hover_text("Moves the shortcut to the Recycle Bin")
                                    .clicked()
                            {
                                recycle = Some(p.clone());
                            }
                        });
                        ui.end_row();
                    }
                });
        });
        if let Some(p) = recycle {
            self.message = Some(
                ctx.journal
                    .recycle(
                        "hosts",
                        format!("Removed the broken favorite {}", p.display()),
                        vec![p],
                    )
                    .map(|_| "Moved to the Recycle Bin.".into())
                    .map_err(|e| format!("{e:#}")),
            );
            self.marks = None;
            self.status.clear();
        }
        if marks
            .iter()
            .any(|m| matches!(m.source, favorites::Source::Browser(_)))
        {
            ui.weak("To remove a browser bookmark, open the link's row in the browser's own bookmark manager; editing its file while it runs would be undone.");
        }
    }

    fn hosts_ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.lines.is_none() {
            self.lines = Some(
                std::fs::read_to_string(hosts_path())
                    .map(|t| parse(&t))
                    .map_err(|e| e.to_string()),
            );
            self.dirty = false;
        }
        if !ctx.elevated {
            ui.weak("Saving changes to the hosts file needs the app to run as administrator.");
        }
        ui.label("Block a website by pointing it to 0.0.0.0, or redirect a name to another address.");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.host)
                    .hint_text("example.com")
                    .desired_width(220.0),
            );
            ui.label("→");
            ui.add(egui::TextEdit::singleline(&mut self.ip).desired_width(130.0));
            if ui.button("Add").clicked() {
                let host = self.host.trim().to_lowercase();
                let ip = self.ip.trim().to_owned();
                self.message = Some(if !valid_host(&host) {
                    Err(format!("\"{host}\" is not a valid host name."))
                } else if ip.parse::<IpAddr>().is_err() {
                    Err(format!("\"{ip}\" is not a valid IP address."))
                } else if let Some(Ok(lines)) = &mut self.lines {
                    let mut add = vec![host.clone()];
                    // Blocking "example.com" usually also means "www.example.com".
                    if !host.starts_with("www.") && host.matches('.').count() == 1 {
                        add.push(format!("www.{host}"));
                    }
                    lines.push(Line::Entry {
                        enabled: true,
                        ip,
                        hosts: add,
                        comment: Some("added by Win Manager RS".into()),
                    });
                    self.dirty = true;
                    self.host.clear();
                    Ok(format!("Added {host}. Click Save to apply."))
                } else {
                    Err("The hosts file could not be read.".into())
                });
            }
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.dirty && ctx.elevated, egui::Button::new("Save"))
                .clicked()
            {
                self.message = Some(self.save(ctx).map_err(|e| format!("{e:#}")));
            }
            if ui
                .add_enabled(self.dirty, egui::Button::new("Discard changes"))
                .clicked()
            {
                self.lines = None;
            }
            if ui.button("Open in Notepad").clicked() {
                let _ = std::process::Command::new("notepad").arg(hosts_path()).spawn();
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));
        });
        if !ctx.elevated {
            ui.weak(ctx.i18n.tr("shell-needs-admin"));
        }
        show_message(ui, &self.message);
        ui.separator();

        let f = self.filter.to_lowercase();
        let mut remove = None;
        let mut changed = false;
        match &mut self.lines {
            Some(Ok(lines)) => {
                let entries = lines.iter().filter(|l| matches!(l, Line::Entry { .. })).count();
                if entries == 0 {
                    ui.weak("The hosts file has no entries.");
                }
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    egui::Grid::new("hosts")
                        .num_columns(4)
                        .striped(true)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            for (i, l) in lines.iter_mut().enumerate() {
                                let Line::Entry {
                                    enabled,
                                    ip,
                                    hosts,
                                    comment,
                                } = l
                                else {
                                    continue;
                                };
                                if !f.is_empty() && !hosts.iter().any(|h| h.contains(&f)) {
                                    continue;
                                }
                                changed |= ui.checkbox(enabled, "").changed();
                                ui.label(hosts.join(" "));
                                ui.weak(format!(
                                    "→ {ip}{}",
                                    comment.as_ref().map(|c| format!("   # {c}")).unwrap_or_default()
                                ));
                                if ui.small_button("Remove").clicked() {
                                    remove = Some(i);
                                }
                                ui.end_row();
                            }
                        });
                });
                if let Some(i) = remove {
                    lines.remove(i);
                    changed = true;
                }
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e.clone());
            }
            None => {}
        }
        if changed {
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Copyright (c) Microsoft\r\n#\r\n# 102.54.94.97 rhino.acme.com # source server\r\n\r\n127.0.0.1\tlocalhost  # loop\r\n0.0.0.0 ads.example.com www.ads.example.com\r\n::1 localhost\r\n";

    #[test]
    fn parse_and_roundtrip() {
        let lines = parse(SAMPLE);
        let entries: Vec<&Line> = lines.iter().filter(|l| matches!(l, Line::Entry { .. })).collect();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries[0],
            &Line::Entry {
                enabled: false,
                ip: "102.54.94.97".into(),
                hosts: vec!["rhino.acme.com".into()],
                comment: Some("source server".into())
            }
        );
        assert!(matches!(entries[1], Line::Entry { enabled: true, comment: Some(c), .. } if c == "loop"));
        // Comments and blank lines survive untouched; a second round trip is stable.
        let rendered = render(&lines);
        assert!(rendered.starts_with("# Copyright (c) Microsoft\r\n#\r\n"));
        assert_eq!(parse(&rendered), lines);
        assert_eq!(render(&parse(&rendered)), rendered);
    }

    #[test]
    fn host_validation() {
        assert!(valid_host("example.com"));
        assert!(valid_host("sub-domain.example.co.uk"));
        assert!(!valid_host("bad host.com"));
        assert!(!valid_host("-x.com"));
        assert!(!valid_host("a..b"));
        assert!(!valid_host(""));
    }

    #[test]
    fn real_hosts_file_parses() {
        let text = std::fs::read_to_string(hosts_path()).unwrap();
        let lines = parse(&text);
        assert_eq!(lines.len(), text.lines().count());
    }
}
