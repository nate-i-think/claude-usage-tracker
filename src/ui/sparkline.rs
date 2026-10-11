//! The footer's 7-day sparkline: seven mini rounded bars, one per local day,
//! each with its orange Fable floor, today's slot outlined, and a tooltip
//! per day ("Tue · 22%").
//!
//! The bar split (Fable floor vs the rest) is the old "Last 7 days" chart's
//! rule; the chart itself is gone (see git history for `ui/charts.rs`).

use chrono::NaiveDate;
use eframe::egui::{vec2, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui};

use crate::analytics::{self, CHART_HEIGHT, FABLE_TO_WEEKLY_ALL_RATIO};

use super::theme::Palette;

/// Width of one bar.
const BAR_W: f32 = 8.0;
/// Space between bars.
const BAR_GAP: f32 = 4.0;
/// Height of the bar area.
pub const HEIGHT: f32 = 26.0;
/// Height of an empty or zero day's stub, so all seven slots read.
const STUB: f32 = 2.0;
/// Gap between a bar's Fable floor and the rest of the bar, in points.
const FLOOR_GAP: f32 = 1.5;
/// The bars' full height is at least this % (small weeks still show
/// shape, a 100% day still fits).
pub const MIN_SCALE: f64 = 50.0;

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

/// Which of `n` equal slots spanning `left..left + width` contains `x`.
pub fn slot_at(x: f32, left: f32, width: f32, n: usize) -> Option<usize> {
    if n == 0 || width <= 0.0 || x < left || x >= left + width {
        return None;
    }
    let i = ((x - left) / (width / n as f32)) as usize;
    Some(i.min(n - 1))
}

/// One day's bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayBar {
    pub date: NaiveDate,
    pub value: Option<f64>,
    pub split: Option<BarSplit>,
}

/// The days' bars, with the Fable floor from each day's Fable% on the
/// 7-day-all scale.
pub fn day_bars(
    daily: &[(NaiveDate, Option<f64>)],
    fable_daily: &[(NaiveDate, Option<f64>)],
) -> Vec<DayBar> {
    let values: Vec<_> = daily.iter().map(|&(_, v)| v).collect();
    let ceilings = analytics::fable_7d_ceilings(fable_daily, FABLE_TO_WEEKLY_ALL_RATIO);
    daily
        .iter()
        .zip(split_bars(&values, &ceilings))
        .map(|(&(date, value), split)| DayBar { date, value, split })
        .collect()
}

/// Hover text: "Tue · 22%", plus " · Fable 11%" when Fable has a share,
/// or "Tue · no data".
pub fn day_tip(bar: &DayBar) -> String {
    let day = analytics::day_label(bar.date);
    let Some(pct) = bar.value else {
        return format!("{day} · no data");
    };
    match bar.split.filter(|s| s.fable > 0.0) {
        Some(s) => format!("{day} · {pct:.0}% · Fable {:.0}%", s.fable),
        None => format!("{day} · {pct:.0}%"),
    }
}

/// The % that maps to the full bar height: the week's highest value, at
/// least [`MIN_SCALE`], at most 100.
pub fn spark_scale(bars: &[DayBar]) -> f64 {
    bars.iter()
        .filter_map(|b| b.value)
        .filter(|v| !v.is_nan())
        .fold(MIN_SCALE, f64::max)
        .min(100.0)
}

/// Whether the floor/rest notch fits: the bar's rest must be taller than
/// the notch, or the notch would eat the bar's top.
pub fn notch_fits(rest_px: f32) -> bool {
    rest_px > FLOOR_GAP + 0.5
}

/// Total width of `n` bars.
pub fn width(n: usize) -> f32 {
    n as f32 * BAR_W + n.saturating_sub(1) as f32 * BAR_GAP
}

