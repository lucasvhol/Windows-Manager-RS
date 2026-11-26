//! Custom-painted hub widgets: category rail items, tool tiles and window headers.

use wm_core::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2,
};
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
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 54.0), Sense::click());
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
    let galley = ui.painter().layout(
        description.to_owned(),
        FontId::proportional(13.0),
        visuals.weak_text_color(),
        (rect.right() - text_left - 30.0).max(40.0),
    );
    // Show at most two lines.
    let clip = Rect::from_min_size(
        Pos2::new(text_left, rect.top() + 36.0),
        Vec2::new(rect.width(), 38.0),
    );
    ui.painter()
        .with_clip_rect(clip)
        .galley(clip.min, galley, visuals.weak_text_color());
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

/// A titled card used on the Home page.
pub fn card<R>(ui: &mut Ui, title: &str, icon: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(format!("{icon}  {title}"))
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(8.0);
            add(ui)
        })
        .inner
}

/// Buttons laid out in equal-width columns, `per_row` to a row, so a group never
/// leaves a lone button on a line of its own. Returns the index clicked.
pub fn button_grid(ui: &mut Ui, labels: &[String], per_row: usize) -> Option<usize> {
    let mut clicked = None;
    let gap = ui.spacing().item_spacing.x;
    let per_row = per_row.max(1);
    let w = ((ui.available_width() - gap * (per_row as f32 - 1.0)) / per_row as f32).max(60.0);
    for (r, row) in labels.chunks(per_row).enumerate() {
        ui.horizontal(|ui| {
            for (c, label) in row.iter().enumerate() {
                let b = egui::Button::new(egui::RichText::new(label).size(13.0))
                    .min_size(Vec2::new(w, 26.0))
                    .truncate();
                if ui.add_sized([w, 26.0], b).on_hover_text(label).clicked() {
                    clicked = Some(r * per_row + c);
                }
            }
        });
    }
    clicked
}

/// A flat icon + label button for the hub header (the original's Update and About).
pub fn header_button(ui: &mut Ui, icon: &str, label: &str) -> Response {
    let text = egui::RichText::new(format!("{icon}  {label}"))
        .size(14.5)
        .strong();
    ui.add(
        egui::Button::new(text)
            .frame(false)
            .min_size(Vec2::new(0.0, 32.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}
