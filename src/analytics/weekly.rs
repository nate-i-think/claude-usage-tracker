//! The weekly projection: where the 7-day-all meter lands at its reset.
//!
//! The 5-hour burn rate (15 minutes of samples) says nothing about the
//! week, so the weekly rate is taken over a long trailing window: the last
//! [`WEEKLY_RATE_HOURS`] of 7-day-all samples, clipped to the current
//! weekly window (`resets_at - 7d`), so a weekly reset never yields a
//! negative or garbage slope. Projected at the reset, it either crosses
//! 100% first ("cap in 2d 5h") or lands under it ("≈78% at reset").

use chrono::{DateTime, Duration, Utc};

use super::Level;

/// The weekly rate is measured over the most recent this-many hours.
pub const WEEKLY_RATE_HOURS: i64 = 24;
/// Fewer hours of samples than this: no rate ("need more history").
pub const WEEKLY_MIN_SPAN_HOURS: i64 = 2;
/// At or below this %/day the week counts as steady.
pub const WEEKLY_STEADY_PER_DAY: f64 = 0.5;
/// A cap closer than this many hours is red, else orange.
const WEEKLY_RED_HOURS: f64 = 24.0;
/// A drop of more than this many points between samples is a reset (the
/// rule of [`super::recent_window`]).
const RESET_DROP: f64 = 5.0;

/// Weekly rate in %/day over the last [`WEEKLY_RATE_HOURS`] of `samples`
/// (7-day-all, oldest first) up to `now`, using only samples since the
/// current weekly window started (`resets_at - 7d`, when known) and after
/// the last drop of more than 5 points (a reset the clipping missed).
/// `None` when the kept samples span less than [`WEEKLY_MIN_SPAN_HOURS`].
pub fn weekly_rate(
    samples: &[(DateTime<Utc>, Option<f64>)],
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let mut start = now - Duration::hours(WEEKLY_RATE_HOURS);
    if let Some(reset) = resets_at {
        start = start.max(reset - Duration::days(7));
    }
    let kept: Vec<(DateTime<Utc>, f64)> = samples
        .iter()
        .filter(|(t, _)| *t >= start && *t <= now)
        .filter_map(|(t, v)| v.filter(|v| !v.is_nan()).map(|v| (*t, v)))
        .collect();
    let after_reset = kept
        .windows(2)
        .rposition(|pair| pair[1].1 + RESET_DROP < pair[0].1)
        .map_or(0, |i| i + 1);
    let [(t0, p0), .., (t1, p1)] = &kept[after_reset..] else {
        return None;
    };
    let span = *t1 - *t0;
    if span < Duration::hours(WEEKLY_MIN_SPAN_HOURS) {
        return None;
    }
    let days = span.as_seconds_f64() / 86_400.0;
    Some((p1 - p0) / days)
}

/// What the weekly projection says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WeeklyOutlook {
    /// Under [`WEEKLY_MIN_SPAN_HOURS`] of usable samples.
    NeedHistory,
    /// The rate is at most [`WEEKLY_STEADY_PER_DAY`], or the meter is
    /// already at 100%.
    Steady { rate: f64 },
    /// The reset (or the current %) is unknown: the rate alone.
    RateOnly { rate: f64 },
    /// 100% comes before the reset, at `at`, `minutes` from now.
    Cap {
        rate: f64,
        at: DateTime<Utc>,
        minutes: f64,
        level: Level,
    },
    /// The reset comes first, with the meter at `at_reset` %.
    Safe {
        rate: f64,
        at_reset: f64,
        reset: DateTime<Utc>,
    },
}

impl WeeklyOutlook {
    /// Color of the outlook: green when steady or safe, red for a cap
    /// within [`WEEKLY_RED_HOURS`], orange for a later one, dim without a
    /// rate. The rate alone is colored by the %/day (green at a pace that
    /// would fill a week in more than a week, i.e. under 100/7 %/day).
    pub fn level(&self) -> Level {
        match *self {
            WeeklyOutlook::NeedHistory => Level::Dim,
            WeeklyOutlook::Steady { .. } | WeeklyOutlook::Safe { .. } => Level::Green,
            WeeklyOutlook::Cap { level, .. } => level,
            WeeklyOutlook::RateOnly { rate } if rate > 100.0 / 7.0 => Level::Orange,
            WeeklyOutlook::RateOnly { .. } => Level::Green,
        }
    }
}

