//! The hero timelines: a usage window (the 5-hour one, or the 7-day one in
//! [`super::week`]) as a smooth area chart, described by a [`Chart`].
//!
//! x runs from the window start to its reset, y from 0 to 100%. On top of
//! light gridlines it paints the usage area (raw window samples, a gradient
//! [`Mesh`] in the state color with a stroked line), the overlay (the
//! 5-hour chart's Fable share under the area, or the weekly chart's Fable
//! 7-day line), a "now" marker with a dot per meter, each meter's dashed
//! projection to the reset or the cap (with a ✕ and "cap 4:10p", or a
//! "safe" end label; "at cap" when it is there), the dashed 100% cap line,
//! and a hover crosshair with a tooltip. Time gaps in the samples (the app
//! was closed) are bridged with a faint dotted line: utilization only rises
//! within a window, so the straight line is the best guess; only resets
//! and missing values truly break. How long a gap breaks the line is per
//! chart (10 minutes for 5 hours, an hour for a week).
//!
//! A chart holds one [`SeriesData`] per account. One account draws as
//! above; several overlay over the union of their windows (see
//! [`overlay_charts`]), each in its series color with a light tint area
//! and a legend in the card's title row, each reset a colored mark on the
//! time axis, and the hover lists every account's value ([`hover_rows`]).
//!
//! Geometry (mapping, segmenting, thinning, dashes, ticks, labels, hover
//! lookup) is in pure functions tested without a GPU; [`show_chart`] only
//! paints.

use std::fmt::Display;
use std::ops::Range;

use chrono::{DateTime, Duration, DurationRound, Local, NaiveTime, TimeZone, Timelike, Utc};
use eframe::egui::{
    epaint::{Mesh, PathStroke},
    pos2,
    text::{LayoutJob, TextFormat},
    vec2, Align2, Color32, FontId, Id, Painter, Pos2, Rect, Response, Sense, Shape, Stroke,
    TextStyle, Ui, Vec2,
};

use crate::analytics::{
    self, alerts::fmt_clock_in, FiveHourState, Projection, WeekSample, WindowSample,
};
use crate::poller::Dashboard;
use crate::profiles::ProfileId;

use super::accounts::AccountView;
use super::motion;
use super::theme::Palette;

/// Height of the timeline in full mode.
pub const FULL_HEIGHT: f32 = 180.0;
/// The card's title.
pub const TITLE: &str = "5-hour window";
/// The 5-hour chart's note while no window is active.
pub const IDLE_NOTE: &str = "No active window · starts with your next message";
/// A chart's note before it has two samples to draw.
const COLLECTING_NOTE: &str = "collecting samples…";
/// On the 5-hour chart, a gap between samples longer than this ends a
/// solid line (the app was closed or polls failed); its two ends are joined
/// by a [`Bridge`].
pub const GAP_BREAK_MINUTES: i64 = 10;
/// The same on the weekly chart.
pub const WEEK_GAP_BREAK_MINUTES: i64 = 60;
/// A drop of more than this many points between samples is a reset and
/// breaks the line (the rule of [`analytics::recent_window`]).
pub const RESET_DROP: f64 = analytics::RESET_DROP;
/// Gridlines with a label, in %.
const GRID_MARKS: [f64; 3] = [25.0, 50.0, 75.0];
/// Usage-area opacity at the line and at the baseline.
const AREA_TOP_ALPHA: f32 = 0.45;
const AREA_BASE_ALPHA: f32 = 0.05;
/// Fable-layer opacity.
const FABLE_ALPHA: f32 = 0.70;
/// 100% cap-line opacity.
const CAP_LINE_ALPHA: f32 = 0.45;
/// Dash and gap lengths of the dashed lines, in points.
const DASH: f32 = 5.0;
const DASH_GAP: f32 = 4.0;
/// Half the arm length of the ✕ cap marker.
const CAP_MARK: f32 = 4.0;
/// Radius of the dot on the curve at now.
const DOT_RADIUS: f32 = 3.5;
/// Radius of the dot marking an isolated sample.
const LONE_RADIUS: f32 = 1.5;
/// Bridge line opacity, and its fills relative to the normal fills.
const BRIDGE_LINE_ALPHA: f32 = 0.40;
const BRIDGE_FILL_SCALE: f32 = 1.0 / 3.0;
/// Dot and gap lengths of the dotted bridge line.
const DOT: f32 = 2.0;
const DOT_GAP: f32 = 3.0;
/// Hover snaps to a sample at most this far away horizontally.
pub const SNAP_PX: f32 = 12.0;

/// One sample on a chart: the main meter and the overlay's raw value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub t: DateTime<Utc>,
    /// The main meter (5-hour, or 7-day all).
    pub value: Option<f64>,
    /// The overlay's meter (Fable 7-day), drawn by [`Overlay::Line`].
    pub overlay: Option<f64>,
}

impl From<&WindowSample> for Point {
    fn from(s: &WindowSample) -> Self {
        Point {
            t: s.t,
            value: s.five_hour,
            overlay: s.fable,
        }
    }
}

impl From<&WeekSample> for Point {
    fn from(s: &WeekSample) -> Self {
        Point {
            t: s.t,
            value: s.seven_day,
            overlay: s.fable,
        }
    }
}

/// Which of a point's values a line follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Series {
    Main,
    Overlay,
}

impl Point {
    pub fn get(&self, series: Series) -> Option<f64> {
        match series {
            Series::Main => self.value,
            Series::Overlay => self.overlay,
        }
    }
}

/// Maps the window's time and 0–100% onto the plot rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Axes {
    pub plot: Rect,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Axes {
    fn span_ms(&self) -> i64 {
        (self.end - self.start).num_milliseconds()
    }

    /// x of `t`; times outside the window extrapolate (the painter clips).
    /// An empty window maps everything to the left edge.
    pub fn x_at(&self, t: DateTime<Utc>) -> f32 {
        let span = self.span_ms();
        if span <= 0 {
            return self.plot.left();
        }
        let frac = (t - self.start).num_milliseconds() as f64 / span as f64;
        self.plot.left() + frac as f32 * self.plot.width()
    }

    /// y of `pct`, clamped to 0–100%.
    pub fn y_at(&self, pct: f64) -> f32 {
        y_in(self.plot, pct)
    }

    /// The time at `x` (inverse of [`Axes::x_at`]), to the millisecond.
    pub fn t_at(&self, x: f32) -> DateTime<Utc> {
        let span = self.span_ms();
        if span <= 0 || self.plot.width() <= 0.0 {
            return self.start;
        }
        let frac = f64::from((x - self.plot.left()) / self.plot.width());
        self.start + Duration::milliseconds((frac * span as f64).round() as i64)
    }

    pub fn pos(&self, t: DateTime<Utc>, pct: f64) -> Pos2 {
        pos2(self.x_at(t), self.y_at(pct))
    }
}

/// y of `pct` in `plot`, on the fixed 0–100% scale every chart shares;
/// clamped to the plot.
pub fn y_in(plot: Rect, pct: f64) -> f32 {
    let frac = (pct.clamp(0.0, 100.0) / 100.0) as f32;
    plot.bottom() - frac * plot.height()
}

/// The plot area inside a chart widget `rect`: room on the left for the
/// "100%" labels (`label_w` wide), on top for the ✕ cap marker or half a
/// label, at the bottom for the x labels (`text_h` tall). Shared by the
/// timeline and the 7-day chart so their gridlines line up side by side.
pub fn plot_rect(rect: Rect, label_w: f32, text_h: f32) -> Rect {
    Rect::from_min_max(
        pos2(
            rect.left() + label_w + 6.0,
            rect.top() + text_h.max(CAP_MARK + 2.0),
        ),
        pos2(rect.right() - CAP_MARK - 2.0, rect.bottom() - text_h - 6.0),
    )
}

/// Index ranges of `samples` drawn as one solid line each: only samples
/// with a main value, broken where a value is missing, the time between
/// neighbors exceeds `gap_break` while the value changed across it (see
/// [`bridges`]; a gap with the same value at both ends is known to be flat
/// and stays solid), or the value drops by more than [`RESET_DROP`] points.
/// Single-sample ranges are kept.
pub fn segments(samples: &[Point], gap_break: Duration) -> Vec<Range<usize>> {
    segments_of(samples, gap_break, Series::Main)
}

/// [`segments`] of either series.
pub fn segments_of(samples: &[Point], gap_break: Duration, series: Series) -> Vec<Range<usize>> {
    let max_gap = gap_break;
    let mut out = Vec::new();
    let mut current: Option<Range<usize>> = None;
    for (i, s) in samples.iter().enumerate() {
        let Some(v) = s.get(series) else {
            out.extend(current.take());
            continue;
        };
        current = match current.take() {
            Some(r) => {
                let prev = &samples[r.end - 1];
                let prev_v = prev.get(series).unwrap_or(v);
                let changed_gap = s.t - prev.t > max_gap && prev_v != v;
                if changed_gap || prev_v - v > RESET_DROP {
                    out.push(r);
                    Some(i..i + 1)
                } else {
                    Some(r.start..i + 1)
                }
            }
            None => Some(i..i + 1),
        };
    }
    out.extend(current);
    out
}

/// A stretch without samples between two sampled points, drawn faintly
/// dotted: the sample indices of its two ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bridge {
    pub from: usize,
    pub to: usize,
}

/// Bridges between consecutive `segs` (from [`segments`]) that only a time
/// gap separates: none where a missing value or a reset broke the line.
pub fn bridges(samples: &[Point], segs: &[Range<usize>]) -> Vec<Bridge> {
    bridges_of(samples, segs, Series::Main)
}

/// [`bridges`] of either series.
pub fn bridges_of(samples: &[Point], segs: &[Range<usize>], series: Series) -> Vec<Bridge> {
    segs.windows(2)
        .filter_map(|pair| {
            let (from, to) = (pair[0].end - 1, pair[1].start);
            if to != from + 1 {
                return None; // missing values in between
            }
            let (a, b) = (samples[from].get(series)?, samples[to].get(series)?);
            (a - b <= RESET_DROP).then_some(Bridge { from, to })
        })
        .collect()
}

/// `(t, value)` of `series` over `range` of `samples`, missing values as 0
/// (segments never hold any).
fn series(samples: &[Point], range: Range<usize>, which: Series) -> Vec<(DateTime<Utc>, f64)> {
    samples[range]
        .iter()
        .map(|s| (s.t, s.get(which).unwrap_or(0.0)))
        .collect()
}

/// The values to draw for whole-percent readings `samples` (one segment,
/// oldest first): a meter that first reads N at `t` crossed N at about
/// `t`, so the line runs straight between change points (the first sample
/// of each new value) instead of holding each value flat, and stays flat
/// from the last change point on (it hasn't reached the next value yet).
/// One value per sample, so hover can still snap to the real readings.
///
/// Each drawn value stays within its own reading's whole percent (at most
/// one point above a rising reading, below a falling one), so a jump of
/// several points after a long flat stretch isn't spread over the stretch
/// as an invented ramp: the line holds near the old value and then rises.
pub fn through_change_points(samples: &[(DateTime<Utc>, f64)]) -> Vec<f64> {
    let changes: Vec<usize> = (0..samples.len())
        .filter(|&i| i == 0 || samples[i].1 != samples[i - 1].1)
        .collect();
    let mut out = Vec::with_capacity(samples.len());
    let mut k = 0;
    for (i, &(t, v)) in samples.iter().enumerate() {
        while k + 1 < changes.len() && changes[k + 1] <= i {
            k += 1;
        }
        let Some(&next) = changes.get(k + 1) else {
            out.push(v); // after the last change point: flat
            continue;
        };
        let (t0, v0) = samples[changes[k]];
        let (t1, v1) = samples[next];
        let span = (t1 - t0).num_milliseconds() as f64;
        let frac = if span > 0.0 {
            (t - t0).num_milliseconds() as f64 / span
        } else {
            0.0
        };
        let lerp = v0 + (v1 - v0) * frac;
        out.push(if v1 >= v0 {
            lerp.clamp(v, v + 1.0)
        } else {
            lerp.clamp(v - 1.0, v)
        });
    }
    out
}

/// The drawn value of every point of `series` (see
/// [`through_change_points`], per segment), `None` outside the segments.
pub fn drawn_values(samples: &[Point], segs: &[Range<usize>], which: Series) -> Vec<Option<f64>> {
    let mut out = vec![None; samples.len()];
    for seg in segs {
        let drawn = through_change_points(&series(samples, seg.clone(), which));
        for (i, v) in seg.clone().zip(drawn) {
            out[i] = Some(v);
        }
    }
    out
}

