//! The "Claude warm" theme: the one place the GUI's colors, fonts, corner
//! radii, shadows and spacing are defined.
//!
//! Two [`Palette`]s (warm charcoal [`DARK`], cream [`LIGHT`]) name every color
//! role the widgets use. [`install`] bundles Inter and writes a styled
//! [`Style`] for *both* egui themes, so a dark/light flip (system or user
//! preference) needs no further work: egui keeps one style per theme and
//! switches between them itself.
//!
//! Contrast rules, enforced by the tests below on both palettes:
//! - text roles (`text`, `text_muted`) and state colors, which color numbers
//!   and labels: at least 4.5:1 on `bg` and `surface`;
//! - graphic roles (`accent`, `fable`, `text_faint`, bars, chips, chart
//!   fills): at least 3:1 on `bg` and `surface`. `accent`/`fable` reach
//!   4.5:1 on `bg` (where the title and burn tag sit) but not on the dark
//!   `surface`, so text on cards should not use them.

use eframe::egui::{
    self, epaint::Shadow, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
    Frame, Margin, Response, Stroke, Style, TextStyle, Theme, Ui, Visuals,
};
use egui::epaint::text::{FontTweak, VariationCoords};

use super::Tone;
use crate::analytics::{Level, FABLE_ORANGE};

/// Inter 4.1 variable (rsms/inter release v4.1, `InterVariable.ttf`, SIL OFL
/// 1.1; license text in `assets/fonts/OFL.txt`, copied from the release's
/// `LICENSE.txt`).
const INTER: &[u8] = include_bytes!("../../assets/fonts/InterVariable.ttf");
/// Font-data key of Inter at its default instance (weight 400).
const INTER_KEY: &str = "Inter";
/// Font-data key of the same Inter file pinned to weight 600.
const INTER_SEMIBOLD_KEY: &str = "Inter SemiBold";
/// Family name for semibold text (headings, big numbers, strong runs).
pub const SEMIBOLD: &str = "semibold";

/// Text style for the big headline percentage (~28 px).
pub const HERO: &str = "Hero";
/// Text style for the stat-chip percentages (~20 px).
pub const CHIP: &str = "Chip";

/// Corner radius of buttons, inputs and other widgets.
pub const WIDGET_RADIUS: u8 = 8;
/// Corner radius of windows, cards and frames.
pub const CARD_RADIUS: u8 = 12;
/// Corner radius of checkbox boxes, so they read as boxes, not pills.
pub const CHECKBOX_RADIUS: u8 = 4;

/// Every color role the GUI uses, for one theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window/panel background.
    pub bg: Color32,
    /// Cards, frames, popups.
    pub surface: Color32,
    /// Buttons and other raised controls on a surface.
    pub raised: Color32,
    /// Raised controls under the pointer.
    pub raised_hover: Color32,
    /// Text inputs (egui's `extreme_bg_color`).
    pub field: Color32,
    /// Hairlines: separators, card and input outlines.
    pub border: Color32,
    /// Edges of interactive controls (inputs, checkboxes, buttons): at
    /// least 3:1 on `bg` and `surface` so controls read as controls.
    pub control_border: Color32,
    pub text: Color32,
    /// Secondary text: axis labels, captions, "weak" egui text.
    pub text_muted: Color32,
    /// Decorative only (placeholders, minor ticks); too faint for text.
    pub text_faint: Color32,
    /// Clay: title, links, focus.
    pub accent: Color32,
    /// Accent tint behind selected items; text on it stays `text`.
    pub accent_soft: Color32,
    /// Clay for text on cards and controls: 4.5:1 on `surface` and `raised`.
    pub accent_text: Color32,
    /// The Fable layer/tag (Claude coral).
    pub fable: Color32,
    /// State colors, low to high.
    pub good: Color32,
    pub warn: Color32,
    pub high: Color32,
    pub danger: Color32,
    /// Neutral chart bars; a clear lightness step from `fable`, so the
    /// Fable floor reads without color vision.
    pub bar: Color32,
    /// Chart gridlines.
    pub grid: Color32,
    /// Empty part of progress bars.
    pub track: Color32,
    /// Window, popup and card shadows.
    pub shadow: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Warm charcoal.
