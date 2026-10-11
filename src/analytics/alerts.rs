//! Edge-triggered threshold alerts for the 5-hour window, and their copy.
//!
//! [`evaluate`] is a pure decision over an [`AlertState`] the caller keeps
//! (and persists, so a restart does not re-fire). Delivery is elsewhere.

use std::fmt::Display;

use chrono::{DateTime, Duration, DurationRound, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::{humanize_until, Projection};

/// A projected cap closer than this is not announced: by then it is noise,
/// not a warning.
pub const CAP_WARNING_MIN_LEAD_MINUTES: i64 = 10;

/// Window keys this close together name the same window. The API's
/// `resets_at` jitters from poll to poll, sub-second usually but by up to
/// about 5 minutes at times, while windows are 5 hours apart.
pub const WINDOW_KEY_TOLERANCE_MINUTES: i64 = 5;

/// An alert to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// The 5-hour window reached 75%; `pct` is the usage then, rounded.
    Crossed75 { pct: u32 },
    /// The 5-hour window reached 90%; `pct` is the usage then, rounded.
    Crossed90 { pct: u32 },
    /// At the current burn rate the 5-hour window hits 100% at `at`, before
    /// it resets.
    ProjectedCap { at: DateTime<Utc> },
}

/// Which alert edges were already consumed in one 5-hour window, keyed by
/// that window's `resets_at` rounded to the minute. Serializable so it can
/// be persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertState {
    window: Option<DateTime<Utc>>,
    crossed_75: bool,
    crossed_90: bool,
    projected_cap: bool,
}

/// Which alert kinds the user wants delivered. All on by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertToggles {
    pub crossed_75: bool,
    pub crossed_90: bool,
    pub projected_cap: bool,
}

impl Default for AlertToggles {
    fn default() -> Self {
        Self {
            crossed_75: true,
            crossed_90: true,
            projected_cap: true,
        }
    }
}

/// The alerts to deliver for this poll, recording them in `state` so each
/// fires at most once per window.
///
/// - `window_key` is the 5-hour `resets_at`. It is rounded to the nearest
///   minute, and a key within [`WINDOW_KEY_TOLERANCE_MINUTES`] of the stored
///   one is the same window (the API's `resets_at` jitters between polls);
///   anything further starts a fresh window. No key: nothing fires and
///   `state` is left alone.
/// - [`AlertKind::Crossed75`] / [`AlertKind::Crossed90`] fire when `pct`
///   first reaches 75 / 90 in the window. When both edges are new at once
///   (a jump straight past 90), only `Crossed90` fires, or `Crossed75` if
///   90 is toggled off.
/// - [`AlertKind::ProjectedCap`] fires once when `projection.cap_at` is set
///   and at least [`CAP_WARNING_MIN_LEAD_MINUTES`] away. A cap that is too
///   close is skipped without being recorded, so it can still fire if a
///   slower pace later pushes it further out.
///
/// An edge reached while its toggle is off still counts as consumed, so
/// turning the toggle on later in the window does not deliver a stale
/// alert.
pub fn evaluate(
    state: &mut AlertState,
    window_key: Option<DateTime<Utc>>,
    pct: Option<f64>,
    projection: &Projection,
    now: DateTime<Utc>,
    enabled: AlertToggles,
) -> Vec<AlertKind> {
    let Some(key) = window_key.map(normalize_key) else {
        return Vec::new();
    };
    if !state.window.is_some_and(|w| same_window(w, key)) {
        *state = AlertState {
            window: Some(key),
            ..AlertState::default()
        };
    }
    let mut fired = Vec::new();

    let reached = |threshold: f64| pct.is_some_and(|p| p >= threshold);
    let new_75 = reached(75.0) && !state.crossed_75;
    let new_90 = reached(90.0) && !state.crossed_90;
    state.crossed_75 |= new_75;
    state.crossed_90 |= new_90;
    let shown = pct.map_or(0, |p| p.round() as u32); // `as` saturates
    if new_90 && enabled.crossed_90 {
        fired.push(AlertKind::Crossed90 { pct: shown });
    } else if new_75 && enabled.crossed_75 {
        fired.push(AlertKind::Crossed75 { pct: shown });
    }

    if let Some(at) = projection.cap_at {
        let lead = at - now >= Duration::minutes(CAP_WARNING_MIN_LEAD_MINUTES);
        if lead && !state.projected_cap {
            state.projected_cap = true;
            if enabled.projected_cap {
                fired.push(AlertKind::ProjectedCap { at });
            }
        }
    }
    fired
}