/// The runs of `Some` in `values[within]`, as index ranges into `values`.
pub fn present_runs(values: &[Option<f64>], within: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for i in within.clone() {
        match (values[i].is_some(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    out.extend(start.map(|s| s..within.end));
    out
}

/// At most two points per whole-pixel column: a column with several points
/// keeps its highest and lowest (in their original order), so spikes
/// survive. Points must be sorted by x.
pub fn thin_columns(points: &[Pos2]) -> Vec<Pos2> {
    let mut out = Vec::with_capacity(points.len());
    let mut i = 0;
    while i < points.len() {
        let column = points[i].x.floor();
        let mut end = i + 1;
        while end < points.len() && points[end].x.floor() == column {
            end += 1;
        }
        let group = &points[i..end];
        if group.len() <= 2 && group.first().map(|p| p.y) != group.last().map(|p| p.y) {
            out.extend_from_slice(group);
        } else {
            let (mut top, mut bottom) = (0, 0);
            for (j, p) in group.iter().enumerate() {
                if p.y < group[top].y {
                    top = j;
                }
                if p.y > group[bottom].y {
                    bottom = j;
                }
            }
            let (a, b) = (top.min(bottom), top.max(bottom));
            out.push(group[a]);
            if b != a {
                out.push(group[b]);
            }
        }
        i = end;
    }
    out
}

/// A triangle strip from the line through `points` down to `baseline`,
/// colored `top` at the line and `bottom` at the baseline. Empty with
/// fewer than two points.
pub fn area_mesh(points: &[Pos2], baseline: f32, top: Color32, bottom: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    if points.len() < 2 {
        return mesh;
    }
    mesh.reserve_vertices(points.len() * 2);
    mesh.reserve_triangles((points.len() - 1) * 2);
    for (i, p) in points.iter().enumerate() {
        mesh.colored_vertex(*p, top);
        mesh.colored_vertex(pos2(p.x, baseline), bottom);
        if i > 0 {
            let (line, base) = (2 * i as u32, 2 * i as u32 + 1);
            mesh.add_triangle(line - 2, line - 1, line);
            mesh.add_triangle(line - 1, base, line);
        }
    }
    mesh
}

/// The dashes of a dashed line from `a` to `b`: `dash` long, `gap` apart,
/// the last one cut at `b`. A non-positive `dash` gives one solid segment.
pub fn dashes(a: Pos2, b: Pos2, dash: f32, gap: f32) -> Vec<[Pos2; 2]> {
    let length = a.distance(b);
    if length <= 0.0 {
        return Vec::new();
    }
    if dash <= 0.0 {
        return vec![[a, b]];
    }
    let dir = (b - a) / length;
    let step = dash + gap.max(0.0);
    let mut out = Vec::with_capacity((length / step) as usize + 1);
    let mut d = 0.0;
    while d < length {
        let end = (d + dash).min(length);
        let to = if end >= length { b } else { a + dir * end };
        out.push([a + dir * d, to]);
        d += step;
    }
    out
}

/// Every local whole hour in `[start, end]` with its 12-hour label ("11",
/// "12", "1", …). Works for any offset, including half-hour ones.
pub fn hour_ticks_in<Tz: TimeZone>(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    tz: &Tz,
) -> Vec<(DateTime<Utc>, String)>
where
    Tz::Offset: Display,
{
    let local = start.with_timezone(tz);
    let into_hour = Duration::seconds(i64::from(local.minute() * 60 + local.second()))
        + Duration::nanoseconds(i64::from(local.nanosecond()));
    let mut t = if into_hour.is_zero() {
        start
    } else {
        start + (Duration::hours(1) - into_hour)
    };
    let mut out = Vec::new();
    while t <= end {
        out.push((t, t.with_timezone(tz).format("%-I").to_string()));
        t += Duration::hours(1);
    }
    out
}

/// Compact local time for axis labels: "3:42p", "12:05a".
pub fn short_time_in<Tz: TimeZone>(t: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let local = t.with_timezone(tz);
    let suffix = if local.hour() < 12 { 'a' } else { 'p' };
    format!("{}{suffix}", local.format("%-I:%M"))
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

/// Every local midnight in `[start, end]` with its short weekday ("Tue"),
/// for the weekly chart's ticks. A midnight a DST jump skips is taken at
/// the first valid instant after it.
pub fn day_ticks_in<Tz: TimeZone>(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    tz: &Tz,
) -> Vec<(DateTime<Utc>, String)>
where
    Tz::Offset: Display,
{
    let mut out = Vec::new();
    if end < start {
        return out;
    }
    let mut day = start.with_timezone(tz).date_naive();
    let last = end.with_timezone(tz).date_naive();
    while day <= last {
        let midnight = day.and_time(NaiveTime::MIN);
        let t = tz
            .from_local_datetime(&midnight)
            .earliest()
            .or_else(|| {
                tz.from_local_datetime(&(midnight + Duration::hours(1)))
                    .earliest()
            })
            .map(|t| t.with_timezone(&Utc));
        if let Some(t) = t.filter(|t| *t >= start && *t <= end) {
            out.push((t, day.format("%a").to_string()));
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    out
}

/// The weekly card's subtitle: "Mon 11p – Mon 11p", from the window start
/// to its reset. `None` when the reset is unknown.
pub fn week_span_in<Tz: TimeZone>(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    reset_known: bool,
    tz: &Tz,
) -> Option<String>
where
    Tz::Offset: Display,
{
    reset_known.then(|| {
        format!(
            "{} – {}",
            weekday_time_in(start, tz),
            weekday_time_in(end, tz)
        )
    })
}

/// The card's subtitle: the window's span, "6:10p – 11:10p" in the
/// [`short_time_in`] format of the "reset 11:10p" label. `None` when the
/// reset is unknown: the end is then only assumed.
pub fn window_span_in<Tz: TimeZone>(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    reset_known: bool,
    tz: &Tz,
) -> Option<String>
where
    Tz::Offset: Display,
{
    reset_known.then(|| format!("{} – {}", short_time_in(start, tz), short_time_in(end, tz)))
}

/// Tooltip over a bridge: "no samples 1:10–1:45 PM", or with both
/// meridiems when they differ ("11:50 AM–12:20 PM").
pub fn gap_tip_in<Tz: TimeZone>(from: DateTime<Utc>, to: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let (a, b) = (from.with_timezone(tz), to.with_timezone(tz));
    let start = if (a.hour() < 12) == (b.hour() < 12) {
        a.format("%-I:%M").to_string()
    } else {
        fmt_clock_in(from, tz)
    };
    format!("no samples {start}–{}", fmt_clock_in(to, tz))
}

/// The sample with a main value nearest in time to `t`.
pub fn nearest_sample(samples: &[Point], t: DateTime<Utc>) -> Option<usize> {
    samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.value.is_some())
        .min_by_key(|(_, s)| (s.t - t).abs())
        .map(|(i, _)| i)
}

/// The projection's % at `t` beyond `now`, held at its end value past the
/// end of the line. `None` at or before `now`, or for a zero-length line.
pub fn projected_pct(
    now: DateTime<Utc>,
    pct_now: f64,
    projection: &Projection,
    t: DateTime<Utc>,
) -> Option<f64> {
    let length = (projection.end_t - now).num_milliseconds();
    if t <= now || length <= 0 {
        return None;
    }
    let frac = ((t - now).num_milliseconds() as f64 / length as f64).min(1.0);
    Some(pct_now + (projection.end_pct - pct_now) * frac)
}

/// Tooltip for a sample: "1:35 PM · 42.0% · Fable 6.1%", without the Fable
/// part when there is none.
pub fn sample_tip(time: &str, pct: f64, fable: Option<f64>) -> String {
    match fable.filter(|f| *f > 0.0) {
        Some(f) => format!("{time} · {pct:.1}% · Fable {f:.1}%"),
        None => format!("{time} · {pct:.1}%"),
    }
}

/// Tooltip for a point on the projection: "3:10 PM · projected 71%".
pub fn projected_tip(time: &str, pct: f64) -> String {
    format!("{time} · projected {pct:.0}%")
}

/// Weekly tooltip for a sample: "Sat 3:12p · 7d all 41% · Fable 52%",
/// without the Fable part when there is none.
pub fn week_sample_tip(time: &str, pct: f64, fable: Option<f64>) -> String {
    match fable {
        Some(f) => format!("{time} · 7d all {pct:.0}% · Fable {f:.0}%"),
        None => format!("{time} · 7d all {pct:.0}%"),
    }
}

/// Weekly tooltip on the projections: "Mon 3p · projected 7d all 71% ·
/// Fable 90%", each part only for a meter that is drawn.
pub fn week_projected_tip(time: &str, all: Option<f64>, fable: Option<f64>) -> String {
    let parts: Vec<String> = [("7d all", all), ("Fable", fable)]
        .iter()
        .filter_map(|(name, v)| v.map(|v| format!("{name} {v:.0}%")))
        .collect();
    format!("{time} · projected {}", parts.join(" · "))
}

/// What the pointer at `x` points at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HoverTarget {
    /// The projection beyond now, at `t`.
    Projected { t: DateTime<Utc>, pct: f64 },
    /// A sample within [`SNAP_PX`].
    Sample(usize),
    /// A bridged gap.
    Gap(Bridge),
    /// Nothing nearby: just the time under the pointer.
    Time(DateTime<Utc>),
}

/// The hover target at `x`: the projection beyond now, else the nearest
/// sample within [`SNAP_PX`], else a bridge spanning the pointer, else the
/// bare time. `projection` is `(pct now, line)`.
pub fn hover_target(
    axes: &Axes,
    samples: &[Point],
    bridges: &[Bridge],
    now: DateTime<Utc>,
    projection: Option<(f64, Projection)>,
    x: f32,
) -> HoverTarget {
    let t = axes.t_at(x);
    if let Some(pct) = projection.and_then(|(pct, p)| projected_pct(now, pct, &p, t)) {
        return HoverTarget::Projected { t, pct };
    }
    if let Some(i) = nearest_sample(samples, t) {
        if (axes.x_at(samples[i].t) - x).abs() <= SNAP_PX {
            return HoverTarget::Sample(i);
        }
    }
    match bridges
        .iter()
        .find(|b| samples[b.from].t < t && t < samples[b.to].t)
    {
        Some(b) => HoverTarget::Gap(*b),
        None => HoverTarget::Time(t),
    }
}

/// Padding of the end label's backing pad around its text.
pub const LABEL_PAD: f32 = 2.0;

/// Where the segment `a`→`b` first touches `rect`, as a fraction of the
/// way from `a` (0 when `a` is inside), or `None` when it misses.
pub fn segment_entry(a: Pos2, b: Pos2, rect: Rect) -> Option<f32> {
    // Liang–Barsky: clip the parameter range 0..=1 against each slab.
    let d = b - a;
    let (mut t0, mut t1) = (0.0_f32, 1.0_f32);
    for (p, q) in [
        (-d.x, a.x - rect.left()),
        (d.x, rect.right() - a.x),
        (-d.y, a.y - rect.top()),
        (d.y, rect.bottom() - a.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None; // parallel to and outside this slab
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
        }
    }
    (t0 <= t1 && d != Vec2::ZERO).then_some(t0)
}

/// `rect` moved (not resized) to lie inside `bounds` where it fits.
fn shift_inside(rect: Rect, bounds: Rect) -> Rect {
    let dx = (bounds.left() - rect.left()).max(0.0) + (bounds.right() - rect.right()).min(0.0);
    let dy = (bounds.top() - rect.top()).max(0.0) + (bounds.bottom() - rect.bottom()).min(0.0);
    let moved = rect.translate(vec2(dx, dy));
    // Wider or taller than the bounds: pin to the left/top edge.
    let x = if moved.width() > bounds.width() {
        bounds.left() - moved.left()
    } else {
        0.0
    };
    let y = if moved.height() > bounds.height() {
        bounds.top() - moved.top()
    } else {
        0.0
    };
    moved.translate(vec2(x, y))
}

/// Clearance between the ✕ and its label, past the mark and the pad.
pub const CAP_LABEL_CLEARANCE: f32 = CAP_MARK + 4.0;
/// Half-size of the box around the ✕ no label may cover.
pub const CAP_KEEP_OUT: f32 = CAP_MARK + 1.0;
/// The "safe" label's clearance and keep-out around the dash's tip.
const SAFE_LABEL_CLEARANCE: f32 = 4.0;
const SAFE_KEEP_OUT: f32 = 2.0;

/// What the projection's end says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndLabel {
    /// "cap 4:10p" with a ✕, at this time.
    Cap(DateTime<Utc>),
    /// "safe": the window resets first.
    Safe,
    /// "at cap": the meter is at 100% (no projection).
    AtCap,
}

/// The end label for projection `p`, or none: without a known reset the
/// window end is only assumed, so neither "safe" nor a cap time is
/// claimed (the footer then shows the rate alone); a zero-length line
/// gets none either.
pub fn end_label(reset_known: bool, p: &Projection, now: DateTime<Utc>) -> Option<EndLabel> {
    if !reset_known {
        return None;
    }
    match p.cap_at {
        Some(cap) => Some(EndLabel::Cap(cap)),
        None if p.end_t > now => Some(EndLabel::Safe),
        None => None,
    }
}

/// Whether `a` and `b` overlap with positive area (touching edges don't).
fn overlaps(a: Rect, b: Rect) -> bool {
    a.min.x < b.max.x && b.min.x < a.max.x && a.min.y < b.max.y && b.min.y < a.max.y
}

/// The line's lowest point (largest y) over `x0..=x1`, for a segment that
/// may not span that range (it is extended flat past its ends).
fn line_low_over(from: Pos2, end: Pos2, x0: f32, x1: f32) -> f32 {
    let y_at = |x: f32| {
        if (end.x - from.x).abs() < f32::EPSILON {
            from.y.max(end.y)
        } else {
            let t = ((x - from.x) / (end.x - from.x)).clamp(0.0, 1.0);
            from.y + (end.y - from.y) * t
        }
    };
    y_at(x0).max(y_at(x1))
}

/// The end label's rect, `size` big, for the projection `from`→`end`,
/// inside `bounds`. `keep_out` is the half-size of the box around the end
/// point the label's pad must never cover (the ✕, or the dash's tip), and
/// `clearance` how far the label sits from the end point.
///
/// Tries, in order: above-left, above-right, left at the top of the bounds,
/// below-right, left (centered on the end point), below-left dropped just
/// under the line, then behind the now marker at the top. The projection starts at now, so the vertical now
/// marker through `from.x` is a keep-out column as well. Takes the first
/// whose padded rect misses the line, the keep-out box and the now column;
/// then the first that misses the box and the column (the dash then stops
/// short of the label, see [`dash_end`]); then the first that misses the
/// box. Every tier also keeps clear of `avoid` (other meters' labels and
/// ✕ marks) when it can.
pub fn place_end_label(
    from: Pos2,
    end: Pos2,
    size: Vec2,
    clearance: f32,
    keep_out: f32,
    bounds: Rect,
    avoid: &[Rect],
) -> Rect {
    let c = clearance;
    let at = |min: Pos2| shift_inside(Rect::from_min_size(min, size), bounds);
    // Left of the end point at the top of the bounds, far enough left that
    // the line (rising toward the end point) has dropped below the label.
    let top_left = {
        let drop = bounds.top() + size.y + 2.0 * LABEL_PAD - end.y;
        let run = end.x - from.x;
        let gap = if drop <= 0.0 {
            c
        } else if run > 0.0 && from.y > end.y {
            c.max(drop * run / (from.y - end.y) + LABEL_PAD)
        } else {
            c
        };
        at(pos2(end.x - gap - size.x, bounds.top()))
    };
    let under_line = {
        let x = shift_inside(
            Rect::from_min_size(pos2(end.x - size.x, end.y), size),
            bounds,
        );
        let low = line_low_over(from, end, x.left() - LABEL_PAD, x.right() + LABEL_PAD);
        at(pos2(x.left(), (end.y + c).max(low + LABEL_PAD + 1.0)))
    };
    let candidates = [
        at(pos2(end.x - size.x, end.y - c - size.y)),
        at(pos2(end.x + c, end.y - c - size.y)),
        top_left,
        at(pos2(end.x + c, end.y + c)),
        at(pos2(end.x - c - size.x, end.y - size.y / 2.0)),
        under_line,
        // Behind now at the top, for a projection too short to hold the
        // label: usage only grows within a window, so the past line runs
        // at or below the dot and the line ahead starts right of now.
        at(pos2(from.x - LABEL_PAD - 1.0 - size.x, bounds.top())),
    ];
    let keep = Rect::from_center_size(end, Vec2::splat(2.0 * keep_out));
    let clear_of_keep = |r: &Rect| !overlaps(r.expand(LABEL_PAD), keep);
    let clear_of_now = |r: &Rect| {
        let padded = r.expand(LABEL_PAD);
        !(padded.left() < from.x && from.x < padded.right())
    };
    let clear_of_line = |r: &Rect| segment_entry(from, end, r.expand(LABEL_PAD)).is_none();
    let clear_of_avoid = |r: &Rect| avoid.iter().all(|a| !overlaps(r.expand(LABEL_PAD), *a));
    let first = |ok: &dyn Fn(&Rect) -> bool| candidates.iter().copied().find(|r| ok(r));
    let tiers: [&dyn Fn(&Rect) -> bool; 3] = [
        &|r| clear_of_keep(r) && clear_of_now(r) && clear_of_line(r),
        &|r| clear_of_keep(r) && clear_of_now(r),
        &clear_of_keep,
    ];
    tiers
        .iter()
        .find_map(|ok| first(&|r| ok(r) && clear_of_avoid(r)))
        .or_else(|| tiers.iter().find_map(|ok| first(ok)))
        .unwrap_or(under_line)
}

/// Where the dashed projection stops: at `end`, or just before it would
/// run into the padded `label`.
pub fn dash_end(from: Pos2, end: Pos2, label: Rect) -> Pos2 {
    match segment_entry(from, end, label.expand(LABEL_PAD)) {
        None => end,
        Some(t) => {
            let d = end - from;
            // A small gap before the pad, never behind the start.
            let back = (LABEL_PAD / d.length()).min(t);
            from + d * (t - back)
        }
    }
}

/// What a chart draws with its main area besides the main line.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// Nothing.
    None,
    /// A clay area under the main area, one value per point (the 5-hour
    /// chart's Fable share).
    Share(Vec<Option<f64>>),
    /// A line of each point's [`Point::overlay`] (the weekly chart's Fable
    /// 7-day meter, its own limit): solid clay for one account, dashed in
    /// the account's color among several.
    Line { color: Color32, dashed: bool },
}

/// How a series fills under its line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AreaStyle {
    /// The gradient in this state color (one account).
    State(Color32),
    /// A light gradient in the series' own color, this opaque at the line
    /// (one account among several).
    Tint(f32),
}

/// One account's layer of a chart: its samples, its meters, its colors.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesData {
    /// Distinguishes this series' eased values (see [`motion_id`]).
    pub key: u64,
    /// The account's name in the legend and the tooltip.
    pub label: String,
    /// The line, the bridges, and (with [`AreaStyle::Tint`]) the area.
    pub color: Color32,
    pub area: AreaStyle,
    pub overlay: Overlay,
    pub points: Vec<Point>,
    /// The meters' lines, each tagged with its [`LineKind`].
    pub lines: Vec<Line>,
    /// Stale data (the last poll failed): painted at [`DIM_ALPHA`].
    pub dimmed: bool,
    /// The account's own window end (its reset): hover reads nothing past
    /// it.
    pub end: DateTime<Utc>,
    /// The snapshot's fetch time in ms, so projections draw in per poll.
    pub poll_key: i64,
}

