//! The hero timeline: the current 5-hour window as a smooth area chart.
//!
//! x runs from the window start to its reset, y from 0 to 100%. On top of
//! light gridlines it paints the usage area (raw window samples, a gradient
//! [`Mesh`] in the state color with a stroked line), the Fable layer under
//! it, a "now" marker with a dot at the current %, the dashed projection to
//! the reset or the cap (with a ✕ and "cap 4:10p", or a "safe" end label),
//! the dashed 100% cap line, and a hover crosshair with a tooltip.
//!
//! Geometry (mapping, segmenting, thinning, dashes, ticks, hover lookup) is
//! in pure functions tested without a GPU; [`show_timeline`] only paints.

use std::fmt::Display;
use std::ops::Range;

use chrono::{DateTime, Duration, Local, TimeZone, Timelike, Utc};
use eframe::egui::{
    epaint::{Mesh, PathStroke},
    pos2, vec2, Align2, Color32, FontId, Painter, Pos2, Rect, Response, Sense, Shape, Stroke,
    TextStyle, Ui,
};

use crate::analytics::{self, Projection, WindowSample};
use crate::poller::Dashboard;

use super::theme::Palette;

/// Height of the timeline in full mode.
pub const FULL_HEIGHT: f32 = 180.0;
/// A gap between samples longer than this breaks the line (the app was
/// closed or polls failed).
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
        let frac = (pct.clamp(0.0, 100.0) / 100.0) as f32;
        self.plot.bottom() - frac * self.plot.height()
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

/// Index ranges of `samples` drawn as one connected line each: only samples
/// with a 5-hour value, broken where a value is missing, the time between
/// neighbors exceeds [`GAP_BREAK_MINUTES`], or the value drops by more than
/// [`RESET_DROP`] points. Single-sample ranges are kept.
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

