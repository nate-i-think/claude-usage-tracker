//! The weekly projections: where a weekly meter (7-day all, or Fable's own
//! 7-day limit) lands at its reset.
//!
//! The 5-hour burn rate (15 minutes of samples) says nothing about the
//! week, so the weekly rate is taken over a long trailing window starting
//! at `start = max(now - 24h, resets_at - 7d)`. The anchor is the latest
//! sample at or before `start` (within the week), and the rate is the rise
//! from it over the exact `now - anchor_t`: about 24 h normally, days when
//! the app was closed (labelled as a gap, "over last 3d (app was closed)").
//! So a burst after a gap isn't averaged away, the samples inside the
//! window give a second rate when they span at least
//! [`WEEKLY_FLOOR_HOURS`], and the higher of the two wins. Without an
//! anchor (the app was closed, or the week just reset) the rate is the rise
//! over the measured span. A rise is never divided by less than
//! [`WEEKLY_FLOOR_HOURS`], so a short burst can't project a cap. Projected
//! at the reset, the meter either crosses 100% first ("cap in 2d 5h") or
//! lands under it ("≈78% at reset").

use chrono::{DateTime, Duration, Utc};

use super::Level;

/// The weekly rate is measured over the most recent this-many hours.
pub const WEEKLY_RATE_HOURS: i64 = 24;
/// A span shorter than this gives no rate ("need more history").
pub const WEEKLY_MIN_SPAN_HOURS: i64 = 2;
/// A rise is never spread over less than this.
pub const WEEKLY_FLOOR_HOURS: i64 = 6;
/// At or below this %/day the week counts as steady.
pub const WEEKLY_STEADY_PER_DAY: f64 = 0.5;
/// A cap closer than this many hours is red, else orange.
const WEEKLY_RED_HOURS: f64 = 24.0;
/// A drop of more than this many points between samples is a reset (the
/// rule of [`super::recent_window`]).
const RESET_DROP: f64 = 5.0;

/// How a weekly rate was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateBasis {
    /// From the anchor (the latest sample at or before the start) to now;
    /// `gap` when the anchor sits more than [`ANCHOR_GAP_MINUTES`] before
    /// the start (the app was closed).
    Anchored { gap: bool },
    /// The samples inside the window, rising faster than the anchored
    /// rate (a burst after a gap).
    Recent,
    /// No anchor: over the measured span.
    Partial,
}

/// An anchor further than this before the start means the app was closed.
pub const ANCHOR_GAP_MINUTES: i64 = 60;

/// A weekly rate and what it was measured over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeeklyRate {
    /// %/day.
    pub per_day: f64,
    /// The span the rate covers: `now - anchor_t` when anchored, else the
    /// measured span.
    pub span: Duration,
    pub basis: RateBasis,
    /// The span was under [`WEEKLY_FLOOR_HOURS`], so the rise was spread
    /// over the floor instead.
    pub floored: bool,
    /// The start was clipped to the weekly window's start (a recent reset).
    pub since_reset: bool,
}

