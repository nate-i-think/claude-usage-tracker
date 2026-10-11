//! Dashboard analytics: burn rate, reset detection, cap ETA, projected use,
//! color levels, countdown formatting, Fable chart ceilings, and the row
//! rules of the dashboard. Submodules: the hero timeline (pace, projection,
//! Fable layer), project-name prettifying, and threshold [`alerts`].
//!
//! Ported from the math in the Python ui.py (removed; see git history),
//! separated from its Rich rendering. Pure: no I/O, no egui. Wherever
//! Python read the clock, `now` is a parameter, so every result is
//! deterministic. Outputs are structured (levels, numbers, plain strings
//! without markup); the GUI and the CLI report render them.

use std::fmt::Display;

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};

use crate::model::{ExtraUsage, Snapshot, UsageWindow};

pub mod alerts;
mod names;
mod timeline;

pub use names::{
    encode_project_path, last_path_component, prettify_project, prettify_project_in,
    project_display_name, PROJECT_NAME_CHARS,
};

pub use timeline::{fable_layer, pace, projection, seven_day_pace, Projection, WindowSample};

/// Burn rate is measured over the most recent this-many minutes of history.
pub const BURN_WINDOW_MINUTES: i64 = 15;

/// In-memory history length; holds 15min of samples even at 5s polls.
pub const HISTORY_LEN: usize = 240;

/// Bar-chart rows; minor ticks (25/50/75/100%) land on 4/8/12/16.
pub const CHART_HEIGHT: usize = 16;

/// Width of a 5-hour chart bucket.
pub const CHART_BUCKET_MINUTES: i64 = 30;

/// Claude coral/orange (#d97757) for the Fable overlay, as RGB bytes.
pub const FABLE_ORANGE: [u8; 3] = [0xd9, 0x77, 0x57];

// Fable is a WEEKLY metric, so its % is a share of the weekly Fable cap. To plot
// Fable's contribution on the *5-hour* chart we take the rise in Fable% since the
// window start and scale it by this ratio = (weekly Fable cap) / (5-hour cap),
// turning a weekly-% into a 5-hour-budget-%. It is NOT in the API.
//
// Calibrated directly from an all-Fable session: when every bit of usage is Fable,
// both meters track the same usage, so R = (5-hour burn) / (Fable weekly burn).
// Measured 1.06%/min ÷ 0.18%/min ≈ 5.9. One-line knob; re-measure the same way
// (during a pure-Fable burst) if your plan changes.
/// Weekly-Fable-% to 5-hour-budget-% scale for the 5-hour chart's orange floor.
pub const FABLE_WEEKLY_TO_5H_RATIO: f64 = 5.9;

// On the 7-day chart the bars are 7-day-*all* utilization. Fable% is a % of the
// Fable cap, which is ~50% of the 7-day-all cap, so Fable's share of a 7-day-all
// bar is fable% × this. (Without it, Fable is drawn 2× too tall — it looked like
// a whole bar when it should be a slice.)
/// Fable-cap-% to 7-day-all-% scale for the 7-day chart's orange floor.
pub const FABLE_TO_WEEKLY_ALL_RATIO: f64 = 0.5;

/// Windows pinned to the dashboard even at 0%, in their row order. The
/// remaining windows follow in snapshot order.
pub const ALWAYS_SHOWN_LABELS: &[&str] = &["5-hour", "7-day all", "Fable"];

// --- Color & formatting helpers -------------------------------------------

/// Severity color of a figure. `Dim` is the "no data" style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Dim,
    Green,
    Yellow,
    Orange,
    Red,
}

/// Color for a utilization percentage: red >= 90, orange >= 75, yellow >= 50,
/// else green; dim when unknown.
pub fn color_for(pct: Option<f64>) -> Level {
    match pct {
        None => Level::Dim,
        Some(p) if p >= 90.0 => Level::Red,
        Some(p) if p >= 75.0 => Level::Orange,
        Some(p) if p >= 50.0 => Level::Yellow,
        Some(_) => Level::Green,
    }
}

/// Countdown to `dt`: "—" when unknown, "now" once reached, "{d}d {h}h" from
/// a day out, else "{h}h {mm:02}m". Whole seconds, truncated toward zero.
pub fn humanize_until(dt: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(dt) = dt else {
        return "—".to_owned();
    };
    // `num_seconds` truncates toward zero, like Python's int().
    let secs = (dt - now).num_seconds();
    if secs <= 0 {
        return "now".to_owned();
    }
    let (h, rem) = (secs / 3600, secs % 3600);
    if h >= 24 {
        let (d, h) = (h / 24, h % 24);
        return format!("{d}d {h}h");
    }
    format!("{h}h {:02}m", rem / 60)
}

/// Minutes as "{h}h {mm:02}m", or just "{m}m" under an hour. Truncates to
/// whole minutes; the split is a Python `divmod` (floor division).
pub fn fmt_min(m: f64) -> String {
    // `as i64` truncates toward zero like Python's int(); divmod floors.
    let m = m as i64;
    let (h, mm) = (m.div_euclid(60), m.rem_euclid(60));
    if h != 0 {
        format!("{h}h {mm:02}m")
    } else {
        format!("{mm}m")
    }
}

/// Burn rate as "{sign}{rate:.2}%/min", with an explicit "+" when >= 0.
pub fn fmt_rate(rate: f64) -> String {
    let sign = if rate >= 0.0 { "+" } else { "" };
    format!("{sign}{rate:.2}%/min")
}

/// Python `(b - a).total_seconds() / 60`.
fn minutes_between(a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
    (b - a).as_seconds_f64() / 60.0
}

// --- Burn rate, projection, headroom --------------------------------------

/// Recent samples, trimmed to post-reset only.
///
/// Keeps the samples from the last `window_minutes` that have a value.
/// Utilization is monotonic-non-decreasing within a window, so a drop in
/// samples can only mean we crossed a reset. After a reset the pre-reset
/// samples are no longer meaningful for burn rate, so we anchor to the most
/// recent reset point: a drop of more than 5 points between consecutive
/// samples starts the returned slice over.
pub fn recent_window(
    history: &[(DateTime<Utc>, Option<f64>)],
    now: DateTime<Utc>,
    window_minutes: i64,
) -> Vec<(DateTime<Utc>, f64)> {
    let cutoff = now - Duration::minutes(window_minutes);
    let recent: Vec<(DateTime<Utc>, f64)> = history
        .iter()
        .filter(|(t, _)| *t >= cutoff)
        .filter_map(|(t, p)| p.map(|p| (*t, p)))
        .collect();
    let reset_idx = recent
        .windows(2)
        .rposition(|pair| pair[1].1 + 5.0 < pair[0].1) // >5% drop = reset
        .map_or(0, |i| i + 1);
    recent[reset_idx..].to_vec()
}

