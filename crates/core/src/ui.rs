//! Small UI helpers shared by all tools.

use crate::egui::{self, Color32, Id, Pos2, Response, Sense, Stroke, Ui, Vec2};
use crate::i18n::I18n;

/// Result of the last user action, shown as a coloured line.
pub type Message = Option<Result<String, String>>;

pub fn show_message(ui: &mut Ui, message: &Message) {
    match message {
        Some(Ok(m)) => {
            ui.colored_label(ui.visuals().text_color(), format!("✔ {m}"));
        }
        Some(Err(e)) => {
            ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
        }
        None => {}
    }
}

/// `1536` → `"1.5 KB"` (binary units, shown with conventional labels).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `93784` → `"1d 2h 3m"`.
pub fn format_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m {}s", secs % 60),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h {m}m"),
    }
}

/// Draws a small line chart of `values` scaled to `0..=max`.
pub fn sparkline(ui: &mut Ui, values: &[f32], max: f32, size: Vec2, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    if values.len() >= 2 && max > 0.0 {
        let step = rect.width() / (values.len() - 1) as f32;
        let points: Vec<Pos2> = values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let y = rect.bottom() - (v / max).clamp(0.0, 1.0) * (rect.height() - 2.0) - 1.0;
                Pos2::new(rect.left() + i as f32 * step, y)
            })
            .collect();
        painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));
    }
    response
}

/// A yes/no confirmation dialog carrying a payload until the user decides.
pub struct Confirm<T> {
    pending: Option<(String, T)>,
}

impl<T> Default for Confirm<T> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<T> Confirm<T> {
    pub fn ask(&mut self, question: impl Into<String>, payload: T) {
        self.pending = Some((question.into(), payload));
    }

    pub fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    /// Shows the dialog if pending; returns the payload once the user confirms.
    pub fn show(&mut self, ctx: &egui::Context, i18n: &I18n) -> Option<T> {
        let (question, _) = self.pending.as_ref()?;
        let mut decision = None;
        let modal = egui::Modal::new(Id::unique("confirm_dialog")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(question);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button(i18n.tr("common-confirm")).clicked() {
                    decision = Some(true);
                }
                if ui.button(i18n.tr("common-cancel")).clicked() {
                    decision = Some(false);
                }
            });
        });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => self.pending.take().map(|(_, p)| p),
            Some(false) => {
                self.pending = None;
                None
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GB");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(59), "0m 59s");
        assert_eq!(format_duration(3_720), "1h 2m");
        assert_eq!(format_duration(93_784), "1d 2h 3m");
    }
}
