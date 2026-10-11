//! The "5-hour window" and "Last 7 days" bar charts with the orange Fable
//! floor (Python `BarChart` in ui.py).
//!
//! A custom painter rather than egui_plot: the axes are fixed at 0–100%, the
//! chart is static (no zoom/pan), and the labels are categorical, all of
//! which are simpler to draw directly than to pin down in a plot widget.

use chrono::{DateTime, Local, NaiveDate, Utc};
use eframe::egui::{pos2, vec2, Align2, Frame, Rect, RichText, Sense, Stroke, TextStyle, Ui};

use crate::analytics::{self, CHART_HEIGHT, FABLE_TO_WEEKLY_ALL_RATIO, FABLE_WEEKLY_TO_5H_RATIO};
use crate::poller::Dashboard;

use super::{bar_track_color, tone_color, Tone};

/// Below this available width the two charts stack vertically.
pub const SIDE_BY_SIDE_MIN_WIDTH: f32 = 520.0;
/// Height of each chart's drawing area (axes, bars, labels).
const CHART_AREA_HEIGHT: f32 = 180.0;
/// Gridlines with a y-axis label, in %.
const Y_MARKS: [f64; 4] = [25.0, 50.0, 75.0, 100.0];
/// Bar width as a share of its slot.
const BAR_FILL: f32 = 0.6;

/// How one bar is drawn: `orange` (the Fable floor) from 0, then `rest` in
/// the bar color on top. Both in y-axis %, summing to the bar height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarSplit {
    pub orange: f64,
    pub rest: f64,
    /// Fable's unrounded share of the bar (the ceiling clipped to the bar),
    /// for the hover text.
    pub fable: f64,
}

/// Split one bar into its orange floor and remainder. `None` value: no bar.
///
/// The bar is clipped to 0–100%. The orange height mirrors the TUI's row
/// rule ([`analytics::orange_rows`] over [`CHART_HEIGHT`] rows), so any
/// nonzero Fable ceiling shows at least one row's worth (100/CHART_HEIGHT %),
/// and never exceeds the bar.
pub fn split_bar(value: Option<f64>, ceiling: Option<f64>) -> Option<BarSplit> {
    let bar = value.filter(|v| !v.is_nan())?.clamp(0.0, 100.0);
    let rows = analytics::orange_rows(ceiling, CHART_HEIGHT);
    let orange = (rows as f64 * 100.0 / CHART_HEIGHT as f64).min(bar);
    let fable = ceiling.filter(|c| *c > 0.0).map_or(0.0, |c| c.min(bar));
    Some(BarSplit {
        orange,
        rest: bar - orange,
        fable,
    })
}

/// [`split_bar`] per value; ceilings missing past the end count as `None`
/// and extra ceilings are ignored.
pub fn split_bars(values: &[Option<f64>], ceilings: &[Option<f64>]) -> Vec<Option<BarSplit>> {
    values
        .iter()
        .enumerate()
        .map(|(i, &v)| split_bar(v, ceilings.get(i).copied().flatten()))
        .collect()
}

/// Hover text: "{label}: {pct:.1}%", plus the Fable part when there is one.
pub fn tooltip_text(label: &str, value: Option<f64>, split: Option<BarSplit>) -> String {
    let Some(pct) = value else {
        return format!("{label}: no data");
    };
    match split.filter(|s| s.fable > 0.0) {
        Some(s) => format!("{label}: {pct:.1}% (Fable {:.1}%)", s.fable),
        None => format!("{label}: {pct:.1}%"),
    }
}

/// Which of `n` equal slots spanning `left..left + width` contains `x`.
pub fn slot_at(x: f32, left: f32, width: f32, n: usize) -> Option<usize> {
    if n == 0 || width <= 0.0 || x < left || x >= left + width {
        return None;
    }
    let i = ((x - left) / (width / n as f32)) as usize;
    Some(i.min(n - 1))
}

/// Whether the charts go side by side at this width (else stacked).
pub fn side_by_side(width: f32) -> bool {
    width >= SIDE_BY_SIDE_MIN_WIDTH
}

