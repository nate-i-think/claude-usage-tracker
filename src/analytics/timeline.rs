//! Hero-timeline math: window samples (5-hour and weekly), pace against an
//! even burn, the dashed projection with its cap crossing, the continuous
//! Fable layer, and thinning a week of samples.
//!
//! Pure and clock-injected like the rest of [`crate::analytics`].

use chrono::{DateTime, Duration, Utc};

use super::FABLE_WEEKLY_TO_5H_RATIO;
use super::{fable_5h_ceilings, lands_after, minutes_between, minutes_to_cap};

/// One history row inside the current 5-hour window: the 5-hour and the
/// Fable weekly utilization at `t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSample {
    pub t: DateTime<Utc>,
    pub five_hour: Option<f64>,
    pub fable: Option<f64>,
}

/// One history row inside the current weekly window: the 7-day-all and the
/// Fable 7-day utilization at `t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeekSample {
    pub t: DateTime<Utc>,
    pub seven_day: Option<f64>,
    pub fable: Option<f64>,
}

/// Bucket length for thinning a week of samples.
pub const WEEK_THIN_BUCKET_MINUTES: i64 = 5;
/// Samples this recent are all kept.
pub const WEEK_KEEP_ALL_MINUTES: i64 = 60;

/// The current weekly window: `resets_at - 7d` to `resets_at`, or the
/// trailing 7 days ending `now` when the reset is unknown.
pub fn week_window(
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let end = resets_at.unwrap_or(now);
    (end - Duration::days(7), end)
}

/// A week of samples (oldest first) thinned to a bounded size: one sample
/// per [`WEEK_THIN_BUCKET_MINUTES`] bucket, at the bucket's last time with
/// each meter's last value in the bucket (a later row missing a meter
/// doesn't wipe it), and every sample of the last [`WEEK_KEEP_ALL_MINUTES`]
/// before `now`. A week at 30 s polls
/// (about 20 000 rows) becomes about 2 100. The weekly meters only rise
/// within a week, so the last sample of a bucket is its highest.
pub fn thin_week(samples: &[WeekSample], now: DateTime<Utc>) -> Vec<WeekSample> {
    let keep_all = now - Duration::minutes(WEEK_KEEP_ALL_MINUTES);
    let bucket_s = WEEK_THIN_BUCKET_MINUTES * 60;
    let bucket = |t: DateTime<Utc>| t.timestamp().div_euclid(bucket_s);
    let mut out: Vec<WeekSample> = Vec::with_capacity(samples.len().min(2_200));
    for s in samples {
        if s.t < keep_all {
            if let Some(last) = out.last_mut() {
                if last.t < keep_all && bucket(last.t) == bucket(s.t) {
                    *last = WeekSample {
                        t: s.t,
                        seven_day: s.seven_day.or(last.seven_day),
                        fable: s.fable.or(last.fable),
                    };
                    continue;
                }
            }
        }
        out.push(*s);
    }
    out
}

/// How far `utilization` is ahead of an even burn across the window:
/// `utilization - 100 × elapsed fraction`, the fraction clamped to 0..=1
/// (so before the start it is the utilization, after the end
/// `utilization - 100`). Positive = ahead of pace (using faster than even),
/// negative = behind. `None` when the window length is zero or negative.
pub fn pace(
    utilization: f64,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let length = minutes_between(window_start, window_end);
    if length <= 0.0 {
        return None;
    }
    let elapsed = (minutes_between(window_start, now) / length).clamp(0.0, 1.0);
    Some(utilization - 100.0 * elapsed)
}

/// [`pace`] for a 7-day window (7-day all, Fable) from its reset time.
///
/// Assumes the window is a fixed 7 days ending at `resets_at`, so it starts
/// at `resets_at - 7 days`; the API reports only the reset time. `None` when
/// the utilization or the reset time is unknown.
pub fn seven_day_pace(
    utilization: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let end = resets_at?;
    pace(utilization?, end - Duration::days(7), end, now)
}

/// The dashed projection on the hero timeline: a straight line from
/// `(now, pct_now)` to `(end_t, end_pct)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    pub end_t: DateTime<Utc>,
    pub end_pct: f64,
    /// When the line reaches 100% at or before the window end; `None` when
    /// it lands after the reset or the pace is steady.
    pub cap_at: Option<DateTime<Utc>>,
}

