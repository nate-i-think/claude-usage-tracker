//! The 5-hour burn row (shown under the timeline, labelled "5h burn") and
//! the footer strip under the charts: the extra-usage mini gauge.

use chrono::{DateTime, Utc};
use eframe::egui::{vec2, Id, Rect, Sense, Ui};

use crate::analytics::{self, BurnLine, Eta, FiveHourState, Level};
use crate::model::ExtraUsage;
use crate::poller::Dashboard;

use super::theme::Palette;
use super::{digits, motion, rows, Run, Tone};

/// Width of the extra-usage gauge's bar.
const GAUGE_W: f32 = 48.0;
/// Thickness of the extra-usage gauge's bar.
const GAUGE_H: f32 = 4.0;
/// Space between the gauge's caption and its content.
const CAPTION_GAP: f32 = 6.0;
/// The burn row's label, so the 5-hour figure never reads as a 7-day one.
pub const BURN_LABEL: &str = "5h burn  ";
/// The burn row while no 5-hour window is active.
pub const IDLE: &str = "idle";
/// Its hover text.
const IDLE_HOVER: &str = "5-hour: no active window; the next message starts one";

/// The 5-hour burn in brief: "+0.21%/min · cap in 1h 48m" (rate and ETA in
/// their level colors) when the cap comes before the reset, "+0.34%/min ·
/// safe until reset" when the reset comes first, "steady", or "need 2
/// samples". With no known reset (`reset_known` false) the cap time can't
/// be weighed against one, so only the rate shows, matching the timeline,
/// which then claims neither "safe" nor a cap.
pub fn burn_summary(line: Option<&BurnLine>, reset_known: bool) -> Vec<Run> {
    let Some(line) = line else {
        return vec![Run::new("need 2 samples", Tone::Dim)];
    };
    match line.eta {
        Eta::Steady => vec![Run::new("steady", Tone::Level(Level::Green))],
        Eta::Minutes { .. } if !reset_known => vec![Run::new(
            analytics::fmt_rate(line.rate),
            Tone::Level(line.rate_level),
        )],
        Eta::Minutes {
            lands_after_reset: true,
            ..
        } => vec![
            Run::new(analytics::fmt_rate(line.rate), Tone::Level(line.rate_level)),
            Run::new(" · ", Tone::Dim),
            Run::new("safe until reset", Tone::Level(Level::Green)),
        ],
        Eta::Minutes { minutes, level, .. } => vec![
            Run::new(analytics::fmt_rate(line.rate), Tone::Level(line.rate_level)),
            Run::new(" · cap in ", Tone::Dim),
            Run::new(analytics::fmt_min(minutes), Tone::Level(level)),
        ],
    }
}

/// The extra-usage gauge's figures.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraGauge {
    /// "$23 / $35"
    pub text: String,
    /// Filled share of the bar, 0..=1.
    pub fraction: f32,
    pub level: Level,
}

/// Whole dollars spent of the limit, and the bar.
pub fn extra_gauge(e: &ExtraUsage) -> ExtraGauge {
    ExtraGauge {
        text: format!(
            "${:.0} / ${:.0}",
            e.used_credits.floor(),
            e.monthly_limit.floor()
        ),
        fraction: (e.utilization / 100.0).clamp(0.0, 1.0) as f32,
        level: analytics::color_for(Some(e.utilization)),
    }
}

/// The 5-hour burn row under the timeline: a muted "5h burn" label, then
/// [`burn_summary`] (wrapping when narrow), with the full burn line on
/// hover, or "idle" while no window is active. Labelled so it never reads
/// as a 7-day figure.
pub fn show_burn_row(ui: &mut Ui, d: &Dashboard, now: DateTime<Utc>) {
    let live = d.five_hour_now(now);
    let mut runs = vec![Run::new(BURN_LABEL, Tone::Dim)];
    if live.state == FiveHourState::Idle {
        runs.push(Run::new(IDLE, Tone::Dim));
        super::show_runs(ui, &runs).on_hover_text(IDLE_HOVER);
        return;
    }
    let burn = analytics::burn_line(&d.history, live.pct, live.resets_at, now);
    runs.extend(burn_summary(burn.as_ref(), live.resets_at.is_some()));
    super::show_runs(ui, &runs).on_hover_text(burn_hover(burn.as_ref()));
}

/// The footer strip on a card: the extra-usage gauge, when analytics says
/// the block is visible; nothing otherwise.
pub fn show_footer(ui: &mut Ui, d: &Dashboard) {
    let Some(extra) = analytics::visible_extra(&d.snapshot) else {
        return;
    };
    let palette = Palette::for_visuals(ui.visuals());
    let g = extra_gauge(extra);
    let painter = ui.painter().clone();
    let small = eframe::egui::TextStyle::Small.resolve(ui.style());
    let caption = painter.layout_no_wrap("extra".to_owned(), small, palette.text_muted);
    let body = eframe::egui::TextStyle::Body.resolve(ui.style());
    let (galley, w) = digits::number_galley(&painter, &g.text, body, palette.level(g.level));
    let hover = super::plain_text(&rows::extra_runs(extra));
    super::theme::card(palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = CAPTION_GAP;
            centered_galley(ui, caption, palette.text_muted).on_hover_text(&hover);
            let (r, resp) = ui.allocate_exact_size(vec2(w, galley.size().y), Sense::hover());
            ui.painter().galley(r.min, galley, palette.text);
            resp.on_hover_text(&hover);
            gauge_bar(ui, &g, palette).on_hover_text(&hover);
        });
    });
}

