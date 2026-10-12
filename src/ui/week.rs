//! The "Last 7 days" chart: one bar per local day (oldest left, today
//! right), the day's highest 7-day-all %, on the timeline's fixed 0–100%
//! axis (same gridlines, labels, font and plot insets, so the two charts
//! line up side by side). Each bar carries its orange Fable floor; today's
//! slot gets a neutral outline and the "today" label; a day without samples
//! is a faint dashed stub, never a zero bar. Hovering a slot reads
//! "Tue Oct 7 · 63% (Fable share 30%)" or "Tue Oct 7 · no data". Under the
//! chart, one row per weekly limit (7-day all, and Fable's own 7-day limit
//! when reported) gives its projection to the weekly reset.
//!
//! The bar split (Fable floor vs the rest) is the old TUI chart's rule (see
//! git history for `ui/charts.rs`). Geometry and text are pure functions
//! tested without a GPU; [`show_week`] only paints.

use std::fmt::Display;

use chrono::{DateTime, Duration, DurationRound, Local, NaiveDate, TimeZone, Timelike, Utc};
use eframe::egui::{
    pos2, vec2, Align2, CornerRadius, Painter, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    TextStyle, Ui,
};

use crate::analytics::{self, Level, WeeklyOutlook, CHART_HEIGHT, FABLE_TO_WEEKLY_ALL_RATIO};
use crate::poller::Dashboard;

use super::layout::CardSize;
use super::theme::Palette;
use super::{timeline, Run, Tone};

/// The card's title.
pub const TITLE: &str = "Last 7 days";
/// Bar width as a share of its slot...
const BAR_FILL: f32 = 0.55;
/// ...but never wider than this, so a wide card keeps slim bars.
const BAR_MAX_W: f32 = 28.0;
/// Height of a zero day's bar, so a measured 0% still reads as a bar.
const ZERO_STUB: f32 = 2.0;
/// Height of a no-data day's dashed outline.
const NO_DATA_STUB: f32 = 14.0;
/// Opacity of the no-data outline (of the muted text color).
const NO_DATA_ALPHA: f32 = 0.55;
/// Dash and gap lengths of the no-data outline, in points.
const NO_DATA_DASH: f32 = 3.0;
const NO_DATA_GAP: f32 = 2.5;
/// Gap between a bar's Fable floor and the rest of the bar, in points.
const FLOOR_GAP: f32 = 1.5;
/// Corner radius of a bar's top.
const BAR_RADIUS: u8 = 3;
/// How far today's outline sits outside its bar.
const TODAY_PAD: f32 = 3.0;
/// Height of the chart when it stacks under the timeline: shorter than the
/// timeline's [`timeline::FULL_HEIGHT`], so both fit the default window.
pub const STACKED_HEIGHT: f32 = 140.0;

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

/// What a day's slot shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BarShape {
    /// No samples that day: a dashed stub, not a zero bar.
    NoData,
    /// A measured 0%: a thin solid stub.
    Zero,
    /// A real bar.
    Bar(BarSplit),
}

/// The shape of `bar`'s slot.
pub fn bar_shape(bar: &DayBar) -> BarShape {
    match bar.split {
        None => BarShape::NoData,
        Some(s) if s.orange + s.rest <= 0.0 => BarShape::Zero,
        Some(s) => BarShape::Bar(s),
    }
}

/// Hover text: "Tue Oct 7 · 63% (Fable share 30%)", without the Fable part
/// when Fable has no share, or "Tue Oct 7 · no data". The share is of the
/// bar (the 7-day-all scale), not Fable's own %.
pub fn day_tip(bar: &DayBar) -> String {
    let day = bar.date.format("%a %b %-d");
    let Some(pct) = bar.value.filter(|v| !v.is_nan()) else {
        return format!("{day} · no data");
    };
    match bar.split.filter(|s| s.fable > 0.0) {
        Some(s) => format!("{day} · {pct:.0}% (Fable share {:.0}%)", s.fable),
        None => format!("{day} · {pct:.0}%"),
    }
}

