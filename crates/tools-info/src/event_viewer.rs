//! Event Viewer: browse Windows event logs through the Windows Event Log API (wevtapi).

use std::collections::HashMap;

use anyhow::Result;
use egui_extras::{Column, TableBuilder};
use windows::Win32::System::EventLog::{
    EVT_HANDLE, EvtClose, EvtFormatMessage, EvtFormatMessageEvent, EvtNext, EvtOpenPublisherMetadata,
    EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender, EvtRenderEventXml,
};
use windows::core::{HRESULT, HSTRING, PCWSTR};
use wm_core::{AppCtx, Category, Tool, egui, task::BackgroundTask};

pub const LOGS: [&str; 4] = ["Application", "System", "Security", "Setup"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Levels {
    pub critical: bool,
    pub error: bool,
    pub warning: bool,
    pub information: bool,
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            critical: true,
            error: true,
            warning: true,
            information: true,
        }
    }
}

/// XPath query selecting the chosen levels (`*` when all are selected).
pub fn level_query(l: Levels) -> String {
    if l.critical && l.error && l.warning && l.information {
        return "*".into();
    }
    let mut parts = Vec::new();
    if l.critical {
        parts.push("Level=1");
    }
    if l.error {
        parts.push("Level=2");
    }
    if l.warning {
        parts.push("Level=3");
    }
    if l.information {
        parts.push("Level=4 or Level=0");
    }
    if parts.is_empty() {
        // Nothing selected: match nothing.
        return "*[System[Level=255]]".into();
    }
    format!("*[System[({})]]", parts.join(" or "))
}

/// Full XPath query: chosen levels, optionally only events from the last `hours`.
pub fn build_query(l: Levels, hours: u32) -> String {
    if hours == 0 {
        return level_query(l);
    }
    let time = format!(
        "TimeCreated[timediff(@SystemTime) <= {}]",
        u64::from(hours) * 3_600_000
    );
    let levels = level_query(l);
    if levels == "*" {
        return format!("*[System[{time}]]");
    }
    // "*[System[(…)]]" -> "*[System[(…) and TimeCreated[…]]]"
    let inner = levels.trim_start_matches("*[System[").trim_end_matches("]]");
    format!("*[System[{inner} and {time}]]")
}

/// Quotes a CSV field when needed.
pub fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

pub fn to_csv(rows: &[&EventRow]) -> String {
    let mut out = String::from("Level,Date and time,Source,Event ID,Message\r\n");
    for r in rows {
        let msg = r.message.replace("\r\n", " ").replace('\n', " ");
        out.push_str(&format!(
            "{},{},{},{},{}\r\n",
            level_name(r.level),
            csv_field(&r.time),
            csv_field(&r.provider),
            r.event_id,
            csv_field(&msg)
        ));
    }
    out
}

/// Grouping key (source, event id) and stats (level, count, latest time, first message line).
type GroupKey = (String, u32);
type GroupStats = (u8, usize, String, String);

pub const RANGES: [(u32, &str); 5] = [
    (0, "Any time"),
    (1, "Last hour"),
    (24, "Last 24 hours"),
    (168, "Last 7 days"),
    (720, "Last 30 days"),
];

pub fn level_name(level: u8) -> &'static str {
    match level {
        1 => "Critical",
        2 => "Error",
        3 => "Warning",
        5 => "Verbose",
        _ => "Information",
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventRow {
    pub time: String,
    pub level: u8,
    pub provider: String,
    pub event_id: u32,
    pub record_id: u64,
    pub message: String,
    pub xml: String,
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Value of `attr` on the first `<tag …>` element in `xml`.
pub fn xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag} "))?;
    let end = start + xml[start..].find('>')?;
    let element = &xml[start..end];
    let pos = element.find(&format!(" {attr}="))? + attr.len() + 2;
    let quote = element[pos..].chars().next()?;
    let value = &element[pos + 1..];
    Some(unescape(&value[..value.find(quote)?]))
}