/// Project `pct_now` forward at `rate_per_min` (the 15-minute burn rate from
/// [`super::compute_rate`]) until `window_end` or 100%, whichever comes
/// first.
///
/// A steady pace (rate unknown, <= 0.01 %/min, or already at 100%; the same
/// rule as [`super::burn_line`]'s "steady") is a flat line to the window end
/// with no cap. Otherwise, like the burn line's ETA, a cap landing exactly
/// at the reset counts as hitting the cap first: the line ends at 100% and
/// `cap_at` is set. A `window_end` already past gives a zero-length line at
/// `now`.
pub fn projection(
    now: DateTime<Utc>,
    pct_now: f64,
    rate_per_min: Option<f64>,
    window_end: DateTime<Utc>,
) -> Projection {
    let end_t = window_end.max(now);
    let flat = Projection {
        end_t,
        end_pct: pct_now,
        cap_at: None,
    };
    let Some(rate) = rate_per_min else {
        return flat;
    };
    let Some(minutes) = minutes_to_cap(pct_now, rate) else {
        return flat;
    };
    if lands_after(minutes, now, window_end) {
        Projection {
            end_t,
            end_pct: pct_now + rate * minutes_between(now, end_t),
            cap_at: None,
        }
    } else {
        let cap = now + minutes_duration(minutes);
        Projection {
            end_t: cap,
            end_pct: 100.0,
            cap_at: Some(cap),
        }
    }
}

/// Per sample, the height of the orange Fable layer under the 5-hour curve:
/// the rise in Fable% since the window's first Fable sample, scaled by
/// [`FABLE_WEEKLY_TO_5H_RATIO`] (the rule of [`fable_5h_ceilings`], per
/// sample instead of per bucket), clipped to the sample's 5-hour value.
/// `None` where either value is missing, and everywhere when the window has
/// no Fable sample.
pub fn fable_layer(samples: &[WindowSample]) -> Vec<Option<f64>> {
    let fable: Vec<_> = samples.iter().map(|s| (s.t, s.fable)).collect();
    fable_5h_ceilings(&fable, FABLE_WEEKLY_TO_5H_RATIO)
        .into_iter()
        .zip(samples)
        .map(|(ceiling, s)| Some(ceiling?.min(s.five_hour?)))
        .collect()
}