pub const DARK: Palette = Palette {
    dark: true,
    bg: hex(0x262624),
    surface: hex(0x30302E),
    raised: hex(0x3A3936),
    raised_hover: hex(0x45443F),
    field: hex(0x1F1E1D),
    border: hex(0x4A4844),
    control_border: hex(0x858279),
    text: hex(0xF5F4EF),
    text_muted: hex(0xA6A39A),
    text_faint: hex(0x7E7B73),
    accent: hex(0xD97757),
    accent_soft: hex(0x4E3D35),
    accent_text: hex(0xE3906F),
    fable: Color32::from_rgb(FABLE_ORANGE[0], FABLE_ORANGE[1], FABLE_ORANGE[2]),
    good: hex(0x7FB069),
    warn: hex(0xE0B341),
    high: hex(0xECA04A),
    danger: hex(0xF07070),
    bar: hex(0xC4C1B8),
    grid: hex(0x464540),
    track: hex(0x4A4844),
    shadow: Color32::from_black_alpha(90),
};

/// Cream.
pub const LIGHT: Palette = Palette {
    dark: false,
    bg: hex(0xFAF9F5),
    surface: hex(0xF0EEE6),
    raised: hex(0xE6E3D8),
    raised_hover: hex(0xDDD9CC),
    field: hex(0xFFFFFF),
    border: hex(0xD6D2C4),
    control_border: hex(0x807D74),
    text: hex(0x141413),
    text_muted: hex(0x6B6A65),
    text_faint: hex(0x8A877E),
    accent: hex(0xB04F30),
    accent_soft: hex(0xE4D1C5),
    accent_text: hex(0x9C4228),
    // #d97757 darkened to stay readable on cream.
    fable: hex(0xB04F30),
    good: hex(0x3B7A2A),
    warn: hex(0x8A6400),
    high: hex(0x864700),
    danger: hex(0xB8312A),
    bar: hex(0x4A4844),
    grid: hex(0xDCD8CB),
    track: hex(0xD9D5C8),
    shadow: Color32::from_black_alpha(28),
};

impl Palette {
    /// The palette matching the current egui visuals.
    pub fn for_visuals(visuals: &Visuals) -> &'static Palette {
        if visuals.dark_mode {
            &DARK
        } else {
            &LIGHT
        }
    }

    /// Color of a severity level.
    pub fn level(&self, level: Level) -> Color32 {
        match level {
            Level::Dim => self.text_muted,
            Level::Green => self.good,
            Level::Yellow => self.warn,
            Level::Orange => self.high,
            Level::Red => self.danger,
        }
    }

    /// Color of a text tone. `Strong` differs from `Normal` by weight (see
    /// [`font_for`]), not color.
    pub fn tone(&self, tone: Tone) -> Color32 {
        match tone {
            Tone::Normal | Tone::Strong => self.text,
            Tone::Dim => self.text_muted,
            Tone::Level(level) => self.level(level),
            Tone::Accent => self.accent,
            Tone::Fable => self.fable,
        }
    }

    fn theme(&self) -> Theme {
        if self.dark {
            Theme::Dark
        } else {
            Theme::Light
        }
    }
}

/// Fonts plus both themes' styles. Call once at startup.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
    apply_style(ctx, &DARK);
    apply_style(ctx, &LIGHT);
}

/// Style the egui theme `palette` belongs to (dark or light).
pub fn apply_style(ctx: &egui::Context, palette: &Palette) {
    ctx.style_mut_of(palette.theme(), |style| style_with(style, palette));
}

/// Inter first in the proportional family, egui's fonts behind it as
/// fallbacks (arrows, ⚙, emoji); a semibold family on the same file.
pub fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert(INTER_KEY.to_owned(), FontData::from_static(INTER).into());
    let semibold = FontTweak {
        coords: VariationCoords::new([(b"wght", 600.0)]),
        ..FontTweak::default()
    };
    fonts.font_data.insert(
        INTER_SEMIBOLD_KEY.to_owned(),
        FontData::from_static(INTER).tweak(semibold).into(),
    );

    let mut fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    // Hack (monospace) covers a few symbols the proportional fonts lack.
    if let Some(mono) = fonts
        .families
        .get(&FontFamily::Monospace)
        .and_then(|f| f.first())
    {
        if !fallbacks.contains(mono) {
            fallbacks.push(mono.clone());
        }
    }
    let family = |first: &str| {
        std::iter::once(first.to_owned())
            .chain(fallbacks.iter().cloned())
            .collect::<Vec<_>>()
    };
    fonts
        .families
        .insert(FontFamily::Proportional, family(INTER_KEY));
    fonts.families.insert(
        FontFamily::Name(SEMIBOLD.into()),
        family(INTER_SEMIBOLD_KEY),
    );
    fonts
}

/// Semibold Inter at `size`.
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

