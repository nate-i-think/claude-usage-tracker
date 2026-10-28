//! The weekly timeline: the current 7-day window drawn by the same widget
//! as the 5-hour one ([`super::timeline::show_chart`]), because the weekly
//! limits often run out first. The 7-day-all area and line in its state
//! color, Fable's own 7-day meter as a clay line (it is a limit of its own,
//! and often the first to cap), a dashed projection per meter from the
//! weekly outlook, daily ticks, and "reset Mon 11p" at the end. Under the
//! chart, one row per weekly limit gives its projection in words.
//!
//! With several accounts the chart overlays them (see
//! [`week_series_chart`]): each account's 7-day-all line solid and its
//! Fable line dashed in its series color, and its rows grouped under its
//! name. The app's [`Freshness`] is the primary account's; each account's
//! is derived from it ([`freshness_of`]).

use std::fmt::Display;

use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use eframe::egui::Ui;

use crate::analytics::{self, Level, Projection, WeeklyOutlook};
use crate::poller::Dashboard;

use super::accounts::AccountView;
use super::layout::CardSize;
use super::theme::Palette;
use super::timeline::{
    self, weekday_time_in, AreaStyle, Chart, EndLabel, Line, LineKind, Overlay, Point, SeriesData,
    TimeStyle,
};
use super::{Run, Tone};

/// The card's title.
pub const TITLE: &str = "7-day window";
/// A bridged gap's fill on the weekly chart: lighter than the 5-hour
/// chart's, so the gaps that remain (the value changed across them) don't
/// read as dark bands.
pub const WEEK_BRIDGE_FILL: f32 = 0.7;

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

/// Whether the Fable row shows: Fable's window reports a %, and not its
/// untouched state (0% with no reset time).
pub fn fable_row_shown(pct: Option<f64>, resets_at: Option<DateTime<Utc>>) -> bool {
    match pct {
        None => false,
        Some(p) => p > 0.0 || resets_at.is_some(),
    }
}

/// How old the snapshot behind the rows is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Freshness {
    /// Time since the snapshot was fetched.
    pub age: Duration,
    /// Older than two poll intervals: the rows are muted.
    pub stale: bool,
}

/// `o` (projected at the snapshot's fetch time) as seen `age` later: a cap
/// "in 16h" then is that much nearer now, never below 0. Absolute times
/// and the % at reset don't change.
pub fn aged(o: WeeklyOutlook, age: Duration) -> WeeklyOutlook {
    match o {
        WeeklyOutlook::Cap {
            rate,
            at,
            minutes,
            level,
        } => WeeklyOutlook::Cap {
            rate,
            at,
            minutes: (minutes - age.as_seconds_f64().max(0.0) / 60.0).max(0.0),
            level,
        },
        other => other,
    }
}

/// The meters with a row: 7-day all always, Fable when
/// [`fable_row_shown`].
pub fn meters(has_fable: bool) -> Vec<Meter> {
    let mut out = vec![Meter::All];
    if has_fable {
        out.push(Meter::Fable);
    }
    out
}

/// The dashed projection and end label of one weekly meter at `pct` now,
/// from its outlook, in a window ending at `window_end`:
/// - a cap before the reset: a line to 100% at the cap time, "cap …";
/// - safe: a line to the % at the reset, "safe";
/// - steady: a flat line to the window end, "safe" when the reset is known;
/// - the rate alone: a line to the window end (at most 100%), no label;
/// - at the cap, or a cap time already passed (an aged outlook): no line,
///   "at cap"; no history or no %: nothing.
pub fn weekly_line(
    o: &WeeklyOutlook,
    pct: Option<f64>,
    now: DateTime<Utc>,
    window_end: DateTime<Utc>,
    reset_known: bool,
) -> (Option<Projection>, Option<EndLabel>) {
    if matches!(o, WeeklyOutlook::AtCap { .. }) {
        return (None, Some(EndLabel::AtCap));
    }
    let Some(pct) = pct else {
        return (None, None);
    };
    let to_end = |end_pct: f64| Projection {
        end_t: window_end,
        end_pct,
        cap_at: None,
    };
    let line = |p: Projection, label| {
        (p.end_t > now)
            .then_some(p)
            .map_or((None, None), |p| (Some(p), label))
    };
    match *o {
        WeeklyOutlook::NeedHistory | WeeklyOutlook::AtCap { .. } => (None, None),
        WeeklyOutlook::Steady { .. } => line(to_end(pct), reset_known.then_some(EndLabel::Safe)),
        WeeklyOutlook::RateOnly { rate } => {
            let days = (window_end - now).as_seconds_f64() / 86_400.0;
            line(to_end((pct + rate.per_day * days).min(100.0)), None)
        }
        WeeklyOutlook::Cap { at, .. } if at <= now => (None, Some(EndLabel::AtCap)),
        WeeklyOutlook::Cap { at, .. } => line(
            Projection {
                end_t: at,
                end_pct: 100.0,
                cap_at: Some(at),
            },
            Some(EndLabel::Cap(at)),
        ),
        WeeklyOutlook::Safe {
            at_reset, reset, ..
        } => line(
            Projection {
                end_t: reset,
                end_pct: at_reset.min(100.0),
                cap_at: None,
            },
            Some(EndLabel::Safe),
        ),
    }
}

