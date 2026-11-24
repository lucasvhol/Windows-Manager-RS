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

/// The app's accent colour (selection, icons, active tabs).
pub const ACCENT: Color32 = Color32::from_rgb(56, 122, 222);

/// One toolbar button: glyph from [`crate::icons`], label, enabled.
pub struct ToolButton<'a> {
    pub icon: &'a str,
    pub label: &'a str,
    pub enabled: bool,
}

impl<'a> ToolButton<'a> {
    pub fn new(icon: &'a str, label: &'a str) -> Self {
        Self {
            icon,
            label,
            enabled: true,
        }
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
}

/// A row of icon-over-label buttons; `None` entries draw a group separator.
/// Returns the index (in `buttons`) of the clicked button.
pub fn toolbar(ui: &mut Ui, buttons: &[Option<ToolButton<'_>>]) -> Option<usize> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        for (i, b) in buttons.iter().enumerate() {
            let Some(b) = b else {
                ui.separator();
                continue;
            };
            let width = (b.label.len() as f32 * 6.6).max(44.0) + 10.0;
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::click());
            let resp = if b.enabled {
                resp
            } else {
                resp.on_hover_cursor(egui::CursorIcon::NotAllowed)
            };
            let color = if !b.enabled {
                ui.visuals().weak_text_color().gamma_multiply(0.6)
            } else if resp.hovered() {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().text_color()
            };
            if b.enabled && resp.hovered() {
                ui.painter()
                    .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }
            let icon_color = if b.enabled { ACCENT } else { color };
            ui.painter().text(
                rect.center_top() + Vec2::new(0.0, 14.0),
                egui::Align2::CENTER_CENTER,
                b.icon,
                egui::FontId::proportional(20.0),
                icon_color,
            );
            ui.painter().text(
                rect.center_bottom() - Vec2::new(0.0, 9.0),
                egui::Align2::CENTER_CENTER,
                b.label,
                egui::FontId::proportional(12.0),
                color,
            );
            if b.enabled && resp.clicked() {
                clicked = Some(i);
            }
        }
    });
    ui.separator();
    clicked
}

/// Left section list (icon + label). The selected entry uses the accent with a notch.
pub fn section_nav(ui: &mut Ui, items: &[(&str, &str)], selected: &mut usize) -> bool {
    let mut changed = false;
    let content = ui.visuals().panel_fill;
    ui.spacing_mut().item_spacing.y = 2.0;
    for (i, (icon, label)) in items.iter().enumerate() {
        if label.is_empty() {
            ui.separator();
            continue;
        }
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
        let sel = *selected == i;
        let p = ui.painter();
        if sel {
            p.rect_filled(rect, 3.0, ACCENT);
            let (r, cy) = (rect.right(), rect.center().y);
            p.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(r, cy - 7.0),
                    Pos2::new(r - 7.0, cy),
                    Pos2::new(r, cy + 7.0),
                ],
                content,
                Stroke::NONE,
            ));
        } else if resp.hovered() {
            p.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let fg = if sel {
            Color32::WHITE
        } else {
            ui.visuals().text_color()
        };
        p.text(
            rect.left_center() + Vec2::new(10.0, 0.0),
            egui::Align2::LEFT_CENTER,
            icon,
            egui::FontId::proportional(17.0),
            if sel { fg } else { ACCENT },
        );
        p.text(
            rect.left_center() + Vec2::new(36.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.0),
            fg,
        );
        if resp.clicked() && !sel {
            *selected = i;
            changed = true;
        }
    }
    changed
}

/// Numbered page tabs ("1 Basics  2 Advanced …").
pub fn numbered_tabs(ui: &mut Ui, labels: &[&str], selected: &mut usize) {
    ui.horizontal(|ui| {
        for (i, label) in labels.iter().enumerate() {
            let sel = *selected == i;
            let text = egui::RichText::new(format!("{}  {label}", i + 1));
            let text = if sel { text.strong().color(ACCENT) } else { text };
            if ui.add(egui::Button::new(text).selected(sel)).clicked() {
                *selected = i;
            }
        }
    });
    ui.add_space(4.0);
}

/// Bottom status bar: a text on the left and optional widgets on the right.
pub fn status_bar(ui: &mut Ui, text: &str, right: impl FnOnce(&mut Ui)) {
    ui.separator();
    ui.horizontal(|ui| {
        ui.weak(text);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
}

/// Checkboxes for the local (fixed and removable) drives, each with a usage bar.
/// An empty `selected` means "the Windows drive only"; the first change makes it
/// explicit. Returns true when the selection changed.
pub fn drive_picker(ui: &mut Ui, selected: &mut Vec<char>) -> bool {
    use crate::drives::{self, DriveKind};
    let system = crate::paths::expand_env("%SystemDrive%")
        .chars()
        .next()
        .unwrap_or('C')
        .to_ascii_uppercase();
    let mut changed = false;
    for d in drives::list()
        .into_iter()
        .filter(|d| matches!(d.kind, DriveKind::Fixed | DriveKind::Removable))
    {
        let mut on = if selected.is_empty() {
            d.letter == system
        } else {
            selected.contains(&d.letter)
        };
        let used = d.used() as f32 / d.total.max(1) as f32;
        ui.vertical(|ui| {
            if ui.checkbox(&mut on, d.display_name()).changed() {
                if selected.is_empty() {
                    selected.push(system);
                }
                selected.retain(|c| *c != d.letter);
                if on {
                    selected.push(d.letter);
                }
                selected.sort_unstable();
                changed = true;
            }
            ui.add(
                egui::ProgressBar::new(used)
                    .desired_width(130.0)
                    .desired_height(6.0),
            )
            .on_hover_text(format!(
                "{} free of {}",
                format_bytes(d.free),
                format_bytes(d.total)
            ));
        });
    }
    changed
}

fn open_requests_id() -> Id {
    Id::unique("wm-open-tool-requests")
}

/// Asks the main window to open another tool (by id) after this frame.
pub fn request_open_tool(ctx: &egui::Context, id: &str) {
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Vec<String>>(open_requests_id())
            .push(id.to_owned())
    });
}

/// Tool ids requested with [`request_open_tool`] since the last call (used by the shell).
pub fn take_open_requests(ctx: &egui::Context) -> Vec<String> {
    ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<Vec<String>>(open_requests_id())))
}

#[cfg(test)]
mod tests {
    #[test]
    fn open_requests_queue() {
        let ctx = crate::egui::Context::default();
        super::request_open_tool(&ctx, "repair");
        super::request_open_tool(&ctx, "fixer");
        assert_eq!(
            super::take_open_requests(&ctx),
            vec!["repair".to_owned(), "fixer".to_owned()]
        );
        assert!(super::take_open_requests(&ctx).is_empty());
    }

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