/// `resets_at` rounded to the nearest minute.
pub fn normalize_key(t: DateTime<Utc>) -> DateTime<Utc> {
    t.duration_round(Duration::minutes(1)).unwrap_or(t)
}

/// Whether two `resets_at` values name the same 5-hour window: within
/// [`WINDOW_KEY_TOLERANCE_MINUTES`] of each other once rounded to the
/// minute, absorbing the API's poll-to-poll jitter.
pub fn same_window(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    (normalize_key(a) - normalize_key(b)).abs() <= Duration::minutes(WINDOW_KEY_TOLERANCE_MINUTES)
}

/// Toast copy for one alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertCopy {
    pub title: String,
    pub body: String,
}

/// [`alert_copy_in`] in local time.
pub fn alert_copy(kind: AlertKind, resets_at: DateTime<Utc>, now: DateTime<Utc>) -> AlertCopy {
    alert_copy_in(kind, resets_at, now, &Local)
}

/// Title and body for `kind`, for the 5-hour window resetting at
/// `resets_at`, with clock times in `tz`:
///
/// - "5-hour usage at 78%" / "Resets in 1h 59m" for either threshold, with
///   the usage when it fired (so a jump to 95% with the 90% alert off
///   still reads "at 95%");
/// - "On pace to hit the 5-hour cap at 4:10 PM" / "Resets at 4:42 PM —
///   consider pausing agents".
pub fn alert_copy_in<Tz: TimeZone>(
    kind: AlertKind,
    resets_at: DateTime<Utc>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> AlertCopy
where
    Tz::Offset: Display,
{
    match kind {
        AlertKind::Crossed75 { pct } | AlertKind::Crossed90 { pct } => AlertCopy {
            title: format!("5-hour usage at {pct}%"),
            body: format!("Resets in {}", humanize_until(Some(resets_at), now)),
        },
        AlertKind::ProjectedCap { at } => AlertCopy {
            title: format!("On pace to hit the 5-hour cap at {}", fmt_clock_in(at, tz)),
            body: format!(
                "Resets at {} — consider pausing agents",
                fmt_clock_in(resets_at, tz)
            ),
        },
    }
}

/// A clock time in `tz` as "4:10 PM".
pub fn fmt_clock_in<Tz: TimeZone>(t: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    t.with_timezone(tz).format("%-I:%M %p").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, h, m, 0).unwrap()
    }

    /// The first window resets at 15:00 UTC; the second at 20:00.
    const KEY_HOURS: [u32; 2] = [15, 20];

    fn key(i: usize) -> Option<DateTime<Utc>> {
        Some(t(KEY_HOURS[i], 0))
    }

    fn no_cap(now: DateTime<Utc>) -> Projection {
        Projection {
            end_t: now,
            end_pct: 0.0,
            cap_at: None,
        }
    }

    fn cap(at: DateTime<Utc>) -> Projection {
        Projection {
            end_t: at,
            end_pct: 100.0,
            cap_at: Some(at),
        }
    }

    fn eval(state: &mut AlertState, k: Option<DateTime<Utc>>, pct: f64) -> Vec<AlertKind> {
        let now = t(12, 0);
        evaluate(
            state,
            k,
            Some(pct),
            &no_cap(now),
            now,
            AlertToggles::default(),
        )
    }

    #[test]
    fn crossed_75_then_90_each_once() {
        let mut s = AlertState::default();
        assert!(eval(&mut s, key(0), 74.9).is_empty());
        assert_eq!(
            eval(&mut s, key(0), 75.0),
            [AlertKind::Crossed75 { pct: 75 }]
        );
        assert!(eval(&mut s, key(0), 80.0).is_empty(), "no double fire");
        assert_eq!(
            eval(&mut s, key(0), 90.0),
            [AlertKind::Crossed90 { pct: 90 }]
        );
        assert!(eval(&mut s, key(0), 99.0).is_empty());
        assert!(eval(&mut s, key(0), 75.0).is_empty());
    }

    #[test]
    fn jump_past_90_fires_only_90() {
        let mut s = AlertState::default();
        assert_eq!(
            eval(&mut s, key(0), 95.0),
            [AlertKind::Crossed90 { pct: 95 }]
        );
        assert!(eval(&mut s, key(0), 96.0).is_empty());
    }

    #[test]
    fn jump_past_90_with_90_off_falls_back_to_75() {
        let mut s = AlertState::default();
        let now = t(12, 0);
        let only_75 = AlertToggles {
            crossed_90: false,
            ..AlertToggles::default()
        };
        let got = evaluate(&mut s, key(0), Some(95.0), &no_cap(now), now, only_75);
        assert_eq!(got, [AlertKind::Crossed75 { pct: 95 }]);
    }

    #[test]
    fn fired_pct_is_rounded_current_usage() {
        let mut s = AlertState::default();
        assert_eq!(
            eval(&mut s, key(0), 77.6),
            [AlertKind::Crossed75 { pct: 78 }]
        );
        assert_eq!(
            eval(&mut s, key(0), 99.5),
            [AlertKind::Crossed90 { pct: 100 }]
        );
    }

    #[test]
    fn same_window_absorbs_jitter_not_the_next_window() {
        let base = key(0).unwrap();
        for jitter_ms in [107, -400, 3_000, 61_000, -290_000, 300_000] {
            assert!(same_window(base, base + Duration::milliseconds(jitter_ms)));
        }
        assert!(!same_window(base, base + Duration::minutes(6)));
        assert!(!same_window(base, base + Duration::hours(5)));
    }

    #[test]
    fn jittering_key_is_the_same_window() {
        let base = key(0).unwrap();
        let mut s = AlertState::default();
        assert_eq!(
            eval(&mut s, Some(base + Duration::microseconds(107_492)), 80.0),
            [AlertKind::Crossed75 { pct: 80 }]
        );
        // Sub-second and few-second jitter on later polls: no re-fire.
        for jitter_ms in [989_019 / 1000, -400, 3_000, -4_500, 61_000, -290_000] {
            let k = base + Duration::milliseconds(jitter_ms);
            assert!(
                eval(&mut s, Some(k), 81.0).is_empty(),
                "re-fired at {jitter_ms} ms"
            );
        }
        // The stored key is the normalized (whole-minute) one.
        assert_eq!(s.window, Some(base));
        // The next window, 5 hours later: fresh.
        let next = base + Duration::hours(5) + Duration::milliseconds(250);
        assert_eq!(
            eval(&mut s, Some(next), 80.0),
            [AlertKind::Crossed75 { pct: 80 }]
        );
        assert_eq!(s.window, Some(base + Duration::hours(5)));
    }

    #[test]
    fn new_window_resets_state() {
        let mut s = AlertState::default();
        assert_eq!(
            eval(&mut s, key(0), 80.0),
            [AlertKind::Crossed75 { pct: 80 }]
        );
        // Reset: back to low usage in the new window, then crossing again.
        assert!(eval(&mut s, key(1), 2.0).is_empty());
        assert_eq!(
            eval(&mut s, key(1), 76.0),
            [AlertKind::Crossed75 { pct: 76 }]
        );
    }

    #[test]
    fn no_window_key_fires_nothing_and_keeps_state() {
        let mut s = AlertState::default();
        assert_eq!(
            eval(&mut s, key(0), 80.0),
            [AlertKind::Crossed75 { pct: 80 }]
        );
        let before = s.clone();
        let now = t(12, 0);
        let got = evaluate(
            &mut s,
            None,
            Some(95.0),
            &cap(t(13, 0)),
            now,
            AlertToggles::default(),
        );
        assert!(got.is_empty());
        assert_eq!(s, before);
    }

    #[test]
    fn unknown_pct_fires_no_threshold() {
        let mut s = AlertState::default();
        let now = t(12, 0);
        let got = evaluate(
            &mut s,
            key(0),
            None,
            &no_cap(now),
            now,
            AlertToggles::default(),
        );
        assert!(got.is_empty());
        assert_eq!(
            eval(&mut s, key(0), 75.0),
            [AlertKind::Crossed75 { pct: 75 }]
        );
    }

    #[test]
    fn projected_cap_needs_ten_minutes_lead() {
        let mut s = AlertState::default();
        let now = t(12, 0);
        let on = AlertToggles::default();
        // 9m59s away: too late to be useful, and not recorded.
        let near = now + Duration::seconds(599);
        assert!(evaluate(&mut s, key(0), Some(50.0), &cap(near), now, on).is_empty());
        // Exactly 10 minutes: fires.
        let at = now + Duration::minutes(10);
        assert_eq!(
            evaluate(&mut s, key(0), Some(50.0), &cap(at), now, on),
            [AlertKind::ProjectedCap { at }]
        );
        // Once per window, even if the cap moves.
        let later = now + Duration::minutes(40);
        assert!(evaluate(&mut s, key(0), Some(55.0), &cap(later), now, on).is_empty());
        // Fires again in the next window.
        assert_eq!(
            evaluate(&mut s, key(1), Some(5.0), &cap(later), now, on),
            [AlertKind::ProjectedCap { at: later }]
        );
    }

    #[test]
    fn projected_cap_past_or_absent_does_not_fire() {
        let mut s = AlertState::default();
        let now = t(12, 0);
        let on = AlertToggles::default();
        let past = now - Duration::minutes(30);
        assert!(evaluate(&mut s, key(0), Some(50.0), &cap(past), now, on).is_empty());
        assert!(evaluate(&mut s, key(0), Some(50.0), &no_cap(now), now, on).is_empty());
    }

    #[test]
    fn threshold_and_cap_fire_together_thresholds_first() {
        let mut s = AlertState::default();
        let now = t(12, 0);
        let at = now + Duration::minutes(30);
        let got = evaluate(
            &mut s,
            key(0),
            Some(80.0),
            &cap(at),
            now,
            AlertToggles::default(),
        );
        assert_eq!(
            got,
            [
                AlertKind::Crossed75 { pct: 80 },
                AlertKind::ProjectedCap { at }
            ]
        );
    }

    #[test]
    fn toggles_off_fire_nothing_and_consume_the_edge() {
        let mut s = AlertState::default();
        let now = t(12, 0);
        let at = now + Duration::minutes(30);
        let off = AlertToggles {
            crossed_75: false,
            crossed_90: false,
            projected_cap: false,
        };
        assert!(evaluate(&mut s, key(0), Some(80.0), &cap(at), now, off).is_empty());
        // Turned back on later in the same window: no stale alerts.
        let on = AlertToggles::default();
        assert!(evaluate(&mut s, key(0), Some(85.0), &cap(at), now, on).is_empty());
        assert_eq!(
            evaluate(&mut s, key(0), Some(91.0), &cap(at), now, on),
            [AlertKind::Crossed90 { pct: 91 }]
        );
    }

    #[test]
    fn state_round_trips_through_json_and_tolerates_missing_fields() {
        let mut s = AlertState::default();
        eval(&mut s, key(0), 80.0);
        let json = serde_json::to_string(&s).unwrap();
        let back: AlertState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // Restart safety: the restored state still suppresses the edge.
        let mut restored = back;
        assert!(eval(&mut restored, key(0), 82.0).is_empty());
        assert_eq!(
            serde_json::from_str::<AlertState>("{}").unwrap(),
            AlertState::default()
        );
        assert_eq!(
            serde_json::from_str::<AlertToggles>(r#"{"crossed_90": false}"#).unwrap(),
            AlertToggles {
                crossed_90: false,
                ..AlertToggles::default()
            }
        );
    }

    // --- copy ---------------------------------------------------------------

    /// UTC-4, so 20:10 UTC reads 4:10 PM.
    fn tz() -> FixedOffset {
        FixedOffset::west_opt(4 * 3600).unwrap()
    }

    #[test]
    fn clock_format_is_12_hour_without_leading_zero() {
        assert_eq!(fmt_clock_in(t(20, 10), &tz()), "4:10 PM");
        assert_eq!(fmt_clock_in(t(13, 5), &tz()), "9:05 AM");
        assert_eq!(fmt_clock_in(t(4, 0), &tz()), "12:00 AM");
    }

    #[test]
    fn threshold_copy() {
        let resets = t(20, 42);
        let now = resets - Duration::minutes(119);
        assert_eq!(
            alert_copy_in(AlertKind::Crossed75 { pct: 75 }, resets, now, &tz()),
            AlertCopy {
                title: "5-hour usage at 75%".into(),
                body: "Resets in 1h 59m".into(),
            }
        );
        assert_eq!(
            alert_copy_in(AlertKind::Crossed90 { pct: 92 }, resets, now, &tz()).title,
            "5-hour usage at 92%"
        );
        // The 90% alert off and a jump to 95%: the 75% alert says 95%.
        assert_eq!(
            alert_copy_in(AlertKind::Crossed75 { pct: 95 }, resets, now, &tz()).title,
            "5-hour usage at 95%"
        );
    }

    #[test]
    fn projected_cap_copy() {
        let kind = AlertKind::ProjectedCap { at: t(20, 10) };
        assert_eq!(
            alert_copy_in(kind, t(20, 42), t(19, 0), &tz()),
            AlertCopy {
                title: "On pace to hit the 5-hour cap at 4:10 PM".into(),
                body: "Resets at 4:42 PM — consider pausing agents".into(),
            }
        );
    }
}
