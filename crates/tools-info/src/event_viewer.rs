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

pub fn query_events(log: &str, levels: Levels, max: usize) -> Result<Vec<EventRow>> {
    let query = unsafe {
        Evt(EvtQuery(
            None,
            &HSTRING::from(log),
            &HSTRING::from(level_query(levels)),
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
        }
    }
}

impl EventViewer {
    fn reload(&mut self, ctx: &egui::Context) {
        let (log, levels, max) = (LOGS[self.log], self.levels, self.max);
        self.selected = None;
        self.task = Some(BackgroundTask::spawn(ctx, move |_| {
            query_events(log, levels, max)
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

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
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

        let mut reload = false;
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("event_log")
                .selected_text(LOGS[self.log])
                .show_ui(ui, |ui| {
                    for (i, name) in LOGS.iter().enumerate() {
                        reload |= ui.selectable_value(&mut self.log, i, *name).changed();
                    }
                });
            reload |= ui.checkbox(&mut self.levels.critical, "Critical").changed();
            reload |= ui.checkbox(&mut self.levels.error, "Error").changed();
            reload |= ui.checkbox(&mut self.levels.warning, "Warning").changed();
            reload |= ui.checkbox(&mut self.levels.information, "Information").changed();
            ui.label("Max:");
            reload |= ui
                .add(egui::DragValue::new(&mut self.max).range(50..=10_000).speed(10))
                .drag_stopped();
            reload |= ui
                .add_enabled(
                    self.task.is_none(),
                    egui::Button::new(ctx.i18n.tr("common-refresh")),
                )
                .clicked();
            if self.task.is_some() {
                ui.spinner();
            }
        });
        if reload && self.task.is_none() {
            self.reload(ui.ctx());
        }
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ctx.i18n.tr("common-filter")));

        let rows = match &self.rows {
            Ok(rows) => rows,
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                if LOGS[self.log] == "Security" && !ctx.elevated {
                    ui.weak(ctx.i18n.tr("shell-needs-admin"));
                }
                return;
            }
        };
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
        ui.weak(format!("{} events", visible.len()));

        let detail_height = if self.selected.is_some() { 220.0 } else { 0.0 };
        let table_height = (ui.available_height() - detail_height).max(150.0);
        let mut clicked = None;
        let error = ui.visuals().error_fg_color;
        let warn = ui.visuals().warn_fg_color;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .max_scroll_height(table_height)
            .column(Column::initial(90.0))
            .column(Column::initial(140.0))
            .column(Column::initial(200.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for title in ["Level", "Date and time", "Source", "Event ID", "Message"] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len(), |mut row| {
                    let r = visible[row.index()];
                    row.set_selected(self.selected == Some(r.record_id));
                    row.col(|ui| {
                        let color = match r.level {
                            1 | 2 => error,
                            3 => warn,
                            _ => ui.visuals().text_color(),
                        };
                        ui.colored_label(color, level_name(r.level));
                    });
                    row.col(|ui| {
                        ui.label(&r.time);
                    });
                    row.col(|ui| {
                        ui.label(&r.provider);
                    });
                    row.col(|ui| {
                        ui.label(r.event_id.to_string());
                    });
                    row.col(|ui| {
                        ui.label(r.message.lines().next().unwrap_or_default());
                    });
                    if row.response().clicked() {
                        clicked = Some(r.record_id);
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }

        if let Some(r) = self
            .selected
            .and_then(|id| rows.iter().find(|r| r.record_id == id))
        {
            ui.separator();
            ui.horizontal(|ui| {
                ui.strong(format!("{} — Event {} — {}", r.provider, r.event_id, r.time));
                if ui.small_button("Copy XML").clicked() {
                    ui.ctx().copy_text(r.xml.clone());
                }
            });
            egui::ScrollArea::vertical()
                .id_salt("event_detail")
                .max_height(180.0)
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
        let rows = query_events("System", Levels::default(), 25).unwrap();
        assert!(!rows.is_empty(), "System log should contain events");
        assert!(rows.len() <= 25);
        assert!(rows.iter().all(|r| !r.provider.is_empty() && !r.time.is_empty()));
        assert!(
            rows.iter().any(|r| !r.message.is_empty()),
            "at least one message should format"
        );
    }
}
