//! Notice cards above the dashboard (errors, warnings, loading): a rounded
//! card tinted by its kind with a round icon badge on the left. What each
//! banner says, and when it shows, stays in `app.rs`.

use eframe::egui::{
    vec2, Align, Align2, Color32, Frame, Layout, Margin, Sense, Stroke, TextStyle, Ui,
};

use super::theme::{self, Palette};

/// Diameter of the icon badge.
const BADGE: f32 = 18.0;

/// What a banner is about; sets its color and icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerKind {
    /// Something is broken (red "!").
    Error,
    /// Data is fine but something needs attention (orange "!").
    Warning,
    /// Waiting on the first poll (muted, spinner).
    Loading,
}

impl BannerKind {
    /// The kind's color on this palette.
    pub fn color(self, palette: &Palette) -> Color32 {
        match self {
            BannerKind::Error => palette.danger,
            BannerKind::Warning => palette.high,
            BannerKind::Loading => palette.text_muted,
        }
    }
}

/// A full-width notice card: icon badge, then `add` in the remaining width.
pub fn banner(ui: &mut Ui, kind: BannerKind, add: impl FnOnce(&mut Ui)) {
    let palette = Palette::for_visuals(ui.visuals());
    let color = kind.color(palette);
    Frame::new()
        .fill(blend(palette.surface, color, 0.10))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(theme::CARD_RADIUS)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                badge(ui, kind, color, palette);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    add(ui);
                });
            });
        });
}

/// The round icon: "!" on the kind color, or a spinner while loading.
fn badge(ui: &mut Ui, kind: BannerKind, color: Color32, palette: &Palette) {
    if kind == BannerKind::Loading {
        ui.add_sized(
            vec2(BADGE, BADGE),
            eframe::egui::Spinner::new().size(BADGE - 2.0),
        );
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(BADGE, BADGE), Sense::hover());
    let painter = ui.painter();
    painter.circle_filled(rect.center(), BADGE / 2.0, color);
    let font = theme::semibold(TextStyle::Small.resolve(ui.style()).size + 1.0);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "!",
        font,
        palette.surface,
    );
}

/// `a` with `t` of `b` mixed in (sRGB, opaque).
fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{DARK, LIGHT};

    #[test]
    fn kinds_use_state_colors() {
        for p in [&DARK, &LIGHT] {
            assert_eq!(BannerKind::Error.color(p), p.danger);
            assert_eq!(BannerKind::Warning.color(p), p.high);
            assert_eq!(BannerKind::Loading.color(p), p.text_muted);
        }
    }

    #[test]
    fn blend_mixes_linearly_in_srgb() {
        let a = Color32::from_rgb(0, 100, 200);
        let b = Color32::from_rgb(100, 100, 0);
        assert_eq!(blend(a, b, 0.0), a);
        assert_eq!(blend(a, b, 1.0), b);
        assert_eq!(blend(a, b, 0.5), Color32::from_rgb(50, 100, 100));
    }
}
