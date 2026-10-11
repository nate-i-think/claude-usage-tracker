//! The egui dashboard. Never does I/O: everything it shows comes from the
//! background [`crate::poller::Poller`].
//!
//! Text is built as [`Run`]s (plain strings tagged with a [`Tone`]) by the
//! testable formatters in [`rows`], then turned into egui text here. This
//! module is the one place tones and severity [`Level`]s become colors.

pub mod app;
pub mod charts;
pub mod rows;
pub mod settings;

pub use app::App;

use eframe::egui::{self, text::LayoutJob, Color32, TextFormat, TextStyle, Ui, Visuals};

use crate::analytics::{Level, FABLE_ORANGE};

/// How a piece of dashboard text is styled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Default text color.
    Normal,
    /// Emphasized (Rich "bold"): the theme's strong text color.
    Strong,
    /// De-emphasized (Rich "dim"): the theme's weak text color.
    Dim,
    /// Severity color.
    Level(Level),
    /// The app's accent (Rich "cyan"): title and the ALL burn tag.
    Accent,
    /// Claude coral (#d97757): the FABLE burn tag.
    Fable,
}

/// A styled piece of text.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    pub tone: Tone,
}

impl Run {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

/// The runs' text concatenated, without styling.
pub fn plain_text(runs: &[Run]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

/// Color of a severity level, picked to stay readable on the current theme.
pub fn level_color(level: Level, visuals: &Visuals) -> Color32 {
    let dark = visuals.dark_mode;
    match level {
        Level::Dim => visuals.weak_text_color(),
        Level::Green if dark => Color32::from_rgb(0x4c, 0xc3, 0x5a),
        Level::Green => Color32::from_rgb(0x1a, 0x7f, 0x37),
        Level::Yellow if dark => Color32::from_rgb(0xe0, 0xc0, 0x40),
        Level::Yellow => Color32::from_rgb(0x9a, 0x67, 0x00),
        // Rich "orange3" is #d78700.
        Level::Orange if dark => Color32::from_rgb(0xd7, 0x87, 0x00),
        Level::Orange => Color32::from_rgb(0xb3, 0x59, 0x00),
        Level::Red if dark => Color32::from_rgb(0xf0, 0x55, 0x55),
        Level::Red => Color32::from_rgb(0xcf, 0x22, 0x2e),
    }
}

/// Color of a tone on the current theme.
pub fn tone_color(tone: Tone, visuals: &Visuals) -> Color32 {
    match tone {
        Tone::Normal => visuals.text_color(),
        Tone::Strong => visuals.strong_text_color(),
        Tone::Dim => visuals.weak_text_color(),
        Tone::Level(level) => level_color(level, visuals),
        Tone::Accent if visuals.dark_mode => Color32::from_rgb(0x4f, 0xc8, 0xdc),
        Tone::Accent => Color32::from_rgb(0x0b, 0x72, 0x85),
        Tone::Fable if visuals.dark_mode => {
            let [r, g, b] = FABLE_ORANGE;
            Color32::from_rgb(r, g, b)
        }
        // #d97757 darkened to stay readable on a light panel.
        Tone::Fable => Color32::from_rgb(0xb0, 0x4f, 0x30),
    }
}

/// Progress-bar track color: a visible groove on either theme.
pub fn bar_track_color(visuals: &Visuals) -> Color32 {
    if visuals.dark_mode {
        Color32::from_gray(58)
    } else {
        Color32::from_gray(212)
    }
}

/// The runs as one egui text job in the body font.
pub fn layout_job(runs: &[Run], ui: &Ui) -> LayoutJob {
    let font = TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    for run in runs {
        let format = TextFormat::simple(font.clone(), tone_color(run.tone, ui.visuals()));
        job.append(&run.text, 0.0, format);
    }
    job
}

/// Font setup: the default proportional font (Ubuntu) has no arrows, so
/// fall back to the bundled monospace font for glyphs like "→".
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mono_first = fonts
        .families
        .get(&egui::FontFamily::Monospace)
        .and_then(|f| f.first())
        .cloned();
    if let (Some(mono), Some(prop)) = (
        mono_first,
        fonts.families.get_mut(&egui::FontFamily::Proportional),
    ) {
        prop.push(mono);
    }
    ctx.set_fonts(fonts);
}

/// Show the runs as a wrapping label.
pub fn show_runs(ui: &mut Ui, runs: &[Run]) -> egui::Response {
    ui.add(egui::Label::new(layout_job(runs, ui)).wrap())
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

    const COLORED: [Tone; 7] = [
        Tone::Level(Level::Green),
        Tone::Level(Level::Yellow),
        Tone::Level(Level::Orange),
        Tone::Level(Level::Red),
        Tone::Accent,
        Tone::Fable,
        Tone::Strong,
    ];

    #[test]
    fn colored_text_readable_on_both_themes() {
        for visuals in [Visuals::dark(), Visuals::light()] {
            for tone in COLORED {
                let ratio = contrast(tone_color(tone, &visuals), visuals.panel_fill);
                assert!(
                    ratio >= 3.0,
                    "{tone:?} contrast {ratio:.2} (dark_mode={})",
                    visuals.dark_mode
                );
            }
        }
    }

    #[test]
    fn fable_is_brand_coral_on_dark_and_darker_on_light() {
        let [r, g, b] = FABLE_ORANGE;
        let coral = Color32::from_rgb(r, g, b);
        assert_eq!(tone_color(Tone::Fable, &Visuals::dark()), coral);
        assert!(luminance(tone_color(Tone::Fable, &Visuals::light())) < luminance(coral));
    }

    #[test]
    fn bar_track_visible_on_both_themes() {
        for visuals in [Visuals::dark(), Visuals::light()] {
            let ratio = contrast(bar_track_color(&visuals), visuals.panel_fill);
            assert!(
                ratio >= 1.25,
                "track contrast {ratio:.2} (dark_mode={})",
                visuals.dark_mode
            );
        }
    }
}