impl SeriesData {
    /// `c` as this series paints it: faded when [`SeriesData::dimmed`].
    pub fn tone(&self, c: Color32) -> Color32 {
        if self.dimmed {
            c.gamma_multiply(DIM_ALPHA)
        } else {
            c
        }
    }

    /// The area's gradient: (at the line, at the baseline).
    pub fn area_colors(&self) -> (Color32, Color32) {
        let (top, bottom) = match self.area {
            AreaStyle::State(c) => (
                c.gamma_multiply(AREA_TOP_ALPHA),
                c.gamma_multiply(AREA_BASE_ALPHA),
            ),
            AreaStyle::Tint(a) => (
                self.color.gamma_multiply(a),
                self.color
                    .gamma_multiply(a * AREA_BASE_ALPHA / AREA_TOP_ALPHA),
            ),
        };
        (self.tone(top), self.tone(bottom))
    }
}

/// One entry of a chart's legend: a swatch (a short line, dashed for the
/// weekly chart's Fable lines) and a name.
#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    pub label: String,
    pub color: Color32,
    pub dashed: bool,
}

/// Opacity at the line of a series' area among several accounts.
pub const SERIES_TINT: f32 = 0.16;
/// Opacity of a stale series.
pub const DIM_ALPHA: f32 = 0.45;
/// At most this many labelled ticks on an overlaid chart's time axis.
const MAX_TICKS: usize = 8;

/// How a chart writes times: "3:10p" for a 5-hour window, "Mon 3p" for a
/// week.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeStyle {
    Clock,
    Weekday,
}

impl TimeStyle {
    /// A compact time for labels: [`short_time_in`] or [`weekday_time_in`].
    pub fn short<Tz: TimeZone>(self, t: DateTime<Utc>, tz: &Tz) -> String
    where
        Tz::Offset: Display,
    {
        match self {
            TimeStyle::Clock => short_time_in(t, tz),
            TimeStyle::Weekday => weekday_time_in(t, tz),
        }
    }

    /// A time for tooltips: "1:35 PM", or "Sat 3:12p".
    pub fn tip<Tz: TimeZone>(self, t: DateTime<Utc>, tz: &Tz) -> String
    where
        Tz::Offset: Display,
    {
        match self {
            TimeStyle::Clock => fmt_clock_in(t, tz),
            TimeStyle::Weekday => weekday_time_in(t, tz),
        }
    }
}

/// Which meter a [`Line`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// The chart's main meter (5-hour, or 7-day all).
    Main,
    /// The overlay's meter (Fable 7-day).
    Overlay,
}

/// One meter's now dot and dashed projection.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub kind: LineKind,
    /// The meter's % now (the dot eases to it).
    pub pct: f64,
    pub projection: Option<Projection>,
    pub label: Option<EndLabel>,
    /// The dot and the dash.
    pub color: Color32,
    /// The ✕ and the "cap …" label.
    pub cap_color: Color32,
}

/// Everything a timeline draws: the window, and per account its samples
/// and meters. One series is one account drawn as it always was (state
/// colors, the Fable layer); two or more overlay in their series colors
/// with a legend.
#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    /// Distinguishes this chart's eased values.
    pub id: &'static str,
    /// The window: one account's, or the union of several (see
    /// [`union_window`]).
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub now: DateTime<Utc>,
    pub series: Vec<SeriesData>,
    /// A longer gap between samples ends a solid line (see [`segments`]).
    pub gap_break: Duration,
    /// Labelled ticks along the bottom.
    pub ticks: Vec<(DateTime<Utc>, String)>,
    /// At the right end of the time axis: "reset 11:10p".
    pub end_label: Option<String>,
    /// Each account's reset, a tick in its color on the time axis (with
    /// two or more series).
    pub end_marks: Vec<(DateTime<Utc>, Color32)>,
    /// Right-aligned in the card's title row (with two or more series).
    pub legend: Vec<LegendEntry>,
    /// Opacity of a bridged gap's fill relative to the normal fill.
    pub bridge_fill: f32,
    pub style: TimeStyle,
    /// Centered in an empty chart in place of "collecting samples…".
    pub note: Option<&'static str>,
}

impl Chart {
    /// Several accounts overlaid (series colors, legend, series hover).
    pub fn overlaid(&self) -> bool {
        self.series.len() >= 2
    }
}

/// A series' key: a hash of its account's id, stable within a run.
pub fn series_key(id: &ProfileId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

/// The id of one eased value of a meter's line: per chart, series, meter.
pub fn motion_id(chart: &'static str, key: u64, kind: LineKind, what: &'static str) -> Id {
    Id::new((chart, key, kind, what))
}

/// The span covering every window: the earliest start to the latest end.
/// `None` without windows.
pub fn union_window(
    windows: impl IntoIterator<Item = (DateTime<Utc>, DateTime<Utc>)>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    windows
        .into_iter()
        .reduce(|(a0, a1), (b0, b1)| (a0.min(b0), a1.max(b1)))
}

/// At most `max` of `ticks`, evenly every n-th from the first.
pub fn thin_ticks(ticks: Vec<(DateTime<Utc>, String)>, max: usize) -> Vec<(DateTime<Utc>, String)> {
    if ticks.len() <= max || max == 0 {
        return ticks;
    }
    let step = ticks.len().div_ceil(max);
    ticks.into_iter().step_by(step).collect()
}

/// The legend of `series`: one entry per series in its color, and a dashed
/// "Fable" entry in `fable_color` when some series draws its Fable meter
/// as a dashed line. Empty with fewer than two series (one account needs
/// none).
pub fn legend_for(series: &[SeriesData], fable_color: Color32) -> Vec<LegendEntry> {
    if series.len() < 2 {
        return Vec::new();
    }
    let mut out: Vec<LegendEntry> = series
        .iter()
        .map(|s| LegendEntry {
            label: s.label.clone(),
            color: s.tone(s.color),
            dashed: false,
        })
        .collect();
    if series
        .iter()
        .any(|s| matches!(s.overlay, Overlay::Line { dashed: true, .. }))
    {
        out.push(LegendEntry {
            label: "Fable".to_owned(),
            color: fable_color,
            dashed: true,
        });
    }
    out
}

/// One account's series (from its single-account chart) restyled to sit
/// among others: its own color for the lines and dots and a light tint
/// area, no Fable share area, Fable 7-day as a dashed line in its color,
/// dimmed when stale. Cap marks keep their colors.
pub fn as_series(mut s: SeriesData, view: &AccountView) -> SeriesData {
    s.key = series_key(view.id);
    s.label = view.label.to_owned();
    s.color = view.color;
    s.area = AreaStyle::Tint(SERIES_TINT);
    s.overlay = match s.overlay {
        Overlay::Line { .. } => Overlay::Line {
            color: view.color,
            dashed: true,
        },
        _ => Overlay::None,
    };
    for l in &mut s.lines {
        l.color = view.color;
    }
    s.dimmed = view.stale;
    s
}

/// One account's chart to overlay with others (see [`overlay_charts`]).
pub struct Part<'a> {
    /// The account's own chart (one series).
    pub chart: Chart,
    pub view: &'a AccountView<'a>,
    /// Its window, `None` while it has no active one (an idle 5 hours).
    pub window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Its reset time is known: the window end gets a mark.
    pub reset_known: bool,
}

/// Several accounts' charts as one: each a series (see [`as_series`]) over
/// the union of their windows, each known reset an end mark in the
/// account's color, `ticks` over the union (thinned to [`MAX_TICKS`]), the
/// union's end time (or "now" with no reset known) at the axis end, and a
/// legend. With no active window at all, the first chart's empty state.
/// `None` without parts.
pub fn overlay_charts(
    parts: Vec<Part>,
    palette: &Palette,
    ticks: impl Fn(DateTime<Utc>, DateTime<Utc>) -> Vec<(DateTime<Utc>, String)>,
) -> Option<Chart> {
    let first = parts.first()?;
    let (id, now, gap_break, bridge_fill, style) = {
        let c = &first.chart;
        (c.id, c.now, c.gap_break, c.bridge_fill, c.style)
    };
    let union = union_window(parts.iter().filter_map(|p| p.window));
    let (start, end) = union.unwrap_or((first.chart.start, first.chart.end));
    let end_marks = parts
        .iter()
        .filter(|p| p.reset_known)
        .filter_map(|p| Some((p.window?.1, view_tone(p.view))))
        .collect::<Vec<_>>();
    let note = if union.is_some() {
        None
    } else {
        first.chart.note
    };
    let end_label = union.map(|_| {
        if end_marks.is_empty() {
            "now".to_owned()
        } else {
            style.short(end, &Local)
        }
    });
    let series: Vec<SeriesData> = parts
        .into_iter()
        .filter_map(|p| {
            let s = p.chart.series.into_iter().next()?;
            Some(as_series(s, p.view))
        })
        .collect();
    Some(Chart {
        id,
        start,
        end,
        now,
        legend: legend_for(&series, palette.text_muted),
        series,
        gap_break,
        ticks: if union.is_some() {
            thin_ticks(ticks(start, end), MAX_TICKS)
        } else {
            Vec::new()
        },
        end_label,
        end_marks,
        bridge_fill,
        style,
        note,
    })
}

/// An account's color as its series paints it: faded when stale.
fn view_tone(view: &AccountView) -> Color32 {
    if view.stale {
        view.color.gamma_multiply(DIM_ALPHA)
    } else {
        view.color
    }
}

/// The 5-hour chart for `d` at `now`. With no active window
/// ([`analytics::FiveHourState::Idle`]: after a reset, before the next
/// message, or once a known reset has passed since the poll) it is empty:
/// no samples, now, projection or times, only [`IDLE_NOTE`]. With a reset
/// unknown but usage above 0 it is the trailing 5 hours ending "now".
pub fn five_hour_chart(d: &Dashboard, now: DateTime<Utc>, palette: &Palette) -> Chart {
    let snap = &d.snapshot;
    let live = d.five_hour_now(now);
    let state = palette.level(analytics::color_for(live.pct));
    let idle = live.state == FiveHourState::Idle;
    let reset_known = live.resets_at.is_some();
    let lines = live
        .pct
        .filter(|_| !idle)
        .map(|p| {
            let rate = analytics::compute_rate(&d.history, now);
            let projection = analytics::projection(now, p, rate, d.window_end);
            Line {
                kind: LineKind::Main,
                pct: p,
                projection: Some(projection),
                label: end_label(reset_known, &projection, now),
                color: state,
                cap_color: palette.danger,
            }
        })
        .into_iter()
        .collect();
    let samples: &[WindowSample] = if idle { &[] } else { &d.window_samples };
    Chart {
        id: "timeline",
        start: d.window_start,
        end: d.window_end,
        now,
        series: vec![SeriesData {
            key: 0,
            label: String::new(),
            color: state,
            area: AreaStyle::State(state),
            overlay: Overlay::Share(analytics::fable_layer(samples)),
            points: samples.iter().map(Point::from).collect(),
            lines,
            dimmed: false,
            end: d.window_end,
            poll_key: snap.fetched_at.timestamp_millis(),
        }],
        gap_break: Duration::minutes(GAP_BREAK_MINUTES),
        bridge_fill: BRIDGE_FILL_SCALE,
        ticks: if idle {
            Vec::new()
        } else {
            hour_ticks_in(d.window_start, d.window_end, &Local)
        },
        end_label: match live.state {
            FiveHourState::Active => Some(format!("reset {}", short_time_in(d.window_end, &Local))),
            FiveHourState::Trailing => Some("now".to_owned()),
            FiveHourState::Idle => None,
        },
        end_marks: Vec::new(),
        legend: Vec::new(),
        style: TimeStyle::Clock,
        note: idle.then_some(IDLE_NOTE),
    }
}

/// The 5-hour chart of several accounts with data (see [`overlay_charts`]):
/// each account's own 5-hour window, the idle ones adding no window.
pub fn five_hour_series_chart(
    views: &[&AccountView],
    now: DateTime<Utc>,
    palette: &Palette,
) -> Option<Chart> {
    let parts = views
        .iter()
        .filter_map(|v| {
            let d = v.data?;
            let live = d.five_hour_now(now);
            let active = live.state != FiveHourState::Idle;
            Some(Part {
                chart: five_hour_chart(d, now, palette),
                view: v,
                window: active.then_some((d.window_start, d.window_end)),
                reset_known: active && live.resets_at.is_some(),
            })
        })
        .collect();
    overlay_charts(parts, palette, |a, b| hour_ticks_in(a, b, &Local))
}

/// The accounts that have data: the ones the charts draw.
pub fn with_data<'v, 'a>(accounts: &'v [AccountView<'a>]) -> Vec<&'v AccountView<'a>> {
    accounts.iter().filter(|v| v.data.is_some()).collect()
}

/// The 5-hour card for the accounts: one account with data as it always
/// was (see [`show_account_card`]), several overlaid (see
/// [`show_series_card`]); 0 (nothing drawn) without data.
pub fn show_timeline_card(
    ui: &mut Ui,
    accounts: &[AccountView],
    now: DateTime<Utc>,
    palette: &Palette,
    size: super::layout::CardSize,
) -> f32 {
    match with_data(accounts)[..] {
        [] => 0.0,
        [v] => show_account_card(ui, v, now, palette, size),
        ref several => show_series_card(ui, several, now, palette, size),
    }
}

/// The timeline's card: the title with the window's span as its subtitle
/// (see [`window_span_in`]), the timeline, `size.chart` tall, and the "5h
/// burn" row under it.
///
/// Returns the content's natural height (before `size.min_content`).
fn show_account_card(
    ui: &mut Ui,
    view: &AccountView,
    now: DateTime<Utc>,
    palette: &Palette,
    size: super::layout::CardSize,
) -> f32 {
    let Some(d) = view.data else { return 0.0 };
    let reset_known = d.five_hour_now(now).resets_at.is_some();
    let span = window_span_in(d.window_start, d.window_end, reset_known, &Local);
    let mut chart = five_hour_chart(d, now, palette);
    chart.series[0].key = series_key(view.id);
    super::theme::card(palette)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let top = ui.cursor().top();
            // Reserved from the cursor, so set before the content.
            ui.set_min_height(size.min_content);
            super::theme::chart_title(ui, palette, TITLE, span.as_deref());
            show_chart(ui, &chart, palette, size.chart);
            super::footer::show_burn_row(ui, d, now);
            // From the cursor: min_rect includes the reserved height.
            ui.cursor().top() - top - ui.spacing().item_spacing.y
        })
        .inner
}

/// The 5-hour card of several accounts: the title with the legend, the
/// overlaid chart, and each account's "5h burn" row under its name.
fn show_series_card(
    ui: &mut Ui,
    views: &[&AccountView],
    now: DateTime<Utc>,
    palette: &Palette,
    size: super::layout::CardSize,
) -> f32 {
    let Some(chart) = five_hour_series_chart(views, now, palette) else {
        return 0.0;
    };
    super::theme::card(palette)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let top = ui.cursor().top();
            ui.set_min_height(size.min_content);
            show_title(ui, palette, TITLE, None, &chart.legend);
            show_chart(ui, &chart, palette, size.chart);
            for v in views {
                let Some(d) = v.data else { continue };
                series_header(ui, palette, v);
                super::footer::show_burn_row(ui, d, now);
            }
            ui.cursor().top() - top - ui.spacing().item_spacing.y
        })
        .inner
}