/// `(rate %/min, span minutes)` of the samples [`recent_window`] kept, or
/// `None` with fewer than two samples or a zero time span.
fn rate_and_span(recent: &[(DateTime<Utc>, f64)]) -> Option<(f64, f64)> {
    let [(t0, p0), .., (t1, p1)] = recent else {
        return None;
    };
    let minutes = minutes_between(*t0, *t1);
    if minutes <= 0.0 {
        return None;
    }
    Some(((p1 - p0) / minutes, minutes))
}

/// Burn rate (%/min) over the most recent [`BURN_WINDOW_MINUTES`] of history,
/// or `None` with fewer than two usable samples (or a zero time span).
pub fn compute_rate(history: &[(DateTime<Utc>, Option<f64>)], now: DateTime<Utc>) -> Option<f64> {
    rate_and_span(&recent_window(history, now, BURN_WINDOW_MINUTES)).map(|(rate, _)| rate)
}

/// Time to the cap at the current pace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Eta {
    /// Rate <= 0.01 %/min or already at or over 100%: shown as a green
    /// "steady".
    Steady,
    /// Minutes until 100% at the current pace, with its color: green when it
    /// lands after the window resets (won't hit the cap first), else red
    /// under 30, orange under 60, yellow otherwise.
    Minutes { minutes: f64, level: Level },
}

impl Eta {
    /// Color of the ETA; `Steady` is green.
    pub fn level(self) -> Level {
        match self {
            Eta::Steady => Level::Green,
            Eta::Minutes { level, .. } => level,
        }
    }
}

/// Minutes until 100% from `pct` at `rate` %/min, or `None` when the pace
/// counts as steady: rate <= 0.01 %/min, or already at or over 100%. The
/// single source of the "steady" rule for [`burn_line`] and [`projection`].
fn minutes_to_cap(pct: f64, rate: f64) -> Option<f64> {
    if rate <= 0.01 || pct >= 100.0 {
        None
    } else {
        Some((100.0 - pct) / rate)
    }
}

/// Whether a cap `minutes` from `now` lands strictly after `reset`. A cap
/// exactly at the reset counts as hitting the cap first.
fn lands_after(minutes: f64, now: DateTime<Utc>, reset: DateTime<Utc>) -> bool {
    minutes > minutes_between(now, reset)
}

/// The burn line: "15m burn {rate}, → 100% in {eta} (over last {span}m,
/// {samples} samples)". Absent (see [`burn_line`]) it reads "15m burn —,
/// → 100% in — (need 2 recent samples)".
#[derive(Debug, Clone, PartialEq)]
pub struct BurnLine {
    /// Burn rate in %/min over the recent window; see [`fmt_rate`].
    pub rate: f64,
    /// Red > 0.5, orange > 0.2, yellow > 0.05, else green.
    pub rate_level: Level,
    pub eta: Eta,
    /// Minutes spanned by the samples the rate was taken over.
    pub span_minutes: f64,
    /// How many samples the rate was taken over.
    pub samples: usize,
}