/// One bar slot as drawn: its axis label, hover label, value and split.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartBar {
    /// Under the bar; may be empty (the 5-hour chart labels full hours only).
    pub axis_label: String,
    /// Names the slot in the hover text.
    pub tip_label: String,
    pub value: Option<f64>,
    pub split: Option<BarSplit>,
}

/// The 5-hour chart's bars: 30-minute buckets, with the Fable floor from
/// the rise in Fable% since the window start.
pub fn five_hour_bars(
    buckets: &[(DateTime<Utc>, Option<f64>)],
    fable_buckets: &[(DateTime<Utc>, Option<f64>)],
) -> Vec<ChartBar> {
    let values: Vec<_> = buckets.iter().map(|&(_, v)| v).collect();
    let ceilings = analytics::fable_5h_ceilings(fable_buckets, FABLE_WEEKLY_TO_5H_RATIO);
    buckets
        .iter()
        .zip(split_bars(&values, &ceilings))
        .map(|(&(t, value), split)| ChartBar {
            axis_label: analytics::bucket_label(t),
            tip_label: t.with_timezone(&Local).format("%H:%M").to_string(),
            value,
            split,
        })
        .collect()
}

/// The 7-day chart's bars: one per local day, with the Fable floor from
/// each day's Fable% on the 7-day-all scale.
pub fn seven_day_bars(
    daily: &[(NaiveDate, Option<f64>)],
    fable_daily: &[(NaiveDate, Option<f64>)],
) -> Vec<ChartBar> {
    let values: Vec<_> = daily.iter().map(|&(_, v)| v).collect();
    let ceilings = analytics::fable_7d_ceilings(fable_daily, FABLE_TO_WEEKLY_ALL_RATIO);
    daily
        .iter()
        .zip(split_bars(&values, &ceilings))
        .map(|(&(d, value), split)| ChartBar {
            axis_label: analytics::day_label(d),
            tip_label: d.format("%a %b %-d").to_string(),
            value,
            split,
        })
        .collect()
}

/// Both charts: side by side when wide enough, else stacked.
pub fn show_charts(ui: &mut Ui, d: &Dashboard) {
    let five = five_hour_bars(&d.five_hour_buckets, &d.fable_buckets);
    let seven = seven_day_bars(&d.daily_history, &d.fable_daily);
    if side_by_side(ui.available_width()) {
        ui.columns(2, |cols| {
            chart(&mut cols[0], "5-hour window", &five);
            chart(&mut cols[1], "Last 7 days", &seven);
        });
    } else {
        chart(ui, "5-hour window", &five);
        ui.add_space(6.0);
        chart(ui, "Last 7 days", &seven);
    }
}

/// One framed chart with its title, like the TUI's panels.
fn chart(ui: &mut Ui, title: &str, bars: &[ChartBar]) {
    let accent = tone_color(Tone::Accent, ui.visuals());
    Frame::group(ui.style())
        .stroke((1.0, accent.gamma_multiply(0.6)))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| ui.label(RichText::new(title).color(accent)));
            if bars.is_empty() {
                ui.vertical_centered(|ui| ui.weak("no data yet"));
            } else {
                plot(ui, bars);
            }
        });
}