/// A chart card's title row: [`super::theme::chart_title`] with the
/// subtitle, or, with a `legend`, the legend right-aligned in its place
/// (entries that don't fit after the title are dropped from the end).
pub fn show_title(
    ui: &mut Ui,
    palette: &Palette,
    title: &str,
    subtitle: Option<&str>,
    legend: &[LegendEntry],
) {
    if legend.is_empty() {
        super::theme::chart_title(ui, palette, title, subtitle);
        return;
    }
    let top = ui.cursor().top();
    super::theme::chart_title(ui, palette, title, None);
    let bottom = ui.cursor().top() - ui.spacing().item_spacing.y;
    let rect = Rect::from_min_max(
        pos2(ui.min_rect().left(), top),
        pos2(ui.min_rect().left() + ui.available_width(), bottom),
    );
    let title_font = super::theme::semibold(TextStyle::Body.resolve(ui.style()).size);
    let small = TextStyle::Small.resolve(ui.style());
    let painter = ui.painter_at(rect);
    let title_w = painter
        .layout_no_wrap(title.to_owned(), title_font, palette.text)
        .size()
        .x;
    let galleys: Vec<_> = legend
        .iter()
        .map(|e| painter.layout_no_wrap(e.label.clone(), small.clone(), palette.text_muted))
        .collect();
    let widths: Vec<f32> = galleys
        .iter()
        .map(|g| LEGEND_SWATCH + LEGEND_SWATCH_GAP + g.size().x)
        .collect();
    let room = rect.width() - title_w - super::theme::CHART_TITLE_GAP;
    let shown = legend_fit(&widths, LEGEND_GAP, room);
    let total: f32 =
        widths[..shown].iter().sum::<f32>() + LEGEND_GAP * shown.saturating_sub(1) as f32;
    let mut x = rect.right() - total;
    let y = rect.center().y;
    for (e, g) in legend.iter().zip(galleys).take(shown) {
        let (a, b) = (pos2(x, y), pos2(x + LEGEND_SWATCH, y));
        let stroke = Stroke::new(2.0, e.color);
        if e.dashed {
            for seg in dashes(a, b, 3.0, 2.0) {
                painter.line_segment(seg, stroke);
            }
        } else {
            painter.line_segment([a, b], stroke);
        }
        x += LEGEND_SWATCH + LEGEND_SWATCH_GAP;
        let w = g.size().x;
        painter.galley(pos2(x, y - g.size().y / 2.0), g, palette.text_muted);
        x += w + LEGEND_GAP;
    }
}

/// A legend swatch's length, its gap to the name, and the gap between
/// entries.
const LEGEND_SWATCH: f32 = 12.0;
const LEGEND_SWATCH_GAP: f32 = 4.0;
const LEGEND_GAP: f32 = 10.0;

/// How many legend entries `widths` wide (`gap` apart), from the first,
/// fit in `room`.
pub fn legend_fit(widths: &[f32], gap: f32, room: f32) -> usize {
    let mut used = 0.0;
    for (i, w) in widths.iter().enumerate() {
        used += w + if i > 0 { gap } else { 0.0 };
        if used > room {
            return i;
        }
    }
    widths.len()
}

/// An account's name over its rows on an overlaid card: a dot in its color
/// and the name, muted; "stale" after it when its last poll failed.
pub fn series_header(ui: &mut Ui, palette: &Palette, view: &AccountView) {
    use eframe::egui::RichText;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new("●").small().color(view_tone(view)));
        ui.label(RichText::new(view.label).small().color(palette.text_muted));
        if view.stale {
            ui.label(RichText::new("· stale").small().color(palette.text_muted));
        }
    });
}

/// Paint `chart`, `height` tall across the available width. Hovering shows
/// a crosshair and a tooltip: one account's sample or projection, or every
/// account's value at the pointer when several are overlaid.
pub fn show_chart(ui: &mut Ui, chart: &Chart, palette: &Palette, height: f32) -> Response {
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
    let axes = Axes {
        plot: plot_rect(rect, label_w, font.size),
        start: chart.start,
        end: chart.end,
    };

    paint_y_axis(&painter, axes.plot, palette, &font);
    paint_time_axis(&painter, &axes, palette, &font, rect.left(), chart);

    let layers: Vec<(Vec<Range<usize>>, Vec<Bridge>)> = chart
        .series
        .iter()
        .map(|s| {
            let segs = segments(&s.points, chart.gap_break);
            let gaps = bridges(&s.points, &segs);
            (segs, gaps)
        })
        .collect();
    if layers.iter().all(|(segs, _)| segs.is_empty()) {
        // Between the 50% and 75% gridlines, so neither strikes it through.
        painter.text(
            pos2(axes.plot.center().x, y_in(axes.plot, 62.5)),
            Align2::CENTER_CENTER,
            chart.note.unwrap_or(COLLECTING_NOTE),
            font,
            palette.text_muted,
        );
        return response;
    }
    // Every account's fills first, so no tint covers another's line.
    let lines: Vec<Vec<Vec<Pos2>>> = chart
        .series
        .iter()
        .zip(&layers)
        .map(|(s, (segs, gaps))| paint_fills(&painter, &axes, palette, chart, s, segs, gaps))
        .collect();
    for ((s, (_, gaps)), lines) in chart.series.iter().zip(&layers).zip(lines) {
        paint_lines(&painter, &axes, chart, s, gaps, lines);
    }

    // Each dot eases to its poll's %; each projection draws in when it first
    // appears and when a poll moves its end. All accounts' meters share one
    // pass, so every end label keeps clear of every other.
    let ctx = ui.ctx().clone();
    let ends: Vec<Ends> = chart
        .series
        .iter()
        .zip(&layers)
        .filter(|(_, (segs, _))| !segs.is_empty())
        .flat_map(|(s, _)| s.lines.iter().map(move |line| (s, line)))
        .map(|(s, line)| Ends {
            pct: motion::eased(
                &ctx,
                motion_id(chart.id, s.key, line.kind, "now-pct"),
                line.pct,
            ),
            projection: line.projection,
            label: line.label,
            share: line.projection.map_or(1.0, |p| {
                motion::draw_in(
                    &ctx,
                    motion_id(chart.id, s.key, line.kind, "projection"),
                    s.poll_key,
                    p.end_pct,
                )
            }),
            color: s.tone(line.color),
            cap_color: s.tone(line.cap_color),
        })
        .collect();
    paint_now(&painter, &axes, palette, chart, &ends, &font);

    if let Some(pointer) = response.hover_pos() {
        if axes.plot.x_range().contains(pointer.x) {
            if chart.overlaid() {
                let t = axes.t_at(pointer.x);
                let rows = hover_rows(chart, t);
                paint_series_hover(&painter, &axes, palette, &rows, t);
                let job = hover_job(&chart.style.tip(t, &Local), &rows, &font, palette.text);
                return response.on_hover_text_at_pointer(job);
            }
            let (s, (segs, gaps)) = (&chart.series[0], &layers[0]);
            // The main meter's projection, else the overlay's, drives the
            // hover beyond now.
            let projecting = [LineKind::Main, LineKind::Overlay]
                .iter()
                .filter_map(|k| s.lines.iter().find(|l| l.kind == *k))
                .find_map(|l| l.projection.map(|p| (l.pct, p)));
            let target = hover_target(&axes, &s.points, gaps, chart.now, projecting, pointer.x);
            let drawn = drawn_values(&s.points, segs, Series::Main);
            let tip = paint_hover(
                &painter, &axes, palette, chart, s, &drawn, target, pointer.x,
            );
            return response.on_hover_text_at_pointer(tip);
        }
    }
    response
}

/// One account's value in the overlaid hover tooltip.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverRow {
    pub label: String,
    pub color: Color32,
    /// The main meter's %.
    pub value: f64,
    /// The Fable meter's % (its share on the 5-hour chart), when shown.
    pub fable: Option<f64>,
    /// Read off the projection beyond now.
    pub projected: bool,
}

/// Every series' value at `t`, in series order. At or before now: the
/// last sample at or before `t`, when it is at most the chart's gap break
/// old (else the account has no reading there and no row). Beyond now: the
/// main meter's projection (held at its end, or at its % without one).
/// Nothing past the account's own window end.
pub fn hover_rows(chart: &Chart, t: DateTime<Utc>) -> Vec<HoverRow> {
    chart
        .series
        .iter()
        .filter(|s| t <= s.end)
        .filter_map(|s| {
            let row = |value, fable, projected| HoverRow {
                label: s.label.clone(),
                color: s.tone(s.color),
                value,
                fable,
                projected,
            };
            if t > chart.now {
                let at = |kind: LineKind| {
                    s.lines.iter().find(|l| l.kind == kind).map(|l| {
                        l.projection
                            .and_then(|p| projected_pct(chart.now, l.pct, &p, t))
                            .unwrap_or(l.pct)
                    })
                };
                return Some(row(at(LineKind::Main)?, at(LineKind::Overlay), true));
            }
            let i = s
                .points
                .iter()
                .rposition(|p| p.t <= t && p.value.is_some())?;
            let p = &s.points[i];
            if t - p.t > chart.gap_break {
                return None;
            }
            let fable = match &s.overlay {
                Overlay::Share(share) => share.get(i).copied().flatten().filter(|f| *f > 0.0),
                Overlay::Line { .. } => p.overlay,
                Overlay::None => None,
            };
            Some(row(p.value?, fable, false))
        })
        .collect()
}

/// A hover row's text after its dot: "nate 46%", "nate 46% · Fable 52%",
/// "work projected 71%".
pub fn hover_row_text(row: &HoverRow) -> String {
    let what = if row.projected { " projected" } else { "" };
    let mut text = format!("{}{what} {:.0}%", row.label, row.value);
    if let Some(f) = row.fable {
        text.push_str(&format!(" · Fable {f:.0}%"));
    }
    text
}

/// The overlaid tooltip as plain text: "3:10 PM\n● nate 46%\n● work 12%".
pub fn hover_tip(time: &str, rows: &[HoverRow]) -> String {
    let mut out = time.to_owned();
    for r in rows {
        out.push_str(&format!("\n● {}", hover_row_text(r)));
    }
    out
}

/// [`hover_tip`] with each dot in its account's color.
fn hover_job(time: &str, rows: &[HoverRow], font: &FontId, text: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = |color| TextFormat {
        font_id: font.clone(),
        color,
        ..TextFormat::default()
    };
    job.append(time, 0.0, format(text));
    for r in rows {
        job.append("\n●", 0.0, format(r.color));
        job.append(&format!(" {}", hover_row_text(r)), 0.0, format(text));
    }
    job
}

/// The overlaid hover: a vertical line at `t` and a dot on each account's
/// value.
fn paint_series_hover(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    rows: &[HoverRow],
    t: DateTime<Utc>,
) {
    let stroke = Stroke::new(1.0, palette.text_muted.gamma_multiply(0.7));
    let x = axes.x_at(t);
    painter.vline(x, axes.plot.y_range(), stroke);
    for r in rows {
        painter.circle(
            axes.pos(t, r.value),
            3.0,
            palette.surface,
            Stroke::new(1.5, r.color),
        );
    }
}

/// The y axis of `plot`: gridlines at 25/50/75 with labels, the dashed 100%
/// cap line, and the baseline. Every chart uses it, so the axes match.
pub fn paint_y_axis(painter: &Painter, plot: Rect, palette: &Palette, font: &FontId) {
    let label_x = plot.left() - 4.0;
    for mark in GRID_MARKS {
        let y = y_in(plot, mark);
        painter.hline(plot.x_range(), y, Stroke::new(1.0, palette.grid));
        painter.text(
            pos2(label_x, y),
            Align2::RIGHT_CENTER,
            format!("{mark:.0}%"),
            font.clone(),
            palette.text_muted,
        );
    }
    let top = y_in(plot, 100.0);
    let cap = Stroke::new(1.0, palette.danger.gamma_multiply(CAP_LINE_ALPHA));
    let ends = (pos2(plot.left(), top), pos2(plot.right(), top));
    paint_dashed(painter, ends.0, ends.1, cap);
    painter.text(
        pos2(label_x, top),
        Align2::RIGHT_CENTER,
        "100%",
        font.clone(),
        palette.text_muted,
    );
    painter.hline(
        plot.x_range(),
        plot.bottom(),
        Stroke::new(1.0, palette.border),
    );
}

/// The chart's ticks along the bottom and its end label ("reset 3:42p") at
/// the right end; tick labels that would collide with it or leave the
/// widget are skipped.
fn paint_time_axis(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    font: &FontId,
    left: f32,
    chart: &Chart,
) {
    let y = axes.plot.bottom() + 3.0;
    let right = axes.plot.right() + CAP_MARK + 2.0;
    let end = chart
        .end_label
        .as_ref()
        .map(|text| painter.layout_no_wrap(text.clone(), font.clone(), palette.text_muted));
    let end_left = end.as_ref().map_or(right, |g| right - g.size().x);
    let tick = Stroke::new(1.0, palette.border);
    for (t, label) in &chart.ticks {
        let x = axes.x_at(*t);
        painter.vline(x, axes.plot.bottom()..=axes.plot.bottom() + 3.0, tick);
        let galley = painter.layout_no_wrap(label.clone(), font.clone(), palette.text_muted);
        let w = galley.size().x;
        let lx = x - w / 2.0;
        if lx >= left && lx + w <= end_left - 6.0 {
            painter.galley(pos2(lx, y), galley, palette.text_muted);
        }
    }
    if let Some(g) = end {
        painter.galley(pos2(end_left, y), g, palette.text_muted);
    }
    for (t, color) in &chart.end_marks {
        let x = axes.x_at(*t);
        painter.vline(
            x,
            axes.plot.bottom() - 4.0..=axes.plot.bottom() + 4.0,
            Stroke::new(2.0, *color),
        );
    }
}

/// One series' fills: the faint bridge fills, the main area per segment
/// (gradient fill), and the Fable share area with a surface-colored
/// separator on top. Returns the main line of each segment (for
/// [`paint_lines`]).
fn paint_fills(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    chart: &Chart,
    s: &SeriesData,
    segs: &[Range<usize>],
    gaps: &[Bridge],
) -> Vec<Vec<Pos2>> {
    let samples = &s.points;
    let baseline = axes.plot.bottom();
    let (top, bottom) = s.area_colors();
    let fable_fill = s.tone(palette.fable.gamma_multiply(FABLE_ALPHA));
    let share = match &s.overlay {
        Overlay::Share(share) => Some(share.as_slice()),
        _ => None,
    };
    let main = |i: usize| axes.pos(samples[i].t, samples[i].value.unwrap_or(0.0));
    for b in gaps {
        let ends = [main(b.from), main(b.to)];
        let scale = chart.bridge_fill;
        painter.add(area_mesh(
            &ends,
            baseline,
            top.gamma_multiply(scale),
            bottom.gamma_multiply(scale),
        ));
        if let Some((Some(a), Some(z))) = share.map(|f| (f[b.from], f[b.to])) {
            let ends = [axes.pos(samples[b.from].t, a), axes.pos(samples[b.to].t, z)];
            let faint = fable_fill.gamma_multiply(scale);
            painter.add(area_mesh(&ends, baseline, faint, faint));
        }
    }
    let mut raw = Vec::new();
    let mut lines = Vec::with_capacity(segs.len());
    for seg in segs {
        // Drawn through the change points, not as whole-percent steps.
        let drawn = through_change_points(&series(samples, seg.clone(), Series::Main));
        raw.clear();
        raw.extend(
            seg.clone()
                .zip(&drawn)
                .map(|(i, v)| axes.pos(samples[i].t, *v)),
        );
        let line = thin_columns(&raw);
        painter.add(area_mesh(&line, baseline, top, bottom));
        if let Some(share) = share {
            for run in present_runs(share, seg.clone()) {
                // Smoothed the same way, and never above the drawn line at
                // any sample (both are straight between samples).
                let values: Vec<_> = run
                    .clone()
                    .map(|i| (samples[i].t, share[i].unwrap_or(0.0)))
                    .collect();
                let layer_v = through_change_points(&values);
                raw.clear();
                raw.extend(
                    run.zip(layer_v)
                        .map(|(i, v)| axes.pos(samples[i].t, v.min(drawn[i - seg.start]))),
                );
                let layer = thin_columns(&raw);
                if layer.len() >= 2 {
                    painter.add(area_mesh(&layer, baseline, fable_fill, fable_fill));
                    painter.add(Shape::line(layer, Stroke::new(1.0, palette.surface)));
                }
            }
        }
        lines.push(line);
    }
    lines
}