/// The x label under a slot: "today", else the short weekday ("Mon").
pub fn axis_label(date: NaiveDate, today: NaiveDate) -> String {
    if date == today {
        "today".to_owned()
    } else {
        analytics::day_label(date)
    }
}

/// The card's subtitle: the week's highest day, "peak 63%", or "no data
/// yet" before any day has samples.
pub fn subtitle(bars: &[DayBar]) -> String {
    bars.iter()
        .filter_map(|b| b.value.filter(|v| !v.is_nan()))
        .reduce(f64::max)
        .map_or_else(
            || "no data yet".to_owned(),
            |peak| format!("peak {peak:.0}%"),
        )
}

/// A weekly limit with a projection row under the chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Meter {
    /// 7-day all.
    All,
    /// Fable's own 7-day limit.
    Fable,
}

impl Meter {
    /// The row's label: "7d all" muted, "Fable" in the Fable clay (like the
    /// Details "FABLE:" tag).
    pub fn label(self) -> Run {
        match self {
            Meter::All => Run::new("7d all", Tone::Dim),
            Meter::Fable => Run::new("Fable", Tone::Fable),
        }
    }

    /// The meter's name in the row's tooltip.
    fn name(self) -> &'static str {
        match self {
            Meter::All => "7-day all",
            Meter::Fable => "Fable 7-day",
        }
    }
}

/// A weekday and compact time, rounded to the nearest minute: "Tue 3p" on
/// the hour, else "Thu 9:30a".
pub fn weekday_time_in<Tz: TimeZone>(t: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let t = t.duration_round(Duration::minutes(1)).unwrap_or(t);
    let local = t.with_timezone(tz);
    let suffix = if local.hour() < 12 { 'a' } else { 'p' };
    let clock = if local.minute() == 0 {
        local.format("%-I").to_string()
    } else {
        local.format("%-I:%M").to_string()
    };
    format!("{} {clock}{suffix}", local.format("%a"))
}

/// A weekly projection in brief, in its level colors:
/// - "+21%/day · cap in 2d 5h (Tue 3p)" (`compact`: without the time),
/// - "+6%/day · ≈78% at reset · safe until Thu 9a" (`compact`: without the
///   % at reset),
/// - "at cap · resets Mon 11p", "+0.2%/day · steady", the rate alone
///   without a reset, or "need more history".
pub fn outlook_runs<Tz: TimeZone>(o: &WeeklyOutlook, tz: &Tz, compact: bool) -> Vec<Run>
where
    Tz::Offset: Display,
{
    let tone = Tone::Level(o.level());
    let sep = || Run::new(" · ", Tone::Dim);
    let rate = |r: &analytics::WeeklyRate| Run::new(analytics::fmt_rate_per_day(r.per_day), tone);
    match o {
        WeeklyOutlook::NeedHistory => vec![Run::new("need more history", Tone::Dim)],
        WeeklyOutlook::AtCap { reset } => {
            let mut runs = vec![Run::new("at cap", tone)];
            if let Some(r) = reset {
                runs.push(sep());
                runs.push(Run::new(
                    format!("resets {}", weekday_time_in(*r, tz)),
                    tone,
                ));
            }
            runs
        }
        WeeklyOutlook::Steady { rate: r } => vec![
            rate(r),
            sep(),
            Run::new("steady", Tone::Level(Level::Green)),
        ],
        WeeklyOutlook::RateOnly { rate: r } => vec![rate(r)],
        WeeklyOutlook::Cap {
            rate: r,
            at,
            minutes,
            ..
        } => {
            let mut runs = vec![
                rate(r),
                Run::new(" · cap in ", Tone::Dim),
                Run::new(analytics::fmt_days(*minutes), tone),
            ];
            if !compact {
                runs.push(Run::new(
                    format!(" ({})", weekday_time_in(*at, tz)),
                    Tone::Dim,
                ));
            }
            runs
        }
        WeeklyOutlook::Safe {
            rate: r,
            at_reset,
            reset,
        } => {
            let mut runs = vec![rate(r), sep()];
            if !compact {
                runs.push(Run::new(format!("≈{at_reset:.0}% at reset"), tone));
                runs.push(sep());
            }
            runs.push(Run::new(
                format!("safe until {}", weekday_time_in(*reset, tz)),
                tone,
            ));
            runs
        }
    }
}

