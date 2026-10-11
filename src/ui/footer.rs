//! The footer strip under the hero timeline: the 7-day sparkline, the
//! 5-hour burn summary, and the extra-usage mini gauge, packed into one row
//! that wraps when narrow.

use chrono::{DateTime, Local, Utc};
use eframe::egui::{vec2, Align, Layout, Rect, Sense, Ui};

use crate::analytics::{self, BurnLine, Eta, Level};
use crate::model::ExtraUsage;
use crate::poller::Dashboard;

use super::layout::{self, GAP};
use super::theme::Palette;
use super::{digits, layout_job, rows, sparkline, Run, Tone};

/// Width of the extra-usage gauge's bar.
const GAUGE_W: f32 = 48.0;
/// Thickness of the extra-usage gauge's bar.
const GAUGE_H: f32 = 4.0;
/// Space between a footer item's caption and its content.
const CAPTION_GAP: f32 = 6.0;

/// The strip's items, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterItem {
    Sparkline,
    Burn,
    Extra,
}

/// Which items show: the sparkline once there is daily history, the burn
/// summary always, the extra gauge when analytics says the block is
/// visible.
pub fn footer_items(has_daily: bool, extra_visible: bool) -> Vec<FooterItem> {
    [
        has_daily.then_some(FooterItem::Sparkline),
        Some(FooterItem::Burn),
        extra_visible.then_some(FooterItem::Extra),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The 5-hour burn in brief: "+0.21%/min · cap in 5h 28m" (rate and ETA in
/// their level colors), "steady", or "need 2 samples".
pub fn burn_summary(line: Option<&BurnLine>) -> Vec<Run> {
    let Some(line) = line else {
        return vec![Run::new("need 2 samples", Tone::Dim)];
    };
    match line.eta {
        Eta::Steady => vec![Run::new("steady", Tone::Level(Level::Green))],
        Eta::Minutes { minutes, level } => vec![
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

/// The footer strip on a card.
pub fn show_footer(ui: &mut Ui, d: &Dashboard, now: DateTime<Utc>) {
    let palette = Palette::for_visuals(ui.visuals());
    let snap = &d.snapshot;
    let extra = analytics::visible_extra(snap);
    let items = footer_items(!d.daily_history.is_empty(), extra.is_some());
    let bars = sparkline::day_bars(&d.daily_history, &d.fable_daily);
    let burn = analytics::burn_line(
        &d.history,
        snap.five_hour_pct(),
        snap.five_hour_resets_at(),
        now,
    );
    let burn_job = layout_job(&burn_summary(burn.as_ref()), ui);
    let small = eframe::egui::TextStyle::Small.resolve(ui.style());
    let painter = ui.painter().clone();
    let caption =
        |text: &str| painter.layout_no_wrap(text.to_owned(), small.clone(), palette.text_muted);
    let gauge = extra.map(extra_gauge);

    // Measure every item first so the strip can pack them.
    let spark_caption = caption("7d");
    let burn_galley = painter.layout_job(burn_job);
    let extra_caption = caption("extra");
    let gauge_text = gauge.as_ref().map(|g| {
        let body = eframe::egui::TextStyle::Body.resolve(ui.style());
        digits::number_galley(&painter, &g.text, body, palette.level(g.level))
    });
    let row_h = burn_galley.size().y.max(sparkline::HEIGHT);
    let widths: Vec<f32> = items
        .iter()
        .map(|item| match item {
            FooterItem::Sparkline => {
                spark_caption.size().x + CAPTION_GAP + sparkline::width(bars.len())
            }
            FooterItem::Burn => burn_galley.size().x,
            FooterItem::Extra => {
                let text_w = gauge_text.as_ref().map_or(0.0, |(_, w)| *w);
                extra_caption.size().x + CAPTION_GAP + text_w + CAPTION_GAP + GAUGE_W
            }
        })
        .collect();

    super::theme::card(palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let gap = 1.5 * GAP;
        let rows = layout::pack_rows(&widths, ui.available_width(), gap);
        let mut it = items.iter().zip(&widths);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = vec2(gap, GAP);
            for count in rows {
                ui.horizontal(|ui| {
                    ui.set_min_height(row_h);
                    for (item, &w) in it.by_ref().take(count) {
                        let size = vec2(w, row_h);
                        ui.allocate_ui_with_layout(
                            size,
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = CAPTION_GAP;
                                match item {
                                    FooterItem::Sparkline => {
                                        centered_galley(
                                            ui,
                                            spark_caption.clone(),
                                            palette.text_muted,
                                        );
                                        sparkline::show_sparkline(
                                            ui,
                                            &bars,
                                            Local::now().date_naive(),
                                        );
                                    }
                                    FooterItem::Burn => {
                                        centered_galley(ui, burn_galley.clone(), palette.text)
                                            .on_hover_text(burn_hover(burn.as_ref()));
                                    }
                                    FooterItem::Extra => {
                                        if let (Some(g), Some((galley, w)), Some(e)) =
                                            (&gauge, gauge_text.clone(), extra)
                                        {
                                            let hover = super::plain_text(&rows::extra_runs(e));
                                            centered_galley(
                                                ui,
                                                extra_caption.clone(),
                                                palette.text_muted,
                                            )
                                            .on_hover_text(&hover);
                                            let (r, resp) = ui.allocate_exact_size(
                                                vec2(w, galley.size().y),
                                                Sense::hover(),
                                            );
                                            ui.painter().galley(r.min, galley, palette.text);
                                            resp.on_hover_text(&hover);
                                            gauge_bar(ui, g, palette).on_hover_text(&hover);
                                        }
                                    }
                                }
                            },
                        );
                    }
                });
            }
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
    let painter = ui.painter();
    painter.rect_filled(track, GAUGE_H / 2.0, palette.track);
    if g.fraction > 0.0 {
        let mut fill = track;
        fill.set_width((GAUGE_W * g.fraction).max(GAUGE_H));
        painter.rect_filled(fill, GAUGE_H / 2.0, palette.level(g.level));
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
    fn items_shown() {
        use FooterItem::*;
        assert_eq!(footer_items(true, true), vec![Sparkline, Burn, Extra]);
        assert_eq!(footer_items(false, true), vec![Burn, Extra]);
        assert_eq!(footer_items(true, false), vec![Sparkline, Burn]);
        assert_eq!(footer_items(false, false), vec![Burn]);
    }

    #[test]
    fn burn_summary_with_eta() {
        let l = line(
            0.214,
            Level::Orange,
            Eta::Minutes {
                minutes: 328.9,
                level: Level::Green,
            },
        );
        let runs = burn_summary(Some(&l));
        assert_eq!(plain_text(&runs), "+0.21%/min · cap in 5h 28m");
        assert!(runs.contains(&Run::new("+0.21%/min", Tone::Level(Level::Orange))));
        assert!(runs.contains(&Run::new("5h 28m", Tone::Level(Level::Green))));
    }

    #[test]
    fn burn_summary_steady_and_unknown() {
        let runs = burn_summary(Some(&line(0.0, Level::Green, Eta::Steady)));
        assert_eq!(runs, vec![Run::new("steady", Tone::Level(Level::Green))]);
        let runs = burn_summary(None);
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