/// Burn line for one window (5-hour for ALL, Fable for FABLE) from its own
/// history, current percentage and reset time. `None` when the rate is
/// unknown (fewer than two recent samples) or `pct` is unknown.
pub fn burn_line(
    history: &[(DateTime<Utc>, Option<f64>)],
    pct: Option<f64>,
    resets: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<BurnLine> {
    let recent = recent_window(history, now, BURN_WINDOW_MINUTES);
    let (rate, span_minutes) = rate_and_span(&recent)?;
    let pct = pct?;

    let rate_level = if rate > 0.5 {
        Level::Red
    } else if rate > 0.2 {
        Level::Orange
    } else if rate > 0.05 {
        Level::Yellow
    } else {
        Level::Green
    };

    let eta = match minutes_to_cap(pct, rate) {
        None => Eta::Steady,
        Some(minutes) => {
            let lands_after_reset = resets.is_some_and(|r| lands_after(minutes, now, r));
            let level = if lands_after_reset {
                Level::Green // won't hit cap before reset
            } else if minutes < 30.0 {
                Level::Red
            } else if minutes < 60.0 {
                Level::Orange
            } else {
                Level::Yellow
            };
            Eta::Minutes { minutes, level }
        }
    };

    Some(BurnLine {
        rate,
        rate_level,
        eta,
        span_minutes,
        samples: recent.len(),
    })
}

/// The projected-use line: "Projected use: {projected}  ·  {advice}".
#[derive(Debug, Clone, PartialEq)]
pub struct Headroom {
    /// Total 5h budget consumed by the time the window resets if the current
    /// pace holds, e.g. "118%" (over 100 means the cap comes first); "0%"
    /// when idle; "—" when the reset time is unknown.
    pub projected: String,
    /// Color of the projected figure.
    pub level: Level,
    /// Advisory copy.
    pub advice: String,
    /// Color of the advisory, or `None` for the default text style.
    pub advice_level: Option<Level>,
}

/// Projected use at reset for the 5-hour window given the ALL burn `rate`
/// (%/min). `None` when the 5-hour percentage is unknown.
///
/// Idle (rate unknown or under 0.05): "0%", green, dim advice. Otherwise
/// projected = pct + rate × minutes-to-reset. Green "safe" when the cap ETA
/// lands after the reset; red with "pause" advice when the ETA is under 20
/// minutes; orange "throttle" advice otherwise.
pub fn headroom(snap: &Snapshot, rate: Option<f64>, now: DateTime<Utc>) -> Option<Headroom> {
    let pct = snap.five_hour_pct()?;
    let headroom = 100.0 - pct;

    let rate = match rate {
        Some(r) if r >= 0.05 => r,
        _ => {
            return Some(Headroom {
                projected: "0%".to_owned(),
                level: Level::Green,
                advice: "idle, safe to spin up agents".to_owned(),
                advice_level: Some(Level::Dim),
            })
        }
    };

    let eta_min = headroom / rate;
    let minutes_to_reset = snap
        .five_hour_resets_at()
        .map_or(f64::INFINITY, |r| minutes_between(now, r));

    // Total projected use at reset = already-used + additional draw if the pace
    // holds. Can exceed 100% (intentional — signals how hard you'd blow the cap);
    // "—" if reset unknown.
    let projected = if minutes_to_reset.is_finite() {
        format!("{:.0}%", pct + rate * minutes_to_reset)
    } else {
        "—".to_owned()
    };

    let (level, advice, advice_level) = if eta_min > minutes_to_reset {
        (
            Level::Green,
            "safe to spin up another agent".to_owned(),
            None,
        )
    } else if eta_min < 20.0 {
        (
            Level::Red,
            format!("ETA cap in {} — pause some agents now", fmt_min(eta_min)),
            Some(Level::Red),
        )
    } else {
        (
            Level::Orange,
            format!("throttle if possible — ETA cap in {}", fmt_min(eta_min)),
            Some(Level::Orange),
        )
    };
    Some(Headroom {
        projected,
        level,
        advice,
        advice_level,
    })
}

// --- Orange Fable floor on charts -----------------------------------------

/// Per-bucket orange ceilings (y-axis %) for the 5-hour chart.
///
/// Fable% is a weekly figure, so only the *rise since the window start*
/// reflects Fable used inside this 5-hour window. Scale that rise by `ratio`
/// (weekly Fable cap / 5-hour cap) to express it on the 5-hour budget. Never
/// negative; buckets with no Fable sample stay `None`, and so does every
/// bucket when no bucket has a sample. Clipping to each bar happens
/// downstream.
pub fn fable_5h_ceilings<T>(fable_buckets: &[(T, Option<f64>)], ratio: f64) -> Vec<Option<f64>> {
    let baseline = fable_buckets.iter().find_map(|(_, v)| *v);
    fable_buckets
        .iter()
        .map(|&(_, v)| Some((v? - baseline?).max(0.0) * ratio))
        .collect()
}

/// Per-day orange ceilings (y-axis %) for the 7-day chart: each day's Fable%
/// converted from the Fable cap to the 7-day-all cap via `ratio`. No-data
/// days contribute 0 (no orange), so every entry is `Some`. Clipping to each
/// bar happens downstream.
pub fn fable_7d_ceilings<T>(fable_daily: &[(T, Option<f64>)], ratio: f64) -> Vec<Option<f64>> {
    fable_daily
        .iter()
        .map(|&(_, v)| Some(ratio * v.unwrap_or(0.0)))
        .collect()
}

/// How many bottom chart rows fall within the orange Fable floor.
///
/// `ceiling` is a y-axis percentage (0–100); returns the count of the lowest
/// rows to paint orange, capped at the chart height. `None`, 0 or negative
/// gives 0. Any positive ceiling paints at least one row, so nonzero Fable
/// usage stays visible even when the floor would round down to zero rows.
/// Rounds half to even, like Python's `round`.
pub fn orange_rows(ceiling: Option<f64>, height: usize) -> usize {
    let Some(ceiling) = ceiling.filter(|c| *c > 0.0) else {
        return 0;
    };
    // Guarantee at least one visible orange block for any nonzero Fable usage,
    // even when the floor would otherwise round down to zero rows.
    let rows = (ceiling * height as f64 / 100.0).round_ties_even() as usize;
    rows.min(height).max(1)
}

// --- Dashboard rows ---------------------------------------------------------

/// Whether a window gets a summary row: always with `show_all`; otherwise
/// only pinned labels ([`ALWAYS_SHOWN_LABELS`]) or windows above 0%.
pub fn window_visible(w: &UsageWindow, show_all: bool) -> bool {
    show_all
        || ALWAYS_SHOWN_LABELS.contains(&w.label.as_str())
        || w.utilization.unwrap_or(0.0) > 0.0
}

/// Dashboard row order: the pinned windows first, then the rest. The burn
/// lines go between the two groups.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowOrder<'a> {
    /// 5-hour, 7-day all, Fable, in that order, each when present.
    pub primary: Vec<&'a UsageWindow>,
    /// Every other window, in snapshot order.
    pub rest: Vec<&'a UsageWindow>,
}

/// Split and order the snapshot's windows for the dashboard.
pub fn ordered_windows(snap: &Snapshot) -> WindowOrder<'_> {
    WindowOrder {
        primary: ALWAYS_SHOWN_LABELS
            .iter()
            .filter_map(|label| snap.window(label))
            .collect(),
        rest: snap
            .windows
            .iter()
            .filter(|w| !ALWAYS_SHOWN_LABELS.contains(&w.label.as_str()))
            .collect(),
    }
}

/// The extra-usage block to show, if any: shown whenever there is a
/// configured budget or accrued spend. claude.ai flips `enabled` to false
/// (e.g. disabled_reason "out_of_credits") while the period's spend is still
/// meaningful, so this does not gate on `enabled`.
pub fn visible_extra(snap: &Snapshot) -> Option<&ExtraUsage> {
    snap.extra
        .as_ref()
        .filter(|e| e.monthly_limit > 0.0 || e.used_credits > 0.0)
}

/// Reason for the "(disabled: {reason})" note: the disabled reason with
/// underscores as spaces, "off" when there is none. `None` while enabled.
pub fn extra_disabled_reason(e: &ExtraUsage) -> Option<String> {
    if e.enabled {
        return None;
    }
    let reason = e
        .disabled_reason
        .as_deref()
        .filter(|r| !r.is_empty())
        .unwrap_or("off");
    Some(reason.replace('_', " "))
}

// --- Bar-chart labels ---------------------------------------------------------

/// 5-hour chart label for the 30-minute bucket starting at `t`, in local
/// time: the hour ("%H") whose HH:00 falls inside the bucket
/// (`t <= HH:00 < t + 30m`), else empty.
///
/// Deviates from Python, which labels only buckets that *start* on the hour:
/// buckets start at `resets_at - 5h`, rarely on the hour,
/// which left the axis blank.
pub fn bucket_hour_label(t: DateTime<Utc>) -> String {
    bucket_hour_label_in(t, &Local)
}

/// [`bucket_hour_label`] in an explicit time zone.
pub fn bucket_hour_label_in<Tz: TimeZone>(t: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let local = t.with_timezone(tz);
    let into_hour = Duration::seconds(i64::from(local.minute() * 60 + local.second()))
        + Duration::nanoseconds(i64::from(local.nanosecond()));
    // Time from the bucket start to the first local HH:00 at or after it.
    let to_hour = if into_hour.is_zero() {
        Duration::zero()
    } else {
        Duration::hours(1) - into_hour
    };
    if to_hour < Duration::minutes(CHART_BUCKET_MINUTES) {
        (t + to_hour).with_timezone(tz).format("%H").to_string()
    } else {
        String::new()
    }
}