/// A row's tooltip: the meter, its full projection, and what the rate was
/// measured over ("7-day all: +51%/day · cap in 16h 0m (Mon 3p)\nrate over
/// last 24h, projected to the weekly reset").
pub fn outlook_hover<Tz: TimeZone>(meter: Meter, o: &WeeklyOutlook, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let full = super::plain_text(&outlook_runs(o, tz, false));
    let how = match (o, o.rate()) {
        (_, Some(r)) => format!(
            "rate {}, projected to the weekly reset",
            analytics::rate_basis_text(&r)
        ),
        (WeeklyOutlook::NeedHistory, _) => format!(
            "needs {}h of samples in the last {}h",
            analytics::WEEKLY_MIN_SPAN_HOURS,
            analytics::WEEKLY_RATE_HOURS
        ),
        _ => "frees up at the weekly reset".to_owned(),
    };
    format!("{}: {full}\n{how}", meter.name())
}

/// `runs` all muted: the rows of a stale snapshot.
pub fn dimmed(runs: Vec<Run>) -> Vec<Run> {
    runs.into_iter()
        .map(|r| Run::new(r.text, Tone::Dim))
        .collect()
}

/// The meters with a row: 7-day all always, Fable when the snapshot
/// reports its window.
pub fn meters(has_fable: bool) -> Vec<Meter> {
    let mut out = vec![Meter::All];
    if has_fable {
        out.push(Meter::Fable);
    }
    out
}

/// Slot `i` of `n` across `plot`: its x range over the plot's full height.
pub fn slot_rect(plot: Rect, i: usize, n: usize) -> Rect {
    let w = plot.width() / n.max(1) as f32;
    let left = plot.left() + w * i as f32;
    Rect::from_x_y_ranges(left..=left + w, plot.y_range())
}

/// Width of a bar in a slot `slot_w` wide.
pub fn bar_width(slot_w: f32) -> f32 {
    (slot_w * BAR_FILL).min(BAR_MAX_W)
}

/// The bar's column in `slot`: [`bar_width`] wide, centered, full height.
pub fn bar_column(slot: Rect) -> Rect {
    Rect::from_center_size(slot.center(), vec2(bar_width(slot.width()), slot.height()))
}

/// Whether the floor/rest notch fits: the bar's rest must be taller than
/// the notch, or the notch would eat the bar's top.
pub fn notch_fits(rest_px: f32) -> bool {
    rest_px > FLOOR_GAP + 0.5
}

/// The y of the top of `shape` drawn on `plot`: the bar's top (at least a
/// zero stub tall), the zero stub's, or the no-data stub's.
pub fn shape_top(plot: Rect, shape: BarShape) -> f32 {
    let base = plot.bottom();
    match shape {
        BarShape::NoData => base - NO_DATA_STUB.min(plot.height()),
        BarShape::Zero => base - ZERO_STUB,
        BarShape::Bar(s) => timeline::y_in(plot, s.orange + s.rest).min(base - ZERO_STUB),
    }
}

/// Today's outline: a few points around the bar (or stub) in `column`,
/// down to the baseline, so it hugs the bar instead of boxing the slot.
pub fn today_outline(plot: Rect, column: Rect, shape: BarShape) -> Rect {
    let top = (shape_top(plot, shape) - TODAY_PAD).max(plot.top());
    Rect::from_x_y_ranges(
        (column.left() - TODAY_PAD)..=(column.right() + TODAY_PAD),
        top..=plot.bottom(),
    )
}

