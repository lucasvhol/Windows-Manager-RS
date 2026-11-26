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

/// Colours shared by the hub and the tool windows.
pub mod palette {
    use super::Color32;
    use crate::Category;

    /// Header and description bands.
    pub fn band(dark: bool) -> Color32 {
        if dark {
            Color32::from_rgb(36, 40, 48)
        } else {
            Color32::from_rgb(226, 232, 241)
        }
    }

    /// The hub's category rail.
    pub fn rail(dark: bool) -> Color32 {
        if dark {
            Color32::from_rgb(30, 33, 39)
        } else {
            Color32::from_rgb(238, 241, 246)
        }
    }

    /// Each category's own colour, used for its icons (the original uses colourful
    /// icons per category; ours are glyphs tinted with these).
    pub fn category(c: Category) -> Color32 {
        match c {
            Category::Home => super::ACCENT,
            Category::OneClick => Color32::from_rgb(240, 152, 44),
            Category::Information => Color32::from_rgb(72, 152, 240),
            Category::Optimizer => Color32::from_rgb(38, 186, 170),
            Category::Cleaner => Color32::from_rgb(112, 192, 72),
            Category::Customization => Color32::from_rgb(170, 124, 236),
            Category::Security => Color32::from_rgb(232, 186, 52),
            Category::Network => Color32::from_rgb(56, 184, 226),
            Category::Misc => Color32::from_rgb(160, 170, 190),
        }
    }
}

/// The app-wide look: one type scale, spacing, rounding and (in dark mode) slightly
/// lifted panel colours. Applied to both themes at start-up.
pub fn apply_style(style: &mut egui::Style, dark: bool) {
    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 6.0);
    s.button_padding = Vec2::new(10.0, 4.0);
    s.interact_size.y = 24.0;
    s.menu_margin = egui::Margin::same(6);
    let v = &mut style.visuals;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke.color = Color32::WHITE;
    v.hyperlink_color = ACCENT;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::same(4);
    }
    v.window_corner_radius = egui::CornerRadius::same(6);
    if dark {
        v.panel_fill = Color32::from_rgb(24, 26, 31);
        v.window_fill = Color32::from_rgb(28, 31, 37);
        v.faint_bg_color = Color32::from_rgb(32, 35, 42);
        v.extreme_bg_color = Color32::from_rgb(17, 18, 22);
        v.widgets.noninteractive.bg_stroke.color = Color32::from_rgb(48, 52, 61);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(44, 48, 57);
        v.widgets.inactive.bg_fill = Color32::from_rgb(44, 48, 57);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(54, 59, 70);
        v.widgets.hovered.bg_fill = Color32::from_rgb(54, 59, 70);
    }
}

/// A centred placeholder for an area that has nothing to show yet: a faint icon, a
/// title and one line of guidance (e.g. "Press Analyze to measure the store").
pub fn empty_state(ui: &mut Ui, icon: &str, title: &str, hint: &str) {
    let h = ui.available_height().max(120.0);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), h),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            ui.add_space((h / 2.0 - 60.0).max(8.0));
            ui.label(
                egui::RichText::new(icon)
                    .size(46.0)
                    .color(ui.visuals().weak_text_color().gamma_multiply(0.6)),
            );
            ui.add_space(4.0);
            ui.label(egui::RichText::new(title).size(15.0).strong());
            if !hint.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(hint).weak()).wrap());
            }
        },
    );
}

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
    let clicked = toolbar_row(ui, buttons, |_| {});
    ui.separator();
    clicked
}