/// Text content of the first `<tag>…</tag>` element in `xml`.
pub fn xml_text(xml: &str, tag: &str) -> Option<String> {
    // Skip longer tags sharing the prefix (e.g. `<EventData>` when looking for `<Event>`).
    let (open, _) = xml
        .match_indices(&format!("<{tag}"))
        .find(|&(i, _)| matches!(xml[i + tag.len() + 1..].chars().next(), Some('>' | ' ')))?;
    let content_start = open + xml[open..].find('>')? + 1;
    let content_end = content_start + xml[content_start..].find(&format!("</{tag}>"))?;
    Some(unescape(&xml[content_start..content_end]))
}

pub fn parse_event_xml(xml: &str) -> EventRow {
    let time = xml_attr(xml, "TimeCreated", "SystemTime")
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default();
    EventRow {
        time,
        level: xml_text(xml, "Level").and_then(|l| l.parse().ok()).unwrap_or(4),
        provider: xml_attr(xml, "Provider", "Name").unwrap_or_default(),
        event_id: xml_text(xml, "EventID").and_then(|l| l.parse().ok()).unwrap_or(0),
        record_id: xml_text(xml, "EventRecordID")
            .and_then(|l| l.parse().ok())
            .unwrap_or(0),
        message: String::new(),
        xml: xml.to_owned(),
    }
}

/// Owns an EVT_HANDLE and closes it on drop.
struct Evt(EVT_HANDLE);

impl Drop for Evt {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = EvtClose(self.0);
            }
        }
    }
}

fn render_xml(event: EVT_HANDLE) -> Result<String> {
    let (mut used, mut count) = (0u32, 0u32);
    unsafe {
        // First call reports the required size (fails with ERROR_INSUFFICIENT_BUFFER).
        let _ = EvtRender(None, event, EvtRenderEventXml.0, 0, None, &mut used, &mut count);
        let mut buf = vec![0u16; used as usize / 2 + 1];
        EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            (buf.len() * 2) as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut used,
            &mut count,
        )?;
        let len = (used as usize / 2).min(buf.len());
        Ok(String::from_utf16_lossy(&buf[..len])
            .trim_end_matches('\0')
            .to_owned())
    }
}

/// ERROR_EVT_UNRESOLVED_VALUE_INSERT / PARAMETER_INSERT: message is still usable.
const PARTIAL_MESSAGE_CODES: [u32; 2] = [15029, 15030];

fn format_message(publisher: EVT_HANDLE, event: EVT_HANDLE) -> Option<String> {
    let mut used = 0u32;
    unsafe {
        let _ = EvtFormatMessage(
            Some(publisher),
            Some(event),
            0,
            None,
            EvtFormatMessageEvent.0,
            None,
            &mut used,
        );
        if used == 0 {
            return None;
        }
        let mut buf = vec![0u16; used as usize];
        if let Err(e) = EvtFormatMessage(
            Some(publisher),
            Some(event),
            0,
            None,
            EvtFormatMessageEvent.0,
            Some(&mut buf),
            &mut used,
        ) && !PARTIAL_MESSAGE_CODES
            .iter()
            .any(|c| e.code() == HRESULT::from_win32(*c))
        {
            return None;
        }
        let text = String::from_utf16_lossy(&buf)
            .trim_end_matches('\0')
            .trim()
            .to_owned();
        (!text.is_empty()).then_some(text)
    }
}