/// `projection` (from `pct` now) and `label` cut at the chart's end `chart_end` (Fable's own
/// reset can fall after the 7-day-all window shown): the line ends at the
/// edge at its interpolated %, and a cap or "safe" beyond the edge loses
/// its label and ✕ (the row under the chart still says it).
pub fn clamp_to_chart(
    pct: f64,
    projection: Option<Projection>,
    label: Option<EndLabel>,
    now: DateTime<Utc>,
    chart_end: DateTime<Utc>,
) -> (Option<Projection>, Option<EndLabel>) {
    let Some(p) = projection.filter(|p| p.end_t > chart_end) else {
        return (projection, label);
    };
    if chart_end <= now {
        return (None, None);
    }
    let frac = (chart_end - now).as_seconds_f64() / (p.end_t - now).as_seconds_f64();
    (
        Some(Projection {
            end_t: chart_end,
            end_pct: pct + (p.end_pct - pct) * frac,
            cap_at: None,
        }),
        None,
    )
}

/// The weekly outlooks of the rows and the chart: 7-day all, and Fable
/// when [`fable_row_shown`], projected at the snapshot's fetch time and
/// aged to `fresh.age` (see [`aged`]).
pub fn outlooks(d: &Dashboard, fresh: Freshness) -> Vec<(Meter, WeeklyOutlook)> {
    let snap = &d.snapshot;
    let at = snap.fetched_at;
    meters(fable_row_shown(snap.fable_pct(), snap.fable_resets_at()))
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
            (m, aged(o, fresh.age))
        })
        .collect()
}

/// The weekly chart for `d` at `now`, with `rows` from [`outlooks`].
pub fn week_chart(
    d: &Dashboard,
    now: DateTime<Utc>,
    palette: &Palette,
    rows: &[(Meter, WeeklyOutlook)],
) -> Chart {
    let snap = &d.snapshot;
    let pct = snap.seven_day_pct();
    let state = palette.level(analytics::color_for(pct));
    let reset_known = snap.seven_day_resets_at().is_some();
    let show_fable = rows.iter().any(|(m, _)| *m == Meter::Fable);
    let lines = rows
        .iter()
        .filter_map(|(m, o)| {
            let (pct, resets, color, cap_color) = match m {
                Meter::All => (
                    pct,
                    snap.seven_day_resets_at(),
                    state,
                    palette.level(o.level()),
                ),
                Meter::Fable => (
                    snap.fable_pct(),
                    snap.fable_resets_at(),
                    palette.fable,
                    palette.fable,
                ),
            };
            let end = resets.unwrap_or(d.week_end);
            let (projection, label) = weekly_line(o, pct, now, end, resets.is_some());
            let pct = pct?;
            let (projection, label) = clamp_to_chart(pct, projection, label, now, d.week_end);
            Some(Line {
                kind: match m {
                    Meter::All => LineKind::Main,
                    Meter::Fable => LineKind::Overlay,
                },
                pct,
                projection,
                label,
                color,
                cap_color,
            })
        })
        .collect();
    let mut points: Vec<Point> = d.week_samples.iter().map(Point::from).collect();
    if !show_fable {
        points.iter_mut().for_each(|p| p.overlay = None);
    }
    Chart {
        id: "week",
        start: d.week_start,
        end: d.week_end,
        now,
        series: vec![SeriesData {
            key: 0,
            label: String::new(),
            color: state,
            area: AreaStyle::State(state),
            overlay: if show_fable {
                Overlay::Line {
                    color: palette.fable,
                    dashed: false,
                }
            } else {
                Overlay::None
            },
            points,
            lines,
            dimmed: false,
            end: d.week_end,
            poll_key: snap.fetched_at.timestamp_millis(),
        }],
        gap_break: Duration::minutes(timeline::WEEK_GAP_BREAK_MINUTES),
        ticks: timeline::day_ticks_in(d.week_start, d.week_end, &Local),
        end_label: Some(if reset_known {
            format!("reset {}", weekday_time_in(d.week_end, &Local))
        } else {
            "now".to_owned()
        }),
        end_marks: Vec::new(),
        legend: Vec::new(),
        bridge_fill: WEEK_BRIDGE_FILL,
        style: TimeStyle::Weekday,
        note: None,
    }
}