/// Paint the bars; `today` gets an accent outline around its slot.
pub fn show_sparkline(ui: &mut Ui, bars: &[DayBar], today: NaiveDate) -> Response {
    let palette = Palette::for_visuals(ui.visuals());
    let (rect, response) = ui.allocate_exact_size(vec2(width(bars.len()), HEIGHT), Sense::hover());
    let painter = ui.painter();
    let scale = spark_scale(bars);
    let px = |pct: f64| (pct / scale) as f32 * rect.height();
    let hovered = response.hover_pos().and_then(|p| {
        slot_at(
            p.x,
            rect.left() - BAR_GAP / 2.0,
            rect.width() + BAR_GAP,
            bars.len(),
        )
    });

    for (i, bar) in bars.iter().enumerate() {
        let left = rect.left() + i as f32 * (BAR_W + BAR_GAP);
        let slot = Rect::from_x_y_ranges(left..=left + BAR_W, rect.y_range());
        let radius = CornerRadius::same(2);
        if bar.date == today {
            // Outlined a hair outside the bar so the clay outline never
            // touches the clay Fable floor.
            painter.rect_stroke(
                slot.expand(2.0),
                CornerRadius::same(3),
                Stroke::new(1.0, palette.accent),
                StrokeKind::Inside,
            );
        }
        if hovered == Some(i) {
            painter.rect_filled(slot.expand(1.0), radius, palette.raised_hover);
        }
        let split = bar.split.filter(|s| s.orange + s.rest > 0.0);
        let Some(s) = split else {
            let stub =
                Rect::from_x_y_ranges(slot.x_range(), (slot.bottom() - STUB)..=slot.bottom());
            painter.rect_filled(stub, 1.0, palette.track);
            continue;
        };
        let orange_px = px(s.orange);
        let total_px = px(s.orange + s.rest).max(STUB);
        let bar_rect =
            Rect::from_x_y_ranges(slot.x_range(), (slot.bottom() - total_px)..=slot.bottom());
        painter.rect_filled(bar_rect, radius, palette.bar);
        if orange_px > 0.0 {
            let floor =
                Rect::from_x_y_ranges(slot.x_range(), (slot.bottom() - orange_px)..=slot.bottom());
            let corners = if s.rest > 0.0 {
                CornerRadius {
                    sw: 2,
                    se: 2,
                    ..CornerRadius::ZERO
                }
            } else {
                radius
            };
            painter.rect_filled(floor, corners, palette.fable);
            if s.rest > 0.0 && notch_fits(total_px - orange_px) {
                let y = floor.top();
                let notch = Rect::from_x_y_ranges(slot.x_range(), (y - FLOOR_GAP)..=y);
                painter.rect_filled(notch, 0.0, palette.surface);
            }
        }
    }
    match hovered {
        Some(i) => response.on_hover_text_at_pointer(day_tip(&bars[i])),
        None => response,
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

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap() // the 5th is a Monday
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
    fn day_floor_is_half_the_fable_pct() {
        let days = [(day(5), Some(30.0)), (day(6), Some(5.0))];
        let fable = [(day(5), Some(50.0)), (day(6), None)];
        let bars = day_bars(&days, &fable);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].date, day(5));
        assert_eq!(bars[0].value, Some(30.0));
        assert_eq!(bars[0].split, split(25.0, 5.0, 25.0));
        assert_eq!(bars[1].split, split(0.0, 5.0, 0.0));
        assert!(day_bars(&[], &fable).is_empty());
    }

    #[test]
    fn day_tips() {
        let bars = day_bars(
            &[(day(6), Some(22.4)), (day(7), Some(30.0)), (day(8), None)],
            &[(day(6), None), (day(7), Some(22.0)), (day(8), None)],
        );
        assert_eq!(day_tip(&bars[0]), "Tue · 22%");
        assert_eq!(day_tip(&bars[1]), "Wed · 30% · Fable 11%");
        assert_eq!(day_tip(&bars[2]), "Thu · no data");
    }

    #[test]
    fn scale_follows_the_week_within_bounds() {
        let bars = |vals: &[Option<f64>]| {
            let daily: Vec<_> = vals
                .iter()
                .enumerate()
                .map(|(i, v)| (day(5 + i as u32), *v))
                .collect();
            day_bars(&daily, &[])
        };
        assert_eq!(spark_scale(&bars(&[Some(10.0), None])), MIN_SCALE);
        assert_eq!(spark_scale(&bars(&[Some(10.0), Some(72.0)])), 72.0);
        assert_eq!(spark_scale(&bars(&[Some(130.0)])), 100.0);
        assert_eq!(spark_scale(&[]), MIN_SCALE);
    }

    #[test]
    fn notch_only_when_rest_is_taller() {
        assert!(!notch_fits(0.5));
        assert!(!notch_fits(FLOOR_GAP));
        assert!(notch_fits(FLOOR_GAP + 1.0));
    }

    #[test]
    fn widths() {
        assert_eq!(width(7), 7.0 * BAR_W + 6.0 * BAR_GAP);
        assert_eq!(width(1), BAR_W);
        assert_eq!(width(0), 0.0);
    }
}
