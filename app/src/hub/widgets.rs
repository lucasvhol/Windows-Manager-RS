//! Custom-painted hub widgets: category rail items, tool tiles and window headers.

use wm_core::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Response, Sense, Stroke, Ui, Vec2};
use wm_core::ui::palette;

/// The app's accent colour (used for selection and icons).
pub use wm_core::ui::ACCENT;

/// Header/band background for the current theme.
pub fn band_fill(ui: &Ui) -> Color32 {
    palette::band(ui.visuals().dark_mode)
}

/// Rail background for the current theme.
pub fn rail_fill(ui: &Ui) -> Color32 {
    palette::rail(ui.visuals().dark_mode)
}

/// A category entry in the left rail: big coloured icon + bold label. The selected
/// entry is filled with the accent and gets a notch pointing into the content area.
pub fn rail_item(
    ui: &mut Ui,
    icon: &str,
    icon_color: Color32,
    label: &str,
    selected: bool,
    content_fill: Color32,
    height: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let p = ui.painter();
    let text_color = if selected {
        Color32::WHITE
    } else if resp.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    if selected {
        p.rect_filled(rect, CornerRadius::ZERO, ACCENT);
        let (r, cy) = (rect.right(), rect.center().y);
        p.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(r, cy - 9.0),
                Pos2::new(r - 9.0, cy),
                Pos2::new(r, cy + 9.0),
            ],
            content_fill,
            Stroke::NONE,
        ));
    } else if resp.hovered() {
        p.rect_filled(
            rect,
            CornerRadius::ZERO,
            ui.visuals().widgets.hovered.weak_bg_fill,
        );
    }
    let icon_color = if selected { Color32::WHITE } else { icon_color };
    p.text(
        Pos2::new(rect.left() + 18.0, rect.center().y),
        Align2::LEFT_CENTER,
        icon,
        FontId::proportional(26.0),
        icon_color,
    );
    p.text(
        Pos2::new(rect.left() + 56.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(16.0),
        text_color,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small entry in the rail's bottom menu (Settings, Change Journal, Update, About).
pub fn rail_link(ui: &mut Ui, icon: &str, label: &str, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(
            rect,
            CornerRadius::ZERO,
            ui.visuals().widgets.hovered.weak_bg_fill,
        );
    }
    let text_color = if resp.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    p.text(
        Pos2::new(rect.left() + 22.0, rect.center().y),
        Align2::LEFT_CENTER,
        icon,
        FontId::proportional(17.0),
        color,
    );
    p.text(
        Pos2::new(rect.left() + 56.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.5),
        text_color,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Height of a tile row.
pub const TILE_HEIGHT: f32 = 96.0;

/// A tool tile like the original's: a large coloured icon, a bold title and a grey
/// two-line description. No frame at rest; a faint fill on hover.
pub fn tile(ui: &mut Ui, width: f32, icon: &str, color: Color32, title: &str, description: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, TILE_HEIGHT), Sense::click());
    let visuals = ui.visuals().clone();
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(6), visuals.widgets.hovered.weak_bg_fill);
    }
    p.text(
        Pos2::new(rect.left() + 34.0, rect.top() + 34.0),
        Align2::CENTER_CENTER,
        icon,
        FontId::proportional(44.0),
        color,
    );
    let text_left = rect.left() + 74.0;
    p.text(
        Pos2::new(text_left, rect.top() + 12.0),
        Align2::LEFT_TOP,
        title,
        FontId::proportional(16.5),
        visuals.strong_text_color(),
    );
    // At most two lines, ending in "…" when the text is longer, as in the original.
    let mut job = egui::text::LayoutJob::simple(
        description.to_owned(),
        FontId::proportional(13.0),
        visuals.weak_text_color(),
        (rect.right() - text_left - 30.0).max(40.0),
    );
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        Pos2::new(text_left, rect.top() + 36.0),
        galley,
        visuals.weak_text_color(),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The slim band at the top of each tool window: icon, title, description and help.
pub fn tool_header(ui: &mut Ui, icon: &str, color: Color32, title: &str, description: &str) {
    egui::Frame::new()
        .fill(band_fill(ui))
        .inner_margin(egui::Margin::symmetric(14, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon).size(22.0).color(color));
                ui.label(egui::RichText::new(title).size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(wm_core::icons::QUESTION).size(18.0).weak())
                        .on_hover_text(description);
                    if !description.is_empty() {
                        ui.add(egui::Label::new(egui::RichText::new(description).weak()).truncate());
                    }
                });
            });
        });
}