/// The font for a run of `tone` at body size `body`: semibold for `Strong`.
pub fn font_for(tone: Tone, body: &FontId) -> FontId {
    match tone {
        Tone::Strong => semibold(body.size),
        _ => body.clone(),
    }
}

/// A checkbox with [`CHECKBOX_RADIUS`] corners (egui shares one radius
/// across all widgets, so the box would otherwise get the button radius).
pub fn checkbox(ui: &mut Ui, checked: &mut bool, text: &str) -> Response {
    ui.scope(|ui| {
        checkbox_corners(ui.visuals_mut());
        ui.checkbox(checked, text)
    })
    .inner
}

/// Tighten the interactive widget corners to [`CHECKBOX_RADIUS`].
fn checkbox_corners(visuals: &mut Visuals) {
    let w = &mut visuals.widgets;
    for state in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        state.corner_radius = CornerRadius::same(CHECKBOX_RADIUS);
    }
}

/// A rounded card on the surface color with a soft shadow.
pub fn card(palette: &Palette) -> Frame {
    Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.border))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::same(10))
        .shadow(Shadow {
            offset: [0, 2],
            blur: 8,
            spread: 0,
            color: palette.shadow,
        })
}

/// Write `palette` into `style`: visuals, spacing and text styles.
pub fn style_with(style: &mut Style, p: &Palette) {
    style.visuals = visuals(p);

    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 6.0);
    s.button_padding = egui::vec2(10.0, 4.0);
    s.window_margin = Margin::same(12);
    s.menu_margin = Margin::same(8);
    s.interact_size.y = 22.0;
    s.indent = 16.0;

    let prop = |size| FontId::proportional(size);
    style.text_styles = [
        (TextStyle::Small, prop(11.5)),
        (TextStyle::Body, prop(14.0)),
        (TextStyle::Button, prop(14.0)),
        (TextStyle::Heading, semibold(18.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
        (TextStyle::Name(CHIP.into()), semibold(20.0)),
        (TextStyle::Name(HERO.into()), semibold(28.0)),
    ]
    .into();
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let widget = CornerRadius::same(WIDGET_RADIUS);
    let card = CornerRadius::same(CARD_RADIUS);

    v.dark_mode = p.dark;
    v.override_text_color = None;
    v.weak_text_color = Some(p.text_muted);
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = card;
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: p.shadow,
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: p.shadow,
    };
    v.faint_bg_color = p.surface;
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.code_bg_color = p.raised;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.high;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.text);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = widget;

    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::new(1.0, p.control_border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = widget;

    w.hovered.bg_fill = p.raised_hover;
    w.hovered.weak_bg_fill = p.raised_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.control_border);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = widget;

    w.active.bg_fill = p.accent_soft;
    w.active.weak_bg_fill = p.accent_soft;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(2.0, p.text);
    w.active.corner_radius = widget;

    w.open.bg_fill = p.surface;
    w.open.weak_bg_fill = p.raised;
    w.open.bg_stroke = Stroke::new(1.0, p.border);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = widget;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance.
    fn luminance(c: Color32) -> f64 {
        let lin = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    /// WCAG contrast ratio, >= 1.
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    const TEXT: f64 = 4.5;
    const GRAPHIC: f64 = 3.0;

    fn assert_contrast(p: &Palette, name: &str, fg: Color32, bg_name: &str, bg: Color32, min: f64) {
        let ratio = contrast(fg, bg);
        assert!(
            ratio >= min,
            "{name} on {bg_name}: {ratio:.2} < {min} (dark={})",
            p.dark
        );
    }

    fn palettes() -> [&'static Palette; 2] {
        [&DARK, &LIGHT]
    }

    #[test]
    fn text_and_state_colors_readable_on_bg_and_surface() {
        for p in palettes() {
            let roles = [
                ("text", p.text),
                ("text_muted", p.text_muted),
                ("good", p.good),
                ("warn", p.warn),
                ("high", p.high),
                ("danger", p.danger),
            ];
            for (name, fg) in roles {
                assert_contrast(p, name, fg, "bg", p.bg, TEXT);
                assert_contrast(p, name, fg, "surface", p.surface, TEXT);
            }
        }
    }

    #[test]
    fn accent_and_fable_text_on_bg_graphic_on_surface() {
        for p in palettes() {
            for (name, fg) in [("accent", p.accent), ("fable", p.fable)] {
                assert_contrast(p, name, fg, "bg", p.bg, TEXT);
                assert_contrast(p, name, fg, "surface", p.surface, GRAPHIC);
            }
        }
    }

    #[test]
    fn faint_is_graphic_grade() {
        for p in palettes() {
            assert_contrast(p, "text_faint", p.text_faint, "bg", p.bg, GRAPHIC);
            assert_contrast(p, "text_faint", p.text_faint, "surface", p.surface, GRAPHIC);
        }
    }

    #[test]
    fn controls_keep_text_readable() {
        for p in palettes() {
            for (name, bg) in [
                ("raised", p.raised),
                ("raised_hover", p.raised_hover),
                ("accent_soft", p.accent_soft),
                ("field", p.field),
            ] {
                assert_contrast(p, "text", p.text, name, bg, TEXT);
            }
            // Muted text also appears as hint text in inputs.
            assert_contrast(p, "text_muted", p.text_muted, "field", p.field, TEXT);
        }
    }

    #[test]
    fn hairlines_and_tracks_visible_but_quiet() {
        for p in palettes() {
            for (name, fg) in [("border", p.border), ("track", p.track)] {
                assert_contrast(p, name, fg, "bg", p.bg, 1.25);
            }
            assert_contrast(p, "grid", p.grid, "surface", p.surface, 1.15);
            // Quiet: well below the faintest text.
            assert!(contrast(p.grid, p.surface) < contrast(p.text_faint, p.surface));
        }
    }

    #[test]
    fn surfaces_step_away_from_bg() {
        for p in palettes() {
            assert!(contrast(p.surface, p.bg) > 1.05, "dark={}", p.dark);
            assert!(contrast(p.raised, p.bg) > contrast(p.surface, p.bg));
        }
    }

    #[test]
    fn state_colors_distinct_from_fable() {
        // The Fable layer sits next to state-colored fills; orange (`high`)
        // is the nearest hue, so it also needs a lightness step.
        for p in palettes() {
            for fg in [p.good, p.warn, p.high, p.danger] {
                assert_ne!(fg, p.fable);
            }
            assert_contrast(p, "high", p.high, "fable", p.fable, 1.3);
        }
    }

    #[test]
    fn control_edges_visible_on_bg_and_surface() {
        for p in palettes() {
            let edge = p.control_border;
            assert_contrast(p, "control_border", edge, "bg", p.bg, GRAPHIC);
            assert_contrast(p, "control_border", edge, "surface", p.surface, GRAPHIC);
            let mut style = Style::default();
            style_with(&mut style, p);
            let w = &style.visuals.widgets;
            // Inputs, checkboxes and buttons draw their edge with these.
            assert_eq!(w.inactive.bg_stroke, Stroke::new(1.0, edge));
            assert_eq!(w.hovered.bg_stroke, Stroke::new(1.0, edge));
        }
    }

    #[test]
    fn accent_text_readable_on_cards_and_controls() {
        for p in palettes() {
            for (name, bg) in [("bg", p.bg), ("surface", p.surface), ("raised", p.raised)] {
                assert_contrast(p, "accent_text", p.accent_text, name, bg, TEXT);
            }
        }
    }

    #[test]
    fn chart_bar_graphic_and_apart_from_fable() {
        for p in palettes() {
            assert_contrast(p, "bar", p.bar, "surface", p.surface, GRAPHIC);
            assert_contrast(p, "bar", p.bar, "fable", p.fable, 1.5);
        }
    }

    #[test]
    fn checkbox_corners_tighter_than_widgets() {
        let mut style = Style::default();
        style_with(&mut style, &DARK);
        let mut v = style.visuals.clone();
        checkbox_corners(&mut v);
        let r = CornerRadius::same(CHECKBOX_RADIUS);
        let w = &v.widgets;
        for state in [&w.inactive, &w.hovered, &w.active, &w.open] {
            assert_eq!(state.corner_radius, r);
        }
        const { assert!(CHECKBOX_RADIUS < WIDGET_RADIUS) };
    }

    #[test]
    fn fable_is_brand_coral_on_dark_and_darker_on_light() {
        let [r, g, b] = FABLE_ORANGE;
        let coral = Color32::from_rgb(r, g, b);
        assert_eq!(DARK.fable, coral);
        assert!(luminance(LIGHT.fable) < luminance(coral));
    }

    #[test]
    fn design_anchor_colors() {
        assert_eq!(DARK.bg, hex(0x262624));
        assert_eq!(DARK.accent, hex(0xD97757));
        assert_eq!(LIGHT.bg, hex(0xFAF9F5));
        assert_eq!(LIGHT.accent, hex(0xB04F30));
    }

    #[test]
    fn for_visuals_follows_dark_mode() {
        assert_eq!(Palette::for_visuals(&Visuals::dark()), &DARK);
        assert_eq!(Palette::for_visuals(&Visuals::light()), &LIGHT);
    }

    #[test]
    fn tones_and_levels_map_to_roles() {
        for p in palettes() {
            assert_eq!(p.tone(Tone::Normal), p.text);
            assert_eq!(p.tone(Tone::Strong), p.text);
            assert_eq!(p.tone(Tone::Dim), p.text_muted);
            assert_eq!(p.tone(Tone::Accent), p.accent);
            assert_eq!(p.tone(Tone::Fable), p.fable);
            assert_eq!(p.tone(Tone::Level(Level::Red)), p.danger);
            assert_eq!(p.level(Level::Green), p.good);
            assert_eq!(p.level(Level::Yellow), p.warn);
            assert_eq!(p.level(Level::Orange), p.high);
            assert_eq!(p.level(Level::Dim), p.text_muted);
        }
    }

    #[test]
    fn style_uses_palette_and_rounding() {
        for p in palettes() {
            let mut style = Style::default();
            style_with(&mut style, p);
            let v = &style.visuals;
            assert_eq!(v.dark_mode, p.dark);
            assert_eq!(v.panel_fill, p.bg);
            assert_eq!(v.window_fill, p.surface);
            assert_eq!(v.text_color(), p.text);
            assert_eq!(v.weak_text_color(), p.text_muted);
            assert_eq!(v.strong_text_color(), p.text);
            assert_eq!(v.extreme_bg_color, p.field);
            assert_eq!(v.hyperlink_color, p.accent);
            assert_eq!(v.window_corner_radius, CornerRadius::same(CARD_RADIUS));
            assert_eq!(
                v.widgets.inactive.corner_radius,
                CornerRadius::same(WIDGET_RADIUS)
            );
            for name in [HERO, CHIP] {
                assert!(style
                    .text_styles
                    .contains_key(&TextStyle::Name(name.into())));
            }
            assert!(
                style.text_styles[&TextStyle::Name(HERO.into())].size
                    > style.text_styles[&TextStyle::Name(CHIP.into())].size
            );
        }
    }

    /// Whether the font data registered as `key` maps `c`.
    fn maps(defs: &FontDefinitions, key: &str, c: char) -> bool {
        use skrifa::MetadataProvider as _;
        let data = &defs.font_data[key];
        skrifa::FontRef::from_index(&data.font, data.index)
            .map(|f| f.charmap().map(c).is_some())
            .unwrap_or(false)
    }

    #[test]
    fn inter_first_with_symbol_fallbacks() {
        let defs = font_definitions();
        let semibold_family = FontFamily::Name(SEMIBOLD.into());
        assert_eq!(defs.families[&FontFamily::Proportional][0], INTER_KEY);
        assert_eq!(defs.families[&semibold_family][0], INTER_SEMIBOLD_KEY);
        // Digits and punctuation come from Inter itself.
        for c in "0123456789%.:+-".chars() {
            assert!(maps(&defs, INTER_KEY, c), "Inter lacks {c:?}");
        }
        // Symbols the current and planned UI uses resolve somewhere in the
        // chain. (egui's `has_glyph` gives false negatives for glyphs in the
        // replacement character's face, so read the charmaps directly.)
        // "✕" (U+2715) is in no reachable charmap (nor was it before Inter):
        // use "×" or paint the shape.
        for family in [FontFamily::Proportional, semibold_family] {
            let chain = &defs.families[&family];
            let missing: String = "→⟳⚙◱▲▼×📌🔄…—·●◔"
                .chars()
                .filter(|&c| !chain.iter().any(|key| maps(&defs, key, c)))
                .collect();
            assert!(missing.is_empty(), "{family:?} lacks {missing:?}");
        }
    }

    #[test]
    fn semibold_is_heavier_inter() {
        let defs = font_definitions();
        let regular = &defs.font_data[INTER_KEY];
        let semibold = &defs.font_data[INTER_SEMIBOLD_KEY];
        assert_eq!(regular.font, semibold.font);
        assert!(regular.tweak.coords == VariationCoords::default());
        assert!(semibold.tweak.coords == VariationCoords::new([(b"wght", 600.0)]));
        let axes = FontData::from_static(INTER).variation_axes();
        let wght = axes.iter().find(|a| a.tag == *b"wght").unwrap();
        assert_eq!(wght.default, 400.0);
        assert!(wght.range.contains(600.0));
    }
}