pub fn query_events(log: &str, levels: Levels, hours: u32, max: usize) -> Result<Vec<EventRow>> {
    let query = unsafe {
        Evt(EvtQuery(
            None,
            &HSTRING::from(log),
            &HSTRING::from(build_query(levels, hours)),
            EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
        )?)
    };
    let mut publishers: HashMap<String, Option<Evt>> = HashMap::new();
    let mut rows = Vec::new();
    'outer: loop {
        let mut handles = [0isize; 64];
        let mut returned = 0u32;
        if unsafe { EvtNext(query.0, &mut handles, u32::MAX, 0, &mut returned) }.is_err() {
            break; // ERROR_NO_MORE_ITEMS or failure
        }
        // Wrap every returned handle first so all are closed even when stopping early.
        let events: Vec<Evt> = handles[..returned as usize]
            .iter()
            .map(|&h| Evt(EVT_HANDLE(h)))
            .collect();
        for ev in &events {
            if rows.len() >= max {
                break 'outer;
            }
            let Ok(xml) = render_xml(ev.0) else { continue };
            let mut row = parse_event_xml(&xml);
            let publisher = publishers.entry(row.provider.clone()).or_insert_with(|| unsafe {
                EvtOpenPublisherMetadata(None, &HSTRING::from(row.provider.as_str()), PCWSTR::null(), 0, 0)
                    .ok()
                    .map(Evt)
            });
            if let Some(p) = publisher {
                row.message = format_message(p.0, ev.0).unwrap_or_default();
            }
            rows.push(row);
        }
    }
    Ok(rows)
}

pub struct EventViewer {
    log: usize,
    levels: Levels,
    max: usize,
    filter: String,
    task: Option<BackgroundTask<Vec<EventRow>>>,
    rows: Result<Vec<EventRow>, String>,
    selected: Option<u64>,
    loaded_once: bool,
    hours: u32,
    /// Event groups (source, ID) that are unfolded.
    expanded: std::collections::HashSet<GroupKey>,
    /// The Query side panel is open (the original starts with it folded).
    query_open: bool,
    message: wm_core::ui::Message,
}

impl Default for EventViewer {
    fn default() -> Self {
        Self {
            log: 1,
            levels: Levels {
                information: false,
                ..Levels::default()
            },
            max: 500,
            filter: String::new(),
            task: None,
            rows: Ok(Vec::new()),
            selected: None,
            loaded_once: false,
            hours: 168,
            expanded: Default::default(),
            query_open: false,
            message: None,
        }
    }
}

impl EventViewer {
    fn reload(&mut self, ctx: &egui::Context) {
        let (log, levels, max, hours) = (LOGS[self.log], self.levels, self.max, self.hours);
        self.selected = None;
        self.task = Some(BackgroundTask::spawn(ctx, move |_| {
            query_events(log, levels, hours, max)
        }));
    }
}