/// The no-data stub's outline: up the left side, across the top, down the
/// right side of a `column`-wide stub standing on the baseline (open at the
/// bottom, which the baseline closes).
pub fn no_data_outline(column: Rect) -> [Pos2; 4] {
    let top = column.bottom() - NO_DATA_STUB.min(column.height());
    [
        pos2(column.left(), column.bottom()),
        pos2(column.left(), top),
        pos2(column.right(), top),
        pos2(column.right(), column.bottom()),
    ]
}

/// Paint the chart of `bars`, `height` tall across the available width, on
/// the timeline's axis, with `today`'s slot marked. Hovering a slot
/// highlights it and shows its tip.
pub fn show_week(
    ui: &mut Ui,
    bars: &[DayBar],
    today: NaiveDate,
    palette: &Palette,
    height: f32,
) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter_at(rect);
    let font = TextStyle::Small.resolve(ui.style());
    let label_w = painter
        .layout_no_wrap("100%".to_owned(), font.clone(), palette.text_muted)
        .size()
        .x;
    let plot = timeline::plot_rect(rect, label_w, font.size);
    let n = bars.len();
    let hovered = response
        .hover_pos()
        .and_then(|p| slot_at(p.x, plot.left(), plot.width(), n));
    if let Some(i) = hovered {
        let slot = slot_rect(plot, i, n).shrink2(vec2(2.0, 0.0));
        painter.rect_filled(slot, 4.0, palette.raised_hover);
    }
    timeline::paint_y_axis(&painter, plot, palette, &font);

    for (i, bar) in bars.iter().enumerate() {
        let slot = slot_rect(plot, i, n);
        let column = bar_column(slot);
        let is_today = bar.date == today;
        let shape = bar_shape(bar);
        if is_today {
            // A neutral outline, so today never competes with the clay
            // Fable floor.
            painter.rect_stroke(
                today_outline(plot, column, shape),
                CornerRadius {
                    nw: 5,
                    ne: 5,
                    ..CornerRadius::ZERO
                },
                Stroke::new(1.0, palette.control_border),
                StrokeKind::Outside,
            );
        }
        paint_bar(&painter, palette, plot, column, shape);

        let color = if is_today {
            palette.text
        } else {
            palette.text_muted
        };
        let label = axis_label(bar.date, today);
        let mut galley = painter.layout_no_wrap(label.clone(), font.clone(), color);
        if galley.size().x > slot.width() {
            // Too tight: two letters, like the TUI's dense slots.
            let short: String = label.chars().take(2).collect();
            galley = painter.layout_no_wrap(short, font.clone(), color);
        }
        let pos = pos2(slot.center().x - galley.size().x / 2.0, plot.bottom() + 3.0);
        painter.galley(pos, galley, color);
    }

    if n == 0 {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "no data yet",
            font,
            palette.text_muted,
        );
    }
    match hovered {
        Some(i) => response.on_hover_text_at_pointer(day_tip(&bars[i])),
        None => response,
    }
}

