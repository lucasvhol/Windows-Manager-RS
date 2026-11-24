//! Custom-painted hub widgets: category rail items, tool tiles and window headers.

use wm_core::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2,
};

/// The app's accent colour (used for selection and icons).
pub use wm_core::ui::ACCENT;

/// Header/band background for the current theme.
pub fn band_fill(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(30, 34, 42)
    } else {
        Color32::from_rgb(226, 232, 241)
    }
}

/// Rail background for the current theme.
pub fn rail_fill(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(24, 27, 33)
    } else {
        Color32::from_rgb(238, 241, 246)
    }
}

/// A category entry in the left rail: big icon + bold label. The selected entry is
/// filled with the accent and gets a notch pointing into the content area.
pub fn rail_item(ui: &mut Ui, icon: &str, label: &str, selected: bool, content_fill: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::click());
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
    let icon_color = if selected { Color32::WHITE } else { ACCENT };
    p.text(
        Pos2::new(rect.left() + 16.0, rect.center().y),
        Align2::LEFT_CENTER,
        icon,
        FontId::proportional(24.0),
        icon_color,
    );
    p.text(
        Pos2::new(rect.left() + 52.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(15.5),
        text_color,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A tool tile: large icon, bold title and a wrapped two-line description.
pub fn tile(ui: &mut Ui, width: f32, icon: &str, title: &str, description: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 78.0), Sense::click());
    let visuals = ui.visuals().clone();
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(8), visuals.widgets.hovered.weak_bg_fill);
        p.rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(1.0, ACCENT.gamma_multiply(0.6)),
            egui::StrokeKind::Inside,
        );
    }
    p.text(
        Pos2::new(rect.left() + 12.0, rect.top() + 26.0),
        Align2::LEFT_CENTER,
        icon,
        FontId::proportional(34.0),
        ACCENT,
    );
    let text_left = rect.left() + 60.0;
    p.text(
        Pos2::new(text_left, rect.top() + 10.0),
        Align2::LEFT_TOP,
        title,
        FontId::proportional(16.0),
        visuals.strong_text_color(),
    );
    let galley = ui.painter().layout(
        description.to_owned(),
        FontId::proportional(12.5),
        visuals.weak_text_color(),
        (rect.right() - text_left - 30.0).max(40.0),
    );
    // Show at most two lines.
    let clip = Rect::from_min_size(
        Pos2::new(text_left, rect.top() + 32.0),
        Vec2::new(rect.width(), 36.0),
    );
    ui.painter()
        .with_clip_rect(clip)
        .galley(clip.min, galley, visuals.weak_text_color());
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Header band at the top of each tool window: icon, title, description and a help mark.
pub fn tool_header(ui: &mut Ui, icon: &str, title: &str, description: &str) {
    egui::Frame::new()
        .fill(band_fill(ui))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon).size(26.0).color(ACCENT));
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(title).size(17.0).strong());
                    if !description.is_empty() {
                        ui.label(egui::RichText::new(description).weak());
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(wm_core::icons::QUESTION).size(20.0))
                        .on_hover_text(description);
                });
            });
        });
}

/// A titled card used on the Home page.
pub fn card<R>(ui: &mut Ui, title: &str, icon: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(format!("{icon}  {title}"))
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(6.0);
            add(ui)
        })
        .inner
}
