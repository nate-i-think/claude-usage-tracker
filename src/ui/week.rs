//! The weekly timeline: the current 7-day window drawn by the same widget
//! as the 5-hour one ([`super::timeline::show_chart`]), because the weekly
//! limits often run out first. The 7-day-all area and line in its state
//! color, Fable's own 7-day meter as a clay line (it is a limit of its own,
//! and often the first to cap), a dashed projection per meter from the
//! weekly outlook, daily ticks, and "reset Mon 11p" at the end. Under the
//! chart, one row per weekly limit gives its projection in words.

use std::fmt::Display;

use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use eframe::egui::Ui;

use crate::analytics::{self, Level, Projection, WeeklyOutlook};
use crate::poller::Dashboard;

use super::layout::CardSize;
use super::theme::Palette;
use super::timeline::{
    self, weekday_time_in, Chart, EndLabel, Line, LineKind, Overlay, Point, TimeStyle,
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
        points,
        gap_break: Duration::minutes(timeline::WEEK_GAP_BREAK_MINUTES),
        state,
        overlay: if show_fable {
            Overlay::Line
        } else {
            Overlay::None
        },
        ticks: timeline::day_ticks_in(d.week_start, d.week_end, &Local),
        end_label: Some(if reset_known {
            format!("reset {}", weekday_time_in(d.week_end, &Local))
        } else {
            "now".to_owned()
        }),
        lines,
        bridge_fill: WEEK_BRIDGE_FILL,
        style: TimeStyle::Weekday,
        poll_key: snap.fetched_at.timestamp_millis(),
        note: None,
    }
}

/// The weekly card: the title with the window's span as its subtitle, the
/// chart, `size.chart` tall, and one projection row per weekly limit under
/// it, muted when `fresh.stale`. Returns the content's natural height
/// (before `size.min_content`).
pub fn show_week_card(
    ui: &mut Ui,
    d: &Dashboard,
    now: DateTime<Utc>,
    palette: &Palette,
    size: CardSize,
    fresh: Freshness,
) -> f32 {
    let rows = outlooks(d, fresh);
    let chart = week_chart(d, now, palette, &rows);
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