/// The weekly projection for the 7-day-all meter at `pct`, resetting at
/// `resets_at`, from `samples` (see [`weekly_rate`]) at `now`.
pub fn weekly_outlook(
    samples: &[(DateTime<Utc>, Option<f64>)],
    pct: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> WeeklyOutlook {
    let Some(rate) = weekly_rate(samples, resets_at, now) else {
        return WeeklyOutlook::NeedHistory;
    };
    if rate <= WEEKLY_STEADY_PER_DAY || pct.is_some_and(|p| p >= 100.0) {
        return WeeklyOutlook::Steady { rate };
    }
    let (Some(pct), Some(reset)) = (pct, resets_at.filter(|r| *r > now)) else {
        return WeeklyOutlook::RateOnly { rate };
    };
    let minutes = (100.0 - pct) / rate * 1440.0;
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
            at_reset: pct + rate * to_reset / 1440.0,
            reset,
        }
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
    fn rate_over_the_trailing_day() {
        // 48 h of samples at 12%/day: only the last 24 h count.
        let s = rising(48, 10.0, 12.0);
        assert!(close(weekly_rate(&s, None, now()).unwrap(), 12.0));
        // A steeper first day doesn't leak in.
        let mut s = rising(24, 40.0, 6.0);
        s.insert(0, (h(-30.0), Some(0.0)));
        assert!(close(weekly_rate(&s, None, now()).unwrap(), 6.0));
        // Missing values are skipped, future samples ignored.
        let s = vec![
            (h(-10.0), Some(20.0)),
            (h(-5.0), None),
            (h(0.0), Some(25.0)),
            (h(1.0), Some(99.0)),
        ];
        assert!(close(weekly_rate(&s, None, now()).unwrap(), 12.0));
    }

    #[test]
    fn rate_is_clipped_to_the_weekly_window() {
        // The week reset 6 h ago: 90% before, 2% → 8% since.
        let reset_at = h(-6.0) + Duration::days(7);
        let mut s = vec![(h(-20.0), Some(88.0)), (h(-7.0), Some(90.0))];
        s.extend([(h(-6.0), Some(2.0)), (h(0.0), Some(8.0))]);
        assert!(close(weekly_rate(&s, Some(reset_at), now()).unwrap(), 24.0));
        // An unknown reset: the drop still restarts the slope.
        assert!(close(weekly_rate(&s, None, now()).unwrap(), 24.0));
    }

    #[test]
    fn rate_needs_two_hours_of_span() {
        let s = vec![(h(-1.9), Some(10.0)), (h(0.0), Some(12.0))];
        assert_eq!(weekly_rate(&s, None, now()), None);
        let s = vec![(h(-2.0), Some(10.0)), (h(0.0), Some(12.0))];
        assert!(close(weekly_rate(&s, None, now()).unwrap(), 24.0));
        assert_eq!(weekly_rate(&[(h(0.0), Some(5.0))], None, now()), None);
        assert_eq!(weekly_rate(&[], None, now()), None);
        // A reset 1 h ago leaves too little of this week.
        let reset_at = h(-1.0) + Duration::days(7);
        let s = rising(24, 10.0, 12.0);
        assert_eq!(weekly_rate(&s, Some(reset_at), now()), None);
    }

    #[test]
    fn cap_before_reset() {
        // 60% rising 20%/day, reset in 3 days: 100% in 2 days.
        let s = rising(24, 40.0, 20.0);
        let reset = h(72.0);
        match weekly_outlook(&s, Some(60.0), Some(reset), now()) {
            WeeklyOutlook::Cap {
                rate,
                at,
                minutes,
                level,
            } => {
                assert!(close(rate, 20.0));
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
        let s = rising(24, 40.0, 6.0);
        let reset = h(48.0);
        let o = weekly_outlook(&s, Some(66.0), Some(reset), now());
        match o {
            WeeklyOutlook::Safe {
                rate,
                at_reset,
                reset: r,
            } => {
                assert!(close(rate, 6.0));
                assert!(close(at_reset, 78.0));
                assert_eq!(r, reset);
            }
            other => panic!("expected safe, got {other:?}"),
        }
        assert_eq!(o.level(), Level::Green);
        // Exactly at the reset counts as the cap.
        let s = rising(24, 40.0, 17.0);
        let o = weekly_outlook(&s, Some(66.0), Some(h(48.0)), now());
        assert!(matches!(o, WeeklyOutlook::Cap { .. }));
    }

    #[test]
    fn unknown_reset_or_pct_is_the_rate_alone() {
        let s = rising(24, 40.0, 20.0);
        assert_eq!(
            weekly_outlook(&s, Some(60.0), None, now()),
            WeeklyOutlook::RateOnly { rate: 20.0 }
        );
        assert_eq!(
            weekly_outlook(&s, None, Some(h(30.0)), now()),
            WeeklyOutlook::RateOnly { rate: 20.0 }
        );
        // A reset already past is as good as unknown.
        assert_eq!(
            weekly_outlook(&s, Some(60.0), Some(h(-1.0)), now()),
            WeeklyOutlook::RateOnly { rate: 20.0 }
        );
        assert_eq!(
            WeeklyOutlook::RateOnly { rate: 20.0 }.level(),
            Level::Orange
        );
        assert_eq!(WeeklyOutlook::RateOnly { rate: 10.0 }.level(), Level::Green);
    }

    #[test]
    fn flat_or_falling_is_steady() {
        for per_day in [0.0, 0.5, -4.0] {
            let s = rising(24, 40.0, per_day);
            let o = weekly_outlook(&s, Some(40.0), Some(h(30.0)), now());
            assert!(
                matches!(o, WeeklyOutlook::Steady { .. }),
                "{per_day}: {o:?}"
            );
            assert_eq!(o.level(), Level::Green);
        }
        // Already capped.
        let s = rising(24, 80.0, 20.0);
        assert!(matches!(
            weekly_outlook(&s, Some(100.0), Some(h(30.0)), now()),
            WeeklyOutlook::Steady { .. }
        ));
        let s = [(h(0.0), Some(1.0))];
        assert_eq!(
            weekly_outlook(&s, Some(1.0), Some(h(30.0)), now()),
            WeeklyOutlook::NeedHistory
        );
        assert_eq!(WeeklyOutlook::NeedHistory.level(), Level::Dim);
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