/// One series' lines over the fills: the dotted bridge lines, the main
/// `lines` (from [`paint_fills`]), then the Fable line.
fn paint_lines(
    painter: &Painter,
    axes: &Axes,
    chart: &Chart,
    s: &SeriesData,
    gaps: &[Bridge],
    lines: Vec<Vec<Pos2>>,
) {
    let samples = &s.points;
    let color = s.tone(s.color);
    let main = |i: usize| axes.pos(samples[i].t, samples[i].value.unwrap_or(0.0));
    let dotted = Stroke::new(1.5, color.gamma_multiply(BRIDGE_LINE_ALPHA));
    for b in gaps {
        for seg in dashes(main(b.from), main(b.to), DOT, DOT_GAP) {
            painter.line_segment(seg, dotted);
        }
    }
    for line in lines {
        paint_polyline(painter, line, color);
    }
    if let Overlay::Line { color, dashed } = s.overlay {
        paint_overlay_line(painter, axes, chart, s, s.tone(color), dashed);
    }
}

/// The overlay meter as a line in `color` (dashed or solid), with its own
/// breaks and dotted bridges.
fn paint_overlay_line(
    painter: &Painter,
    axes: &Axes,
    chart: &Chart,
    s: &SeriesData,
    color: Color32,
    dashed: bool,
) {
    let samples = &s.points;
    let segs = segments_of(samples, chart.gap_break, Series::Overlay);
    let gaps = bridges_of(samples, &segs, Series::Overlay);
    let at = |i: usize| axes.pos(samples[i].t, samples[i].overlay.unwrap_or(0.0));
    let dotted = Stroke::new(1.5, color.gamma_multiply(BRIDGE_LINE_ALPHA));
    for b in &gaps {
        for seg in dashes(at(b.from), at(b.to), DOT, DOT_GAP) {
            painter.line_segment(seg, dotted);
        }
    }
    for seg in segs {
        let drawn = through_change_points(&series(samples, seg.clone(), Series::Overlay));
        let raw: Vec<Pos2> = seg
            .zip(drawn)
            .map(|(i, v)| axes.pos(samples[i].t, v))
            .collect();
        let line = thin_columns(&raw);
        if dashed && line.len() >= 2 {
            for piece in dashed_polyline(&line, DASH, DASH_GAP) {
                painter.add(Shape::line(piece, PathStroke::new(1.5, color)));
            }
        } else {
            paint_polyline(painter, line, color);
        }
    }
}

/// The dashes along the polyline through `points`, `dash` long and `gap`
/// apart, the pattern running on across its vertices (so a line of many
/// short steps still reads as dashed). Each dash is a polyline of its own.
pub fn dashed_polyline(points: &[Pos2], dash: f32, gap: f32) -> Vec<Vec<Pos2>> {
    let mut out: Vec<Vec<Pos2>> = Vec::new();
    if points.len() < 2 || dash <= 0.0 {
        return out;
    }
    let gap = gap.max(0.0);
    // Distance into the current dash (on) or gap (off).
    let (mut on, mut into) = (true, 0.0_f32);
    let mut current: Vec<Pos2> = vec![points[0]];
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let length = a.distance(b);
        if length <= 0.0 {
            continue;
        }
        let dir = (b - a) / length;
        let mut d = 0.0;
        while d < length {
            let left = if on { dash } else { gap } - into;
            let step = left.min(length - d);
            d += step;
            into += step;
            let p = a + dir * d;
            if on {
                current.push(p);
            }
            if into >= if on { dash } else { gap } {
                if on {
                    out.push(std::mem::take(&mut current));
                } else {
                    current = vec![p];
                }
                on = !on;
                into = 0.0;
            }
        }
    }
    if on && current.len() >= 2 {
        out.push(current);
    }
    out
}

/// A solid 2 pt line, or a dot for a lone sample.
fn paint_polyline(painter: &Painter, line: Vec<Pos2>, color: Color32) {
    match line[..] {
        [] => {}
        // A lone sample between gaps or breaks: a dot, not a line.
        [p] => {
            painter.circle_filled(p, LONE_RADIUS, color);
        }
        _ => {
            painter.add(Shape::line(line, PathStroke::new(2.0, color)));
        }
    }
}

/// What [`paint_now`] draws for one meter.
struct Ends {
    /// The dot's % (eased).
    pct: f64,
    projection: Option<Projection>,
    label: Option<EndLabel>,
    /// Share of the projection drawn in so far, 0..=1.
    share: f32,
    color: Color32,
    cap_color: Color32,
}

/// Opacity of the projection's end mark and label for a draw-in `share`:
/// they fade in over the last 30% of the line.
pub fn end_fade(share: f32) -> f32 {
    ((share - 0.7) / 0.3).clamp(0.0, 1.0)
}

/// The text of an end label: "cap 4:10p" (or "cap Mon 10:45a"), "safe",
/// "at cap" (the plot shows only the mark for it; see [`label_shown`]).
pub fn end_label_text<Tz: TimeZone>(label: EndLabel, style: TimeStyle, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    match label {
        EndLabel::Cap(t) => format!("cap {}", style.short(t, tz)),
        EndLabel::Safe => "safe".to_owned(),
        EndLabel::AtCap => "at cap".to_owned(),
    }
}

/// Whether an end label's text goes in the plot. "At cap" never does (a ✕
/// on the 100% line marks it, and the chip reads 100%), and
/// a "cap …" or "safe" label only when the projection runs far enough
/// right to hold it (`run` from now to its end, `room` the label's width
/// plus its clearance); otherwise only the ✕ (or nothing, for "safe")
/// shows, so an end minutes away doesn't crowd the plot's corner.
pub fn label_shown(label: EndLabel, run: f32, room: f32) -> bool {
    match label {
        EndLabel::AtCap => false,
        EndLabel::Cap(_) | EndLabel::Safe => run >= room,
    }
}

/// The now marker, each meter's dashed projection with its cap ✕ and
/// label (or "safe" / "at cap"), and the dots at the current %. Labels are
/// placed in turn, each clear of the ✕ marks and the labels before it.
fn paint_now(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    chart: &Chart,
    ends: &[Ends],
    font: &FontId,
) {
    let x = axes.x_at(chart.now);
    painter.vline(
        x,
        axes.plot.y_range(),
        Stroke::new(1.0, palette.text_muted.gamma_multiply(0.6)),
    );
    let bounds = Rect::from_min_max(
        pos2(axes.plot.left(), painter.clip_rect().top()),
        pos2(painter.clip_rect().right(), axes.plot.bottom()),
    );
    let from = |e: &Ends| axes.pos(chart.now, e.pct);
    let to = |e: &Ends| {
        e.projection
            .map_or(from(e), |p| axes.pos(p.end_t, p.end_pct))
    };
    // Where a meter's ✕ goes: the cap, or on the 100% line at now when it
    // is already there.
    let mark_at = |e: &Ends| match e.label {
        Some(EndLabel::Cap(_)) => Some(to(e)),
        Some(EndLabel::AtCap) => Some(axes.pos(chart.now, 100.0)),
        _ => None,
    };
    // Every ✕ and every now dot is a keep-out for every label.
    let mut avoid: Vec<Rect> = ends
        .iter()
        .filter_map(mark_at)
        .map(|p| Rect::from_center_size(p, Vec2::splat(2.0 * CAP_KEEP_OUT)))
        .chain(
            ends.iter()
                .map(|e| Rect::from_center_size(from(e), Vec2::splat(2.0 * (DOT_RADIUS + 1.5)))),
        )
        .collect();
    for e in ends {
        let (from, to) = (from(e), to(e));
        let fade = if e.projection.is_some() {
            end_fade(e.share)
        } else {
            1.0
        };
        let placed = e.label.and_then(|kind| {
            let (color, clearance, keep_out) = match kind {
                EndLabel::Cap(_) => (e.cap_color, CAP_LABEL_CLEARANCE, CAP_KEEP_OUT),
                EndLabel::Safe => (palette.good, SAFE_LABEL_CLEARANCE, SAFE_KEEP_OUT),
                EndLabel::AtCap => (palette.danger, SAFE_LABEL_CLEARANCE, DOT_RADIUS + 1.0),
            };
            let text = end_label_text(kind, chart.style, &Local);
            let galley = painter.layout_no_wrap(text, font.clone(), color);
            if !label_shown(kind, to.x - from.x, galley.size().x + clearance) {
                return None;
            }
            let own: Vec<Rect> = avoid.iter().copied().filter(|r| !r.contains(to)).collect();
            let rect = place_end_label(from, to, galley.size(), clearance, keep_out, bounds, &own);
            avoid.push(rect.expand(LABEL_PAD));
            Some((galley, rect, color))
        });
        if e.projection.is_some() {
            let stop = placed
                .as_ref()
                .map_or(to, |(_, rect, _)| dash_end(from, to, *rect));
            paint_dashed(
                painter,
                from,
                from + (stop - from) * e.share,
                Stroke::new(1.5, e.color),
            );
        }
        if let Some(m) = mark_at(e).filter(|_| fade > 0.0) {
            let stroke = Stroke::new(2.0, e.cap_color.gamma_multiply(fade));
            let (a, b) = (vec2(CAP_MARK, CAP_MARK), vec2(CAP_MARK, -CAP_MARK));
            painter.line_segment([m - a, m + a], stroke);
            painter.line_segment([m - b, m + b], stroke);
        }
        if let Some((galley, rect, color)) = placed.filter(|_| fade > 0.0) {
            painter.rect_filled(
                rect.expand(LABEL_PAD),
                3.0,
                palette.surface.gamma_multiply(fade),
            );
            painter.galley_with_override_text_color(rect.min, galley, color.gamma_multiply(fade));
        }
    }
    for e in ends {
        painter.circle(
            from(e),
            DOT_RADIUS,
            e.color,
            Stroke::new(1.5, palette.surface),
        );
    }
}

/// Crosshair for the hover `target` and its tooltip text: a full
/// crosshair on a sample or the projection, a vertical line over a gap or
/// an empty spot.
#[allow(clippy::too_many_arguments)]
fn paint_hover(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    chart: &Chart,
    series: &SeriesData,
    drawn: &[Option<f64>],
    target: HoverTarget,
    pointer_x: f32,
) -> String {
    let samples = &series.points;
    let style = chart.style;
    let time = |t: DateTime<Utc>| style.tip(t, &Local);
    let point = |t: DateTime<Utc>, pct: f64| Some(axes.pos(t, pct));
    let (x, point, tip) = match target {
        HoverTarget::Projected { t, pct } => {
            // Each meter at `t`: on its projection, else held at its %.
            let at = |kind: LineKind| {
                series.lines.iter().find(|l| l.kind == kind).map(|l| {
                    l.projection
                        .and_then(|p| projected_pct(chart.now, l.pct, &p, t))
                        .unwrap_or(l.pct)
                })
            };
            let tip = match style {
                TimeStyle::Clock => projected_tip(&time(t), pct),
                TimeStyle::Weekday => {
                    week_projected_tip(&time(t), at(LineKind::Main), at(LineKind::Overlay))
                }
            };
            (axes.x_at(t), point(t, pct), tip)
        }
        HoverTarget::Sample(i) => {
            let s = &samples[i];
            let pct = s.value.unwrap_or(0.0);
            // The tip reads the real sample; the dot sits on the drawn line.
            let on_line = drawn.get(i).copied().flatten().unwrap_or(pct);
            let tip = match (&series.overlay, style) {
                (Overlay::Share(share), _) => sample_tip(&time(s.t), pct, share[i]),
                (Overlay::Line { .. }, TimeStyle::Weekday) => {
                    week_sample_tip(&time(s.t), pct, s.overlay)
                }
                (_, TimeStyle::Weekday) => week_sample_tip(&time(s.t), pct, None),
                (_, TimeStyle::Clock) => sample_tip(&time(s.t), pct, None),
            };
            (axes.x_at(s.t), point(s.t, on_line), tip)
        }
        HoverTarget::Gap(b) => {
            let (from, to) = (samples[b.from].t, samples[b.to].t);
            let tip = match style {
                TimeStyle::Clock => gap_tip_in(from, to, &Local),
                TimeStyle::Weekday => format!("no samples {} – {}", time(from), time(to)),
            };
            (pointer_x, None, tip)
        }
        HoverTarget::Time(t) => (pointer_x, None, time(t)),
    };
    let stroke = Stroke::new(1.0, palette.text_muted.gamma_multiply(0.7));
    if let Some(p) = point {
        painter.vline(p.x, axes.plot.y_range(), stroke);
        painter.hline(axes.plot.x_range(), p.y, stroke);
        painter.circle(p, 3.0, palette.surface, Stroke::new(1.5, palette.text));
    } else {
        painter.vline(x, axes.plot.y_range(), stroke);
    }
    tip
}