/// The chart's card: the title with the peak as its subtitle, the chart,
/// and one projection row per weekly limit under it (see [`meters`]). The
/// projections are taken at the snapshot's fetch time; `stale` mutes them.
/// Returns the content's natural height (before `size.min_content`).
pub fn show_week_card(
    ui: &mut Ui,
    d: &Dashboard,
    today: NaiveDate,
    palette: &Palette,
    size: CardSize,
    stale: bool,
) -> f32 {
    let bars = day_bars(&d.daily_history, &d.fable_daily);
    let snap = &d.snapshot;
    let at = snap.fetched_at;
    let has_fable = snap.window(crate::model::FABLE_LABEL).is_some();
    let rows: Vec<(Meter, WeeklyOutlook)> = meters(has_fable)
        .into_iter()
        .map(|m| {
            let o = match m {
                Meter::All => analytics::weekly_outlook(
                    &d.seven_day_recent,
                    snap.seven_day_pct(),
                    snap.seven_day_resets_at(),
                    at,
                ),
                Meter::Fable => analytics::weekly_outlook(
                    &d.fable_recent,
                    snap.fable_pct(),
                    snap.fable_resets_at(),
                    at,
                ),
            };
            (m, o)
        })
        .collect();
    super::theme::card(palette)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let top = ui.cursor().top();
            // Reserved from the cursor, so set before the content.
            ui.set_min_height(size.min_content);
            super::theme::chart_title(ui, palette, TITLE, Some(&subtitle(&bars)));
            show_week(ui, &bars, today, palette, size.chart);
            outlook_rows(ui, &rows, stale);
            // From the cursor: min_rect includes the reserved height.
            let natural = ui.cursor().top() - top - ui.spacing().item_spacing.y;
            natural
        })
        .inner
}

/// The projection rows: labels in an aligned column, then the projection,
/// compact when the full one would wrap (the tooltip has it in full).
fn outlook_rows(ui: &mut Ui, rows: &[(Meter, WeeklyOutlook)], stale: bool) {
    let painter = ui.painter().clone();
    let label_w = rows
        .iter()
        .map(|(m, _)| {
            painter
                .layout_job(super::layout_job(&[m.label()], ui))
                .size()
                .x
        })
        .fold(0.0, f32::max);
    let gap = 8.0;
    for (meter, outlook) in rows {
        let hover = outlook_hover(*meter, outlook, &Local);
        let text_w = ui.available_width() - label_w - gap;
        let full = outlook_runs(outlook, &Local, false);
        let full_w = painter.layout_job(super::layout_job(&full, ui)).size().x;
        let runs = if full_w > text_w {
            outlook_runs(outlook, &Local, true)
        } else {
            full
        };
        let (label, runs) = if stale {
            (dimmed(vec![meter.label()]), dimmed(runs))
        } else {
            (vec![meter.label()], runs)
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let r = super::show_runs(ui, &label);
            ui.add_space(label_w - r.rect.width() + gap);
            super::show_runs(ui, &runs);
        })
        .response
        .on_hover_text(hover);
    }
}