/// Local time for tooltips: "1:35 PM".
pub fn tooltip_time_in<Tz: TimeZone>(t: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    t.with_timezone(tz).format("%-I:%M %p").to_string()
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

/// Left edge for a label `width` wide centered on `center`, kept inside
/// `left..right` (pinned left when it doesn't fit).
pub fn label_left(center: f32, width: f32, left: f32, right: f32) -> f32 {
    (center - width / 2.0).min(right - width).max(left)
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
    let text_h = font.size;
    let axes = Axes {
        plot: Rect::from_min_max(
            pos2(
                rect.left() + label_w + 6.0,
                rect.top() + text_h.max(CAP_MARK + 2.0),
            ),
            pos2(rect.right() - CAP_MARK - 2.0, rect.bottom() - text_h - 6.0),
        ),
        start: d.window_start,
        end: d.window_end,
    };

    paint_grid(&painter, &axes, palette, &font);
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
    paint_areas(&painter, &axes, palette, state, samples, &fable, &segs);

    let projection = pct.map(|p| {
        let rate = analytics::compute_rate(&d.history, now);
        (p, analytics::projection(now, p, rate, d.window_end))
    });
    paint_now(&painter, &axes, palette, state, now, projection, &font);

    if let Some(pointer) = response.hover_pos() {
        if axes.plot.x_range().contains(pointer.x) {
            let tip = paint_hover(
                &painter, &axes, palette, now, samples, &fable, projection, pointer,
            );
            if let Some(tip) = tip {
                return response.on_hover_text_at_pointer(tip);
            }
        }
    }
    response
}

/// Gridlines at 25/50/75 with labels, the dashed 100% cap line, and the
/// baseline.
fn paint_grid(painter: &Painter, axes: &Axes, palette: &Palette, font: &FontId) {
    let label_x = axes.plot.left() - 4.0;
    for mark in GRID_MARKS {
        let y = axes.y_at(mark);
        painter.hline(axes.plot.x_range(), y, Stroke::new(1.0, palette.grid));
        painter.text(
            pos2(label_x, y),
            Align2::RIGHT_CENTER,
            format!("{mark:.0}%"),
            font.clone(),
            palette.text_muted,
        );
    }
    let top = axes.y_at(100.0);
    let cap = Stroke::new(1.0, palette.danger.gamma_multiply(CAP_LINE_ALPHA));
    let ends = (pos2(axes.plot.left(), top), pos2(axes.plot.right(), top));
    paint_dashed(painter, ends.0, ends.1, cap);
    painter.text(
        pos2(label_x, top),
        Align2::RIGHT_CENTER,
        "100%",
        font.clone(),
        palette.text_muted,
    );
    painter.hline(
        axes.plot.x_range(),
        axes.plot.bottom(),
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

/// The usage area per segment (gradient fill), the Fable layer over its
/// base with a surface-colored separator on top, then the usage line.
fn paint_areas(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    state: Color32,
    samples: &[WindowSample],
    fable: &[Option<f64>],
    segs: &[Range<usize>],
) {
    let baseline = axes.plot.bottom();
    let top = state.gamma_multiply(AREA_TOP_ALPHA);
    let bottom = state.gamma_multiply(AREA_BASE_ALPHA);
    let fable_fill = palette.fable.gamma_multiply(FABLE_ALPHA);
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
    for line in lines {
        match line[..] {
            [] => {}
            // An isolated sample (gaps on both sides): a dot, not a line.
            [p] => {
                painter.circle_filled(p, LONE_RADIUS, state);
            }
            _ => {
                painter.add(Shape::line(line, PathStroke::new(2.0, state)));
            }
        }
    }
}

/// The now marker, the dashed projection with its cap ✕ and label or the
/// "safe" end label, and the dot at the current %.
fn paint_now(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    state: Color32,
    now: DateTime<Utc>,
    projection: Option<(f64, Projection)>,
    font: &FontId,
) {
    let x = axes.x_at(now);
    painter.vline(
        x,
        axes.plot.y_range(),
        Stroke::new(1.0, palette.text_muted.gamma_multiply(0.6)),
    );
    let Some((pct, p)) = projection else {
        return;
    };
    let from = axes.pos(now, pct);
    let to = axes.pos(p.end_t, p.end_pct);
    paint_dashed(painter, from, to, Stroke::new(1.5, state));

    if let Some(cap) = p.cap_at {
        let c = axes.pos(cap, 100.0);
        let stroke = Stroke::new(2.0, palette.danger);
        let (a, b) = (vec2(CAP_MARK, CAP_MARK), vec2(CAP_MARK, -CAP_MARK));
        painter.line_segment([c - a, c + a], stroke);
        painter.line_segment([c - b, c + b], stroke);
        let galley = painter.layout_no_wrap(
            format!("cap {}", short_time_in(cap, &Local)),
            font.clone(),
            palette.danger,
        );
        let lx = label_left(
            c.x,
            galley.size().x,
            axes.plot.left(),
            painter.clip_rect().right(),
        );
        painter.galley(pos2(lx, c.y + CAP_MARK + 2.0), galley, palette.danger);
    } else if p.end_t > now {
        let y = (to.y - 4.0).max(axes.plot.top() + font.size);
        painter.text(
            pos2(painter.clip_rect().right(), y),
            Align2::RIGHT_BOTTOM,
            "safe",
            font.clone(),
            palette.good,
        );
    }

    painter.circle(from, DOT_RADIUS, state, Stroke::new(1.5, palette.surface));
}

/// Crosshair at the hovered sample (or projection point beyond now) and
/// its tooltip text.
#[allow(clippy::too_many_arguments)]
fn paint_hover(
    painter: &Painter,
    axes: &Axes,
    palette: &Palette,
    now: DateTime<Utc>,
    samples: &[WindowSample],
    fable: &[Option<f64>],
    projection: Option<(f64, Projection)>,
    pointer: Pos2,
) -> Option<String> {
    let t = axes.t_at(pointer.x);
    let projected = projection.and_then(|(pct, p)| Some((t, projected_pct(now, pct, &p, t)?)));
    let (at, pct, tip) = match projected {
        Some((t, pct)) => (t, pct, projected_tip(&tooltip_time_in(t, &Local), pct)),
        None => {
            let i = nearest_sample(samples, t)?;
            let s = &samples[i];
            let pct = s.five_hour?;
            let time = tooltip_time_in(s.t, &Local);
            (s.t, pct, sample_tip(&time, pct, fable[i]))
        }
    };
    let p = axes.pos(at, pct);
    let stroke = Stroke::new(1.0, palette.text_muted.gamma_multiply(0.7));
    painter.vline(p.x, axes.plot.y_range(), stroke);
    painter.hline(axes.plot.x_range(), p.y, stroke);
    painter.circle(p, 3.0, palette.surface, Stroke::new(1.5, palette.text));
    Some(tip)
}

fn paint_dashed(painter: &Painter, a: Pos2, b: Pos2, stroke: Stroke) {
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
        assert!(thin.len() <= 200, "{}", thin.len());
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
        assert!(mesh.indices.iter().all(|&i| (i as usize) < 6));
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
        assert_eq!(tooltip_time_in(t, &Utc), "3:42 PM");
        let morning = Utc.with_ymd_and_hms(2026, 10, 11, 0, 5, 0).unwrap();
        assert_eq!(short_time_in(morning, &Utc), "12:05a");
        assert_eq!(tooltip_time_in(morning, &Utc), "12:05 AM");
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        assert_eq!(short_time_in(t, &ist), "9:12p");
    }

    #[test]
    fn label_left_keeps_labels_inside() {
        assert_eq!(label_left(50.0, 20.0, 0.0, 100.0), 40.0);
        assert_eq!(label_left(5.0, 20.0, 0.0, 100.0), 0.0);
        assert_eq!(label_left(98.0, 20.0, 0.0, 100.0), 80.0);
        // Wider than the room: pinned left.
        assert_eq!(label_left(50.0, 200.0, 0.0, 100.0), 0.0);
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
