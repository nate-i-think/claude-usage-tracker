//! The weekly timeline: the current 7-day window drawn by the same widget
//! as the 5-hour one ([`super::timeline::show_chart`]), because the weekly
//! limits often run out first. The 7-day-all area and line in its state
//! color, a dashed projection from the weekly outlook, daily ticks, and
//! "reset Mon 11p" at the end. The chips and the chart's cap and "safe"
//! marks carry the projection, so no rows repeat it in words.
//!
//! With several accounts the chart overlays them (see
//! [`week_series_chart`]): each account's 7-day-all line in its series
//! color.

use chrono::{DateTime, Duration, Local, Utc};
use eframe::egui::Ui;

use crate::analytics::{self, Projection, WeeklyOutlook};
use crate::poller::Dashboard;

use super::accounts::AccountView;
use super::layout::CardSize;
use super::theme::Palette;
use super::timeline::{
    self, weekday_time_in, AreaStyle, Chart, EndLabel, Line, Point, SeriesData, TimeStyle,
};

/// The card's title.
pub const TITLE: &str = "7-day window";
/// A bridged gap's fill on the weekly chart: lighter than the 5-hour
/// chart's, so the gaps that remain (the value changed across them) don't
/// read as dark bands.
pub const WEEK_BRIDGE_FILL: f32 = 0.7;

/// The dashed projection and end label of the weekly meter at `pct` now,
/// from its outlook, in a window ending at `window_end`:
/// - a cap before the reset: a line to 100% at the cap time, "cap …";
/// - safe: a line to the % at the reset, "safe";
/// - steady: a flat line to the window end, "safe" when the reset is known;
/// - the rate alone: a line to the window end (at most 100%), no label;
/// - at the cap, or a cap time already passed (a stale snapshot): no line,
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

/// The weekly outlook of the chart: 7-day all, projected at the
/// snapshot's fetch time.
pub fn outlook(d: &Dashboard) -> WeeklyOutlook {
    let snap = &d.snapshot;
    analytics::weekly_outlook(
        &d.seven_day_recent,
        snap.seven_day_pct(),
        snap.seven_day_resets_at(),
        snap.fetched_at,
    )
}