/// Axes, gridlines, bars and labels, with hover text per slot.
fn plot(ui: &mut Ui, bars: &[ChartBar]) {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), CHART_AREA_HEIGHT),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let font = TextStyle::Small.resolve(ui.style());
    let dim = visuals.weak_text_color();
    let grid = bar_track_color(visuals);
    let bar_color = tone_color(Tone::Accent, visuals);
    let orange = tone_color(Tone::Fable, visuals);

    let y_label_w = painter
        .layout_no_wrap("100%".to_owned(), font.clone(), dim)
        .size()
        .x;
    let text_h = font.size;
    let area = Rect::from_min_max(
        pos2(rect.left() + y_label_w + 6.0, rect.top() + text_h / 2.0),
        pos2(rect.right() - 4.0, rect.bottom() - text_h - 6.0),
    );
    let y_at = |pct: f64| area.bottom() - (pct / 100.0) as f32 * area.height();

    for mark in Y_MARKS {
        let y = y_at(mark);
        painter.hline(area.x_range(), y, Stroke::new(1.0, grid));
        painter.text(
            pos2(area.left() - 4.0, y),
            Align2::RIGHT_CENTER,
            format!("{mark:.0}%"),
            font.clone(),
            dim,
        );
    }
    painter.text(
        pos2(area.left() - 4.0, area.bottom()),
        Align2::RIGHT_CENTER,
        "0%",
        font.clone(),
        dim,
    );

    let n = bars.len();
    let slot_w = area.width() / n as f32;
    let hovered = response
        .hover_pos()
        .and_then(|p| slot_at(p.x, area.left(), area.width(), n));
    for (i, bar) in bars.iter().enumerate() {
        let cx = area.left() + slot_w * (i as f32 + 0.5);
        if hovered == Some(i) {
            let slot =
                Rect::from_x_y_ranges((cx - slot_w / 2.0)..=(cx + slot_w / 2.0), area.y_range());
            painter.rect_filled(slot, 0.0, grid.gamma_multiply(0.5));
        }
        if let Some(s) = bar.split {
            let half = slot_w * BAR_FILL / 2.0;
            let column = |from: f64, to: f64| {
                Rect::from_x_y_ranges((cx - half)..=(cx + half), y_at(to)..=y_at(from))
            };
            if s.orange > 0.0 {
                painter.rect_filled(column(0.0, s.orange), 0.0, orange);
            }
            if s.rest > 0.0 {
                painter.rect_filled(column(s.orange, s.orange + s.rest), 0.0, bar_color);
            }
        }
        if !bar.axis_label.is_empty() {
            let mut galley = painter.layout_no_wrap(bar.axis_label.clone(), font.clone(), dim);
            if galley.size().x > slot_w {
                // Too tight: two letters, like the TUI's dense slots.
                let short: String = bar.axis_label.chars().take(2).collect();
                galley = painter.layout_no_wrap(short, font.clone(), dim);
            }
            let pos = pos2(cx - galley.size().x / 2.0, area.bottom() + 3.0);
            painter.galley(pos, galley, dim);
        }
    }
    painter.hline(area.x_range(), area.bottom(), Stroke::new(1.0, dim));
    painter.vline(area.left(), area.y_range(), Stroke::new(1.0, dim));

    if let Some(i) = hovered {
        let bar = &bars[i];
        response.on_hover_text_at_pointer(tooltip_text(&bar.tip_label, bar.value, bar.split));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: f64 = 100.0 / CHART_HEIGHT as f64;

    fn split(orange: f64, rest: f64, fable: f64) -> Option<BarSplit> {
        Some(BarSplit {
            orange,
            rest,
            fable,
        })
    }

    #[test]
    fn none_value_draws_no_bar() {
        assert_eq!(split_bar(None, Some(50.0)), None);
        assert_eq!(split_bar(None, None), None);
    }

    #[test]
    fn no_fable_is_all_bar_color() {
        assert_eq!(split_bar(Some(40.0), None), split(0.0, 40.0, 0.0));
        assert_eq!(split_bar(Some(40.0), Some(0.0)), split(0.0, 40.0, 0.0));
        assert_eq!(split_bar(Some(40.0), Some(-3.0)), split(0.0, 40.0, 0.0));
    }

    #[test]
    fn orange_follows_ceiling_in_whole_rows() {
        // 25% is exactly 4 rows.
        assert_eq!(split_bar(Some(60.0), Some(25.0)), split(25.0, 35.0, 25.0));
        // 20% rounds to 3 rows (3.2).
        assert_eq!(
            split_bar(Some(60.0), Some(20.0)),
            split(3.0 * ROW, 60.0 - 3.0 * ROW, 20.0)
        );
    }

    #[test]
    fn orange_clipped_to_bar() {
        assert_eq!(split_bar(Some(30.0), Some(80.0)), split(30.0, 0.0, 30.0));
        assert_eq!(split_bar(Some(0.0), Some(10.0)), split(0.0, 0.0, 0.0));
    }

    #[test]
    fn tiny_fable_still_shows_one_row() {
        assert_eq!(
            split_bar(Some(50.0), Some(0.5)),
            split(ROW, 50.0 - ROW, 0.5)
        );
        // ...but still never taller than the bar.
        assert_eq!(split_bar(Some(2.0), Some(0.5)), split(2.0, 0.0, 0.5));
    }

    #[test]
    fn bar_clipped_to_axis() {
        assert_eq!(split_bar(Some(130.0), None), split(0.0, 100.0, 0.0));
        assert_eq!(split_bar(Some(-5.0), None), split(0.0, 0.0, 0.0));
        assert_eq!(split_bar(Some(f64::NAN), None), None);
    }

    #[test]
    fn split_bars_pairs_by_index() {
        // Fewer ceilings than values: the missing ones count as None.
        assert_eq!(
            split_bars(&[Some(50.0), None, Some(10.0)], &[Some(25.0)]),
            vec![split(25.0, 25.0, 25.0), None, split(0.0, 10.0, 0.0)]
        );
        // More ceilings than values: the extras are ignored.
        assert_eq!(
            split_bars(&[Some(50.0)], &[None, Some(25.0), Some(25.0)]),
            vec![split(0.0, 50.0, 0.0)]
        );
        assert!(split_bars(&[], &[Some(1.0)]).is_empty());
    }

    #[test]
    fn tooltip_shows_value_and_fable_part() {
        assert_eq!(tooltip_text("Mon", None, None), "Mon: no data");
        assert_eq!(
            tooltip_text("Mon", Some(42.24), split_bar(Some(42.24), None)),
            "Mon: 42.2%"
        );
        assert_eq!(
            tooltip_text("10:30", Some(42.0), split_bar(Some(42.0), Some(12.34))),
            "10:30: 42.0% (Fable 12.3%)"
        );
    }

    #[test]
    fn slot_lookup() {
        assert_eq!(slot_at(5.0, 0.0, 100.0, 10), Some(0));
        assert_eq!(slot_at(10.0, 0.0, 100.0, 10), Some(1));
        assert_eq!(slot_at(99.9, 0.0, 100.0, 10), Some(9));
        assert_eq!(slot_at(100.0, 0.0, 100.0, 10), None);
        assert_eq!(slot_at(-0.1, 0.0, 100.0, 10), None);
        assert_eq!(slot_at(50.0, 0.0, 100.0, 0), None);
        assert_eq!(slot_at(50.0, 0.0, 0.0, 3), None);
    }

    #[test]
    fn five_hour_floor_is_scaled_rise_since_window_start() {
        let t0 = DateTime::from_timestamp(1_791_000_000, 0).unwrap();
        let at = |i: i64| t0 + chrono::Duration::minutes(30 * i);
        let buckets = [(at(0), Some(10.0)), (at(1), Some(40.0)), (at(2), None)];
        // Fable 10% -> 12%: a 2-point rise, x5.9 = 11.8% -> 2 rows (12.5%).
        let fable = [
            (at(0), Some(10.0)),
            (at(1), Some(12.0)),
            (at(2), Some(12.0)),
        ];
        let bars = five_hour_bars(&buckets, &fable);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].split, split(0.0, 10.0, 0.0));
        let s = bars[1].split.unwrap();
        assert_eq!(s.orange, 2.0 * ROW);
        assert_eq!(s.rest, 40.0 - 2.0 * ROW);
        assert!((s.fable - 11.8).abs() < 1e-9);
        assert_eq!(bars[2].split, None);
    }

    #[test]
    fn seven_day_floor_is_half_the_fable_pct() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(); // a Monday
        let days = [(d, Some(30.0)), (d.succ_opt().unwrap(), Some(5.0))];
        let fable = [(d, Some(50.0)), (d.succ_opt().unwrap(), None)];
        let bars = seven_day_bars(&days, &fable);
        assert_eq!(bars[0].axis_label, "Mon");
        assert_eq!(bars[0].tip_label, "Mon Oct 5");
        assert_eq!(bars[0].split, split(25.0, 5.0, 25.0));
        assert_eq!(bars[1].split, split(0.0, 5.0, 0.0));
        assert!(seven_day_bars(&[], &fable).is_empty());
    }

    #[test]
    fn layout_switches_at_threshold() {
        assert!(side_by_side(SIDE_BY_SIDE_MIN_WIDTH));
        assert!(side_by_side(900.0));
        assert!(!side_by_side(SIDE_BY_SIDE_MIN_WIDTH - 1.0));
    }
}