/// 7-day chart label for a (local calendar) day: the weekday ("%a").
pub fn day_label(d: NaiveDate) -> String {
    d.format("%a").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const NOW: DateTime<Utc> = DateTime::from_timestamp_nanos(1_791_000_000_000_000_000);

    fn minutes(m: f64) -> Duration {
        Duration::milliseconds((m * 60_000.0).round() as i64)
    }

    fn window(
        label: &str,
        utilization: Option<f64>,
        resets_at: Option<DateTime<Utc>>,
    ) -> UsageWindow {
        UsageWindow {
            label: label.into(),
            utilization,
            resets_at,
        }
    }

    /// From the Python test_ui.py `_snap` (removed): a 5-hour window at
    /// `pct` resetting in `minutes_to_reset`.
    fn snap(pct: f64, minutes_to_reset: f64) -> Snapshot {
        Snapshot {
            fetched_at: NOW,
            windows: vec![window(
                "5-hour",
                Some(pct),
                Some(NOW + minutes(minutes_to_reset)),
            )],
            extra: None,
        }
    }

    /// From the Python test_ui.py `_snap_with_windows` (removed): a live
    /// 5-hour window plus `extra` windows.
    fn snap_with_windows(extra: Vec<UsageWindow>) -> Snapshot {
        let mut windows = vec![window("5-hour", Some(10.0), Some(NOW + minutes(120.0)))];
        windows.extend(extra);
        Snapshot {
            fetched_at: NOW,
            windows,
            extra: None,
        }
    }

    /// From the Python test_ui.py `_history` (removed):
    /// `[(minutes_ago, pct), ...]` -> samples, oldest first.
    fn history(points: &[(f64, f64)]) -> Vec<(DateTime<Utc>, Option<f64>)> {
        points
            .iter()
            .map(|&(m, p)| (NOW - minutes(m), Some(p)))
            .collect()
    }

    fn projected_pct(h: &Headroom) -> i64 {
        h.projected
            .strip_suffix('%')
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("could not parse projected use from {:?}", h.projected))
    }

    // --- constants ----------------------------------------------------------

    #[test]
    fn constants_match_python() {
        assert_eq!(BURN_WINDOW_MINUTES, 15);
        assert_eq!(HISTORY_LEN, 240);
        assert_eq!(CHART_HEIGHT, 16);
        assert_eq!(FABLE_ORANGE, [217, 119, 87]); // #d97757, "217;119;87" truecolor
        assert_eq!(FABLE_WEEKLY_TO_5H_RATIO, 5.9);
        assert_eq!(FABLE_TO_WEEKLY_ALL_RATIO, 0.5);
        assert_eq!(ALWAYS_SHOWN_LABELS, ["5-hour", "7-day all", "Fable"]);
    }

    // --- color_for ----------------------------------------------------------

    #[test]
    fn color_for_boundaries() {
        assert_eq!(color_for(None), Level::Dim);
        assert_eq!(color_for(Some(-5.0)), Level::Green);
        assert_eq!(color_for(Some(0.0)), Level::Green);
        assert_eq!(color_for(Some(49.999)), Level::Green);
        assert_eq!(color_for(Some(50.0)), Level::Yellow);
        assert_eq!(color_for(Some(74.999)), Level::Yellow);
        assert_eq!(color_for(Some(75.0)), Level::Orange);
        assert_eq!(color_for(Some(89.999)), Level::Orange);
        assert_eq!(color_for(Some(90.0)), Level::Red);
        assert_eq!(color_for(Some(100.0)), Level::Red);
        assert_eq!(color_for(Some(250.0)), Level::Red);
    }

    // --- humanize_until -------------------------------------------------------

    #[test]
    fn humanize_until_forms() {
        assert_eq!(humanize_until(None, NOW), "—");
        assert_eq!(humanize_until(Some(NOW), NOW), "now");
        assert_eq!(humanize_until(Some(NOW - Duration::hours(3)), NOW), "now");
        // int() truncates toward zero: -0.5s and +59.9s are 0 and 59 seconds.
        assert_eq!(
            humanize_until(Some(NOW - Duration::milliseconds(500)), NOW),
            "now"
        );
        assert_eq!(
            humanize_until(Some(NOW + Duration::milliseconds(59_900)), NOW),
            "0h 00m"
        );
        assert_eq!(
            humanize_until(Some(NOW + Duration::seconds(60)), NOW),
            "0h 01m"
        );
        assert_eq!(humanize_until(Some(NOW + minutes(90.0)), NOW), "1h 30m");
        assert_eq!(humanize_until(Some(NOW + minutes(119.0)), NOW), "1h 59m");
        assert_eq!(
            humanize_until(Some(NOW + Duration::hours(23) + minutes(59.0)), NOW),
            "23h 59m"
        );
        assert_eq!(
            humanize_until(Some(NOW + Duration::hours(24)), NOW),
            "1d 0h"
        );
        assert_eq!(
            humanize_until(Some(NOW + Duration::hours(25) + minutes(59.0)), NOW),
            "1d 1h"
        );
        assert_eq!(
            humanize_until(Some(NOW + Duration::days(5) + Duration::hours(7)), NOW),
            "5d 7h"
        );
    }

    // --- fmt_min ----------------------------------------------------------------

    #[test]
    fn fmt_min_forms() {
        assert_eq!(fmt_min(0.0), "0m");
        assert_eq!(fmt_min(0.9), "0m");
        assert_eq!(fmt_min(5.0), "5m");
        assert_eq!(fmt_min(59.9), "59m");
        assert_eq!(fmt_min(60.0), "1h 00m");
        assert_eq!(fmt_min(65.0), "1h 05m");
        assert_eq!(fmt_min(117.037), "1h 57m");
        assert_eq!(fmt_min(600.0), "10h 00m");
        // Python divmod floors on negatives: divmod(-100, 60) == (-2, 20).
        assert_eq!(fmt_min(-100.0), "-2h 20m");
    }

    #[test]
    fn fmt_rate_sign_rule() {
        assert_eq!(fmt_rate(0.5), "+0.50%/min");
        assert_eq!(fmt_rate(0.0), "+0.00%/min");
        assert_eq!(fmt_rate(0.025), "+0.03%/min");
        assert_eq!(fmt_rate(-0.5), "-0.50%/min");
        assert_eq!(fmt_rate(1.2345), "+1.23%/min");
    }

    // --- recent_window / compute_rate ---------------------------------------------

    #[test]
    fn recent_window_filters_by_cutoff_and_drops_none() {
        let hist = vec![
            (NOW - minutes(16.0), Some(1.0)), // outside the window
            (NOW - minutes(15.0), Some(2.0)), // exactly at the cutoff: kept
            (NOW - minutes(10.0), None),
            (NOW - minutes(5.0), Some(3.0)),
            (NOW, Some(4.0)),
        ];
        assert_eq!(
            recent_window(&hist, NOW, BURN_WINDOW_MINUTES),
            vec![
                (NOW - minutes(15.0), 2.0),
                (NOW - minutes(5.0), 3.0),
                (NOW, 4.0)
            ]
        );
        assert_eq!(
            recent_window(&hist, NOW, 5),
            vec![(NOW - minutes(5.0), 3.0), (NOW, 4.0)]
        );
        assert!(recent_window(&[], NOW, BURN_WINDOW_MINUTES).is_empty());
    }

    #[test]
    fn recent_window_trims_to_most_recent_reset() {
        // 40 -> 2 is a reset (>5 drop); so is 30 -> 1 later on. Anchor to the last.
        let hist = history(&[
            (14.0, 40.0),
            (12.0, 2.0),
            (10.0, 10.0),
            (8.0, 30.0),
            (6.0, 1.0),
            (2.0, 3.0),
        ]);
        let got = recent_window(&hist, NOW, BURN_WINDOW_MINUTES);
        assert_eq!(
            got,
            vec![(NOW - minutes(6.0), 1.0), (NOW - minutes(2.0), 3.0)]
        );
    }

    #[test]
    fn recent_window_small_dips_are_not_resets() {
        // A drop of exactly 5 is jitter, not a reset; more than 5 is.
        let five = history(&[(10.0, 20.0), (5.0, 15.0), (1.0, 16.0)]);
        assert_eq!(recent_window(&five, NOW, BURN_WINDOW_MINUTES).len(), 3);
        let more = history(&[(10.0, 20.0), (5.0, 14.9), (1.0, 16.0)]);
        assert_eq!(recent_window(&more, NOW, BURN_WINDOW_MINUTES).len(), 2);
    }

    #[test]
    fn compute_rate_basic_and_after_reset() {
        assert_eq!(
            compute_rate(&history(&[(10.0, 20.0), (2.0, 24.0)]), NOW),
            Some(0.5)
        );
        // Pre-reset samples are ignored: rate comes from (6m ago, 1%) -> (2m ago, 3%).
        let hist = history(&[(14.0, 40.0), (8.0, 44.0), (6.0, 1.0), (2.0, 3.0)]);
        assert_eq!(compute_rate(&hist, NOW), Some(0.5));
        // Falling utilization inside the jitter band gives a negative rate.
        assert_eq!(
            compute_rate(&history(&[(10.0, 20.0), (0.0, 19.0)]), NOW),
            Some(-0.1)
        );
    }

    #[test]
    fn compute_rate_needs_two_spaced_samples() {
        assert_eq!(compute_rate(&[], NOW), None);
        assert_eq!(compute_rate(&history(&[(1.0, 20.0)]), NOW), None);
        // Two samples at the same instant: zero span.
        assert_eq!(
            compute_rate(&history(&[(1.0, 20.0), (1.0, 24.0)]), NOW),
            None
        );
        // Only one sample inside the window.
        assert_eq!(
            compute_rate(&history(&[(20.0, 20.0), (1.0, 24.0)]), NOW),
            None
        );
        // None values do not count as samples.
        let hist = vec![(NOW - minutes(5.0), None), (NOW, Some(3.0))];
        assert_eq!(compute_rate(&hist, NOW), None);
    }

    // --- Combined burn line ---------------------------------------------------

    #[test]
    fn all_burn_line_fields() {
        // test_all_burn_line_is_single_combined_line: 0.5%/min, 24%, resets in 3h.
        let hist = history(&[(10.0, 20.0), (2.0, 24.0)]);
        let line = burn_line(&hist, Some(24.0), Some(NOW + Duration::hours(3)), NOW).unwrap();
        assert_eq!(
            line,
            BurnLine {
                rate: 0.5,
                rate_level: Level::Orange, // 0.5 is not > 0.5
                eta: Eta::Minutes {
                    minutes: 152.0, // (100 - 24) / 0.5, before the 180m reset
                    level: Level::Yellow,
                },
                span_minutes: 8.0,
                samples: 2,
            }
        );
        assert_eq!(fmt_rate(line.rate), "+0.50%/min");
    }

    #[test]
    fn all_burn_no_data() {
        // test_all_burn_no_data_single_line: "need 2 recent samples".
        assert_eq!(burn_line(&[], None, None, NOW), None);
        assert_eq!(burn_line(&[], Some(10.0), None, NOW), None);
        assert_eq!(
            burn_line(&history(&[(1.0, 20.0)]), Some(20.0), None, NOW),
            None
        );
        // Rate known but no utilization: still the placeholder line.
        let hist = history(&[(10.0, 20.0), (2.0, 24.0)]);
        assert_eq!(burn_line(&hist, None, None, NOW), None);
    }

    #[test]
    fn fable_burn_uses_its_own_history() {
        // test_fable_burn_uses_its_own_history: independent sourcing.
        let all = burn_line(
            &history(&[(10.0, 20.0), (2.0, 24.0)]),
            Some(24.0),
            None,
            NOW,
        )
        .unwrap();
        let fable = burn_line(&history(&[(10.0, 8.0), (2.0, 8.2)]), Some(8.2), None, NOW).unwrap();
        assert_eq!(fmt_rate(all.rate), "+0.50%/min");
        assert_ne!(fmt_rate(fable.rate), "+0.50%/min");
        assert!((fable.rate - 0.025).abs() < 1e-9, "{}", fable.rate);
        assert_eq!(fable.rate_level, Level::Green);
    }

    #[test]
    fn eta_green_when_it_lands_after_reset() {
        // test_reason_notes_dropped_from_combined_line: the "won't hit cap" path.
        let hist = history(&[(10.0, 20.0), (2.0, 24.0)]);
        let line = burn_line(&hist, Some(24.0), Some(NOW + minutes(5.0)), NOW).unwrap();
        assert_eq!(
            line.eta,
            Eta::Minutes {
                minutes: 152.0,
                level: Level::Green
            }
        );
        // Reset exactly at the ETA is not "after": the risk colors apply.
        let line = burn_line(&hist, Some(24.0), Some(NOW + minutes(152.0)), NOW).unwrap();
        assert_eq!(line.eta.level(), Level::Yellow);
        // No reset known: risk colors apply.
        let line = burn_line(&hist, Some(24.0), None, NOW).unwrap();
        assert_eq!(line.eta.level(), Level::Yellow);
    }

    #[test]
    fn eta_risk_colors() {
        let hist = history(&[(10.0, 20.0), (0.0, 30.0)]); // 1.0%/min
        let eta_at = |pct: f64| burn_line(&hist, Some(pct), None, NOW).unwrap().eta;
        assert_eq!(
            eta_at(71.0),
            Eta::Minutes {
                minutes: 29.0,
                level: Level::Red
            }
        );
        assert_eq!(
            eta_at(70.0),
            Eta::Minutes {
                minutes: 30.0,
                level: Level::Orange
            }
        );
        assert_eq!(
            eta_at(41.0),
            Eta::Minutes {
                minutes: 59.0,
                level: Level::Orange
            }
        );
        assert_eq!(
            eta_at(40.0),
            Eta::Minutes {
                minutes: 60.0,
                level: Level::Yellow
            }
        );
        assert_eq!(
            eta_at(10.0),
            Eta::Minutes {
                minutes: 90.0,
                level: Level::Yellow
            }
        );
    }

    #[test]
    fn eta_steady_when_flat_or_capped() {
        // rate <= 0.01: steady. 0.08 over 8 minutes is exactly 0.01.
        let flat = history(&[(10.0, 20.0), (2.0, 20.08)]);
        let line = burn_line(&flat, Some(20.08), None, NOW).unwrap();
        assert!(line.rate <= 0.01, "{}", line.rate);
        assert_eq!(line.eta, Eta::Steady);
        assert_eq!(line.eta.level(), Level::Green);
        // Falling: steady too.
        let falling = history(&[(10.0, 20.0), (2.0, 19.0)]);
        assert_eq!(
            burn_line(&falling, Some(19.0), None, NOW).unwrap().eta,
            Eta::Steady
        );
        // At or over the cap: steady regardless of pace.
        let fast = history(&[(10.0, 90.0), (2.0, 100.0)]);
        let line = burn_line(&fast, Some(100.0), None, NOW).unwrap();
        assert_eq!(line.eta, Eta::Steady);
        assert_eq!(line.rate_level, Level::Red);
        assert_eq!(
            burn_line(&fast, Some(101.0), None, NOW).unwrap().eta,
            Eta::Steady
        );
    }

    #[test]
    fn rate_level_thresholds() {
        let level_at = |pct_now: f64| {
            burn_line(
                &history(&[(10.0, 0.0), (0.0, pct_now)]),
                Some(pct_now),
                None,
                NOW,
            )
            .unwrap()
            .rate_level
        };
        assert_eq!(level_at(0.5), Level::Green); // 0.05 is not > 0.05
        assert_eq!(level_at(0.6), Level::Yellow);
        assert_eq!(level_at(2.0), Level::Yellow); // 0.2 is not > 0.2
        assert_eq!(level_at(2.1), Level::Orange);
        assert_eq!(level_at(5.0), Level::Orange); // 0.5 is not > 0.5
        assert_eq!(level_at(5.1), Level::Red);
        assert_eq!(level_at(-1.0), Level::Green);
    }

    #[test]
    fn burn_line_span_and_samples_after_reset() {
        let hist = history(&[
            (14.0, 40.0),
            (12.0, 44.0),
            (9.0, 1.0),
            (6.0, 2.0),
            (3.0, 3.0),
        ]);
        let line = burn_line(&hist, Some(3.0), None, NOW).unwrap();
        assert_eq!(line.samples, 3);
        assert_eq!(line.span_minutes, 6.0);
        assert!((line.rate - 1.0 / 3.0).abs() < 1e-12);
    }

    // --- Headroom / projected use -----------------------------------------------

    #[test]
    fn projected_use_includes_already_used_portion() {
        // 21% already used, 0.675%/min pace, 144 min to reset.
        // Future draw alone = 0.675 * 144 ≈ 97%, but TOTAL projected use must add
        // the 21% already spent → ≈ 118%, i.e. on pace to blow past the cap.
        let h = headroom(&snap(21.0, 144.0), Some(0.675), NOW).unwrap();
        let projected = projected_pct(&h);
        assert!(
            projected > 100,
            "expected cap breach (>100%), got {projected}%"
        );
        assert_eq!(projected, 118, "expected 21 + round(0.675*144)=118");
    }

    #[test]
    fn eta_cap_uses_hours_and_minutes() {
        // eta_min = (100-21)/0.675 ≈ 117 min → must read "1h 57m", not "117m".
        let h = headroom(&snap(21.0, 144.0), Some(0.675), NOW).unwrap();
        assert!(
            h.advice.contains("1h 57m"),
            "expected Xh XXm ETA, got: {:?}",
            h.advice
        );
        assert!(
            !h.advice.contains("117m"),
            "raw-minutes ETA still present: {:?}",
            h.advice
        );
        assert_eq!(
            h,
            Headroom {
                projected: "118%".into(),
                level: Level::Orange,
                advice: "throttle if possible — ETA cap in 1h 57m".into(),
                advice_level: Some(Level::Orange),
            }
        );
    }

    #[test]
    fn safe_pace_projects_below_100() {
        // 10% used, gentle 0.1%/min, resets in 100 min → 10 + 10 = 20% total.
        let h = headroom(&snap(10.0, 100.0), Some(0.1), NOW).unwrap();
        assert_eq!(projected_pct(&h), 20);
        assert!(h.advice.contains("safe"), "{:?}", h.advice);
        assert_eq!(
            h,
            Headroom {
                projected: "20%".into(),
                level: Level::Green,
                advice: "safe to spin up another agent".into(),
                advice_level: None,
            }
        );
    }

    #[test]
    fn headroom_idle_branch() {
        let idle = Headroom {
            projected: "0%".into(),
            level: Level::Green,
            advice: "idle, safe to spin up agents".into(),
            advice_level: Some(Level::Dim),
        };
        assert_eq!(headroom(&snap(40.0, 100.0), None, NOW), Some(idle.clone()));
        assert_eq!(
            headroom(&snap(40.0, 100.0), Some(0.049), NOW),
            Some(idle.clone())
        );
        assert_eq!(headroom(&snap(40.0, 100.0), Some(-1.0), NOW), Some(idle));
        // 0.05 itself is not idle.
        assert_ne!(
            headroom(&snap(40.0, 100.0), Some(0.05), NOW)
                .unwrap()
                .projected,
            "0%"
        );
    }

    #[test]
    fn headroom_red_when_cap_within_20_minutes() {
        // 80% used, 1.1%/min, resets in 60 min: ETA 18.18m < 20 and before reset.
        let h = headroom(&snap(80.0, 60.0), Some(1.1), NOW).unwrap();
        assert_eq!(
            h,
            Headroom {
                projected: "146%".into(), // 80 + 1.1 * 60
                level: Level::Red,
                advice: "ETA cap in 18m — pause some agents now".into(),
                advice_level: Some(Level::Red),
            }
        );
        // ETA of exactly 20 minutes is orange, not red.
        let h = headroom(&snap(80.0, 60.0), Some(1.0), NOW).unwrap();
        assert_eq!(h.level, Level::Orange);
        assert_eq!(h.advice, "throttle if possible — ETA cap in 20m");
    }

    #[test]
    fn headroom_safe_boundary_is_strict() {
        // ETA equal to minutes-to-reset is not "after" the reset: orange.
        let h = headroom(&snap(50.0, 100.0), Some(0.5), NOW).unwrap();
        assert_eq!(h.level, Level::Orange);
        assert_eq!(h.projected, "100%");
        // One minute more and the cap lands after the reset: safe.
        let h = headroom(&snap(50.0, 99.0), Some(0.5), NOW).unwrap();
        assert_eq!(h.level, Level::Green);
        assert_eq!(h.projected, "100%"); // 50 + 0.5 * 99 = 99.5 rounds half-even to 100
    }

    #[test]
    fn headroom_unknown_reset_projects_dash() {
        let mut s = snap(30.0, 100.0);
        s.windows[0].resets_at = None;
        let h = headroom(&s, Some(0.5), NOW).unwrap();
        assert_eq!(h.projected, "—");
        // ETA 140m: not under 20, and "after reset" cannot hold without a reset.
        assert_eq!(h.level, Level::Orange);
        assert_eq!(h.advice, "throttle if possible — ETA cap in 2h 20m");
        let h = headroom(&s, Some(5.0), NOW).unwrap();
        assert_eq!(h.projected, "—");
        assert_eq!(h.level, Level::Red);
        assert_eq!(h.advice, "ETA cap in 14m — pause some agents now");
    }

    #[test]
    fn headroom_none_without_five_hour_pct() {
        let mut s = snap(30.0, 100.0);
        s.windows[0].utilization = None;
        assert_eq!(headroom(&s, Some(0.5), NOW), None);
        s.windows.clear();
        assert_eq!(headroom(&s, Some(0.5), NOW), None);
        assert_eq!(headroom(&s, None, NOW), None);
    }

    // --- Orange Fable floor on charts -----------------------------------------

    #[test]
    fn orange_rows_math() {
        assert_eq!(orange_rows(Some(25.0), 16), 4); // bottom 25% of 16 rows
        assert_eq!(orange_rows(Some(0.0), 16), 0);
        assert_eq!(orange_rows(None, 16), 0);
        assert_eq!(orange_rows(Some(-3.0), 16), 0);
    }

    #[test]
    fn orange_rows_at_least_one_when_nonzero() {
        // Any nonzero Fable usage must show at least one orange block, even if the
        // floor would otherwise round down to zero rows (e.g. Fable 6% -> 3% floor).
        assert_eq!(orange_rows(Some(3.0), 16), 1);
        assert_eq!(orange_rows(Some(0.4), 16), 1);
    }

    #[test]
    fn orange_rows_caps_at_height_and_rounds_half_even() {
        assert_eq!(orange_rows(Some(100.0), 16), 16);
        assert_eq!(orange_rows(Some(250.0), 16), 16);
        assert_eq!(orange_rows(Some(50.0), 8), 4);
        // Python round(): 2.5 -> 2, 3.5 -> 4.
        assert_eq!(orange_rows(Some(15.625), 16), 2);
        assert_eq!(orange_rows(Some(21.875), 16), 4);
    }

    #[test]
    fn fable_5h_ceilings_scale_change_since_window_start() {
        // Fable is a WEEKLY metric; the 5h chart should show only the rise since the
        // window start, scaled by R (weekly cap / 5h cap) to reach 5h-budget terms.
        let buckets = [(NOW, Some(4.0)), (NOW, Some(5.0)), (NOW, Some(6.0))]; // window start at 4%
        assert_eq!(
            fable_5h_ceilings(&buckets, 2.0),
            vec![Some(0.0), Some(2.0), Some(4.0)]
        );
    }

    #[test]
    fn fable_5h_ceilings_all_none_when_no_fable_data() {
        let buckets: [(DateTime<Utc>, Option<f64>); 2] = [(NOW, None), (NOW, None)];
        assert_eq!(fable_5h_ceilings(&buckets, 2.0), vec![None, None]);
        let empty: [(DateTime<Utc>, Option<f64>); 0] = [];
        assert!(fable_5h_ceilings(&empty, 2.0).is_empty());
    }

    #[test]
    fn fable_5h_ceilings_never_negative() {
        // A reset/None dip must not produce a negative (upward-inverted) floor.
        let buckets = [(NOW, Some(5.0)), (NOW, Some(4.0))];
        assert_eq!(fable_5h_ceilings(&buckets, 3.0), vec![Some(0.0), Some(0.0)]);
    }

    #[test]
    fn fable_5h_ceilings_baseline_is_first_sample_and_gaps_stay_none() {
        // Leading empty buckets do not set the baseline; later gaps stay None.
        let buckets = [
            (NOW, None),
            (NOW, Some(10.0)),
            (NOW, None),
            (NOW, Some(11.0)),
        ];
        assert_eq!(
            fable_5h_ceilings(&buckets, 5.9),
            vec![None, Some(0.0), None, Some(5.9)]
        );
    }

    #[test]
    fn fable_7d_ceilings_scale_by_weekly_cap_fraction() {
        // fable% is a % of the Fable cap, which is only ~50% of the 7-day-all cap, so
        // its share of the 7-day-all bar is fable% x 0.5 (None days -> 0, no orange).
        let day = NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        let daily = [(day, Some(10.0)), (day, None), (day, Some(4.0))];
        assert_eq!(
            fable_7d_ceilings(&daily, 0.5),
            vec![Some(5.0), Some(0.0), Some(2.0)]
        );
        let empty: [(NaiveDate, Option<f64>); 0] = [];
        assert!(fable_7d_ceilings(&empty, 0.5).is_empty());
    }

    // --- Window rows ----------------------------------------------------------------

    #[test]
    fn fable_row_pinned_even_at_zero_percent() {
        // Fable is an always-shown row: at 0% a non-pinned row would be hidden.
        assert!(window_visible(&window("Fable", Some(0.0), None), false));
        assert!(window_visible(&window("Fable", None, None), false));
        assert!(window_visible(&window("5-hour", None, None), false));
        assert!(window_visible(&window("7-day all", Some(0.0), None), false));
    }

    #[test]
    fn sonnet_row_no_longer_pinned() {
        // Sonnet is unpinned: an inactive (None) or 0% Sonnet window is hidden...
        assert!(!window_visible(&window("Sonnet 7d", None, None), false));
        assert!(!window_visible(
            &window("Sonnet 7d", Some(0.0), None),
            false
        ));
        assert!(!window_visible(
            &window("Sonnet 7d", Some(-1.0), None),
            false
        ));
        // ...unless it has usage, or show_all is on.
        assert!(window_visible(&window("Sonnet 7d", Some(0.1), None), false));
        assert!(window_visible(&window("Sonnet 7d", None, None), true));
        assert!(window_visible(&window("Sonnet 7d", Some(0.0), None), true));
    }

    #[test]
    fn summary_order_primary_then_rest() {
        // test_summary_order_bars_grouped_then_burns: 5-hour, 7-day all, Fable first.
        let s = snap_with_windows(vec![
            window("Sonnet 7d", None, None),
            window("Fable", Some(8.0), None),
            window("Opus 7d", Some(3.0), None),
            window("7-day all", Some(14.0), Some(NOW + Duration::days(5))),
        ]);
        let order = ordered_windows(&s);
        let labels = |ws: &[&UsageWindow]| ws.iter().map(|w| w.label.clone()).collect::<Vec<_>>();
        assert_eq!(labels(&order.primary), ["5-hour", "7-day all", "Fable"]);
        assert_eq!(labels(&order.rest), ["Sonnet 7d", "Opus 7d"]);
        assert_eq!(order.primary[2].utilization, Some(8.0));
    }

    #[test]
    fn ordered_windows_skips_missing_primaries() {
        let s = snap_with_windows(vec![window("Opus 7d", Some(3.0), None)]);
        let order = ordered_windows(&s);
        assert_eq!(order.primary.len(), 1);
        assert_eq!(order.primary[0].label, "5-hour");
        assert_eq!(order.rest.len(), 1);
        assert_eq!(order.rest[0].label, "Opus 7d");

        let empty = Snapshot {
            fetched_at: NOW,
            windows: vec![],
            extra: None,
        };
        let order = ordered_windows(&empty);
        assert!(order.primary.is_empty() && order.rest.is_empty());
    }

    // --- Extra usage ------------------------------------------------------------------

    fn extra(enabled: bool, limit: f64, used: f64, reason: Option<&str>) -> ExtraUsage {
        ExtraUsage {
            enabled,
            monthly_limit: limit,
            used_credits: used,
            utilization: if limit > 0.0 {
                used / limit * 100.0
            } else {
                0.0
            },
            currency: "USD".into(),
            disabled_reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn extra_usage_shows_when_disabled_but_has_spend() {
        // claude.ai reports is_enabled=False (e.g. disabled_reason "out_of_credits")
        // while real spend is still accrued for the period. The dashboard must keep
        // showing the spend instead of hiding the whole line.
        let mut s = snap(10.0, 120.0);
        s.extra = Some(extra(false, 35.0, 22.63, Some("out_of_credits")));
        let e = visible_extra(&s).expect("extra-usage line missing");
        assert_eq!(e.used_credits, 22.63);
        assert_eq!(extra_disabled_reason(e), Some("out of credits".into()));
    }

    #[test]
    fn extra_usage_hidden_when_no_budget_and_no_spend() {
        // No configured limit and nothing spent → nothing to show.
        let mut s = snap(10.0, 120.0);
        s.extra = Some(extra(false, 0.0, 0.0, None));
        assert_eq!(visible_extra(&s), None);
        s.extra = Some(extra(true, 0.0, 0.0, None));
        assert_eq!(visible_extra(&s), None);
        s.extra = None;
        assert_eq!(visible_extra(&s), None);
    }

    #[test]
    fn extra_usage_shown_with_budget_or_spend_alone() {
        let mut s = snap(10.0, 120.0);
        s.extra = Some(extra(true, 35.0, 0.0, None));
        assert!(visible_extra(&s).is_some());
        s.extra = Some(extra(false, 0.0, 0.01, None));
        assert!(visible_extra(&s).is_some());
    }

    #[test]
    fn extra_disabled_reason_text() {
        assert_eq!(
            extra_disabled_reason(&extra(true, 35.0, 1.0, Some("spend_limit"))),
            None
        );
        assert_eq!(
            extra_disabled_reason(&extra(false, 35.0, 1.0, Some("spend_limit"))),
            Some("spend limit".into())
        );
        assert_eq!(
            extra_disabled_reason(&extra(false, 35.0, 1.0, None)),
            Some("off".into())
        );
        // Python `or`: an empty reason also reads "off".
        assert_eq!(
            extra_disabled_reason(&extra(false, 35.0, 1.0, Some(""))),
            Some("off".into())
        );
    }

    // --- Chart labels -------------------------------------------------------------------

    #[test]
    fn bucket_hour_labels_mark_the_bucket_containing_each_hour() {
        let t = |h, m| Utc.with_ymd_and_hms(2026, 10, 11, h, m, 0).unwrap();
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        // Starts on the hour: contains it.
        assert_eq!(bucket_hour_label_in(t(17, 0), &plus2), "19");
        // :15 to :45 holds no hour; :45 to :15 holds the next one.
        assert_eq!(bucket_hour_label_in(t(17, 15), &plus2), "");
        assert_eq!(bucket_hour_label_in(t(17, 45), &plus2), "20");
        // :30 to :00 excludes the end.
        assert_eq!(bucket_hour_label_in(t(17, 30), &plus2), "");
        assert_eq!(
            bucket_hour_label_in(t(17, 30) + Duration::seconds(1), &plus2),
            "20"
        );
        assert_eq!(bucket_hour_label_in(t(21, 50), &plus2), "00"); // next local day
                                                                   // Half-hour zone: 17:00Z is 22:30 local, 17:15Z is 22:45 local.
        let india = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        assert_eq!(bucket_hour_label_in(t(17, 0), &india), "");
        assert_eq!(bucket_hour_label_in(t(17, 15), &india), "23");
        assert_eq!(bucket_hour_label_in(t(17, 30), &india), "23");
        assert_eq!(
            bucket_hour_label(t(17, 45)),
            bucket_hour_label_in(t(17, 45), &Local)
        );
    }

    #[test]
    fn day_labels_are_weekday_abbreviations() {
        let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
        assert_eq!(day_label(d(11)), "Sun");
        assert_eq!(day_label(d(12)), "Mon");
        assert_eq!(day_label(d(17)), "Sat");
    }
}
