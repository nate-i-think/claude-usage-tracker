//! The egui dashboard. Never does I/O: everything it shows comes from the
//! background [`crate::poller::Poller`].
//!
//! Text is built as [`Run`]s (plain strings tagged with a [`Tone`]) by the
//! testable formatters in [`rows`], then turned into egui text here. Colors,
//! fonts and spacing come from [`theme`], the one source of visual truth.

pub mod accounts;
pub mod alerts;
pub mod app;
pub mod banners;
pub mod chips;
pub mod details;
pub mod digits;
pub mod footer;
pub mod layout;
pub mod mode;
pub mod motion;
pub mod notify;
pub mod pill;
pub mod rows;
pub mod settings;
pub mod theme;
pub mod timeline;
pub mod tray;
pub mod week;
pub mod window;

pub use app::App;

use eframe::egui::{self, text::LayoutJob, Color32, TextFormat, TextStyle, Ui, Visuals};

use crate::analytics::Level;

use theme::Palette;

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
    /// The app's accent (Rich "cyan"): the "claude.ai usage" title of the
    /// text report.
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

/// Color of a severity level on the current theme.
pub fn level_color(level: Level, visuals: &Visuals) -> Color32 {
    Palette::for_visuals(visuals).level(level)
}

/// Color of a tone on the current theme.
pub fn tone_color(tone: Tone, visuals: &Visuals) -> Color32 {
    Palette::for_visuals(visuals).tone(tone)
}

/// Progress-bar track color on the current theme.
pub fn bar_track_color(visuals: &Visuals) -> Color32 {
    Palette::for_visuals(visuals).track
}

/// The runs as one egui text job in the body font; `Strong` runs semibold.
pub fn layout_job(runs: &[Run], ui: &Ui) -> LayoutJob {
    let body = TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    for run in runs {
        let format = TextFormat::simple(
            theme::font_for(run.tone, &body),
            tone_color(run.tone, ui.visuals()),
        );
        job.append(&run.text, 0.0, format);
    }
    job
}

/// Show the runs as a wrapping label.
pub fn show_runs(ui: &mut Ui, runs: &[Run]) -> egui::Response {
    ui.add(egui::Label::new(layout_job(runs, ui)).wrap())
}