/// Weekly rate over the trailing window of `samples` (one weekly meter,
/// oldest first, possibly with samples before the window) up to `now`.
///
/// Only samples within the current week (`resets_at - 7d`, when known) and
/// after the last drop of more than 5 points (a reset the clipping missed)
/// count. `None` with nothing measured inside the window, or a span under
/// [`WEEKLY_MIN_SPAN_HOURS`].
pub fn weekly_rate(
    samples: &[(DateTime<Utc>, Option<f64>)],
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<WeeklyRate> {
    let week_start = resets_at.map(|r| r - Duration::days(7));
    let trailing = now - Duration::hours(WEEKLY_RATE_HOURS);
    let start = week_start.filter(|w| *w > trailing).unwrap_or(trailing);
    let since_reset = start > trailing;
    let valid: Vec<(DateTime<Utc>, f64)> = samples
        .iter()
        .filter(|(t, _)| *t <= now && week_start.is_none_or(|w| *t >= w))
        .filter_map(|(t, v)| v.filter(|v| !v.is_nan()).map(|v| (*t, v)))
        .collect();
    let after_reset = valid
        .windows(2)
        .rposition(|pair| pair[1].1 + RESET_DROP < pair[0].1)
        .map_or(0, |i| i + 1);
    let valid = &valid[after_reset..];
    let &(last_t, last) = valid.last()?;
    let floor = Duration::hours(WEEKLY_FLOOR_HOURS);
    let rate = |from: f64, span: Duration, basis| WeeklyRate {
        per_day: (last - from) / (span.max(floor).as_seconds_f64() / 86_400.0),
        span,
        basis,
        floored: span < floor,
        since_reset,
    };
    let anchor = valid.iter().rev().find(|(t, _)| *t <= start);
    let best = match anchor {
        Some(_) if last_t <= start => return None, // nothing inside the window
        Some(&(anchor_t, anchor_v)) => {
            let gap = start - anchor_t > Duration::minutes(ANCHOR_GAP_MINUTES);
            let anchored = rate(anchor_v, now - anchor_t, RateBasis::Anchored { gap });
            // After a gap, the samples inside the window, when they span the
            // floor: a burst since the app came back outranks the long
            // average. Without a gap the anchored rate already covers them.
            let inside = valid.iter().find(|(t, _)| *t > start).filter(|_| gap);
            let recent = inside
                .map(|&(first_t, first)| (first, last_t - first_t))
                .filter(|(_, span)| *span >= floor)
                .map(|(first, span)| rate(first, span, RateBasis::Recent));
            match recent {
                Some(r) if r.per_day > anchored.per_day + RATE_EPSILON => r,
                _ => anchored,
            }
        }
        None => {
            let &(first_t, first) = valid.first()?;
            rate(first, last_t - first_t, RateBasis::Partial)
        }
    };
    (best.span >= Duration::hours(WEEKLY_MIN_SPAN_HOURS)).then_some(best)
}

/// Rates closer than this (%/day) count as equal, so the anchored rate
/// keeps the label when the window's own samples say the same.
const RATE_EPSILON: f64 = 1e-6;

/// What a weekly projection says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WeeklyOutlook {
    /// Not enough samples for a rate.
    NeedHistory,
    /// The meter is at or over 100%; it frees up at `reset`.
    AtCap { reset: Option<DateTime<Utc>> },
    /// The rate is at most [`WEEKLY_STEADY_PER_DAY`].
    Steady { rate: WeeklyRate },
    /// The reset (or the current %) is unknown: the rate alone.
    RateOnly { rate: WeeklyRate },
    /// 100% comes before the reset, at `at`, `minutes` from now.
    Cap {
        rate: WeeklyRate,
        at: DateTime<Utc>,
        minutes: f64,
        level: Level,
    },
    /// The reset comes first, with the meter at `at_reset` %.
    Safe {
        rate: WeeklyRate,
        at_reset: f64,
        reset: DateTime<Utc>,
    },
}

impl WeeklyOutlook {
    /// Color of the outlook: red at the cap or for a cap within a day,
    /// orange for a later cap, green when steady or safe, dim without a
    /// rate. The rate alone is orange at a pace that fills a week in under
    /// a week (over 100/7 %/day), else green.
    pub fn level(&self) -> Level {
        match *self {
            WeeklyOutlook::NeedHistory => Level::Dim,
            WeeklyOutlook::AtCap { .. } => Level::Red,
            WeeklyOutlook::Steady { .. } | WeeklyOutlook::Safe { .. } => Level::Green,
            WeeklyOutlook::Cap { level, .. } => level,
            WeeklyOutlook::RateOnly { rate } if rate.per_day > 100.0 / 7.0 => Level::Orange,
            WeeklyOutlook::RateOnly { .. } => Level::Green,
        }
    }

    /// The rate behind the outlook, if any.
    pub fn rate(&self) -> Option<WeeklyRate> {
        match *self {
            WeeklyOutlook::NeedHistory | WeeklyOutlook::AtCap { .. } => None,
            WeeklyOutlook::Steady { rate }
            | WeeklyOutlook::RateOnly { rate }
            | WeeklyOutlook::Cap { rate, .. }
            | WeeklyOutlook::Safe { rate, .. } => Some(rate),
        }
    }
}

