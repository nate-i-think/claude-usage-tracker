//! The hero timeline: the current 5-hour window as a smooth area chart.
//!
//! x runs from the window start to its reset, y from 0 to 100%. On top of
//! light gridlines it paints the usage area (raw window samples, a gradient
//! [`Mesh`] in the state color with a stroked line), the Fable layer under
//! it, a "now" marker with a dot at the current %, the dashed projection to
//! the reset or the cap (with a ✕ and "cap 4:10p", or a "safe" end label),
//! the dashed 100% cap line, and a hover crosshair with a tooltip. Time
//! gaps in the samples (the app was closed) are bridged with a faint dotted
//! line: 5-hour utilization only rises within a window, so the straight
//! line is the best guess; only resets and missing values truly break.
//!
//! Geometry (mapping, segmenting, thinning, dashes, ticks, hover lookup) is
//! in pure functions tested without a GPU; [`show_timeline`] only paints.

use std::fmt::Display;
use std::ops::Range;

use chrono::{DateTime, Duration, Local, TimeZone, Timelike, Utc};
use eframe::egui::{
    epaint::{Mesh, PathStroke},
    pos2, vec2, Align2, Color32, FontId, Id, Painter, Pos2, Rect, Response, Sense, Shape, Stroke,
    TextStyle, Ui, Vec2,
};

use crate::analytics::{self, alerts::fmt_clock_in, Projection, WindowSample};
use crate::poller::Dashboard;

use super::motion;
use super::theme::Palette;