/// The full burn line, for the summary's tooltip.
fn burn_hover(line: Option<&BurnLine>) -> String {
    format!("5-hour: {}", super::plain_text(&rows::burn_body(line)))
}

fn centered_galley(
    ui: &mut Ui,
    galley: std::sync::Arc<eframe::egui::Galley>,
    color: eframe::egui::Color32,
) -> eframe::egui::Response {
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    ui.painter().galley(rect.min, galley, color);
    response
}

fn gauge_bar(ui: &mut Ui, g: &ExtraGauge, palette: &Palette) -> eframe::egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(GAUGE_W, GAUGE_H * 3.0), Sense::hover());
    let track = Rect::from_center_size(rect.center(), vec2(GAUGE_W, GAUGE_H));
    let ctx = ui.ctx();
    let fraction = motion::eased(ctx, Id::new("extra-gauge-fraction"), g.fraction);
    let color = motion::eased(ctx, Id::new("extra-gauge-color"), palette.level(g.level));
    let painter = ui.painter();
    painter.rect_filled(track, GAUGE_H / 2.0, palette.track);
    if fraction > 0.0 {
        let mut fill = track;
        fill.set_width((GAUGE_W * fraction).max(GAUGE_H));
        painter.rect_filled(fill, GAUGE_H / 2.0, color);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::plain_text;

    fn line(rate: f64, rate_level: Level, eta: Eta) -> BurnLine {
        BurnLine {
            rate,
            rate_level,
            eta,
            span_minutes: 14.0,
            samples: 8,
        }
    }

    #[test]
    fn burn_summary_cap_before_reset() {
        let l = line(
            0.214,
            Level::Orange,
            Eta::Minutes {
                minutes: 108.9,
                level: Level::Yellow,
                lands_after_reset: false,
            },
        );
        let runs = burn_summary(Some(&l), true);
        assert_eq!(plain_text(&runs), "+0.21%/min · cap in 1h 48m");
        assert!(runs.contains(&Run::new("+0.21%/min", Tone::Level(Level::Orange))));
        assert!(runs.contains(&Run::new("1h 48m", Tone::Level(Level::Yellow))));
    }

    #[test]
    fn burn_summary_safe_when_the_cap_lands_after_reset() {
        let l = line(
            0.34,
            Level::Orange,
            Eta::Minutes {
                minutes: 328.9,
                level: Level::Green,
                lands_after_reset: true,
            },
        );
        let runs = burn_summary(Some(&l), true);
        assert_eq!(plain_text(&runs), "+0.34%/min · safe until reset");
        assert!(runs.contains(&Run::new("safe until reset", Tone::Level(Level::Green))));
        assert!(!plain_text(&runs).contains("cap"));
    }

    #[test]
    fn burn_summary_without_a_known_reset_is_the_rate_alone() {
        let l = line(
            0.5,
            Level::Orange,
            Eta::Minutes {
                minutes: 60.0,
                level: Level::Orange,
                lands_after_reset: false,
            },
        );
        let runs = burn_summary(Some(&l), false);
        assert_eq!(
            runs,
            vec![Run::new("+0.50%/min", Tone::Level(Level::Orange))]
        );
        // Steady and unknown read the same either way.
        let steady = line(0.0, Level::Green, Eta::Steady);
        assert_eq!(
            burn_summary(Some(&steady), false),
            burn_summary(Some(&steady), true)
        );
        assert_eq!(burn_summary(None, false), burn_summary(None, true));
    }

    #[test]
    fn burn_summary_steady_and_unknown() {
        let runs = burn_summary(Some(&line(0.0, Level::Green, Eta::Steady)), true);
        assert_eq!(runs, vec![Run::new("steady", Tone::Level(Level::Green))]);
        let runs = burn_summary(None, true);
        assert_eq!(runs, vec![Run::new("need 2 samples", Tone::Dim)]);
    }

    fn extra(used: f64, limit: f64, utilization: f64) -> ExtraUsage {
        ExtraUsage {
            enabled: true,
            monthly_limit: limit,
            used_credits: used,
            utilization,
            currency: "USD".to_owned(),
            disabled_reason: None,
        }
    }

    #[test]
    fn gauge_figures() {
        let g = extra_gauge(&extra(23.70, 35.0, 67.7));
        assert_eq!(g.text, "$23 / $35");
        assert!((g.fraction - 0.677).abs() < 1e-6);
        assert_eq!(g.level, Level::Yellow);
        let g = extra_gauge(&extra(76.83, 75.0, 102.4));
        assert_eq!(g.text, "$76 / $75");
        assert_eq!(g.fraction, 1.0);
        assert_eq!(g.level, Level::Red);
        let g = extra_gauge(&extra(0.0, 0.0, 0.0));
        assert_eq!(g.fraction, 0.0);
    }
}