/// The projection for one weekly meter at `pct`, resetting at `resets_at`,
/// from its `samples` (see [`weekly_rate`]) at `now`. At or over 100% it is
/// [`WeeklyOutlook::AtCap`] whatever the history.
pub fn weekly_outlook(
    samples: &[(DateTime<Utc>, Option<f64>)],
    pct: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> WeeklyOutlook {
    if pct.is_some_and(|p| p >= 100.0) {
        return WeeklyOutlook::AtCap { reset: resets_at };
    }
    let Some(rate) = weekly_rate(samples, resets_at, now) else {
        return WeeklyOutlook::NeedHistory;
    };
    if rate.per_day <= WEEKLY_STEADY_PER_DAY {
        return WeeklyOutlook::Steady { rate };
    }
    let (Some(pct), Some(reset)) = (pct, resets_at.filter(|r| *r > now)) else {
        return WeeklyOutlook::RateOnly { rate };
    };
    let minutes = (100.0 - pct) / rate.per_day * 1440.0;
    let to_reset = (reset - now).as_seconds_f64() / 60.0;
    if minutes <= to_reset {
        let level = if minutes < WEEKLY_RED_HOURS * 60.0 {
            Level::Red
        } else {
            Level::Orange
        };
        let at = now + Duration::milliseconds((minutes * 60_000.0).round() as i64);
        WeeklyOutlook::Cap {
            rate,
            at,
            minutes,
            level,
        }
    } else {
        WeeklyOutlook::Safe {
            rate,
            at_reset: pct + rate.per_day * to_reset / 1440.0,
            reset,
        }
    }
}

/// What the rate was measured over, for a tooltip: "over last 24h",
/// "over last 3d (app was closed)", "over last 8h" (a recent burst), "over
/// last 3h (app was closed)", "over last 5h (since the weekly reset)", plus
/// ", spread over 6h" when the floor applied.
pub fn rate_basis_text(rate: &WeeklyRate) -> String {
    let why = match (rate.basis, rate.since_reset) {
        (_, true) => " (since the weekly reset)",
        (RateBasis::Anchored { gap: true } | RateBasis::Partial, false) => " (app was closed)",
        (RateBasis::Anchored { gap: false } | RateBasis::Recent, false) => "",
    };
    let floor = if rate.floored {
        format!(", spread over {WEEKLY_FLOOR_HOURS}h")
    } else {
        String::new()
    };
    format!("over last {}{why}{floor}", fmt_span(rate.span))
}

/// A rate's span: whole days from 48 h ("3d"), else whole hours ("24h"),
/// at least "1h".
pub fn fmt_span(span: Duration) -> String {
    let hours = span.num_minutes() as f64 / 60.0;
    if hours >= 48.0 {
        format!("{:.0}d", (hours / 24.0).round())
    } else {
        format!("{:.0}h", hours.round().max(1.0))
    }
}

/// A %/day rate: "+21%/day", "+0.4%/day" under 1, "-3%/day".
pub fn fmt_rate_per_day(rate: f64) -> String {
    let sign = if rate >= 0.0 { "+" } else { "" };
    if rate.abs() < 1.0 {
        format!("{sign}{rate:.1}%/day")
    } else {
        format!("{sign}{rate:.0}%/day")
    }
}

/// A span of minutes in days and hours, "2d 5h", or as [`super::fmt_min`]
/// ("5h 12m") under a day.
pub fn fmt_days(minutes: f64) -> String {
    let m = minutes.max(0.0) as i64;
    if m < 1440 {
        return super::fmt_min(m as f64);
    }
    format!("{}d {}h", m / 1440, (m % 1440) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    fn h(hours: f64) -> DateTime<Utc> {
        now() + Duration::milliseconds((hours * 3_600_000.0) as i64)
    }

    /// Samples every hour from `from` hours ago to now, rising `per_day`.
    fn rising(from: i64, start: f64, per_day: f64) -> Vec<(DateTime<Utc>, Option<f64>)> {
        (0..=from)
            .map(|i| {
                let ago = (from - i) as f64;
                (h(-ago), Some(start + per_day * (from as f64 - ago) / 24.0))
            })
            .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn anchored_rate_over_the_trailing_day() {
        // 48 h of samples at 12%/day: anchored right at 24 h ago.
        let r = weekly_rate(&rising(48, 10.0, 12.0), None, now()).unwrap();
        assert!(close(r.per_day, 12.0));
        assert_eq!(r.basis, RateBasis::Anchored { gap: false });
        assert_eq!(r.span, Duration::hours(24));
        assert!(!r.since_reset);
        // Missing values are skipped; future samples are ignored.
        let s = vec![
            (h(-30.0), Some(0.0)),
            (h(-24.0), Some(40.0)),
            (h(-5.0), None),
            (h(0.0), Some(46.0)),
            (h(1.0), Some(99.0)),
        ];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert!(close(r.per_day, 6.0));
    }

    #[test]
    fn a_normal_anchor_just_before_the_start_spans_exactly_to_it() {
        // The anchor 20 minutes before the start (the last poll before the
        // window), then hourly: the span is 24 h 20 m and the rate exact.
        let at = |ago: f64| (h(-ago), Some(10.0 + 12.0 * (25.0 - ago) / 24.0));
        let mut s = vec![at(24.0 + 20.0 / 60.0)];
        s.extend((0..=23).rev().map(|ago| at(f64::from(ago))));
        let r = weekly_rate(&s, None, now()).unwrap();
        assert_eq!(r.span, Duration::minutes(24 * 60 + 20));
        assert_eq!(r.basis, RateBasis::Anchored { gap: false });
        assert!(close(r.per_day, 12.0));
        assert_eq!(rate_basis_text(&r), "over last 24h");
    }

    #[test]
    fn an_anchor_days_back_is_divided_over_the_gap() {
        // Anchor 20% three days before the start (Tue), app back an hour
        // ago, 50% now (Fri): +30 over 4 days is 7.5%/day, not 30%/day.
        let s = vec![
            (h(-96.0), Some(20.0)),
            (h(-1.0), Some(49.5)),
            (h(0.0), Some(50.0)),
        ];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert_eq!(r.basis, RateBasis::Anchored { gap: true });
        assert_eq!(r.span, Duration::hours(96));
        assert!(close(r.per_day, 7.5));
        assert_eq!(rate_basis_text(&r), "over last 4d (app was closed)");
        // The controller's example: anchor 72 h ago: +30 over 3 days.
        let s = vec![(h(-72.0), Some(20.0)), (h(0.0), Some(50.0))];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert!(close(r.per_day, 10.0));
        // An anchor 2 h before the start is a gap too.
        let s = vec![(h(-26.0), Some(48.0)), (h(0.0), Some(56.0))];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert_eq!(r.basis, RateBasis::Anchored { gap: true });
        assert!(close(r.per_day, 8.0 * 24.0 / 26.0));
        assert_eq!(rate_basis_text(&r), "over last 26h (app was closed)");
    }

    #[test]
    fn a_burst_after_a_gap_outranks_the_long_average() {
        // Anchor 20% three days back; since 8 h ago 30% → 50%. Anchored:
        // +30 over 3 days = 10%/day; the window's own 8 h: 60%/day.
        let s = vec![
            (h(-72.0), Some(20.0)),
            (h(-8.0), Some(30.0)),
            (h(-4.0), Some(40.0)),
            (h(0.0), Some(50.0)),
        ];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert_eq!(r.basis, RateBasis::Recent);
        assert_eq!(r.span, Duration::hours(8));
        assert!(close(r.per_day, 60.0));
        assert_eq!(rate_basis_text(&r), "over last 8h");
        // A burst under the 6 h floor doesn't count: the anchored rate.
        let s = vec![
            (h(-72.0), Some(20.0)),
            (h(-5.0), Some(30.0)),
            (h(0.0), Some(50.0)),
        ];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert_eq!(r.basis, RateBasis::Anchored { gap: true });
        assert!(close(r.per_day, 10.0));
        // A slower window doesn't lower the anchored rate.
        let s = vec![
            (h(-30.0), Some(10.0)),
            (h(-20.0), Some(30.0)),
            (h(0.0), Some(31.0)),
        ];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert!(matches!(r.basis, RateBasis::Anchored { .. }));
        assert!(close(r.per_day, 21.0 * 24.0 / 30.0));
    }

    #[test]
    fn unanchored_rate_over_the_measured_span_with_a_floor() {
        // History starts 10 h ago: 10 h measured, no floor.
        let r = weekly_rate(&rising(10, 20.0, 12.0), None, now()).unwrap();
        assert!(close(r.per_day, 12.0));
        assert_eq!(r.basis, RateBasis::Partial);
        assert!(!r.floored);
        assert_eq!(r.span, Duration::hours(10));
        // 3 h measured: +3 points spread over the 6 h floor is 12%/day,
        // not 24%/day.
        let s = vec![(h(-3.0), Some(20.0)), (h(0.0), Some(23.0))];
        let r = weekly_rate(&s, None, now()).unwrap();
        assert!(close(r.per_day, 12.0));
        assert_eq!(r.basis, RateBasis::Partial);
        assert!(r.floored);
        assert_eq!(r.span, Duration::hours(3));
        // Under 2 h: no rate.
        let s = vec![(h(-1.9), Some(10.0)), (h(0.0), Some(12.0))];
        assert_eq!(weekly_rate(&s, None, now()), None);
        assert_eq!(weekly_rate(&[(h(0.0), Some(5.0))], None, now()), None);
        assert_eq!(weekly_rate(&[], None, now()), None);
    }

    #[test]
    fn an_anchor_with_nothing_after_the_start_gives_no_rate() {
        let s = vec![(h(-30.0), Some(10.0)), (h(-25.0), Some(12.0))];
        assert_eq!(weekly_rate(&s, None, now()), None);
    }

    #[test]
    fn rate_is_clipped_to_the_weekly_window() {
        // The week reset 6 h ago: 90% before, 2% → 8% since. The samples
        // before the reset are dropped, so the 6 h since are measured.
        let reset_at = h(-6.0) + Duration::days(7);
        let s = vec![
            (h(-20.0), Some(88.0)),
            (h(-7.0), Some(90.0)),
            (h(-6.0), Some(2.0)),
            (h(0.0), Some(8.0)),
        ];
        let r = weekly_rate(&s, Some(reset_at), now()).unwrap();
        assert!(close(r.per_day, 24.0));
        assert!(r.since_reset);
        // An unknown reset: the drop still restarts the slope.
        let r = weekly_rate(&s, None, now()).unwrap();
        assert!(close(r.per_day, 24.0));
        assert!(!r.since_reset);
        // A reset 3 h ago with a sample right at it: anchored, but the
        // 3 h rise is spread over the 6 h floor.
        let reset_at = h(-3.0) + Duration::days(7);
        let s = rising(24, 10.0, 12.0);
        let r = weekly_rate(&s, Some(reset_at), now()).unwrap();
        assert_eq!(r.basis, RateBasis::Anchored { gap: false });
        assert!(r.floored && r.since_reset);
        assert!(close(r.per_day, 6.0));
        // A reset 1 h ago leaves too little of this week.
        let reset_at = h(-1.0) + Duration::days(7);
        assert_eq!(
            weekly_rate(&rising(24, 10.0, 12.0), Some(reset_at), now()),
            None
        );
    }

    #[test]
    fn cap_before_reset() {
        // 60% rising 20%/day, reset in 3 days: 100% in 2 days.
        let s = rising(48, 20.0, 20.0);
        let reset = h(72.0);
        match weekly_outlook(&s, Some(60.0), Some(reset), now()) {
            WeeklyOutlook::Cap {
                rate,
                at,
                minutes,
                level,
            } => {
                assert!(close(rate.per_day, 20.0));
                assert!(close(minutes, 2.0 * 1440.0));
                assert_eq!(at, h(48.0));
                assert_eq!(level, Level::Orange);
            }
            other => panic!("expected a cap, got {other:?}"),
        }
        // Under a day away: red.
        let o = weekly_outlook(&s, Some(90.0), Some(reset), now());
        assert_eq!(o.level(), Level::Red);
    }

    #[test]
    fn safe_when_the_reset_comes_first() {
        let s = rising(48, 30.0, 6.0);
        let reset = h(48.0);
        let o = weekly_outlook(&s, Some(66.0), Some(reset), now());
        match o {
            WeeklyOutlook::Safe {
                rate,
                at_reset,
                reset: r,
            } => {
                assert!(close(rate.per_day, 6.0));
                assert!(close(at_reset, 78.0));
                assert_eq!(r, reset);
            }
            other => panic!("expected safe, got {other:?}"),
        }
        assert_eq!(o.level(), Level::Green);
        // Exactly at the reset counts as the cap.
        let s = rising(48, 30.0, 17.0);
        let o = weekly_outlook(&s, Some(66.0), Some(h(48.0)), now());
        assert!(matches!(o, WeeklyOutlook::Cap { .. }));
    }

    #[test]
    fn unknown_reset_or_pct_is_the_rate_alone() {
        let s = rising(48, 20.0, 20.0);
        let only = |o: WeeklyOutlook| matches!(o, WeeklyOutlook::RateOnly { .. });
        assert!(only(weekly_outlook(&s, Some(60.0), None, now())));
        assert!(only(weekly_outlook(&s, None, Some(h(30.0)), now())));
        // A reset already past is as good as unknown.
        assert!(only(weekly_outlook(&s, Some(60.0), Some(h(-1.0)), now())));
        let o = weekly_outlook(&s, Some(60.0), None, now());
        assert_eq!(o.level(), Level::Orange);
        assert!(close(o.rate().unwrap().per_day, 20.0));
        let slow = weekly_outlook(&rising(48, 20.0, 10.0), Some(60.0), None, now());
        assert_eq!(slow.level(), Level::Green);
    }

    #[test]
    fn flat_or_falling_is_steady() {
        for per_day in [0.0, 0.5, -4.0] {
            let s = rising(48, 40.0, per_day);
            let o = weekly_outlook(&s, Some(40.0), Some(h(30.0)), now());
            assert!(
                matches!(o, WeeklyOutlook::Steady { .. }),
                "{per_day}: {o:?}"
            );
            assert_eq!(o.level(), Level::Green);
        }
        let s = [(h(0.0), Some(1.0))];
        assert_eq!(
            weekly_outlook(&s, Some(1.0), Some(h(30.0)), now()),
            WeeklyOutlook::NeedHistory
        );
        assert_eq!(WeeklyOutlook::NeedHistory.level(), Level::Dim);
        assert_eq!(WeeklyOutlook::NeedHistory.rate(), None);
    }

    #[test]
    fn at_cap_is_red_never_steady() {
        let s = rising(48, 80.0, 0.0);
        let o = weekly_outlook(&s, Some(100.0), Some(h(30.0)), now());
        assert_eq!(
            o,
            WeeklyOutlook::AtCap {
                reset: Some(h(30.0))
            }
        );
        assert_eq!(o.level(), Level::Red);
        assert_eq!(o.rate(), None);
        // Even with no history at all.
        assert_eq!(
            weekly_outlook(&[], Some(103.0), None, now()),
            WeeklyOutlook::AtCap { reset: None }
        );
    }

    #[test]
    fn spans() {
        assert_eq!(fmt_span(Duration::minutes(24 * 60 + 20)), "24h");
        assert_eq!(fmt_span(Duration::hours(47)), "47h");
        assert_eq!(fmt_span(Duration::hours(48)), "2d");
        assert_eq!(fmt_span(Duration::hours(84)), "4d");
        assert_eq!(fmt_span(Duration::minutes(10)), "1h");
    }

    #[test]
    fn basis_texts() {
        let r = |span_h: i64, basis, since_reset| WeeklyRate {
            per_day: 1.0,
            span: Duration::hours(span_h),
            basis,
            floored: span_h < WEEKLY_FLOOR_HOURS,
            since_reset,
        };
        assert_eq!(
            rate_basis_text(&r(24, RateBasis::Anchored { gap: false }, false)),
            "over last 24h"
        );
        assert_eq!(
            rate_basis_text(&r(72, RateBasis::Anchored { gap: true }, false)),
            "over last 3d (app was closed)"
        );
        assert_eq!(
            rate_basis_text(&r(8, RateBasis::Recent, false)),
            "over last 8h"
        );
        assert_eq!(
            rate_basis_text(&r(10, RateBasis::Partial, false)),
            "over last 10h (app was closed)"
        );
        assert_eq!(
            rate_basis_text(&r(3, RateBasis::Partial, false)),
            "over last 3h (app was closed), spread over 6h"
        );
        assert_eq!(
            rate_basis_text(&r(5, RateBasis::Partial, true)),
            "over last 5h (since the weekly reset), spread over 6h"
        );
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_rate_per_day(21.4), "+21%/day");
        assert_eq!(fmt_rate_per_day(0.42), "+0.4%/day");
        assert_eq!(fmt_rate_per_day(-3.2), "-3%/day");
        assert_eq!(fmt_days(2.0 * 1440.0 + 5.0 * 60.0 + 59.0), "2d 5h");
        assert_eq!(fmt_days(1440.0), "1d 0h");
        assert_eq!(fmt_days(312.0), "5h 12m");
    }
}