/// A dashed line from `a` to `b` in the timeline's dash pattern.
pub fn paint_dashed(painter: &Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    for seg in dashes(a, b, DASH, DASH_GAP) {
        painter.line_segment(seg, stroke);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use eframe::egui::{pos2, Rect};

    // --- the 5-hour chart's window -------------------------------------------

    /// A dashboard polled at `at` with `pct` and `resets_at`, its window as
    /// the poller sets it, and two samples in it.
    fn five_dash(at: DateTime<Utc>, pct: f64, resets_at: Option<DateTime<Utc>>) -> Dashboard {
        use crate::model::{Snapshot, UsageWindow};
        let end = resets_at.unwrap_or(at);
        let sample = |m: i64, v: f64| WindowSample {
            t: at - Duration::minutes(m),
            five_hour: Some(v),
            fable: None,
        };
        Dashboard {
            snapshot: Snapshot {
                fetched_at: at,
                windows: vec![UsageWindow {
                    label: "5-hour".to_owned(),
                    utilization: Some(pct),
                    resets_at,
                }],
                extra: None,
                breakdown: None,
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            seven_day_recent: Vec::new(),
            fable_recent: Vec::new(),
            burners: Vec::new(),
            window_start: end - Duration::hours(5),
            window_end: end,
            window_samples: vec![sample(10, pct / 2.0), sample(0, pct)],
            week_start: at - Duration::days(7),
            week_end: at,
            week_samples: Vec::new(),
            persist_error: None,
            plan: None,
            token_expires_at: None,
        }
    }

    fn t_at(h: u32, m: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 10, 12, h, m, 0).unwrap()
    }

    fn assert_idle(c: &Chart) {
        assert!(c.series[0].points.is_empty(), "{:?}", c.series[0].points);
        assert!(c.series[0].lines.is_empty(), "no now dot or projection");
        assert!(c.ticks.is_empty(), "no times on an idle axis");
        assert_eq!(c.end_label, None);
        assert_eq!(c.note, Some(IDLE_NOTE));
    }

    #[test]
    fn five_hour_chart_idle_after_a_reset_is_empty() {
        let mut d = five_dash(t_at(14, 53), 0.0, None);
        d.window_samples.clear(); // as the poller leaves it
        assert_idle(&five_hour_chart(&d, t_at(14, 53), &crate::ui::theme::DARK));
    }

    #[test]
    fn five_hour_chart_idle_once_the_reset_passes_between_polls() {
        let d = five_dash(t_at(14, 10), 80.0, Some(t_at(14, 20)));
        let live = five_hour_chart(&d, t_at(14, 15), &crate::ui::theme::DARK);
        assert_eq!(live.series[0].points.len(), 2);
        assert_eq!(live.series[0].lines.len(), 1);
        assert!(live.end_label.as_deref().unwrap().starts_with("reset "));
        assert_eq!(live.note, None);
        // 14:21: past the chart's end, before the next poll.
        assert_idle(&five_hour_chart(&d, t_at(14, 21), &crate::ui::theme::DARK));
        assert_idle(&five_hour_chart(&d, t_at(14, 20), &crate::ui::theme::DARK));
    }

    #[test]
    fn five_hour_chart_trailing_without_a_reset_ends_now() {
        let d = five_dash(t_at(14, 53), 6.0, None);
        let c = five_hour_chart(&d, t_at(14, 53), &crate::ui::theme::DARK);
        assert_eq!(c.series[0].points.len(), 2);
        assert_eq!(c.end_label.as_deref(), Some("now"));
        assert_eq!(c.series[0].lines.len(), 1);
        assert_eq!(c.series[0].lines[0].label, None, "no safe or cap claimed");
        assert_eq!(c.note, None);
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 10, 17, 0).unwrap()
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        t0() + Duration::minutes(minutes)
    }

    fn s(m: i64, five: Option<f64>) -> Point {
        Point {
            t: at(m),
            value: five,
            overlay: None,
        }
    }

    fn gap() -> Duration {
        Duration::minutes(GAP_BREAK_MINUTES)
    }

    /// 300 px wide (one px per minute), 100 px tall (one px per %), at (10, 20).
    fn axes() -> Axes {
        Axes {
            plot: Rect::from_min_max(pos2(10.0, 20.0), pos2(310.0, 120.0)),
            start: at(0),
            end: at(300),
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    // --- mapping ------------------------------------------------------------

    #[test]
    fn time_maps_linearly_across_the_window() {
        let a = axes();
        assert!(close(a.x_at(at(0)), 10.0));
        assert!(close(a.x_at(at(150)), 160.0));
        assert!(close(a.x_at(at(300)), 310.0));
        // Outside the window: extrapolated, the painter clips.
        assert!(close(a.x_at(at(-30)), -20.0));
    }

    #[test]
    fn x_maps_back_to_time() {
        let a = axes();
        assert_eq!(a.t_at(160.0), at(150));
        assert_eq!(a.t_at(10.0), at(0));
    }

    #[test]
    fn pct_maps_bottom_up_and_clamps() {
        let a = axes();
        assert!(close(a.y_at(0.0), 120.0));
        assert!(close(a.y_at(25.0), 95.0));
        assert!(close(a.y_at(100.0), 20.0));
        assert!(close(a.y_at(130.0), 20.0));
        assert!(close(a.y_at(-5.0), 120.0));
    }

    #[test]
    fn empty_window_maps_to_the_left_edge() {
        let a = Axes {
            end: at(0),
            ..axes()
        };
        assert!(close(a.x_at(at(10)), 10.0));
        assert_eq!(a.t_at(200.0), at(0));
    }

    // --- segmenting ---------------------------------------------------------

    #[test]
    fn steady_samples_are_one_segment() {
        let samples = [s(0, Some(1.0)), s(5, Some(2.0)), s(10, Some(4.0))];
        assert_eq!(segments(&samples, gap()), vec![0..3]);
    }

    #[test]
    fn none_values_break_and_are_skipped() {
        let samples = [
            s(0, Some(1.0)),
            s(1, Some(2.0)),
            s(2, None),
            s(3, Some(3.0)),
            s(4, Some(4.0)),
        ];
        assert_eq!(segments(&samples, gap()), vec![0..2, 3..5]);
    }

    #[test]
    fn gaps_over_ten_minutes_break() {
        let samples = [
            s(0, Some(1.0)),
            s(10, Some(2.0)), // exactly 10 min: still joined
            s(21, Some(3.0)), // 11 min: break
        ];
        assert_eq!(segments(&samples, gap()), vec![0..2, 2..3]);
    }

    #[test]
    fn drops_over_five_points_are_resets() {
        let samples = [
            s(0, Some(40.0)),
            s(1, Some(35.0)), // drop of exactly 5: not a reset
            s(2, Some(29.0)), // drop of 6: reset
            s(3, Some(30.0)),
        ];
        assert_eq!(segments(&samples, gap()), vec![0..2, 2..4]);
    }

    #[test]
    fn single_and_no_samples() {
        assert!(segments(&[], gap()).is_empty());
        assert!(segments(&[s(0, None)], gap()).is_empty());
        assert_eq!(segments(&[s(0, Some(3.0))], gap()), vec![0..1]);
    }

    #[test]
    fn present_runs_split_on_none_within_a_range() {
        let values = [Some(1.0), None, Some(2.0), Some(3.0), None, Some(4.0)];
        assert_eq!(present_runs(&values, 0..6), vec![0..1, 2..4, 5..6]);
        assert_eq!(present_runs(&values, 2..5), vec![2..4]);
        assert!(present_runs(&values, 1..2).is_empty());
    }

    #[test]
    fn bridges_span_time_gaps() {
        let samples = [
            s(0, Some(1.0)),
            s(5, Some(2.0)),
            s(30, Some(4.0)), // 25 min gap: bridged
            s(35, Some(5.0)),
            s(80, Some(6.0)), // 45 min gap to a lone sample: bridged
        ];
        let segs = segments(&samples, gap());
        assert_eq!(segs, vec![0..2, 2..4, 4..5]);
        assert_eq!(
            bridges(&samples, &segs),
            vec![Bridge { from: 1, to: 2 }, Bridge { from: 3, to: 4 }]
        );
    }

    #[test]
    fn bridges_not_across_resets_or_missing_values() {
        let samples = [
            s(0, Some(40.0)),
            s(30, Some(2.0)), // gap and a reset: no bridge
            s(31, Some(3.0)),
            s(32, None), // missing value: no bridge
            s(33, Some(4.0)),
            s(34, Some(5.0)),
            s(60, Some(1.0)), // gap with a drop of exactly 4: bridged
        ];
        let segs = segments(&samples, gap());
        assert_eq!(segs, vec![0..1, 1..3, 4..6, 6..7]);
        assert_eq!(bridges(&samples, &segs), vec![Bridge { from: 5, to: 6 }]);
    }

    #[test]
    fn bridges_need_two_segments() {
        assert!(bridges(&[], &[]).is_empty());
        let samples = [s(0, Some(1.0)), s(1, Some(2.0))];
        assert!(bridges(&samples, &segments(&samples, gap())).is_empty());
    }

    // --- thinning -----------------------------------------------------------

    #[test]
    fn sparse_points_are_kept() {
        let points = [pos2(0.5, 10.0), pos2(3.0, 20.0), pos2(9.9, 5.0)];
        assert_eq!(thin_columns(&points), points.to_vec());
    }

    #[test]
    fn dense_columns_keep_their_min_and_max_in_order() {
        let points = [
            pos2(1.1, 50.0),
            pos2(1.3, 40.0), // column 1 max (top)
            pos2(1.5, 60.0), // column 1 min (bottom)
            pos2(1.7, 55.0),
            pos2(2.2, 30.0),
            pos2(2.6, 30.0), // flat column: one point
        ];
        assert_eq!(
            thin_columns(&points),
            vec![pos2(1.3, 40.0), pos2(1.5, 60.0), pos2(2.2, 30.0)]
        );
    }

    #[test]
    fn thinning_never_exceeds_two_points_per_column() {
        let points: Vec<_> = (0..1000)
            .map(|i| pos2(i as f32 * 0.1, (i % 7) as f32))
            .collect();
        let thin = thin_columns(&points);
        let mut per_column = std::collections::HashMap::new();
        for p in &thin {
            *per_column.entry(p.x.floor() as i32).or_insert(0) += 1;
        }
        assert_eq!(per_column.len(), 100, "every column keeps a point");
        assert!(per_column.values().all(|&n| n <= 2), "{per_column:?}");
        assert!(thin.windows(2).all(|w| w[0].x <= w[1].x));
    }

    // --- mesh ---------------------------------------------------------------

    #[test]
    fn area_mesh_is_a_strip_to_the_baseline() {
        let top = Color32::from_rgba_premultiplied(100, 0, 0, 100);
        let bottom = Color32::from_rgba_premultiplied(10, 0, 0, 10);
        let points = [pos2(0.0, 10.0), pos2(5.0, 20.0), pos2(10.0, 15.0)];
        let mesh = area_mesh(&points, 50.0, top, bottom);
        assert_eq!(mesh.vertices.len(), 6);
        assert_eq!(mesh.indices.len(), 2 * 3 * 2);
        for (i, p) in points.iter().enumerate() {
            let (line, base) = (&mesh.vertices[2 * i], &mesh.vertices[2 * i + 1]);
            assert_eq!((line.pos, line.color), (*p, top));
            assert_eq!((base.pos, base.color), (pos2(p.x, 50.0), bottom));
        }
        // Two triangles per column pair: (line, base, next line) and
        // (base, next base, next line).
        assert_eq!(mesh.indices, [0, 1, 2, 1, 3, 2, 2, 3, 4, 3, 5, 4]);
        // Every triangle winds the same way (no flipped faces).
        let signed_area = |t: &[u32]| {
            let [a, b, c] = [0, 1, 2].map(|k| mesh.vertices[t[k] as usize].pos);
            (b - a).x * (c - a).y - (b - a).y * (c - a).x
        };
        let areas: Vec<f32> = mesh.indices.chunks(3).map(signed_area).collect();
        assert!(
            areas.iter().all(|&a| a < 0.0) || areas.iter().all(|&a| a > 0.0),
            "{areas:?}"
        );
    }

    #[test]
    fn area_mesh_needs_two_points() {
        let c = Color32::WHITE;
        assert!(area_mesh(&[], 50.0, c, c).is_empty());
        assert!(area_mesh(&[pos2(1.0, 1.0)], 50.0, c, c).is_empty());
    }

    // --- dashes -------------------------------------------------------------

    #[test]
    fn dashes_alternate_and_truncate_the_last() {
        let d = dashes(pos2(0.0, 0.0), pos2(20.0, 0.0), 5.0, 3.0);
        assert_eq!(
            d,
            vec![
                [pos2(0.0, 0.0), pos2(5.0, 0.0)],
                [pos2(8.0, 0.0), pos2(13.0, 0.0)],
                [pos2(16.0, 0.0), pos2(20.0, 0.0)],
            ]
        );
    }

    #[test]
    fn dashes_follow_diagonals() {
        let d = dashes(pos2(0.0, 0.0), pos2(30.0, 40.0), 10.0, 10.0);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0], [pos2(0.0, 0.0), pos2(6.0, 8.0)]);
        assert_eq!(d[2][1], pos2(30.0, 40.0));
    }

    #[test]
    fn dashes_degenerate_inputs() {
        assert!(dashes(pos2(1.0, 1.0), pos2(1.0, 1.0), 5.0, 3.0).is_empty());
        let solid = dashes(pos2(0.0, 0.0), pos2(9.0, 0.0), 0.0, 3.0);
        assert_eq!(solid, vec![[pos2(0.0, 0.0), pos2(9.0, 0.0)]]);
    }

    // --- ticks and labels ---------------------------------------------------

    fn labels(ticks: &[(DateTime<Utc>, String)]) -> Vec<&str> {
        ticks.iter().map(|(_, l)| l.as_str()).collect()
    }

    #[test]
    fn hour_ticks_on_local_hours_in_twelve_hour_form() {
        // 10:17 to 15:17 UTC.
        let ticks = hour_ticks_in(at(0), at(300), &Utc);
        assert_eq!(labels(&ticks), ["11", "12", "1", "2", "3"]);
        assert_eq!(
            ticks[0].0,
            Utc.with_ymd_and_hms(2026, 10, 11, 11, 0, 0).unwrap()
        );
    }

    #[test]
    fn hour_ticks_include_window_edges_on_the_hour() {
        let start = Utc.with_ymd_and_hms(2026, 10, 11, 10, 0, 0).unwrap();
        let ticks = hour_ticks_in(start, start + Duration::hours(5), &Utc);
        assert_eq!(labels(&ticks), ["10", "11", "12", "1", "2", "3"]);
    }

    #[test]
    fn hour_ticks_with_half_hour_offsets() {
        // UTC+5:30: 10:17 UTC is 15:47 local; local hours fall on UTC :30.
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        let ticks = hour_ticks_in(at(0), at(300), &ist);
        assert_eq!(labels(&ticks), ["4", "5", "6", "7", "8"]);
        assert_eq!(
            ticks[0].0,
            Utc.with_ymd_and_hms(2026, 10, 11, 10, 30, 0).unwrap()
        );
        // UTC-3:30: 10:17 UTC is 6:47 local.
        let nst = FixedOffset::west_opt(3 * 3600 + 1800).unwrap();
        let ticks = hour_ticks_in(at(0), at(300), &nst);
        assert_eq!(labels(&ticks), ["7", "8", "9", "10", "11"]);
        assert_eq!(
            ticks[0].0,
            Utc.with_ymd_and_hms(2026, 10, 11, 10, 30, 0).unwrap()
        );
    }

    #[test]
    fn hour_ticks_empty_or_inverted_window() {
        assert!(hour_ticks_in(at(0), at(30), &Utc).is_empty());
        assert!(hour_ticks_in(at(300), at(0), &Utc).is_empty());
    }

    #[test]
    fn short_and_tooltip_times() {
        let t = Utc.with_ymd_and_hms(2026, 10, 11, 15, 42, 9).unwrap();
        assert_eq!(short_time_in(t, &Utc), "3:42p");
        let morning = Utc.with_ymd_and_hms(2026, 10, 11, 0, 5, 0).unwrap();
        assert_eq!(short_time_in(morning, &Utc), "12:05a");
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        assert_eq!(short_time_in(t, &ist), "9:12p");
    }

    #[test]
    fn window_span_subtitle() {
        let t = |h, m| Utc.with_ymd_and_hms(2026, 10, 11, h, m, 0).unwrap();
        assert_eq!(
            window_span_in(t(18, 10), t(23, 10), true, &Utc).as_deref(),
            Some("6:10p – 11:10p")
        );
        assert_eq!(
            window_span_in(t(9, 30), t(14, 30), true, &Utc).as_deref(),
            Some("9:30a – 2:30p")
        );
        // An unknown reset: the end is only assumed, so no span.
        assert_eq!(window_span_in(t(18, 10), t(23, 10), false, &Utc), None);
    }

    #[test]
    fn plot_rect_insets() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 180.0));
        let plot = plot_rect(rect, 30.0, 11.5);
        assert_eq!(plot.left(), 36.0);
        assert_eq!(plot.top(), 11.5);
        assert_eq!(plot.right(), 300.0 - CAP_MARK - 2.0);
        assert_eq!(plot.bottom(), 180.0 - 11.5 - 6.0);
        // A small font still leaves room for the ✕ at 100%.
        assert_eq!(plot_rect(rect, 30.0, 4.0).top(), CAP_MARK + 2.0);
        assert_eq!(y_in(plot, 100.0), plot.top());
        assert_eq!(y_in(plot, 0.0), plot.bottom());
        assert_eq!(y_in(plot, 150.0), plot.top());
    }

    #[test]
    fn gap_tips() {
        let t = |h, m| Utc.with_ymd_and_hms(2026, 10, 11, h, m, 0).unwrap();
        assert_eq!(
            gap_tip_in(t(13, 10), t(13, 45), &Utc),
            "no samples 1:10–1:45 PM"
        );
        assert_eq!(
            gap_tip_in(t(11, 50), t(12, 20), &Utc),
            "no samples 11:50 AM–12:20 PM"
        );
    }

    // --- end label --------------------------------------------------------

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
    }

    #[test]
    fn end_mark_fades_in_over_the_last_of_the_draw_in() {
        assert_eq!(end_fade(0.0), 0.0);
        assert_eq!(end_fade(0.7), 0.0);
        assert!((end_fade(0.85) - 0.5).abs() < 1e-5);
        assert_eq!(end_fade(1.0), 1.0);
    }

    #[test]
    fn segment_entry_fraction() {
        let r = rect(40.0, 40.0, 60.0, 60.0);
        let t = segment_entry(pos2(0.0, 50.0), pos2(100.0, 50.0), r).unwrap();
        assert!((t - 0.4).abs() < 1e-6, "{t}");
        assert_eq!(
            segment_entry(pos2(50.0, 50.0), pos2(100.0, 50.0), r),
            Some(0.0)
        );
        assert_eq!(segment_entry(pos2(0.0, 0.0), pos2(100.0, 0.0), r), None);
        // Stops short of the rect.
        assert_eq!(segment_entry(pos2(0.0, 50.0), pos2(30.0, 50.0), r), None);
        // A point segment.
        assert_eq!(segment_entry(pos2(1.0, 1.0), pos2(1.0, 1.0), r), None);
    }

    const LABEL: Vec2 = vec2(30.0, 10.0);

    #[test]
    fn end_label_above_left_of_a_rising_line() {
        let bounds = rect(0.0, 0.0, 200.0, 120.0);
        let got = place_end_label(
            pos2(0.0, 100.0),
            pos2(100.0, 40.0),
            LABEL,
            4.0,
            2.0,
            bounds,
            &[],
        );
        assert_eq!(got, rect(70.0, 26.0, 100.0, 36.0));
    }

    #[test]
    fn end_label_above_right_when_the_top_is_tight() {
        let bounds = rect(0.0, 0.0, 200.0, 120.0);
        let (from, end) = (pos2(0.0, 100.0), pos2(100.0, 8.0));
        let got = place_end_label(from, end, LABEL, 4.0, 2.0, bounds, &[]);
        assert_eq!(got, rect(104.0, 0.0, 134.0, 10.0));
        assert_eq!(segment_entry(from, end, got.expand(LABEL_PAD)), None);
    }

    #[test]
    fn end_label_flat_line_sits_above_it() {
        let bounds = rect(0.0, 0.0, 102.0, 120.0);
        let (from, end) = (pos2(50.0, 60.0), pos2(100.0, 60.0));
        let got = place_end_label(from, end, LABEL, 4.0, 2.0, bounds, &[]);
        assert_eq!(got, rect(70.0, 46.0, 100.0, 56.0));
    }

    /// Real proportions: the cap point sits on the 100% line, 11.5 pt
    /// under the widget top; "cap 6:08p" is about 60 x 14.
    fn assert_cap_label_clear(bounds: Rect, end: Pos2, slope: f32) {
        let size = vec2(60.0, 14.0);
        let dx = (200.0_f32).min((bounds.bottom() - 2.0 - end.y) / slope);
        let from = pos2(end.x - dx, end.y + slope * dx);
        let got = place_end_label(
            from,
            end,
            size,
            CAP_LABEL_CLEARANCE,
            CAP_KEEP_OUT,
            bounds,
            &[],
        );
        let case = format!("end {end:?} slope {slope} -> {got:?}");
        assert!(bounds.contains_rect(got), "outside: {case}");
        let keep = Rect::from_center_size(end, Vec2::splat(2.0 * CAP_KEEP_OUT));
        assert!(
            !overlaps(got.expand(LABEL_PAD), keep),
            "covers the cap mark: {case}"
        );
        assert_eq!(
            segment_entry(from, end, got.expand(LABEL_PAD)),
            None,
            "on the line: {case}"
        );
        // The projection starts at now, so the now marker is the vertical
        // line through `from`.
        let padded = got.expand(LABEL_PAD);
        assert!(
            !(padded.left() < from.x && from.x < padded.right()),
            "on the now line: {case}"
        );
    }

    #[test]
    fn cap_label_never_covers_the_mark_or_the_line() {
        for right in [360.0, 300.0] {
            let bounds = rect(40.0, 0.0, right, 150.0);
            let plot_right = right - CAP_MARK - 2.0;
            for back in [0.0, 10.0, 20.0, 30.0, 40.0, 150.0] {
                for slope in [0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0] {
                    assert_cap_label_clear(bounds, pos2(plot_right - back, 11.5), slope);
                }
            }
        }
    }

    #[test]
    fn end_label_cornered_falls_back_clear_of_the_mark() {
        // A widget too small for any clear spot: the label still avoids the
        // mark, and the dash stops short of the label.
        let bounds = rect(0.0, 0.0, 70.0, 40.0);
        let (from, end) = (pos2(0.0, 30.0), pos2(64.0, 11.5));
        let got = place_end_label(from, end, vec2(60.0, 14.0), 8.0, 5.0, bounds, &[]);
        let keep = Rect::from_center_size(end, Vec2::splat(10.0));
        assert!(!overlaps(got.expand(LABEL_PAD), keep), "{got:?}");
        let stop = dash_end(from, end, got);
        assert_eq!(segment_entry(from, stop, got.expand(LABEL_PAD)), None);
    }

    #[test]
    fn end_labels_need_a_known_reset() {
        let now = t0();
        let cap = Projection {
            end_t: now + Duration::minutes(30),
            end_pct: 100.0,
            cap_at: Some(now + Duration::minutes(30)),
        };
        let safe = Projection {
            end_t: now + Duration::minutes(90),
            end_pct: 80.0,
            cap_at: None,
        };
        assert_eq!(end_label(true, &cap, now), Some(EndLabel::Cap(cap.end_t)));
        assert_eq!(end_label(true, &safe, now), Some(EndLabel::Safe));
        assert_eq!(end_label(false, &cap, now), None);
        assert_eq!(end_label(false, &safe, now), None);
        // A zero-length line (window over) gets no label.
        let over = Projection {
            end_t: now,
            end_pct: 50.0,
            cap_at: None,
        };
        assert_eq!(end_label(true, &over, now), None);
    }

    #[test]
    fn dash_runs_to_the_end_when_clear() {
        let label = rect(70.0, 26.0, 100.0, 36.0);
        assert_eq!(
            dash_end(pos2(0.0, 100.0), pos2(100.0, 40.0), label),
            pos2(100.0, 40.0)
        );
    }

    // --- hover --------------------------------------------------------------

    #[test]
    fn nearest_sample_skips_missing_values() {
        let samples = [
            s(0, Some(1.0)),
            s(10, None),
            s(20, Some(3.0)),
            s(30, Some(4.0)),
        ];
        assert_eq!(nearest_sample(&samples, at(-50)), Some(0));
        assert_eq!(nearest_sample(&samples, at(9)), Some(0));
        assert_eq!(
            nearest_sample(&samples, at(11)),
            Some(2),
            "None at 10 skipped"
        );
        assert_eq!(nearest_sample(&samples, at(26)), Some(3));
        assert_eq!(nearest_sample(&samples, at(500)), Some(3));
        assert_eq!(nearest_sample(&[s(0, None)], at(0)), None);
        assert_eq!(nearest_sample(&[], at(0)), None);
    }

    #[test]
    fn hover_snaps_within_snap_px_then_gaps_then_time() {
        // One px per minute.
        let a = axes();
        let samples = [s(0, Some(1.0)), s(10, Some(2.0)), s(60, Some(5.0))];
        let gaps = bridges(&samples, &segments(&samples, gap()));
        assert_eq!(gaps, vec![Bridge { from: 1, to: 2 }]);
        let now = at(100);
        let x = |m: f32| a.plot.left() + m;
        let target = |m| hover_target(&a, &samples, &gaps, now, None, x(m));
        assert_eq!(target(3.0), HoverTarget::Sample(0));
        assert_eq!(
            target(10.0 + SNAP_PX),
            HoverTarget::Sample(1),
            "at the edge"
        );
        assert_eq!(target(60.0 - SNAP_PX), HoverTarget::Sample(2));
        assert_eq!(target(35.0), HoverTarget::Gap(gaps[0]));
        assert_eq!(target(85.0), HoverTarget::Time(at(85)), "nothing near");
    }

    #[test]
    fn hover_beyond_now_is_the_projection() {
        let a = axes();
        let samples = [s(0, Some(10.0)), s(5, Some(12.0))];
        let p = Projection {
            end_t: at(300),
            end_pct: 60.0,
            cap_at: None,
        };
        let now = at(100);
        let x = |m: f32| a.plot.left() + m;
        let target = |m| hover_target(&a, &samples, &[], now, Some((10.0, p)), x(m));
        assert_eq!(
            target(200.0),
            HoverTarget::Projected {
                t: at(200),
                pct: 35.0
            }
        );
        assert_eq!(target(4.0), HoverTarget::Sample(1));
        // Without a projection, beyond now is just the time.
        let bare = hover_target(&a, &samples, &[], now, None, x(200.0));
        assert_eq!(bare, HoverTarget::Time(at(200)));
    }

    #[test]
    fn projected_pct_interpolates_beyond_now() {
        let p = Projection {
            end_t: at(300),
            end_pct: 80.0,
            cap_at: None,
        };
        let now = at(100);
        assert_eq!(
            projected_pct(now, 40.0, &p, at(100)),
            None,
            "not beyond now"
        );
        assert_eq!(projected_pct(now, 40.0, &p, at(50)), None);
        assert_eq!(projected_pct(now, 40.0, &p, at(200)), Some(60.0));
        assert_eq!(projected_pct(now, 40.0, &p, at(300)), Some(80.0));
        // Past the line's end (a cap before reset): its end value.
        let cap = Projection {
            end_t: at(200),
            end_pct: 100.0,
            cap_at: Some(at(200)),
        };
        assert_eq!(projected_pct(now, 40.0, &cap, at(250)), Some(100.0));
        // A zero-length line projects nothing.
        let zero = Projection {
            end_t: now,
            end_pct: 40.0,
            cap_at: None,
        };
        assert_eq!(projected_pct(now, 40.0, &zero, at(150)), None);
    }

    #[test]
    fn day_ticks_at_local_midnights() {
        let t = |d, h| Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap();
        // UTC: Mon 12 23:00 → Mon 19 23:00 has six midnights, Tue..Sun.
        let ticks = day_ticks_in(t(12, 23), t(19, 23), &Utc);
        let labels: Vec<&str> = ticks.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, ["Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Mon"]);
        assert_eq!(ticks[0].0, t(13, 0));
        // A window starting at midnight includes it.
        assert_eq!(
            day_ticks_in(t(13, 0), t(13, 5), &Utc),
            [(t(13, 0), "Tue".into())]
        );
        // In UTC-4, local midnight is 04:00 UTC.
        let edt = FixedOffset::west_opt(4 * 3600).unwrap();
        let ticks = day_ticks_in(t(12, 0), t(13, 12), &edt);
        assert_eq!(ticks, [(t(12, 4), "Mon".into()), (t(13, 4), "Tue".into())]);
        assert!(day_ticks_in(t(13, 1), t(13, 23), &Utc).is_empty());
        assert!(day_ticks_in(t(14, 0), t(13, 0), &Utc).is_empty());
    }

    #[test]
    fn week_span_and_end_labels() {
        let t = |d, h| Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap();
        assert_eq!(
            week_span_in(t(6, 23), t(13, 23), true, &Utc).as_deref(),
            Some("Tue 11p – Tue 11p")
        );
        assert_eq!(week_span_in(t(6, 23), t(13, 23), false, &Utc), None);
        let cap = EndLabel::Cap(Utc.with_ymd_and_hms(2026, 10, 12, 10, 45, 0).unwrap());
        assert_eq!(end_label_text(cap, TimeStyle::Clock, &Utc), "cap 10:45a");
        assert_eq!(
            end_label_text(cap, TimeStyle::Weekday, &Utc),
            "cap Mon 10:45a"
        );
        assert_eq!(
            end_label_text(EndLabel::Safe, TimeStyle::Weekday, &Utc),
            "safe"
        );
        assert_eq!(
            end_label_text(EndLabel::AtCap, TimeStyle::Clock, &Utc),
            "at cap"
        );
    }

    #[test]
    fn week_tips() {
        assert_eq!(
            week_sample_tip("Sat 3:12p", 41.2, Some(52.0)),
            "Sat 3:12p · 7d all 41% · Fable 52%"
        );
        assert_eq!(
            week_sample_tip("Sat 3:12p", 41.2, None),
            "Sat 3:12p · 7d all 41%"
        );
        assert_eq!(
            week_projected_tip("Mon 3p", Some(71.4), Some(90.0)),
            "Mon 3p · projected 7d all 71% · Fable 90%"
        );
        assert_eq!(
            week_projected_tip("Mon 3p", Some(71.4), None),
            "Mon 3p · projected 7d all 71%"
        );
        assert_eq!(
            week_projected_tip("Mon 3p", None, Some(100.0)),
            "Mon 3p · projected Fable 100%"
        );
    }

    #[test]
    fn overlay_series_segments_on_its_own_values() {
        let p = |m: i64, v: Option<f64>, o: Option<f64>| Point {
            t: at(m),
            value: v,
            overlay: o,
        };
        let samples = [
            p(0, Some(10.0), Some(20.0)),
            p(30, Some(11.0), None),
            p(60, Some(12.0), Some(22.0)),
            p(200, Some(13.0), Some(23.0)),
        ];
        let hour = Duration::minutes(WEEK_GAP_BREAK_MINUTES);
        assert_eq!(segments(&samples, hour), vec![0..3, 3..4]);
        let o = segments_of(&samples, hour, Series::Overlay);
        assert_eq!(o, vec![0..1, 2..3, 3..4]);
        // Only the time gap is bridged; the missing value is not.
        assert_eq!(
            bridges_of(&samples, &o, Series::Overlay),
            vec![Bridge { from: 2, to: 3 }]
        );
    }

    #[test]
    fn points_from_window_and_week_samples() {
        let w = WindowSample {
            t: at(0),
            five_hour: Some(1.0),
            fable: Some(2.0),
        };
        assert_eq!(
            Point::from(&w),
            Point {
                t: at(0),
                value: Some(1.0),
                overlay: Some(2.0)
            }
        );
        let k = WeekSample {
            t: at(0),
            seven_day: Some(3.0),
            fable: None,
        };
        assert_eq!(
            Point::from(&k),
            Point {
                t: at(0),
                value: Some(3.0),
                overlay: None
            }
        );
    }

    #[test]
    fn a_second_label_keeps_clear_of_the_first() {
        let bounds = rect(0.0, 0.0, 200.0, 120.0);
        let (from, end) = (pos2(0.0, 100.0), pos2(100.0, 40.0));
        let first = place_end_label(from, end, LABEL, 4.0, 2.0, bounds, &[]);
        // The same spot is taken: the next label goes elsewhere.
        let taken = [first.expand(LABEL_PAD)];
        let second = place_end_label(from, end, LABEL, 4.0, 2.0, bounds, &taken);
        assert_ne!(second, first);
        assert!(!overlaps(second.expand(LABEL_PAD), taken[0]));
    }

    #[test]
    fn lines_run_through_change_points() {
        let v = |pairs: &[(i64, f64)]| -> Vec<(DateTime<Utc>, f64)> {
            pairs.iter().map(|&(m, x)| (at(m), x)).collect()
        };
        // 10 held for 30 minutes, then 11, then 12: straight between the
        // first sightings, flat after the last one.
        let got = through_change_points(&v(&[
            (0, 10.0),
            (10, 10.0),
            (20, 10.0),
            (30, 11.0),
            (40, 11.0),
            (50, 12.0),
            (60, 12.0),
            (70, 12.0),
        ]));
        let want = [
            10.0,
            10.0 + 1.0 / 3.0,
            10.0 + 2.0 / 3.0,
            11.0,
            11.5,
            12.0,
            12.0,
            12.0,
        ];
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-9, "{got:?}");
        }
        // Change points keep their raw value; never above the next one.
        assert_eq!(through_change_points(&v(&[(0, 5.0)])), [5.0]);
        assert!(through_change_points(&[]).is_empty());
        let flat = through_change_points(&v(&[(0, 7.0), (5, 7.0)]));
        assert_eq!(flat, [7.0, 7.0]);
        // A small dip (not a reset) interpolates down and back.
        let dip = through_change_points(&v(&[(0, 10.0), (10, 9.0), (20, 9.0), (30, 10.0)]));
        assert_eq!(dip, [10.0, 9.0, 9.5, 10.0]);
    }

    #[test]
    fn a_multi_point_jump_holds_then_rises() {
        let v = |pairs: &[(i64, f64)]| -> Vec<(DateTime<Utc>, f64)> {
            pairs.iter().map(|&(m, x)| (at(m), x)).collect()
        };
        // 40% over a 2 h lunch, then 55%: no 2 h ramp to 55.
        let got = through_change_points(&v(&[(0, 40.0), (60, 40.0), (120, 40.0), (121, 55.0)]));
        assert_eq!(got[0], 40.0);
        assert!(got[1] <= 41.0 && got[2] <= 41.0, "{got:?}");
        assert_eq!(got[3], 55.0);
        // A fall is held the other way.
        let got = through_change_points(&v(&[(0, 50.0), (60, 50.0), (61, 30.0)]));
        assert!(got[1] >= 49.0 && got[1] <= 50.0, "{got:?}");
        // Within a segment, drawn values follow; outside, none.
        let pts: Vec<Point> = [(0, Some(1.0)), (5, None), (6, Some(2.0)), (16, Some(3.0))]
            .iter()
            .map(|&(m, x)| Point {
                t: at(m),
                value: x,
                overlay: None,
            })
            .collect();
        let segs = segments(&pts, gap());
        let drawn = drawn_values(&pts, &segs, Series::Main);
        assert_eq!(drawn, vec![Some(1.0), None, Some(2.0), Some(3.0)]);
    }

    #[test]
    fn a_flat_gap_stays_solid_a_changed_one_bridges() {
        let samples = [
            s(0, Some(40.0)),
            s(120, Some(40.0)), // 2 h, same value: known flat, solid
            s(240, Some(42.0)), // 2 h, changed: break and bridge
        ];
        let segs = segments(&samples, gap());
        assert_eq!(segs, vec![0..2, 2..3]);
        assert_eq!(bridges(&samples, &segs), vec![Bridge { from: 1, to: 2 }]);
    }

    #[test]
    fn cap_text_only_with_room() {
        let cap = EndLabel::Cap(at(0));
        assert!(label_shown(cap, 80.0, 70.0));
        assert!(!label_shown(cap, 60.0, 70.0));
        assert!(!label_shown(EndLabel::AtCap, 500.0, 10.0));
        assert!(!label_shown(EndLabel::Safe, 0.0, 70.0));
        assert!(label_shown(EndLabel::Safe, 80.0, 70.0));
    }

    // --- several accounts -----------------------------------------------------

    fn view<'a>(
        id: &'a ProfileId,
        label: &'a str,
        d: &'a Dashboard,
        stale: bool,
    ) -> AccountView<'a> {
        AccountView {
            id,
            label,
            color: crate::ui::theme::DARK.series(id.as_str().len() as u8),
            data: Some(d),
            error: None,
            stale,
        }
    }

    #[test]
    fn union_window_spans_every_window() {
        assert_eq!(union_window([]), None);
        assert_eq!(union_window([(at(0), at(300))]), Some((at(0), at(300))));
        assert_eq!(
            union_window([(at(60), at(360)), (at(0), at(300)), (at(30), at(330))]),
            Some((at(0), at(360)))
        );
    }

    #[test]
    fn overlaid_five_hour_chart_spans_both_windows_with_a_mark_per_reset() {
        let now = t_at(14, 0);
        let a = five_dash(now, 40.0, Some(t_at(15, 0)));
        let b = five_dash(now, 10.0, Some(t_at(17, 30)));
        let (ia, ib) = (ProfileId::new("default"), ProfileId::new("work"));
        let (va, vb) = (view(&ia, "nate", &a, false), view(&ib, "work", &b, true));
        let c = five_hour_series_chart(&[&va, &vb], now, &crate::ui::theme::DARK).unwrap();
        assert!(c.overlaid());
        assert_eq!((c.start, c.end), (t_at(10, 0), t_at(17, 30)));
        assert_eq!(
            c.end_marks,
            vec![
                (t_at(15, 0), va.color),
                (t_at(17, 30), vb.color.gamma_multiply(DIM_ALPHA))
            ]
        );
        assert_eq!(c.series.len(), 2);
        for (s, v) in c.series.iter().zip([&va, &vb]) {
            assert_eq!(s.color, v.color);
            assert_eq!(s.label, v.label);
            assert_eq!(s.area, AreaStyle::Tint(SERIES_TINT));
            assert_eq!(s.overlay, Overlay::None, "no Fable share area");
            assert!(s.lines.iter().all(|l| l.color == v.color));
            assert_eq!(s.key, series_key(v.id));
        }
        // The stale account is dimmed, the fresh one not.
        assert!(!c.series[0].dimmed && c.series[1].dimmed);
        assert_eq!(
            c.series[1].tone(vb.color),
            vb.color.gamma_multiply(DIM_ALPHA)
        );
        let (top, _) = c.series[1].area_colors();
        assert_eq!(
            top,
            vb.color
                .gamma_multiply(SERIES_TINT)
                .gamma_multiply(DIM_ALPHA)
        );
        assert_eq!(c.legend.len(), 2);
        assert_eq!(c.legend[0].label, "nate");
        assert!(c.ticks.len() <= MAX_TICKS);
        assert!(c.note.is_none());
    }

    #[test]
    fn overlaid_idle_accounts_add_no_window() {
        let now = t_at(14, 0);
        let a = five_dash(now, 40.0, Some(t_at(15, 0)));
        let mut idle = five_dash(now, 0.0, None);
        idle.window_samples.clear();
        let (ia, ib) = (ProfileId::new("a"), ProfileId::new("b"));
        let (va, vb) = (view(&ia, "a", &a, false), view(&ib, "b", &idle, false));
        let c = five_hour_series_chart(&[&va, &vb], now, &crate::ui::theme::DARK).unwrap();
        assert_eq!((c.start, c.end), (t_at(10, 0), t_at(15, 0)));
        assert_eq!(c.end_marks.len(), 1);
        assert!(c.series[1].points.is_empty() && c.series[1].lines.is_empty());
        // Both idle: the empty state.
        let vc = view(&ia, "a", &idle, false);
        let c = five_hour_series_chart(&[&vc, &vb], now, &crate::ui::theme::DARK).unwrap();
        assert_eq!(c.note, Some(IDLE_NOTE));
        assert!(c.ticks.is_empty() && c.end_marks.is_empty() && c.end_label.is_none());
    }

    #[test]
    fn one_account_draws_as_it_always_did() {
        let now = t_at(14, 0);
        let d = five_dash(now, 40.0, Some(t_at(15, 0)));
        let id = ProfileId::new("default");
        let views = [view(&id, "nate", &d, true)];
        assert_eq!(with_data(&views).len(), 1);
        let c = five_hour_chart(&d, now, &crate::ui::theme::DARK);
        assert!(!c.overlaid());
        let s = &c.series[0];
        let state = crate::ui::theme::DARK.level(analytics::color_for(Some(40.0)));
        assert_eq!((s.color, s.area), (state, AreaStyle::State(state)));
        assert!(matches!(s.overlay, Overlay::Share(_)));
        assert!(!s.dimmed);
        assert_eq!(
            s.area_colors(),
            (
                state.gamma_multiply(AREA_TOP_ALPHA),
                state.gamma_multiply(AREA_BASE_ALPHA)
            )
        );
        assert!(c.legend.is_empty() && c.end_marks.is_empty());
        assert_eq!(legend_for(&c.series, Color32::GRAY), vec![]);
    }

    fn series_of(label: &str, points: Vec<Point>, lines: Vec<Line>) -> SeriesData {
        SeriesData {
            key: label.len() as u64,
            label: label.to_owned(),
            color: Color32::RED,
            area: AreaStyle::Tint(SERIES_TINT),
            overlay: Overlay::Line {
                color: Color32::RED,
                dashed: true,
            },
            points,
            lines,
            dimmed: false,
            end: at(300),
            poll_key: 0,
        }
    }

    fn chart_of(series: Vec<SeriesData>, now: DateTime<Utc>) -> Chart {
        Chart {
            id: "test",
            start: at(0),
            end: at(300),
            now,
            series,
            gap_break: gap(),
            ticks: Vec::new(),
            end_label: None,
            end_marks: Vec::new(),
            legend: Vec::new(),
            bridge_fill: 1.0,
            style: TimeStyle::Clock,
            note: None,
        }
    }

    #[test]
    fn hover_rows_read_each_series_and_skip_gaps() {
        let main = |pct, end_pct| Line {
            kind: LineKind::Main,
            pct,
            projection: Some(Projection {
                end_t: at(300),
                end_pct,
                cap_at: None,
            }),
            label: None,
            color: Color32::RED,
            cap_color: Color32::RED,
        };
        let a = series_of(
            "nate",
            vec![
                Point {
                    t: at(0),
                    value: Some(10.0),
                    overlay: Some(50.0),
                },
                Point {
                    t: at(5),
                    value: Some(12.0),
                    overlay: None,
                },
                Point {
                    t: at(40),
                    value: Some(20.0),
                    overlay: None,
                },
            ],
            vec![main(20.0, 60.0)],
        );
        let mut b = series_of(
            "work",
            vec![
                Point {
                    t: at(20),
                    value: Some(5.0),
                    overlay: None,
                },
                Point {
                    t: at(38),
                    value: Some(6.0),
                    overlay: None,
                },
            ],
            vec![main(5.0, 5.0)],
        );
        b.overlay = Overlay::None;
        b.end = at(200);
        let c = chart_of(vec![a, b], at(100));
        let read = |m| -> Vec<(String, f64, Option<f64>, bool)> {
            hover_rows(&c, at(m))
                .into_iter()
                .map(|r| (r.label, r.value, r.fable, r.projected))
                .collect()
        };
        // Only "nate" has a reading by then, with its Fable 7-day.
        assert_eq!(read(2), vec![("nate".into(), 10.0, Some(50.0), false)]);
        assert_eq!(
            read(25),
            vec![("work".into(), 5.0, None, false)],
            "nate's last sample is 20 min old: a gap, no row"
        );
        assert_eq!(
            read(14),
            vec![("nate".into(), 12.0, None, false)],
            "work starts at 20"
        );
        // Both: each account's last sample at or before the time.
        assert_eq!(
            read(45),
            vec![
                ("nate".into(), 20.0, None, false),
                ("work".into(), 6.0, None, false)
            ]
        );
        assert!(read(55).is_empty(), "both readings over 10 min old");
        // Beyond now: the projections, nothing past an account's own end.
        assert_eq!(
            read(200),
            vec![
                ("nate".into(), 40.0, None, true),
                ("work".into(), 5.0, None, true)
            ]
        );
        assert_eq!(read(250), vec![("nate".into(), 50.0, None, true)]);
        // Before every sample: nothing.
        assert!(hover_rows(&c, at(-5)).is_empty());
    }

    #[test]
    fn hover_tip_lists_every_series() {
        let row = |label: &str, value, fable, projected| HoverRow {
            label: label.to_owned(),
            color: Color32::RED,
            value,
            fable,
            projected,
        };
        assert_eq!(
            hover_tip(
                "3:10 PM",
                &[
                    row("nate", 46.2, None, false),
                    row("work", 12.0, None, false)
                ]
            ),
            "3:10 PM\n● nate 46%\n● work 12%"
        );
        assert_eq!(
            hover_row_text(&row("nate", 41.0, Some(52.0), false)),
            "nate 41% · Fable 52%"
        );
        assert_eq!(
            hover_row_text(&row("work", 71.0, None, true)),
            "work projected 71%"
        );
        assert_eq!(hover_tip("3:10 PM", &[]), "3:10 PM");
    }

    #[test]
    fn motion_ids_differ_per_account() {
        let (a, b) = (
            series_key(&ProfileId::new("default")),
            series_key(&ProfileId::new("work")),
        );
        assert_ne!(a, b);
        assert_eq!(a, series_key(&ProfileId::new("default")), "stable");
        let id = |k| motion_id("timeline", k, LineKind::Main, "now-pct");
        assert_ne!(id(a), id(b));
        assert_ne!(
            motion_id("timeline", a, LineKind::Main, "now-pct"),
            motion_id("week", a, LineKind::Main, "now-pct")
        );
        assert_ne!(
            id(a),
            motion_id("timeline", a, LineKind::Overlay, "now-pct")
        );
    }

    #[test]
    fn legend_only_with_two_or_more_series() {
        let one = vec![series_of("nate", Vec::new(), Vec::new())];
        assert!(legend_for(&one, Color32::GRAY).is_empty());
        let mut two = vec![one[0].clone(), series_of("work", Vec::new(), Vec::new())];
        two[1].overlay = Overlay::None;
        two[1].dimmed = true;
        let legend = legend_for(&two, Color32::GRAY);
        let names: Vec<_> = legend
            .iter()
            .map(|e| (e.label.as_str(), e.dashed))
            .collect();
        assert_eq!(names, [("nate", false), ("work", false), ("Fable", true)]);
        assert_eq!(legend[1].color, Color32::RED.gamma_multiply(DIM_ALPHA));
        assert_eq!(legend[2].color, Color32::GRAY);
        // No dashed Fable line: no Fable entry.
        two[0].overlay = Overlay::None;
        assert_eq!(legend_for(&two, Color32::GRAY).len(), 2);
    }

    #[test]
    fn legend_entries_fit_from_the_first() {
        assert_eq!(legend_fit(&[30.0, 40.0], 10.0, 80.0), 2);
        assert_eq!(legend_fit(&[30.0, 40.0], 10.0, 79.0), 1);
        assert_eq!(legend_fit(&[30.0, 40.0], 10.0, 20.0), 0);
        assert_eq!(legend_fit(&[], 10.0, 20.0), 0);
    }

    #[test]
    fn ticks_thin_to_a_maximum() {
        let ticks: Vec<_> = (0..14).map(|i| (at(i), i.to_string())).collect();
        let thin = thin_ticks(ticks.clone(), 8);
        assert_eq!(labels(&thin), ["0", "2", "4", "6", "8", "10", "12"]);
        assert_eq!(thin_ticks(ticks[..8].to_vec(), 8).len(), 8);
    }

    #[test]
    fn dashes_run_on_across_vertices() {
        // Ten 1-px steps: one 5-px dash, a 4-px gap, then the next dash.
        let points: Vec<Pos2> = (0..=10).map(|i| pos2(i as f32, 0.0)).collect();
        let pieces = dashed_polyline(&points, 5.0, 4.0);
        let ends: Vec<(f32, f32)> = pieces
            .iter()
            .map(|p| (p.first().unwrap().x, p.last().unwrap().x))
            .collect();
        assert_eq!(ends, [(0.0, 5.0), (9.0, 10.0)]);
        // Around a corner: the dash bends with the line.
        let bent = dashed_polyline(&[pos2(0.0, 0.0), pos2(3.0, 0.0), pos2(3.0, 3.0)], 5.0, 4.0);
        assert_eq!(
            bent[0],
            vec![pos2(0.0, 0.0), pos2(3.0, 0.0), pos2(3.0, 2.0)]
        );
        assert!(dashed_polyline(&[pos2(0.0, 0.0)], 5.0, 4.0).is_empty());
    }

    #[test]
    fn tooltip_texts() {
        assert_eq!(
            sample_tip("1:35 PM", 42.0, Some(6.08)),
            "1:35 PM · 42.0% · Fable 6.1%"
        );
        assert_eq!(sample_tip("1:35 PM", 42.0, None), "1:35 PM · 42.0%");
        assert_eq!(sample_tip("1:35 PM", 42.0, Some(0.0)), "1:35 PM · 42.0%");
        assert_eq!(projected_tip("3:10 PM", 71.4), "3:10 PM · projected 71%");
    }
}