/// One account's weekly rows on an overlaid card: its outlooks and whether
/// they are muted.
pub struct AccountRows<'a> {
    pub view: &'a AccountView<'a>,
    pub rows: Vec<(Meter, WeeklyOutlook)>,
    pub stale: bool,
}

/// `fresh` (the primary account's, from the app) for an account whose
/// snapshot is `age` old and whose last poll failed when `poll_failed`.
/// The app knows the poll interval, so only the primary's staleness is
/// given: an account at least as old as a stale primary is stale too, as
/// is one whose last poll failed. Its outlooks are aged by its own age.
pub fn freshness_of(fresh: Freshness, age: Duration, poll_failed: bool) -> Freshness {
    Freshness {
        age,
        stale: poll_failed || (fresh.stale && age >= fresh.age),
    }
}

/// The weekly rows of every account with data (see [`freshness_of`]).
pub fn account_rows<'a>(
    views: &[&'a AccountView<'a>],
    now: DateTime<Utc>,
    fresh: Freshness,
) -> Vec<AccountRows<'a>> {
    views
        .iter()
        .filter_map(|v| {
            let d = v.data?;
            let f = freshness_of(fresh, now - d.snapshot.fetched_at, v.stale);
            Some(AccountRows {
                view: v,
                rows: outlooks(d, f),
                stale: f.stale,
            })
        })
        .collect()
}

/// The weekly chart of several accounts (see [`timeline::overlay_charts`]):
/// each account's 7-day-all line solid and its Fable line dashed, in its
/// color, its projections cut at its own window end. An account whose
/// week is already over (stale data from before its reset) adds no window
/// and draws no now dot or projection.
pub fn week_series_chart(
    accounts: &[AccountRows],
    now: DateTime<Utc>,
    palette: &Palette,
) -> Option<Chart> {
    let parts = accounts
        .iter()
        .filter_map(|a| {
            let d = a.view.data?;
            let mut chart = week_chart(d, now, palette, &a.rows);
            // A stale account still errored past its reset: its week is
            // over, so it adds no window, no end mark, no now dot.
            let live = d.week_end > now;
            if !live {
                chart.series.iter_mut().for_each(|s| s.lines.clear());
            }
            Some(timeline::Part {
                chart,
                view: a.view,
                window: live.then_some((d.week_start, d.week_end)),
                reset_known: live && d.snapshot.seven_day_resets_at().is_some(),
            })
        })
        .collect();
    timeline::overlay_charts(parts, palette, |a, b| timeline::day_ticks_in(a, b, &Local))
}

/// The weekly card for the accounts: one account with data as it always
/// was (see [`show_account_card`], muted by `fresh`), several overlaid (see
/// [`show_series_card`]; each account's freshness from `fresh` by
/// [`freshness_of`]); 0 (nothing drawn) without data.
pub fn show_week_card(
    ui: &mut Ui,
    accounts: &[AccountView],
    now: DateTime<Utc>,
    palette: &Palette,
    size: CardSize,
    fresh: Freshness,
) -> f32 {
    match timeline::with_data(accounts)[..] {
        [] => 0.0,
        [v] => show_account_card(ui, v, now, palette, size, fresh),
        ref several => show_series_card(ui, several, now, palette, size, fresh),
    }
}