/// The toolbar buttons in one row, with `right` drawn right-aligned after them.
pub fn toolbar_row(
    ui: &mut Ui,
    buttons: &[Option<ToolButton<'_>>],
    right: impl FnOnce(&mut Ui),
) -> Option<usize> {
    let mut clicked = None;
    let width_of = |b: &ToolButton<'_>| (b.label.len() as f32 * 6.6).max(44.0) + 10.0;
    ui.horizontal(|ui| {
        // Buttons that don't fit go into a "More" menu at the end instead of being cut off.
        let more_w = 54.0;
        let mut budget = ui.available_width() - more_w;
        let total: f32 = buttons
            .iter()
            .map(|b| b.as_ref().map_or(10.0, width_of) + ui.spacing().item_spacing.x)
            .sum();
        let overflow = total > ui.available_width();
        let mut hidden: Vec<usize> = Vec::new();
        for (i, b) in buttons.iter().enumerate() {
            let w = b.as_ref().map_or(10.0, width_of) + ui.spacing().item_spacing.x;
            if overflow && (!hidden.is_empty() || w > budget) {
                if b.is_some() {
                    hidden.push(i);
                }
                continue;
            }
            budget -= w;
            let Some(b) = b else {
                ui.separator();
                continue;
            };
            let width = width_of(b);
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
        if !hidden.is_empty() {
            ui.menu_button(
                egui::RichText::new(format!("{}  More", crate::icons::CARET_DOWN)).size(12.5),
                |ui| {
                    for &i in &hidden {
                        if let Some(b) = &buttons[i]
                            && ui
                                .add_enabled(b.enabled, egui::Button::new(format!("{}  {}", b.icon, b.label)))
                                .clicked()
                        {
                            clicked = Some(i);
                            ui.close();
                        }
                    }
                },
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
    clicked
}

/// Regions of a tool window, for tools whose [`crate::Tool::layout`] is
/// [`crate::Layout::Fill`]. Call the edge regions first (toolbar, options,
/// status, left), then [`frame::central`] for the rest, like the original's windows:
/// toolbar on top, an optional left pane, results filling the window, and a status
/// bar pinned to the bottom.
pub mod frame {
    use super::{Color32, ToolButton, Ui, toolbar_row};
    use crate::egui;

    fn strip_fill(ui: &Ui) -> Color32 {
        if ui.visuals().dark_mode {
            Color32::from_rgb(30, 33, 39)
        } else {
            Color32::from_rgb(244, 246, 249)
        }
    }

    /// The icon-over-label toolbar along the top; `right` sits at its right end.
    pub fn toolbar(
        ui: &mut Ui,
        buttons: &[Option<ToolButton<'_>>],
        right: impl FnOnce(&mut Ui),
    ) -> Option<usize> {
        let fill = strip_fill(ui);
        egui::Panel::top("tf-toolbar")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ui, |ui| toolbar_row(ui, buttons, right))
            .inner
    }

    /// A compact strip under the toolbar for the tool's options (checkboxes, pickers).
    pub fn options(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
        egui::Panel::top("tf-options")
            .resizable(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| {
                ui.horizontal_wrapped(add);
            });
    }

    /// The status bar pinned to the bottom: a summary on the left, widgets on the right.
    pub fn status(ui: &mut Ui, text: &str, right: impl FnOnce(&mut Ui)) {
        let fill = strip_fill(ui);
        egui::Panel::bottom("tf-status")
            .resizable(false)
            .exact_size(30.0)
            .frame(
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(egui::Margin::symmetric(12, 4)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.weak(text);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
                });
            });
    }

    /// A resizable pane on the left (a list, tree or sections).
    pub fn left<R>(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
        egui::Panel::left("tf-left")
            .resizable(true)
            .default_size(width)
            .min_size(140.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin::same(8)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("tf-left-scroll")
                    .auto_shrink(false)
                    .show(ui, add)
                    .inner
            })
            .inner
    }

    /// The main area: fills what's left and scrolls on its own.
    pub fn central<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("tf-central-scroll")
                    .auto_shrink(false)
                    .show(ui, add)
                    .inner
            })
            .inner
    }

    /// Like [`central`] but without the scroll area, for content that scrolls itself
    /// (tables) or fills the space (charts).
    pub fn central_fixed<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ui, add)
            .inner
    }
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
            let text = if label.is_empty() {
                egui::RichText::new(format!("  {}  ", i + 1))
            } else {
                egui::RichText::new(format!("{}  {label}", i + 1))
            };
            // The selected button is filled with the selection colour, so its text is white.
            let text = if sel {
                text.strong().color(Color32::WHITE)
            } else {
                text
            };
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