/// Height of the timeline in full mode.
pub const FULL_HEIGHT: f32 = 180.0;
/// The card's title.
pub const TITLE: &str = "5-hour window";
/// A gap between samples longer than this ends a solid line (the app was
/// closed or polls failed); its two ends are joined by a [`Bridge`].
pub const GAP_BREAK_MINUTES: i64 = 10;
/// A drop of more than this many points between samples is a reset and
/// breaks the line (the rule of [`analytics::recent_window`]).
pub const RESET_DROP: f64 = 5.0;
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
/// with a 5-hour value, broken where a value is missing, the time between
/// neighbors exceeds [`GAP_BREAK_MINUTES`] (see [`bridges`]), or the value
/// drops by more than [`RESET_DROP`] points. Single-sample ranges are kept.
pub fn segments(samples: &[WindowSample]) -> Vec<Range<usize>> {
    let max_gap = Duration::minutes(GAP_BREAK_MINUTES);
    let mut out = Vec::new();
    let mut current: Option<Range<usize>> = None;
    for (i, s) in samples.iter().enumerate() {
        let Some(v) = s.five_hour else {
            out.extend(current.take());
            continue;
        };
        current = match current.take() {
            Some(r) => {
                let prev = &samples[r.end - 1];
                let prev_v = prev.five_hour.unwrap_or(v);
                if s.t - prev.t > max_gap || prev_v - v > RESET_DROP {
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
pub fn bridges(samples: &[WindowSample], segs: &[Range<usize>]) -> Vec<Bridge> {
    segs.windows(2)
        .filter_map(|pair| {
            let (from, to) = (pair[0].end - 1, pair[1].start);
            if to != from + 1 {
                return None; // missing values in between
            }
            let (a, b) = (samples[from].five_hour?, samples[to].five_hour?);
            (a - b <= RESET_DROP).then_some(Bridge { from, to })
        })
        .collect()
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

/// The sample with a 5-hour value nearest in time to `t`.
pub fn nearest_sample(samples: &[WindowSample], t: DateTime<Utc>) -> Option<usize> {
    samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.five_hour.is_some())
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
    samples: &[WindowSample],
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
/// box.
pub fn place_end_label(
    from: Pos2,
    end: Pos2,
    size: Vec2,
    clearance: f32,
    keep_out: f32,
    bounds: Rect,
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
    let first = |ok: &dyn Fn(&Rect) -> bool| candidates.iter().copied().find(|r| ok(r));
    first(&|r| clear_of_keep(r) && clear_of_now(r) && clear_of_line(r))
        .or_else(|| first(&|r| clear_of_keep(r) && clear_of_now(r)))
        .or_else(|| first(&clear_of_keep))
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

/// The timeline's card: the title with the window's span as its subtitle
/// (see [`window_span_in`]), then the timeline, `height` tall.
pub fn show_timeline_card(
    ui: &mut Ui,
    d: &Dashboard,
    now: DateTime<Utc>,
    palette: &Palette,
    height: f32,
) {
    let reset_known = d.snapshot.five_hour_resets_at().is_some();
    let span = window_span_in(d.window_start, d.window_end, reset_known, &Local);
    super::theme::card(palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        super::theme::chart_title(ui, palette, TITLE, span.as_deref());
        show_timeline(ui, d, now, palette, height);
    });
}

/// Paint the timeline for `d` at `now`, `height` tall across the available
/// width. Hovering shows a crosshair and a tooltip.
pub fn show_timeline(
    ui: &mut Ui,
    d: &Dashboard,
    now: DateTime<Utc>,
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
    let axes = Axes {
        plot: plot_rect(rect, label_w, font.size),
        start: d.window_start,
        end: d.window_end,
    };

    paint_y_axis(&painter, axes.plot, palette, &font);
    paint_time_axis(&painter, &axes, palette, &font, rect.left());

    let samples = &d.window_samples;
    let segs = segments(samples);
    if segs.is_empty() {
        painter.text(
            axes.plot.center(),
            Align2::CENTER_CENTER,
            "collecting samples…",
            font,
            palette.text_muted,
        );
        return response;
    }

    let pct = d.snapshot.five_hour_pct();
    let state = palette.level(analytics::color_for(pct));
    let fable = analytics::fable_layer(samples);
    let gaps = bridges(samples, &segs);
    paint_areas(
        &painter, &axes, palette, state, samples, &fable, &segs, &gaps,
    );

    let projection = pct.map(|p| {
        let rate = analytics::compute_rate(&d.history, now);
        (p, analytics::projection(now, p, rate, d.window_end))
    });
    let reset_known = d.snapshot.five_hour_resets_at().is_some();
    // The dot eases to each poll's %; the projection draws in when it
    // first appears and when a poll moves its end.
    let ctx = ui.ctx().clone();
    let ends = projection.map(|(pct, p)| Ends {
        pct: motion::eased(&ctx, Id::new("timeline-now-pct"), pct),
        projection: p,
        label: end_label(reset_known, &p, now),
        share: motion::draw_in(
            &ctx,
            Id::new("timeline-projection"),
            d.snapshot.fetched_at.timestamp_millis(),
            p.end_pct,
        ),
    });
    paint_now(&painter, &axes, palette, state, now, ends, &font);

    if let Some(pointer) = response.hover_pos() {
        if axes.plot.x_range().contains(pointer.x) {
            let target = hover_target(&axes, samples, &gaps, now, projection, pointer.x);
            let tip = paint_hover(&painter, &axes, palette, samples, &fable, target, pointer.x);
            return response.on_hover_text_at_pointer(tip);
        }
    }
    response
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

/// Hour ticks along the bottom and "reset 3:42p" at the right end; hour
/// labels that would collide with it or leave the widget are skipped.
fn paint_time_axis(painter: &Painter, axes: &Axes, palette: &Palette, font: &FontId, left: f32) {
    let y = axes.plot.bottom() + 3.0;
    let reset = painter.layout_no_wrap(
        format!("reset {}", short_time_in(axes.end, &Local)),
        font.clone(),
        palette.text_muted,
    );
    let reset_left = axes.plot.right() + CAP_MARK + 2.0 - reset.size().x;
    let tick = Stroke::new(1.0, palette.border);
    for (t, label) in hour_ticks_in(axes.start, axes.end, &Local) {
        let x = axes.x_at(t);
        painter.vline(x, axes.plot.bottom()..=axes.plot.bottom() + 3.0, tick);
        let galley = painter.layout_no_wrap(label, font.clone(), palette.text_muted);
        let w = galley.size().x;
        let lx = x - w / 2.0;
        if lx >= left && lx + w <= reset_left - 6.0 {
            painter.galley(pos2(lx, y), galley, palette.text_muted);
        }
    }
    painter.galley(pos2(reset_left, y), reset, palette.text_muted);
}

/// The faint bridge fills (usage and Fable), the usage area per segment
/// (gradient fill), the Fable layer over its base with a surface-colored
/// separator on top, then the usage lines and the dotted bridge lines.
#[allow(clippy::too_many_arguments)]
fn paint_areas(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    state: Color32,
    samples: &[WindowSample],
    fable: &[Option<f64>],
    segs: &[Range<usize>],
    gaps: &[Bridge],
) {
    let baseline = axes.plot.bottom();
    let top = state.gamma_multiply(AREA_TOP_ALPHA);
    let bottom = state.gamma_multiply(AREA_BASE_ALPHA);
    let fable_fill = palette.fable.gamma_multiply(FABLE_ALPHA);
    let five = |i: usize| axes.pos(samples[i].t, samples[i].five_hour.unwrap_or(0.0));
    for b in gaps {
        let ends = [five(b.from), five(b.to)];
        let scale = BRIDGE_FILL_SCALE;
        painter.add(area_mesh(
            &ends,
            baseline,
            top.gamma_multiply(scale),
            bottom.gamma_multiply(scale),
        ));
        if let (Some(a), Some(z)) = (fable[b.from], fable[b.to]) {
            let ends = [axes.pos(samples[b.from].t, a), axes.pos(samples[b.to].t, z)];
            let faint = fable_fill.gamma_multiply(scale);
            painter.add(area_mesh(&ends, baseline, faint, faint));
        }
    }
    let mut raw = Vec::new();
    let mut lines = Vec::with_capacity(segs.len());
    for seg in segs {
        raw.clear();
        raw.extend(
            samples[seg.clone()]
                .iter()
                .filter_map(|s| s.five_hour.map(|v| axes.pos(s.t, v))),
        );
        let line = thin_columns(&raw);
        painter.add(area_mesh(&line, baseline, top, bottom));

        for run in present_runs(fable, seg.clone()) {
            raw.clear();
            raw.extend(run.map(|i| axes.pos(samples[i].t, fable[i].unwrap_or(0.0))));
            let layer = thin_columns(&raw);
            if layer.len() >= 2 {
                painter.add(area_mesh(&layer, baseline, fable_fill, fable_fill));
                painter.add(Shape::line(layer, Stroke::new(1.0, palette.surface)));
            }
        }
        lines.push(line);
    }
    let dotted = Stroke::new(1.5, state.gamma_multiply(BRIDGE_LINE_ALPHA));
    for b in gaps {
        for seg in dashes(five(b.from), five(b.to), DOT, DOT_GAP) {
            painter.line_segment(seg, dotted);
        }
    }
    for line in lines {
        match line[..] {
            [] => {}
            // A lone sample between gaps or breaks: a dot, not a line.
            [p] => {
                painter.circle_filled(p, LONE_RADIUS, state);
            }
            _ => {
                painter.add(Shape::line(line, PathStroke::new(2.0, state)));
            }
        }
    }
}

/// What [`paint_now`] draws beyond the marker line.
struct Ends {
    /// The dot's % (eased).
    pct: f64,
    projection: Projection,
    label: Option<EndLabel>,
    /// Share of the projection drawn in so far, 0..=1.
    share: f32,
}

/// Opacity of the projection's end mark and label for a draw-in `share`:
/// they fade in over the last 30% of the line.
pub fn end_fade(share: f32) -> f32 {
    ((share - 0.7) / 0.3).clamp(0.0, 1.0)
}

/// The now marker, the dashed projection with its cap ✕ and label or the
/// "safe" end label, and the dot at the current %.
fn paint_now(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    state: Color32,
    now: DateTime<Utc>,
    ends: Option<Ends>,
    font: &FontId,
) {
    let x = axes.x_at(now);
    painter.vline(
        x,
        axes.plot.y_range(),
        Stroke::new(1.0, palette.text_muted.gamma_multiply(0.6)),
    );
    let Some(Ends {
        pct,
        projection: p,
        label: kind,
        share,
    }) = ends
    else {
        return;
    };
    let fade = end_fade(share);
    let from = axes.pos(now, pct);
    let to = axes.pos(p.end_t, p.end_pct);
    // The end label ("cap 4:10p" by the ✕, or "safe"), placed off the line
    // inside the widget; the dash stops short of it if it must.
    let label = kind.map(|k| match k {
        EndLabel::Cap(cap) => (
            format!("cap {}", short_time_in(cap, &Local)),
            palette.danger,
            CAP_LABEL_CLEARANCE,
            CAP_KEEP_OUT,
        ),
        EndLabel::Safe => (
            "safe".to_owned(),
            palette.good,
            SAFE_LABEL_CLEARANCE,
            SAFE_KEEP_OUT,
        ),
    });
    let bounds = Rect::from_min_max(
        pos2(axes.plot.left(), painter.clip_rect().top()),
        pos2(painter.clip_rect().right(), axes.plot.bottom()),
    );
    let placed = label.map(|(text, color, clearance, keep_out)| {
        let galley = painter.layout_no_wrap(text, font.clone(), color);
        let rect = place_end_label(from, to, galley.size(), clearance, keep_out, bounds);
        (galley, rect, color)
    });
    let stop = placed
        .as_ref()
        .map_or(to, |(_, rect, _)| dash_end(from, to, *rect));
    paint_dashed(
        painter,
        from,
        from + (stop - from) * share,
        Stroke::new(1.5, state),
    );

    if matches!(kind, Some(EndLabel::Cap(_))) && fade > 0.0 {
        let stroke = Stroke::new(2.0, palette.danger.gamma_multiply(fade));
        let (a, b) = (vec2(CAP_MARK, CAP_MARK), vec2(CAP_MARK, -CAP_MARK));
        painter.line_segment([to - a, to + a], stroke);
        painter.line_segment([to - b, to + b], stroke);
    }
    if let Some((galley, rect, color)) = placed.filter(|_| fade > 0.0) {
        painter.rect_filled(
            rect.expand(LABEL_PAD),
            3.0,
            palette.surface.gamma_multiply(fade),
        );
        painter.galley_with_override_text_color(rect.min, galley, color.gamma_multiply(fade));
    }

    painter.circle(from, DOT_RADIUS, state, Stroke::new(1.5, palette.surface));
}

/// Crosshair for the hover `target` and its tooltip text: a full
/// crosshair on a sample or the projection, a vertical line over a gap or
/// an empty spot.
fn paint_hover(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    samples: &[WindowSample],
    fable: &[Option<f64>],
    target: HoverTarget,
    pointer_x: f32,
) -> String {
    let point = |t: DateTime<Utc>, pct: f64| Some(axes.pos(t, pct));
    let (x, point, tip) = match target {
        HoverTarget::Projected { t, pct } => (
            axes.x_at(t),
            point(t, pct),
            projected_tip(&fmt_clock_in(t, &Local), pct),
        ),
        HoverTarget::Sample(i) => {
            let s = &samples[i];
            let pct = s.five_hour.unwrap_or(0.0);
            let tip = sample_tip(&fmt_clock_in(s.t, &Local), pct, fable[i]);
            (axes.x_at(s.t), point(s.t, pct), tip)
        }
        HoverTarget::Gap(b) => {
            let (from, to) = (samples[b.from].t, samples[b.to].t);
            (pointer_x, None, gap_tip_in(from, to, &Local))
        }
        HoverTarget::Time(t) => (pointer_x, None, fmt_clock_in(t, &Local)),
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

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 10, 17, 0).unwrap()
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        t0() + Duration::minutes(minutes)
    }

    fn s(m: i64, five: Option<f64>) -> WindowSample {
        WindowSample {
            t: at(m),
            five_hour: five,
            fable: None,
        }
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
        assert_eq!(segments(&samples), vec![0..3]);
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
        assert_eq!(segments(&samples), vec![0..2, 3..5]);
    }

    #[test]
    fn gaps_over_ten_minutes_break() {
        let samples = [
            s(0, Some(1.0)),
            s(10, Some(2.0)), // exactly 10 min: still joined
            s(21, Some(3.0)), // 11 min: break
        ];
        assert_eq!(segments(&samples), vec![0..2, 2..3]);
    }

    #[test]
    fn drops_over_five_points_are_resets() {
        let samples = [
            s(0, Some(40.0)),
            s(1, Some(35.0)), // drop of exactly 5: not a reset
            s(2, Some(29.0)), // drop of 6: reset
            s(3, Some(30.0)),
        ];
        assert_eq!(segments(&samples), vec![0..2, 2..4]);
    }

    #[test]
    fn single_and_no_samples() {
        assert!(segments(&[]).is_empty());
        assert!(segments(&[s(0, None)]).is_empty());
        assert_eq!(segments(&[s(0, Some(3.0))]), vec![0..1]);
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
        let segs = segments(&samples);
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
        let segs = segments(&samples);
        assert_eq!(segs, vec![0..1, 1..3, 4..6, 6..7]);
        assert_eq!(bridges(&samples, &segs), vec![Bridge { from: 5, to: 6 }]);
    }

    #[test]
    fn bridges_need_two_segments() {
        assert!(bridges(&[], &[]).is_empty());
        let samples = [s(0, Some(1.0)), s(1, Some(2.0))];
        assert!(bridges(&samples, &segments(&samples)).is_empty());
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
        let got = place_end_label(pos2(0.0, 100.0), pos2(100.0, 40.0), LABEL, 4.0, 2.0, bounds);
        assert_eq!(got, rect(70.0, 26.0, 100.0, 36.0));
    }

    #[test]
    fn end_label_above_right_when_the_top_is_tight() {
        let bounds = rect(0.0, 0.0, 200.0, 120.0);
        let (from, end) = (pos2(0.0, 100.0), pos2(100.0, 8.0));
        let got = place_end_label(from, end, LABEL, 4.0, 2.0, bounds);
        assert_eq!(got, rect(104.0, 0.0, 134.0, 10.0));
        assert_eq!(segment_entry(from, end, got.expand(LABEL_PAD)), None);
    }

    #[test]
    fn end_label_flat_line_sits_above_it() {
        let bounds = rect(0.0, 0.0, 102.0, 120.0);
        let (from, end) = (pos2(50.0, 60.0), pos2(100.0, 60.0));
        let got = place_end_label(from, end, LABEL, 4.0, 2.0, bounds);
        assert_eq!(got, rect(70.0, 46.0, 100.0, 56.0));
    }

    /// Real proportions: the cap point sits on the 100% line, 11.5 pt
    /// under the widget top; "cap 6:08p" is about 60 x 14.
    fn assert_cap_label_clear(bounds: Rect, end: Pos2, slope: f32) {
        let size = vec2(60.0, 14.0);
        let dx = (200.0_f32).min((bounds.bottom() - 2.0 - end.y) / slope);
        let from = pos2(end.x - dx, end.y + slope * dx);
        let got = place_end_label(from, end, size, CAP_LABEL_CLEARANCE, CAP_KEEP_OUT, bounds);
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
        let got = place_end_label(from, end, vec2(60.0, 14.0), 8.0, 5.0, bounds);
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
        let gaps = bridges(&samples, &segments(&samples));
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