impl Tool for EventViewer {
    fn id(&self) -> &'static str {
        "events"
    }

    fn category(&self) -> Category {
        Category::Information
    }

    fn layout(&self) -> wm_core::Layout {
        wm_core::Layout::Fill
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        use wm_core::{icons, ui::frame};
        if !self.loaded_once {
            self.loaded_once = true;
            self.reload(ui.ctx());
        }
        if let Some(task) = &mut self.task {
            task.poll();
            if let Some(result) = task.take_result() {
                self.rows = result;
                self.task = None;
            }
        }
        // Taken out for this frame (so the regions below can borrow `self`), put back at the end.
        let rows_result = std::mem::replace(&mut self.rows, Ok(Vec::new()));
        let empty = Vec::new();
        let rows: &Vec<EventRow> = rows_result.as_ref().unwrap_or(&empty);
        let f = self.filter.to_lowercase();
        let visible: Vec<&EventRow> = rows
            .iter()
            .filter(|r| {
                f.is_empty()
                    || r.provider.to_lowercase().contains(&f)
                    || r.message.to_lowercase().contains(&f)
                    || r.event_id.to_string() == f
            })
            .collect();
        let mut reload = false;

        // The original's Query side panel: folded to a narrow strip, or the filters.
        let fill = ui.visuals().faint_bg_color;
        egui::Panel::left("ev-query")
            .resizable(false)
            .exact_size(if self.query_open { 220.0 } else { 30.0 })
            .frame(egui::Frame::new().fill(fill).inner_margin(egui::Margin::same(6)))
            .show(ui, |ui| {
                if !self.query_open {
                    ui.vertical_centered(|ui| {
                        if ui
                            .add(egui::Button::new("»").frame(false))
                            .on_hover_text("Query")
                            .clicked()
                        {
                            self.query_open = true;
                        }
                        ui.add_space(8.0);
                        for c in "Query".chars() {
                            ui.weak(c.to_string());
                        }
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    ui.strong("Query");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new("«").frame(false)).clicked() {
                            self.query_open = false;
                        }
                    });
                });
                ui.separator();
                ui.label("Log");
                egui::ComboBox::from_id_salt("event_log")
                    .selected_text(LOGS[self.log])
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for (i, name) in LOGS.iter().enumerate() {
                            reload |= ui.selectable_value(&mut self.log, i, *name).changed();
                        }
                    });
                ui.add_space(6.0);
                ui.label("Level");
                reload |= ui.checkbox(&mut self.levels.critical, "Critical").changed();
                reload |= ui.checkbox(&mut self.levels.error, "Error").changed();
                reload |= ui.checkbox(&mut self.levels.warning, "Warning").changed();
                reload |= ui.checkbox(&mut self.levels.information, "Information").changed();
                ui.add_space(6.0);
                ui.label("Logged");
                egui::ComboBox::from_id_salt("event_range")
                    .selected_text(
                        RANGES
                            .iter()
                            .find(|(h, _)| *h == self.hours)
                            .map_or("", |(_, l)| *l),
                    )
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for (h, label) in RANGES {
                            reload |= ui.selectable_value(&mut self.hours, h, label).changed();
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("At most");
                    reload |= ui
                        .add(egui::DragValue::new(&mut self.max).range(50..=10_000).speed(10))
                        .drag_stopped();
                    ui.label("events");
                });
                ui.add_space(10.0);
                if ui
                    .add_enabled(
                        self.task.is_none(),
                        egui::Button::new(ctx.i18n.tr("common-refresh")),
                    )
                    .clicked()
                {
                    reload = true;
                }
            });

        // Status bar: totals, Export to CSV and search, like the original.
        let loading = self.task.is_some();
        let status = format!(
            "Total events of {}: {}; Current event count: {}",
            LOGS[self.log],
            rows.len(),
            visible.len()
        );
        let mut export = false;
        let search_hint = format!("{}  {}", icons::MAGNIFYING_GLASS, ctx.i18n.tr("common-filter"));
        frame::status(ui, &status, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(search_hint)
                    .desired_width(180.0),
            );
            export = ui
                .add_enabled(!visible.is_empty(), egui::Button::new("Export to CSV"))
                .clicked();
            if loading {
                ui.spinner();
            }
        });
        if export
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("CSV", &["csv"])
                .set_file_name(format!("{}-events.csv", LOGS[self.log]))
                .save_file()
        {
            self.message = Some(
                std::fs::write(&path, to_csv(&visible))
                    .map(|()| format!("Saved {} events to {}.", visible.len(), path.display()))
                    .map_err(|e| e.to_string()),
            );
        }

        // The selected event's details, in a resizable panel at the bottom.
        if let Some(r) = self
            .selected
            .and_then(|id| rows.iter().find(|r| r.record_id == id))
        {
            egui::Panel::bottom("event_detail")
                .resizable(true)
                .default_size(200.0)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("{} — Event {} — {}", r.provider, r.event_id, r.time));
                        if ui.small_button("Copy XML").clicked() {
                            ui.ctx().copy_text(r.xml.clone());
                        }
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("event_detail")
                        .show(ui, |ui| {
                            if r.message.is_empty() {
                                ui.weak("No message text is available for this event; see the XML below.");
                            } else {
                                ui.add(egui::Label::new(&r.message).selectable(true));
                            }
                            egui::CollapsingHeader::new("XML").show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(&r.xml).monospace().small())
                                        .selectable(true),
                                );
                            });
                        });
                });
        }

        let needs_admin = LOGS[self.log] == "Security" && !ctx.elevated;
        let admin_hint = ctx.i18n.tr("shell-needs-admin");
        frame::central_fixed(ui, |ui| {
            wm_core::ui::show_message(ui, &self.message);
            if let Err(e) = &rows_result {
                ui.colored_label(ui.visuals().error_fg_color, e);
                if needs_admin {
                    ui.weak(admin_hint);
                }
                return;
            }
            self.tree_table(ui, &visible);
        });
        self.rows = rows_result;
        if reload && self.task.is_none() {
            self.reload(ui.ctx());
        }
    }
}