/// One day's bar in `column` (full plot height): the dashed no-data stub,
/// the zero stub, or the bar with its Fable floor and notch.
fn paint_bar(painter: &Painter, palette: &Palette, plot: Rect, column: Rect, shape: BarShape) {
    let base = plot.bottom();
    let x = column.x_range();
    match shape {
        BarShape::NoData => {
            let stroke = Stroke::new(1.0, palette.text_muted.gamma_multiply(NO_DATA_ALPHA));
            for edge in no_data_outline(column).windows(2) {
                for seg in timeline::dashes(edge[0], edge[1], NO_DATA_DASH, NO_DATA_GAP) {
                    painter.line_segment(seg, stroke);
                }
            }
        }
        BarShape::Zero => {
            let stub = Rect::from_x_y_ranges(x, (base - ZERO_STUB)..=base);
            painter.rect_filled(stub, 1.0, palette.bar);
        }
        BarShape::Bar(s) => {
            let top_y = shape_top(plot, shape);
            let floor_y = timeline::y_in(plot, s.orange);
            let top = CornerRadius {
                nw: BAR_RADIUS,
                ne: BAR_RADIUS,
                ..CornerRadius::ZERO
            };
            painter.rect_filled(Rect::from_x_y_ranges(x, top_y..=base), top, palette.bar);
            if s.orange > 0.0 {
                let corners = if s.rest > 0.0 {
                    CornerRadius::ZERO
                } else {
                    top
                };
                let floor = Rect::from_x_y_ranges(x, floor_y..=base);
                painter.rect_filled(floor, corners, palette.fable);
                if s.rest > 0.0 && notch_fits(floor_y - top_y) {
                    // A notch in the card color between the floor and the
                    // bar, so the split doesn't rely on hue alone.
                    let notch = Rect::from_x_y_ranges(x, (floor_y - FLOOR_GAP)..=floor_y);
                    painter.rect_filled(notch, 0.0, palette.surface);
                }
            }
        }
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
    fn shapes_tell_no_data_from_zero() {
        let bars = day_bars(
            &[(day(5), None), (day(6), Some(0.0)), (day(7), Some(40.0))],
            &[(day(5), Some(20.0)), (day(6), None), (day(7), None)],
        );
        assert_eq!(bar_shape(&bars[0]), BarShape::NoData);
        assert_eq!(bar_shape(&bars[1]), BarShape::Zero);
        assert_eq!(
            bar_shape(&bars[2]),
            BarShape::Bar(split(0.0, 40.0, 0.0).unwrap())
        );
    }

    #[test]
    fn day_tips() {
        let bars = day_bars(
            &[(day(6), Some(22.4)), (day(7), Some(63.0)), (day(8), None)],
            &[(day(6), None), (day(7), Some(60.0)), (day(8), None)],
        );
        assert_eq!(day_tip(&bars[0]), "Tue Oct 6 · 22%");
        assert_eq!(day_tip(&bars[1]), "Wed Oct 7 · 63% (Fable share 30%)");
        assert_eq!(day_tip(&bars[2]), "Thu Oct 8 · no data");
    }

    #[test]
    fn axis_labels_name_today() {
        assert_eq!(axis_label(day(5), day(11)), "Mon");
        assert_eq!(axis_label(day(10), day(11)), "Sat");
        assert_eq!(axis_label(day(11), day(11)), "today");
    }

    #[test]
    fn subtitle_is_the_peak_day() {
        let bars = day_bars(
            &[(day(5), Some(12.0)), (day(6), None), (day(7), Some(63.4))],
            &[],
        );
        assert_eq!(subtitle(&bars), "peak 63%");
        let empty = day_bars(&[(day(5), None), (day(6), None)], &[]);
        assert_eq!(subtitle(&empty), "no data yet");
        assert_eq!(subtitle(&[]), "no data yet");
    }

    #[test]
    fn slots_split_the_plot_evenly() {
        let plot = Rect::from_min_max(pos2(40.0, 10.0), pos2(390.0, 150.0));
        let first = slot_rect(plot, 0, 7);
        assert_eq!(first.left(), 40.0);
        assert_eq!(first.width(), 50.0);
        assert_eq!(first.y_range(), plot.y_range());
        let last = slot_rect(plot, 6, 7);
        assert_eq!(last.right(), 390.0);
        // Hover lookup agrees with the slots.
        assert_eq!(slot_at(last.center().x, 40.0, 350.0, 7), Some(6));
        assert_eq!(slot_at(first.right() - 0.1, 40.0, 350.0, 7), Some(0));
    }

    #[test]
    fn bars_are_slim_and_centered() {
        assert_eq!(bar_width(40.0), 22.0);
        assert_eq!(bar_width(100.0), BAR_MAX_W);
        let slot = Rect::from_min_max(pos2(40.0, 10.0), pos2(90.0, 150.0));
        let col = bar_column(slot);
        assert_eq!(col.center().x, 65.0);
        assert_eq!(col.width(), BAR_MAX_W.min(50.0 * BAR_FILL));
        assert_eq!(col.y_range(), slot.y_range());
    }

    #[test]
    fn no_data_outline_stands_on_the_baseline() {
        let col = Rect::from_min_max(pos2(10.0, 0.0), pos2(30.0, 100.0));
        let [a, b, c, d] = no_data_outline(col);
        assert_eq!(a, pos2(10.0, 100.0));
        assert_eq!(b, pos2(10.0, 100.0 - NO_DATA_STUB));
        assert_eq!(c, pos2(30.0, 100.0 - NO_DATA_STUB));
        assert_eq!(d, pos2(30.0, 100.0));
        // Never taller than the plot.
        let short = Rect::from_min_max(pos2(0.0, 95.0), pos2(10.0, 100.0));
        assert_eq!(no_data_outline(short)[1].y, 95.0);
    }

    #[test]
    fn today_outline_hugs_the_bar() {
        let plot = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        let col = Rect::from_min_max(pos2(10.0, 0.0), pos2(30.0, 100.0));
        let bar = BarShape::Bar(split(0.0, 40.0, 0.0).unwrap());
        assert_eq!(shape_top(plot, bar), 60.0);
        let r = today_outline(plot, col, bar);
        assert_eq!(r.left(), 10.0 - TODAY_PAD);
        assert_eq!(r.right(), 30.0 + TODAY_PAD);
        assert_eq!(r.top(), 60.0 - TODAY_PAD);
        assert_eq!(r.bottom(), 100.0);
        // A full bar's outline stays inside the plot.
        let full = BarShape::Bar(split(0.0, 100.0, 0.0).unwrap());
        assert_eq!(today_outline(plot, col, full).top(), 0.0);
        // Stubs: zero and no data.
        assert_eq!(shape_top(plot, BarShape::Zero), 100.0 - ZERO_STUB);
        assert_eq!(shape_top(plot, BarShape::NoData), 100.0 - NO_DATA_STUB);
        // A 0.5% bar is still a stub tall.
        let tiny = BarShape::Bar(split(0.0, 0.5, 0.0).unwrap());
        assert_eq!(shape_top(plot, tiny), 100.0 - ZERO_STUB);
    }

    #[test]
    fn weekday_times() {
        let t = |d, h, m, s| Utc.with_ymd_and_hms(2026, 10, d, h, m, s).unwrap();
        assert_eq!(weekday_time_in(t(13, 15, 0, 0), &Utc), "Tue 3p");
        assert_eq!(weekday_time_in(t(15, 9, 30, 0), &Utc), "Thu 9:30a");
        assert_eq!(weekday_time_in(t(11, 0, 5, 0), &Utc), "Sun 12:05a");
        // Rounded to the nearest minute first.
        assert_eq!(weekday_time_in(t(13, 14, 59, 31), &Utc), "Tue 3p");
        assert_eq!(weekday_time_in(t(13, 15, 0, 29), &Utc), "Tue 3p");
        assert_eq!(weekday_time_in(t(13, 15, 0, 31), &Utc), "Tue 3:01p");
    }

    fn rate(per_day: f64) -> analytics::WeeklyRate {
        analytics::WeeklyRate {
            per_day,
            span: Duration::hours(24),
            basis: analytics::RateBasis::Anchored { gap: false },
            floored: false,
            since_reset: false,
        }
    }

    #[test]
    fn outlook_copy() {
        use crate::ui::plain_text;
        let t = |d, h| Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap();
        let cap = WeeklyOutlook::Cap {
            rate: rate(51.3),
            at: t(12, 15),
            minutes: 16.0 * 60.0,
            level: Level::Red,
        };
        let runs = outlook_runs(&cap, &Utc, false);
        assert_eq!(plain_text(&runs), "+51%/day · cap in 16h 00m (Mon 3p)");
        assert!(runs.contains(&Run::new("16h 00m", Tone::Level(Level::Red))));
        assert!(runs.contains(&Run::new("+51%/day", Tone::Level(Level::Red))));
        assert_eq!(
            plain_text(&outlook_runs(&cap, &Utc, true)),
            "+51%/day · cap in 16h 00m"
        );

        let safe = WeeklyOutlook::Safe {
            rate: rate(6.0),
            at_reset: 77.6,
            reset: t(15, 9),
        };
        let runs = outlook_runs(&safe, &Utc, false);
        assert_eq!(
            plain_text(&runs),
            "+6%/day · ≈78% at reset · safe until Thu 9a"
        );
        assert!(runs.contains(&Run::new("safe until Thu 9a", Tone::Level(Level::Green))));
        assert_eq!(
            plain_text(&outlook_runs(&safe, &Utc, true)),
            "+6%/day · safe until Thu 9a"
        );

        let at_cap = WeeklyOutlook::AtCap {
            reset: Some(t(12, 23)),
        };
        let runs = outlook_runs(&at_cap, &Utc, false);
        assert_eq!(plain_text(&runs), "at cap · resets Mon 11p");
        assert!(runs.contains(&Run::new("at cap", Tone::Level(Level::Red))));
        assert_eq!(
            plain_text(&outlook_runs(
                &WeeklyOutlook::AtCap { reset: None },
                &Utc,
                false
            )),
            "at cap"
        );

        let steady = outlook_runs(&WeeklyOutlook::Steady { rate: rate(0.2) }, &Utc, false);
        assert_eq!(plain_text(&steady), "+0.2%/day · steady");
        let only = outlook_runs(&WeeklyOutlook::RateOnly { rate: rate(20.0) }, &Utc, false);
        assert_eq!(only, vec![Run::new("+20%/day", Tone::Level(Level::Orange))]);
        let need = outlook_runs(&WeeklyOutlook::NeedHistory, &Utc, false);
        assert_eq!(need, vec![Run::new("need more history", Tone::Dim)]);
        for o in [cap, safe, at_cap] {
            assert!(!plain_text(&outlook_runs(&o, &Utc, false)).contains("till"));
        }
    }

    #[test]
    fn outlook_hovers_say_what_the_rate_covers() {
        let t = |d, h| Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap();
        let cap = WeeklyOutlook::Cap {
            rate: rate(51.0),
            at: t(12, 15),
            minutes: 16.0 * 60.0,
            level: Level::Red,
        };
        assert_eq!(
            outlook_hover(Meter::All, &cap, &Utc),
            "7-day all: +51%/day · cap in 16h 00m (Mon 3p)\nrate over last 24h, projected to the weekly reset"
        );
        let partial = WeeklyOutlook::RateOnly {
            rate: analytics::WeeklyRate {
                per_day: 12.0,
                span: Duration::hours(3),
                basis: analytics::RateBasis::Partial,
                floored: true,
                since_reset: false,
            },
        };
        assert_eq!(
            outlook_hover(Meter::Fable, &partial, &Utc),
            "Fable 7-day: +12%/day\nrate over last 3h (app was closed), spread over 6h, projected to the weekly reset"
        );
        assert_eq!(
            outlook_hover(Meter::All, &WeeklyOutlook::NeedHistory, &Utc),
            "7-day all: need more history\nneeds 2h of samples in the last 24h"
        );
        assert_eq!(
            outlook_hover(Meter::Fable, &WeeklyOutlook::AtCap { reset: None }, &Utc),
            "Fable 7-day: at cap\nfrees up at the weekly reset"
        );
    }

    #[test]
    fn rows_per_meter_and_labels() {
        assert_eq!(meters(false), vec![Meter::All]);
        assert_eq!(meters(true), vec![Meter::All, Meter::Fable]);
        assert_eq!(Meter::All.label(), Run::new("7d all", Tone::Dim));
        assert_eq!(Meter::Fable.label(), Run::new("Fable", Tone::Fable));
    }

    #[test]
    fn stale_rows_are_muted() {
        let runs = vec![
            Run::new("+51%/day", Tone::Level(Level::Red)),
            Run::new(" · ", Tone::Dim),
        ];
        assert_eq!(
            dimmed(runs),
            vec![Run::new("+51%/day", Tone::Dim), Run::new(" · ", Tone::Dim)]
        );
    }

    #[test]
    fn notch_only_when_rest_is_taller() {
        assert!(!notch_fits(0.5));
        assert!(!notch_fits(FLOOR_GAP));
        assert!(notch_fits(FLOOR_GAP + 1.0));
    }
}
