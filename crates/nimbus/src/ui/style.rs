use egui::{Color32, Stroke};

pub const ACCENT: Color32 = Color32::from_rgb(16, 124, 16);
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(58, 196, 58);
pub const BG: Color32 = Color32::from_rgb(10, 12, 11);
pub const BG_CARD: Color32 = Color32::from_rgb(28, 32, 30);
pub const BG_CARD_HOVER: Color32 = Color32::from_rgb(38, 44, 41);
pub const TEXT_DIM: Color32 = Color32::from_rgb(150, 160, 155);

pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(30.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(17.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(17.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(13.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(14.0, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(16.0, 8.0);
        style.spacing.interact_size.y = 34.0;

        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = Color32::from_rgb(22, 25, 24);
        v.extreme_bg_color = Color32::from_rgb(20, 23, 22);
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.5, Color32::WHITE);
        v.hyperlink_color = ACCENT_BRIGHT;

        let w = &mut v.widgets;
        w.inactive.weak_bg_fill = Color32::from_rgb(36, 41, 39);
        w.inactive.bg_fill = Color32::from_rgb(36, 41, 39);
        w.inactive.fg_stroke = Stroke::new(1.0, Color32::from_gray(225));
        w.hovered.weak_bg_fill = Color32::from_rgb(52, 60, 56);
        w.hovered.bg_fill = Color32::from_rgb(52, 60, 56);
        w.hovered.bg_stroke = Stroke::new(1.0, ACCENT_BRIGHT);
        // Keyboard / controller focus uses the "active" style: make it loud.
        w.active.weak_bg_fill = ACCENT;
        w.active.bg_fill = ACCENT;
        w.active.bg_stroke = Stroke::new(2.5, Color32::WHITE);
        w.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    });
}