/// A row of the grouped view: an event group, or one event inside an open group.
enum TreeRow<'a> {
    Group(GroupKey, GroupStats),
    Event(&'a EventRow),
}

/// Groups events by (source, ID), most frequent first; each group keeps its newest
/// time and first message line.
fn groups<'a>(visible: &[&'a EventRow]) -> Vec<(GroupKey, GroupStats, Vec<&'a EventRow>)> {
    let mut out: Vec<(GroupKey, GroupStats, Vec<&EventRow>)> = Vec::new();
    for r in visible {
        let key = (r.provider.clone(), r.event_id);
        match out.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, g, list)) => {
                g.1 += 1;
                list.push(r);
            }
            None => out.push((
                key,
                (
                    r.level,
                    1,
                    r.time.clone(),
                    r.message.lines().next().unwrap_or_default().to_owned(),
                ),
                vec![r],
            )),
        }
    }
    out.sort_by_key(|g| std::cmp::Reverse(g.1.1));
    out
}

impl EventViewer {
    /// The original's grouped list: ID | Creation Time | Level | Source | Messages, one
    /// row per event ID that unfolds into its events.
    fn tree_table(&mut self, ui: &mut egui::Ui, visible: &[&EventRow]) {
        let mut rows: Vec<TreeRow<'_>> = Vec::new();
        for (key, stats, events) in groups(visible) {
            let open = self.expanded.contains(&key);
            rows.push(TreeRow::Group(key, stats));
            if open {
                rows.extend(events.into_iter().map(TreeRow::Event));
            }
        }
        let (error, warn) = (ui.visuals().error_fg_color, ui.visuals().warn_fg_color);
        let level_color = |ui: &egui::Ui, level: u8| match level {
            1 | 2 => error,
            3 => warn,
            _ => ui.visuals().text_color(),
        };
        let mut toggle = None;
        let mut clicked = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::initial(110.0))
            .column(Column::initial(150.0))
            .column(Column::initial(80.0))
            .column(Column::initial(200.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for title in ["ID", "Creation Time", "Level", "Source", "Messages"] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| match &rows[row.index()] {
                    TreeRow::Group(key, (level, count, latest, msg)) => {
                        let open = self.expanded.contains(key);
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(if open { "▾" } else { "▸" });
                                ui.colored_label(level_color(ui, *level), wm_core::icons::WARNING);
                                ui.strong(key.1.to_string());
                                ui.weak(format!("({count})"));
                            });
                        });
                        row.col(|ui| {
                            ui.label(latest);
                        });
                        row.col(|ui| {
                            ui.colored_label(level_color(ui, *level), level_name(*level));
                        });
                        row.col(|ui| {
                            ui.label(&key.0);
                        });
                        row.col(|ui| {
                            ui.label(msg);
                        });
                        if row.response().clicked() {
                            toggle = Some(key.clone());
                        }
                    }
                    TreeRow::Event(r) => {
                        row.set_selected(self.selected == Some(r.record_id));
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.add_space(22.0);
                                ui.weak(r.event_id.to_string());
                            });
                        });
                        row.col(|ui| {
                            ui.label(&r.time);
                        });
                        row.col(|ui| {
                            ui.colored_label(level_color(ui, r.level), level_name(r.level));
                        });
                        row.col(|ui| {
                            ui.label(&r.provider);
                        });
                        row.col(|ui| {
                            ui.label(r.message.lines().next().unwrap_or_default());
                        });
                        if row.response().clicked() {
                            clicked = Some(r.record_id);
                        }
                    }
                });
            });
        if let Some(k) = toggle
            && !self.expanded.remove(&k)
        {
            self.expanded.insert(k);
        }
        if clicked.is_some() {
            self.selected = clicked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System>\
        <Provider Name='Service Control Manager' Guid='{555908d1-a6d7-4695-8e1e-26931d2012f4}' EventSourceName='Service Control Manager'/>\
        <EventID Qualifiers='16384'>7036</EventID><Version>0</Version><Level>4</Level>\
        <TimeCreated SystemTime='2026-11-23T22:31:10.1234567Z'/><EventRecordID>98765</EventRecordID>\
        </System><EventData><Data Name='param1'>A &amp; B</Data></EventData></Event>";

    #[test]
    fn events_group_by_source_and_id_most_frequent_first() {
        let mut a = parse_event_xml(SAMPLE);
        a.message = "First line\nsecond".into();
        let mut b = a.clone();
        b.record_id += 1;
        let mut c = a.clone();
        c.event_id = 41;
        let rows = [&a, &b, &c];
        let g = groups(&rows);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, ("Service Control Manager".to_owned(), 7036));
        assert_eq!(g[0].1.1, 2, "count");
        assert_eq!(g[0].1.3, "First line");
        assert_eq!(g[0].2.len(), 2);
        assert_eq!(g[1].0.1, 41);
    }

    #[test]
    fn parse_sample_event() {
        let r = parse_event_xml(SAMPLE);
        assert_eq!(r.provider, "Service Control Manager");
        assert_eq!(r.event_id, 7036);
        assert_eq!(r.level, 4);
        assert_eq!(r.record_id, 98765);
        assert!(r.time.starts_with("2026-11-2"), "{}", r.time);
        assert_eq!(xml_text(SAMPLE, "Data").as_deref(), Some("A & B"));
        // <EventData> must not be mistaken for <Event>.
        assert_eq!(
            xml_text("<EventData>x</EventData><Event>y</Event>", "Event").as_deref(),
            Some("y")
        );
    }

    #[test]
    fn time_range_queries_and_csv() {
        assert_eq!(build_query(Levels::default(), 0), "*");
        assert_eq!(
            build_query(Levels::default(), 24),
            "*[System[TimeCreated[timediff(@SystemTime) <= 86400000]]]"
        );
        let errors = Levels {
            critical: false,
            error: true,
            warning: false,
            information: false,
        };
        assert_eq!(
            build_query(errors, 1),
            "*[System[(Level=2) and TimeCreated[timediff(@SystemTime) <= 3600000]]]"
        );
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("plain"), "plain");
        // A real time-limited query must be accepted by the event log service.
        query_events("System", Levels::default(), 24, 5).unwrap();
    }

    #[test]
    fn level_queries() {
        assert_eq!(level_query(Levels::default()), "*");
        let only_errors = Levels {
            critical: false,
            error: true,
            warning: false,
            information: false,
        };
        assert_eq!(level_query(only_errors), "*[System[(Level=2)]]");
        let none = Levels {
            critical: false,
            error: false,
            warning: false,
            information: false,
        };
        assert_eq!(level_query(none), "*[System[Level=255]]");
    }

    #[test]
    fn query_system_log() {
        let rows = query_events("System", Levels::default(), 0, 25).unwrap();
        assert!(!rows.is_empty(), "System log should contain events");
        assert!(rows.len() <= 25);
        assert!(rows.iter().all(|r| !r.provider.is_empty() && !r.time.is_empty()));
        assert!(
            rows.iter().any(|r| !r.message.is_empty()),
            "at least one message should format"
        );
    }
}