/// The weekly card of several accounts: the title with the legend, the
/// overlaid chart, and each account's projection rows under its name.
fn show_series_card(
    ui: &mut Ui,
    views: &[&AccountView],
    now: DateTime<Utc>,
    palette: &Palette,
    size: CardSize,
    fresh: Freshness,
) -> f32 {
    let accounts = account_rows(views, now, fresh);
    let Some(chart) = week_series_chart(&accounts, now, palette) else {
        return 0.0;
    };
    super::theme::card(palette)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let top = ui.cursor().top();
            ui.set_min_height(size.min_content);
            timeline::show_title(ui, palette, TITLE, None, &chart.legend);
            timeline::show_chart(ui, &chart, palette, size.chart);
            for a in &accounts {
                timeline::series_header(ui, palette, a.view);
                outlook_rows(ui, &a.rows, a.stale);
            }
            ui.cursor().top() - top - ui.spacing().item_spacing.y
        })
        .inner
}

/// The weekly card: the title with the window's span as its subtitle, the
/// chart, `size.chart` tall, and one projection row per weekly limit under
/// it, muted when `fresh.stale`. Returns the content's natural height
/// (before `size.min_content`).
fn show_account_card(
    ui: &mut Ui,
    view: &AccountView,
    now: DateTime<Utc>,
    palette: &Palette,
    size: CardSize,
    fresh: Freshness,
) -> f32 {
    let Some(d) = view.data else { return 0.0 };
    let rows = outlooks(d, fresh);
    let mut chart = week_chart(d, now, palette, &rows);
    chart.series[0].key = timeline::series_key(view.id);
    let reset_known = d.snapshot.seven_day_resets_at().is_some();
    let span = timeline::week_span_in(d.week_start, d.week_end, reset_known, &Local);
    super::theme::card(palette)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let top = ui.cursor().top();
            // Reserved from the cursor, so set before the content.
            ui.set_min_height(size.min_content);
            super::theme::chart_title(ui, palette, TITLE, span.as_deref());
            timeline::show_chart(ui, &chart, palette, size.chart);
            outlook_rows(ui, &rows, fresh.stale);
            // From the cursor: min_rect includes the reserved height.
            ui.cursor().top() - top - ui.spacing().item_spacing.y
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::ProfileId;

    fn t(d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap()
    }

    #[test]
    fn weekly_lines_follow_the_outlook() {
        let now = t(12, 12);
        let end = t(13, 23);
        // A cap before the reset: to 100% at the cap, "cap …".
        let cap = WeeklyOutlook::Cap {
            rate: rate(51.0),
            at: t(13, 3),
            minutes: 15.0 * 60.0,
            level: Level::Red,
        };
        let (p, l) = weekly_line(&cap, Some(68.0), now, end, true);
        assert_eq!(
            p,
            Some(Projection {
                end_t: t(13, 3),
                end_pct: 100.0,
                cap_at: Some(t(13, 3))
            })
        );
        assert_eq!(l, Some(EndLabel::Cap(t(13, 3))));
        // Safe: to the % at the reset.
        let safe = WeeklyOutlook::Safe {
            rate: rate(6.0),
            at_reset: 77.5,
            reset: end,
        };
        let (p, l) = weekly_line(&safe, Some(70.0), now, end, true);
        assert_eq!(p.map(|p| (p.end_t, p.end_pct)), Some((end, 77.5)));
        assert_eq!(l, Some(EndLabel::Safe));
        // Steady: flat to the end; "safe" only with a known reset.
        let steady = WeeklyOutlook::Steady { rate: rate(0.1) };
        let (p, l) = weekly_line(&steady, Some(40.0), now, end, true);
        assert_eq!(p.map(|p| (p.end_t, p.end_pct)), Some((end, 40.0)));
        assert_eq!(l, Some(EndLabel::Safe));
        assert_eq!(weekly_line(&steady, Some(40.0), now, end, false).1, None);
        // The rate alone: to the window end, at most 100%, no label.
        let only = WeeklyOutlook::RateOnly { rate: rate(24.0) };
        let (p, l) = weekly_line(&only, Some(40.0), now, t(13, 12), false);
        assert_eq!(p.map(|p| p.end_pct), Some(64.0));
        assert_eq!(l, None);
        let (p, _) = weekly_line(&only, Some(90.0), now, t(13, 12), false);
        assert_eq!(p.map(|p| p.end_pct), Some(100.0));
        // At the cap: no line, "at cap".
        let at_cap = WeeklyOutlook::AtCap { reset: Some(end) };
        assert_eq!(
            weekly_line(&at_cap, Some(100.0), now, end, true),
            (None, Some(EndLabel::AtCap))
        );
        // Nothing to draw.
        assert_eq!(
            weekly_line(&WeeklyOutlook::NeedHistory, Some(40.0), now, end, true),
            (None, None)
        );
        assert_eq!(weekly_line(&safe, None, now, end, true), (None, None));
        // A window already over (unknown reset: it ends now): no line.
        assert_eq!(
            weekly_line(&only, Some(40.0), now, now, false),
            (None, None)
        );
        // A stale cap time already past is drawn from now.
        let late = WeeklyOutlook::Cap {
            rate: rate(51.0),
            at: t(12, 10),
            minutes: 0.0,
            level: Level::Red,
        };
        assert_eq!(
            weekly_line(&late, Some(99.0), now, end, true),
            (None, Some(EndLabel::AtCap)),
            "a cap time already passed shows as at the cap"
        );
    }

    #[test]
    fn projections_stop_at_the_chart_end() {
        let now = t(12, 12);
        let chart_end = t(13, 12);
        // Fable caps two days out, after the window shown: the line ends at
        // the edge, halfway from 60 to 100, without its label.
        let p = Projection {
            end_t: t(14, 12),
            end_pct: 100.0,
            cap_at: Some(t(14, 12)),
        };
        let (q, l) = clamp_to_chart(
            60.0,
            Some(p),
            Some(EndLabel::Cap(t(14, 12))),
            now,
            chart_end,
        );
        assert_eq!(
            q,
            Some(Projection {
                end_t: chart_end,
                end_pct: 80.0,
                cap_at: None
            })
        );
        assert_eq!(l, None);
        // Inside the chart: untouched.
        let inside = Projection {
            end_t: t(13, 0),
            end_pct: 100.0,
            cap_at: Some(t(13, 0)),
        };
        let label = Some(EndLabel::Cap(t(13, 0)));
        assert_eq!(
            clamp_to_chart(60.0, Some(inside), label, now, chart_end),
            (Some(inside), label)
        );
        assert_eq!(
            clamp_to_chart(60.0, None, Some(EndLabel::AtCap), now, chart_end),
            (None, Some(EndLabel::AtCap))
        );
        // A chart already over: nothing.
        assert_eq!(clamp_to_chart(60.0, Some(p), label, now, now), (None, None));
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

    // --- several accounts -----------------------------------------------------

    /// A dashboard polled at `at` with 7-day all at `pct` resetting at
    /// `reset`, and Fable 7-day at `fable` (resetting two days after) when
    /// given; its week ends at the reset.
    fn week_dash(
        at: DateTime<Utc>,
        pct: f64,
        reset: DateTime<Utc>,
        fable: Option<f64>,
    ) -> Dashboard {
        use crate::analytics::WeekSample;
        use crate::model::{Snapshot, UsageWindow, FABLE_LABEL, SEVEN_DAY_LABEL};
        let mut windows = vec![UsageWindow {
            label: SEVEN_DAY_LABEL.to_owned(),
            utilization: Some(pct),
            resets_at: Some(reset),
        }];
        if let Some(f) = fable {
            windows.push(UsageWindow {
                label: FABLE_LABEL.to_owned(),
                utilization: Some(f),
                resets_at: Some(reset + Duration::days(2)),
            });
        }
        let sample = |h: i64, v: f64| WeekSample {
            t: at - Duration::hours(h),
            seven_day: Some(v),
            fable,
        };
        Dashboard {
            snapshot: Snapshot {
                fetched_at: at,
                windows,
                extra: None,
                breakdown: None,
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            seven_day_recent: Vec::new(),
            fable_recent: Vec::new(),
            burners: Vec::new(),
            window_start: at,
            window_end: at,
            window_samples: Vec::new(),
            week_start: reset - Duration::days(7),
            week_end: reset,
            week_samples: vec![sample(2, pct - 1.0), sample(0, pct)],
            persist_error: None,
            plan: None,
            token_expires_at: None,
        }
    }

    fn view<'a>(id: &'a ProfileId, d: &'a Dashboard, stale: bool) -> AccountView<'a> {
        AccountView {
            id,
            label: id.as_str(),
            color: crate::ui::theme::DARK.series(id.as_str().len() as u8),
            data: Some(d),
            error: None,
            stale,
        }
    }

    #[test]
    fn overlaid_week_chart_dashes_fable_only_where_reported() {
        let now = t(12, 12);
        let palette = &crate::ui::theme::DARK;
        let a = week_dash(now, 40.0, t(14, 0), Some(30.0));
        let b = week_dash(now, 20.0, t(16, 0), None);
        let (ia, ib) = (ProfileId::new("default"), ProfileId::new("work"));
        let (va, vb) = (view(&ia, &a, false), view(&ib, &b, true));
        let fresh = Freshness {
            age: Duration::zero(),
            stale: false,
        };
        let mut accounts = account_rows(&[&va, &vb], now, fresh);
        assert_eq!(accounts[0].rows.len(), 2, "7d all and Fable");
        // Outlooks with lines: "default" caps inside its week, its Fable
        // (resetting two days after) runs past it; "work" is safe.
        accounts[0].rows = vec![
            (
                Meter::All,
                WeeklyOutlook::Cap {
                    rate: rate(30.0),
                    at: t(13, 12),
                    minutes: 24.0 * 60.0,
                    level: Level::Red,
                },
            ),
            (Meter::Fable, WeeklyOutlook::RateOnly { rate: rate(10.0) }),
        ];
        accounts[1].rows = vec![(
            Meter::All,
            WeeklyOutlook::Safe {
                rate: rate(2.0),
                at_reset: 28.0,
                reset: t(16, 0),
            },
        )];
        assert_eq!(accounts[1].rows.len(), 1);
        assert!(
            !accounts[0].stale && accounts[1].stale,
            "a failed poll mutes"
        );
        let c = week_series_chart(&accounts, now, palette).unwrap();
        assert_eq!((c.start, c.end), (t(7, 0), t(16, 0)));
        assert_eq!(c.end_marks.len(), 2);
        assert_eq!(
            c.series[0].overlay,
            Overlay::Line {
                color: va.color,
                dashed: true
            }
        );
        assert_eq!(c.series[1].overlay, Overlay::None);
        assert!(c.series[1].dimmed);
        assert!(c.series[0].points.iter().all(|p| p.overlay == Some(30.0)));
        assert!(c.series[1].points.iter().all(|p| p.overlay.is_none()));
        // Each projection stays within its own account's window: the
        // cap and "safe" as projected, Fable's cut at "default"'s end.
        let ends = |s: &SeriesData| -> Vec<Option<DateTime<Utc>>> {
            s.lines
                .iter()
                .map(|l| l.projection.map(|p| p.end_t))
                .collect()
        };
        assert_eq!(ends(&c.series[0]), [Some(t(13, 12)), Some(a.week_end)]);
        assert_eq!(ends(&c.series[1]), [Some(b.week_end)]);
        for (s, end) in c.series.iter().zip([a.week_end, b.week_end]) {
            for l in &s.lines {
                assert!(l.projection.is_some_and(|p| p.end_t <= end));
            }
        }
        let legend: Vec<_> = c
            .legend
            .iter()
            .map(|e| (e.label.as_str(), e.dashed))
            .collect();
        assert_eq!(
            legend,
            [("default", false), ("work", false), ("Fable", true)]
        );
    }

    #[test]
    fn an_account_past_its_reset_adds_no_window() {
        let now = t(12, 12);
        let palette = &crate::ui::theme::DARK;
        // "old" errored since before its reset on the 10th.
        let old = week_dash(t(9, 0), 60.0, t(10, 0), None);
        let live = week_dash(now, 20.0, t(16, 0), None);
        let (io, il) = (ProfileId::new("old"), ProfileId::new("live"));
        let (vo, vl) = (view(&io, &old, true), view(&il, &live, false));
        let fresh = Freshness {
            age: Duration::zero(),
            stale: false,
        };
        let mut accounts = account_rows(&[&vo, &vl], now, fresh);
        accounts[0].rows = vec![(Meter::All, WeeklyOutlook::Steady { rate: rate(0.1) })];
        let c = week_series_chart(&accounts, now, palette).unwrap();
        assert_eq!((c.start, c.end), (live.week_start, live.week_end));
        assert_eq!(c.end_marks, vec![(live.week_end, vl.color)]);
        assert!(c.series[0].lines.is_empty(), "no now dot or projection");
        assert_eq!(c.series.len(), 2, "still in the legend");
    }

    #[test]
    fn one_account_week_chart_keeps_the_fable_clay_line() {
        let now = t(12, 12);
        let palette = &crate::ui::theme::DARK;
        let d = week_dash(now, 40.0, t(14, 0), Some(30.0));
        let fresh = Freshness {
            age: Duration::zero(),
            stale: false,
        };
        let c = week_chart(&d, now, palette, &outlooks(&d, fresh));
        let s = &c.series[0];
        assert_eq!(
            s.overlay,
            Overlay::Line {
                color: palette.fable,
                dashed: false
            }
        );
        let state = palette.level(analytics::color_for(Some(40.0)));
        assert_eq!((s.color, s.area), (state, AreaStyle::State(state)));
        assert!(c.legend.is_empty() && c.end_marks.is_empty());
    }

    #[test]
    fn each_account_ages_and_mutes_by_its_own_snapshot() {
        let fresh = Freshness {
            age: Duration::minutes(30),
            stale: true,
        };
        let f = freshness_of(fresh, Duration::minutes(40), false);
        assert_eq!(f.age, Duration::minutes(40));
        assert!(f.stale, "older than a stale primary");
        assert!(!freshness_of(fresh, Duration::minutes(2), false).stale);
        assert!(
            freshness_of(fresh, Duration::minutes(2), true).stale,
            "poll failed"
        );
        let ok = Freshness {
            stale: false,
            ..fresh
        };
        assert!(!freshness_of(ok, Duration::minutes(40), false).stale);
    }

    #[test]
    fn rows_per_meter_and_labels() {
        assert_eq!(meters(false), vec![Meter::All]);
        assert_eq!(meters(true), vec![Meter::All, Meter::Fable]);
        assert_eq!(Meter::All.label(), Run::new("7d all", Tone::Dim));
        assert_eq!(Meter::Fable.label(), Run::new("Fable", Tone::Fable));
    }

    #[test]
    fn fable_row_hidden_when_untouched() {
        let reset = Utc.with_ymd_and_hms(2026, 10, 13, 6, 0, 0).unwrap();
        assert!(!fable_row_shown(None, Some(reset)));
        assert!(!fable_row_shown(Some(0.0), None));
        assert!(fable_row_shown(Some(0.0), Some(reset)));
        assert!(fable_row_shown(Some(12.0), None));
    }

    #[test]
    fn cap_in_is_aged_to_now() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 15, 0, 0).unwrap();
        let cap = WeeklyOutlook::Cap {
            rate: rate(51.0),
            at,
            minutes: 16.0 * 60.0,
            level: Level::Red,
        };
        let WeeklyOutlook::Cap { minutes, at: a, .. } = aged(cap, Duration::minutes(90)) else {
            panic!("still a cap");
        };
        assert_eq!(minutes, 14.5 * 60.0);
        assert_eq!(a, at);
        let WeeklyOutlook::Cap { minutes, .. } = aged(cap, Duration::days(2)) else {
            panic!("still a cap");
        };
        assert_eq!(minutes, 0.0);
        // Compact copy then shows the aged figure.
        let runs = outlook_runs(&aged(cap, Duration::minutes(90)), &Utc, true);
        assert_eq!(crate::ui::plain_text(&runs), "+51%/day · cap in 14h 30m");
        // Other outlooks are untouched.
        let safe = WeeklyOutlook::Safe {
            rate: rate(6.0),
            at_reset: 70.0,
            reset: at,
        };
        assert_eq!(aged(safe, Duration::hours(3)), safe);
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
}