/// The weekly chart for `d` at `now`, with `outlook` from [`outlook()`].
pub fn week_chart(
    d: &Dashboard,
    now: DateTime<Utc>,
    palette: &Palette,
    outlook: &WeeklyOutlook,
) -> Chart {
    let snap = &d.snapshot;
    let pct = snap.seven_day_pct();
    let state = palette.level(analytics::color_for(pct));
    let resets = snap.seven_day_resets_at();
    let end = resets.unwrap_or(d.week_end);
    let (projection, label) = weekly_line(outlook, pct, now, end, resets.is_some());
    let lines = pct
        .map(|pct| Line {
            pct,
            projection,
            label,
            color: state,
            cap_color: palette.level(outlook.level()),
        })
        .into_iter()
        .collect();
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
            points: d.week_samples.iter().map(Point::from).collect(),
            lines,
            dimmed: false,
            end: d.week_end,
            poll_key: snap.fetched_at.timestamp_millis(),
        }],
        gap_break: Duration::minutes(timeline::WEEK_GAP_BREAK_MINUTES),
        ticks: timeline::day_ticks_in(d.week_start, d.week_end, &Local),
        end_label: Some(if resets.is_some() {
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

/// One account's weekly outlook on an overlaid card.
pub struct AccountOutlook<'a> {
    pub view: &'a AccountView<'a>,
    pub outlook: WeeklyOutlook,
}

/// The weekly outlook of every account with data (see [`outlook()`]).
pub fn account_outlooks<'a>(views: &[&'a AccountView<'a>]) -> Vec<AccountOutlook<'a>> {
    views
        .iter()
        .filter_map(|v| {
            Some(AccountOutlook {
                view: v,
                outlook: outlook(v.data?),
            })
        })
        .collect()
}

/// The weekly chart of several accounts (see [`timeline::overlay_charts`]):
/// each account's 7-day-all line in its color, its projection ending
/// within its own window. An account whose week is already over (stale
/// data from before its reset) adds no window and draws no now dot or
/// projection.
pub fn week_series_chart(
    accounts: &[AccountOutlook],
    now: DateTime<Utc>,
    palette: &Palette,
) -> Option<Chart> {
    let parts = accounts
        .iter()
        .filter_map(|a| {
            let d = a.view.data?;
            let mut chart = week_chart(d, now, palette, &a.outlook);
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
    timeline::overlay_charts(parts, |a, b| timeline::day_ticks_in(a, b, &Local))
}

/// The weekly card for the accounts: one account with data as it always
/// was (see [`show_account_card`]), several overlaid (see
/// [`show_series_card`]); 0 (nothing drawn) without data.
pub fn show_week_card(
    ui: &mut Ui,
    accounts: &[AccountView],
    now: DateTime<Utc>,
    palette: &Palette,
    size: CardSize,
) -> f32 {
    match timeline::with_data(accounts)[..] {
        [] => 0.0,
        [v] => show_account_card(ui, v, now, palette, size),
        ref several => show_series_card(ui, several, now, palette, size),
    }
}

/// The weekly card of several accounts: the title with the legend and the
/// overlaid chart.
fn show_series_card(
    ui: &mut Ui,
    views: &[&AccountView],
    now: DateTime<Utc>,
    palette: &Palette,
    size: CardSize,
) -> f32 {
    let accounts = account_outlooks(views);
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
            ui.cursor().top() - top - ui.spacing().item_spacing.y
        })
        .inner
}

/// The weekly card: the title with the window's span as its subtitle and
/// the chart, `size.chart` tall. Returns the content's natural height
/// (before `size.min_content`).
fn show_account_card(
    ui: &mut Ui,
    view: &AccountView,
    now: DateTime<Utc>,
    palette: &Palette,
    size: CardSize,
) -> f32 {
    let Some(d) = view.data else { return 0.0 };
    let mut chart = week_chart(d, now, palette, &outlook(d));
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
            // From the cursor: min_rect includes the reserved height.
            ui.cursor().top() - top - ui.spacing().item_spacing.y
        })
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::Level;
    use crate::profiles::ProfileId;
    use chrono::TimeZone;

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

    // --- several accounts -----------------------------------------------------

    /// A dashboard polled at `at` with 7-day all at `pct` resetting at
    /// `reset`; its week ends at the reset.
    fn week_dash(at: DateTime<Utc>, pct: f64, reset: DateTime<Utc>) -> Dashboard {
        use crate::analytics::WeekSample;
        use crate::model::{Snapshot, UsageWindow, SEVEN_DAY_LABEL};
        let sample = |h: i64, v: f64| WeekSample {
            t: at - Duration::hours(h),
            seven_day: Some(v),
        };
        Dashboard {
            snapshot: Snapshot {
                fetched_at: at,
                windows: vec![UsageWindow {
                    label: SEVEN_DAY_LABEL.to_owned(),
                    utilization: Some(pct),
                    resets_at: Some(reset),
                }],
                extra: None,
                breakdown: None,
            },
            history: Vec::new(),
            seven_day_recent: Vec::new(),
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
    fn overlaid_week_chart_projects_each_account_in_its_window() {
        let now = t(12, 12);
        let palette = &crate::ui::theme::DARK;
        let a = week_dash(now, 40.0, t(14, 0));
        let b = week_dash(now, 20.0, t(16, 0));
        let (ia, ib) = (ProfileId::new("default"), ProfileId::new("work"));
        let (va, vb) = (view(&ia, &a, false), view(&ib, &b, true));
        let mut accounts = account_outlooks(&[&va, &vb]);
        // Outlooks with lines: "default" caps inside its week, "work" is
        // safe.
        accounts[0].outlook = WeeklyOutlook::Cap {
            rate: rate(30.0),
            at: t(13, 12),
            minutes: 24.0 * 60.0,
            level: Level::Red,
        };
        accounts[1].outlook = WeeklyOutlook::Safe {
            rate: rate(2.0),
            at_reset: 28.0,
            reset: t(16, 0),
        };
        let c = week_series_chart(&accounts, now, palette).unwrap();
        assert_eq!((c.start, c.end), (t(7, 0), t(16, 0)));
        assert_eq!(c.end_marks.len(), 2);
        assert!(c.series[1].dimmed);
        // Each projection stays within its own account's window: the
        // cap and "safe" as projected.
        let ends = |s: &SeriesData| -> Vec<Option<DateTime<Utc>>> {
            s.lines
                .iter()
                .map(|l| l.projection.map(|p| p.end_t))
                .collect()
        };
        assert_eq!(ends(&c.series[0]), [Some(t(13, 12))]);
        assert_eq!(ends(&c.series[1]), [Some(b.week_end)]);
        for (s, end) in c.series.iter().zip([a.week_end, b.week_end]) {
            for l in &s.lines {
                assert!(l.projection.is_some_and(|p| p.end_t <= end));
            }
        }
        let legend: Vec<_> = c.legend.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(legend, ["default", "work"]);
    }

    #[test]
    fn an_account_past_its_reset_adds_no_window() {
        let now = t(12, 12);
        let palette = &crate::ui::theme::DARK;
        // "old" errored since before its reset on the 10th.
        let old = week_dash(t(9, 0), 60.0, t(10, 0));
        let live = week_dash(now, 20.0, t(16, 0));
        let (io, il) = (ProfileId::new("old"), ProfileId::new("live"));
        let (vo, vl) = (view(&io, &old, true), view(&il, &live, false));
        let mut accounts = account_outlooks(&[&vo, &vl]);
        accounts[0].outlook = WeeklyOutlook::Steady { rate: rate(0.1) };
        let c = week_series_chart(&accounts, now, palette).unwrap();
        assert_eq!((c.start, c.end), (live.week_start, live.week_end));
        assert_eq!(c.end_marks, vec![(live.week_end, vl.color)]);
        assert!(c.series[0].lines.is_empty(), "no now dot or projection");
        assert_eq!(c.series.len(), 2, "still in the legend");
    }

    #[test]
    fn one_account_week_chart_in_state_colors() {
        let now = t(12, 12);
        let palette = &crate::ui::theme::DARK;
        let d = week_dash(now, 40.0, t(14, 0));
        let c = week_chart(&d, now, palette, &outlook(&d));
        let s = &c.series[0];
        let state = palette.level(analytics::color_for(Some(40.0)));
        assert_eq!((s.color, s.area), (state, AreaStyle::State(state)));
        assert_eq!(s.lines.len(), 1);
        assert!(c.legend.is_empty() && c.end_marks.is_empty());
    }
}