fn minutes_duration(minutes: f64) -> Duration {
    Duration::microseconds((minutes * 60e6).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn week(m: i64, v: f64) -> WeekSample {
        WeekSample {
            t: t0() + Duration::minutes(m),
            seven_day: Some(v),
            fable: None,
        }
    }

    #[test]
    fn week_window_from_the_reset_or_trailing() {
        let reset = t0() + Duration::days(2);
        assert_eq!(
            week_window(Some(reset), t0()),
            (reset - Duration::days(7), reset)
        );
        assert_eq!(week_window(None, t0()), (t0() - Duration::days(7), t0()));
    }

    #[test]
    fn thin_week_keeps_the_last_per_bucket_and_the_last_hour() {
        // t0 is on a 5-minute boundary. Minute samples for 2 hours.
        let samples: Vec<_> = (0..120).map(|m| week(m, m as f64)).collect();
        let now = t0() + Duration::minutes(120);
        let thin = thin_week(&samples, now);
        // The first hour: one per 5-minute bucket, the last of each.
        let old: Vec<f64> = thin
            .iter()
            .filter(|s| s.t < now - Duration::minutes(60))
            .map(|s| s.seven_day.unwrap())
            .collect();
        assert_eq!(
            old,
            (0..12).map(|b| f64::from(b * 5 + 4)).collect::<Vec<_>>()
        );
        // The last hour: every sample.
        assert_eq!(thin.len(), 12 + 60);
        assert_eq!(thin.last(), samples.last());
        assert!(thin.windows(2).all(|w| w[0].t < w[1].t));
        assert!(thin_week(&[], now).is_empty());
    }

    #[test]
    fn thin_week_keeps_each_meters_last_value_in_a_bucket() {
        let row = |m: i64, seven: Option<f64>, fable: Option<f64>| WeekSample {
            t: t0() + Duration::minutes(m),
            seven_day: seven,
            fable,
        };
        let samples = [
            row(0, Some(10.0), Some(20.0)),
            row(1, Some(11.0), None),
            row(2, None, None),
        ];
        let thin = thin_week(&samples, t0() + Duration::hours(3));
        assert_eq!(thin, [row(2, Some(11.0), Some(20.0))]);
    }

    #[test]
    fn thin_week_bounds_a_week_of_30s_polls() {
        let now = t0() + Duration::days(7);
        let samples: Vec<_> = (0..7 * 24 * 120)
            .map(|i| WeekSample {
                t: t0() + Duration::seconds(30 * i),
                seven_day: Some(1.0),
                fable: Some(2.0),
            })
            .collect();
        let thin = thin_week(&samples, now);
        assert!(thin.len() <= 7 * 24 * 12 + 120 + 1, "{}", thin.len());
        assert!(thin.len() >= 7 * 24 * 12);
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 10, 0, 0).unwrap()
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        t0() + Duration::minutes(minutes)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // --- pace -------------------------------------------------------------

    #[test]
    fn pace_at_start_is_the_utilization() {
        assert_eq!(pace(12.0, at(0), at(300), at(0)), Some(12.0));
    }

    #[test]
    fn pace_mid_window_ahead_and_behind() {
        // 2h of 5h elapsed = 40% expected.
        assert!(close(pace(46.0, at(0), at(300), at(120)).unwrap(), 6.0));
        assert!(close(pace(36.0, at(0), at(300), at(120)).unwrap(), -4.0));
    }

    #[test]
    fn pace_at_end_is_utilization_minus_100() {
        assert_eq!(pace(80.0, at(0), at(300), at(300)), Some(-20.0));
    }

    #[test]
    fn pace_outside_window_clamps_elapsed() {
        assert_eq!(pace(5.0, at(0), at(300), at(-60)), Some(5.0));
        assert_eq!(pace(90.0, at(0), at(300), at(400)), Some(-10.0));
    }

    #[test]
    fn pace_zero_or_negative_length_is_none() {
        assert_eq!(pace(50.0, at(0), at(0), at(0)), None);
        assert_eq!(pace(50.0, at(10), at(0), at(5)), None);
    }

    #[test]
    fn seven_day_pace_starts_seven_days_before_reset() {
        let reset = at(0) + Duration::days(7);
        // Half a week elapsed = 50% expected.
        let now = at(0) + Duration::hours(84);
        assert!(close(
            seven_day_pace(Some(60.0), Some(reset), now).unwrap(),
            10.0
        ));
        assert_eq!(seven_day_pace(Some(60.0), None, now), None);
        assert_eq!(seven_day_pace(None, Some(reset), now), None);
    }

    // --- projection ---------------------------------------------------------

    #[test]
    fn projection_rate_none_is_flat_to_window_end() {
        let p = projection(at(60), 30.0, None, at(300));
        assert_eq!(
            p,
            Projection {
                end_t: at(300),
                end_pct: 30.0,
                cap_at: None
            }
        );
    }

    #[test]
    fn projection_steady_rate_is_flat() {
        for rate in [0.01, 0.0, -0.3] {
            let p = projection(at(60), 30.0, Some(rate), at(300));
            assert_eq!(p.end_t, at(300));
            assert_eq!(p.end_pct, 30.0);
            assert_eq!(p.cap_at, None);
        }
    }

    #[test]
    fn projection_already_at_cap_is_flat_without_cap() {
        let p = projection(at(60), 100.0, Some(1.0), at(300));
        assert_eq!((p.end_t, p.end_pct, p.cap_at), (at(300), 100.0, None));
    }

    #[test]
    fn projection_cap_before_reset() {
        // 40% at 0.5%/min: 100% in 120 min, at 2:00 in; reset at 4:00 in.
        let p = projection(at(120), 40.0, Some(0.5), at(300));
        assert_eq!(p.end_t, at(240));
        assert_eq!(p.end_pct, 100.0);
        assert_eq!(p.cap_at, Some(at(240)));
    }

    #[test]
    fn projection_cap_after_reset_ends_at_reset() {
        // 40% at 0.2%/min: 100% in 300 min; reset in 180 min -> 76%.
        let p = projection(at(120), 40.0, Some(0.2), at(300));
        assert_eq!(p.end_t, at(300));
        assert!(close(p.end_pct, 76.0), "{}", p.end_pct);
        assert_eq!(p.cap_at, None);
    }

    #[test]
    fn projection_cap_exactly_at_reset_counts_as_cap() {
        // 40% at 0.5%/min: 100% in exactly the 120 min left.
        let p = projection(at(180), 40.0, Some(0.5), at(300));
        assert_eq!(p.end_t, at(300));
        assert_eq!(p.end_pct, 100.0);
        assert_eq!(p.cap_at, Some(at(300)));
    }

    #[test]
    fn projection_matches_burn_line_eta_semantics() {
        // Same inputs through burn_line: its ETA is "not after reset" (red)
        // exactly when the projection reports a cap.
        let now = at(180);
        let history = vec![(now - Duration::minutes(10), Some(35.0)), (now, Some(40.0))];
        let line = super::super::burn_line(&history, Some(40.0), Some(at(300)), now).unwrap();
        let p = projection(now, 40.0, Some(line.rate), at(300));
        assert!(p.cap_at.is_some());
        assert_ne!(line.eta.level(), super::super::Level::Green);
    }

    #[test]
    fn projection_after_window_end_is_zero_length() {
        let p = projection(at(320), 40.0, Some(0.1), at(300));
        assert_eq!(p.end_t, at(320));
        assert_eq!(p.end_pct, 40.0);
        assert_eq!(p.cap_at, None);
    }

    // --- fable_layer --------------------------------------------------------

    fn s(m: i64, five: Option<f64>, fable: Option<f64>) -> WindowSample {
        WindowSample {
            t: at(m),
            five_hour: five,
            fable,
        }
    }

    #[test]
    fn fable_layer_is_scaled_rise_since_first_fable_sample() {
        let samples = [
            s(0, Some(10.0), Some(20.0)),
            s(5, Some(20.0), Some(21.0)),
            s(10, Some(40.0), Some(22.0)),
        ];
        let layer = fable_layer(&samples);
        assert_eq!(layer[0], Some(0.0));
        assert!(close(layer[1].unwrap(), FABLE_WEEKLY_TO_5H_RATIO));
        assert!(close(layer[2].unwrap(), 2.0 * FABLE_WEEKLY_TO_5H_RATIO));
    }

    #[test]
    fn fable_layer_baseline_skips_leading_gaps() {
        let samples = [
            s(0, Some(5.0), None),
            s(5, Some(10.0), Some(3.0)),
            s(10, Some(30.0), Some(4.0)),
        ];
        let layer = fable_layer(&samples);
        assert_eq!(layer[0], None);
        assert_eq!(layer[1], Some(0.0));
        assert!(close(layer[2].unwrap(), FABLE_WEEKLY_TO_5H_RATIO));
    }

    #[test]
    fn fable_layer_none_gaps_stay_none() {
        let samples = [
            s(0, Some(5.0), Some(1.0)),
            s(5, Some(9.0), None),
            s(10, None, Some(2.0)),
            s(15, Some(30.0), Some(2.0)),
        ];
        let layer = fable_layer(&samples);
        assert_eq!(layer[1], None, "no Fable value");
        assert_eq!(layer[2], None, "no 5-hour value to sit under");
        assert!(layer[3].is_some());
    }

    #[test]
    fn fable_layer_clips_to_curve_and_never_negative() {
        let samples = [
            s(0, Some(10.0), Some(10.0)),
            s(5, Some(12.0), Some(15.0)), // 5 × 5.9 = 29.5, clipped to 12
            s(10, Some(12.0), Some(9.0)), // drop below baseline -> 0
        ];
        assert_eq!(fable_layer(&samples), [Some(0.0), Some(12.0), Some(0.0)]);
    }

    #[test]
    fn fable_layer_without_fable_or_samples() {
        assert!(fable_layer(&[]).is_empty());
        let samples = [s(0, Some(10.0), None), s(5, Some(20.0), None)];
        assert_eq!(fable_layer(&samples), [None, None]);
    }
}
