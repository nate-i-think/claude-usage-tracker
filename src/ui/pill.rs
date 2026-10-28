//! The pill: the window shrunk to a ~300x44 borderless strip showing the
//! 5-hour window at a glance — mini ring, big %, "5h · 1h59m" countdown,
//! pace delta, and a state dot that pulses near the cap.
//!
//! Interaction: drag anywhere moves the window, a click expands to the full
//! dashboard, right-click swaps the content for a row of actions (a popup
//! menu would not fit in a 44-point window), hover shows the full stats.
//!
//! The model, copy, ring angles and slot layout are pure and tested; the
//! painter at the bottom only draws what they decide.

use std::fmt::Display;
use std::sync::Arc;

use chrono::{DateTime, Local, TimeZone, Utc};
use eframe::egui::{
    self, epaint::Shadow, pos2, vec2, Color32, FontId, Galley, Id, Pos2, Rect, RichText, Sense,
    Stroke, Ui, ViewportCommand,
};

use crate::analytics::{self, Level};

use super::chips::{self, Chip};
use super::digits;
use super::motion;
use super::theme::{self, Palette};

/// Inner padding at the pill's left and right ends.
pub const PAD_X: f32 = 12.0;
/// Space between the pill's pieces.
pub const GAP: f32 = 8.0;
/// Mini ring: outer radius and stroke width.
pub const RING_R: f32 = 11.0;
pub const RING_W: f32 = 3.5;
/// State dot radius.
pub const DOT_R: f32 = 4.5;
/// Fonts: the big %, the countdown, the pace delta.
pub const PCT_SIZE: f32 = 20.0;
pub const COUNTDOWN_SIZE: f32 = 13.0;
pub const PACE_SIZE: f32 = 12.0;

/// What the pill shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PillModel {
    /// The 5-hour utilization.
    pub pct: Option<f64>,
    /// The 5-hour pace delta (see [`chips::pace_text`]).
    pub pace: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    /// No 5-hour window is active: "5h · idle" in place of the countdown.
    pub idle: bool,
}

impl PillModel {
    /// Nothing polled yet.
    pub const EMPTY: PillModel = PillModel {
        pct: None,
        pace: None,
        resets_at: None,
        idle: false,
    };
}

/// The countdown's place while no 5-hour window is active.
pub const IDLE_COUNTDOWN: &str = "5h · idle";

/// One account's piece of the pill: its 5-hour window, plus the weekly
/// figures the tooltip lists.
#[derive(Debug, Clone, PartialEq)]
pub struct PillSegment {
    /// The account's series color.
    pub color: Color32,
    /// The account's label.
    pub label: String,
    /// The 5-hour utilization.
    pub pct: Option<f64>,
    /// The 5-hour state level ([`analytics::color_for`] of `pct`).
    pub level: Level,
    pub resets_at: Option<DateTime<Utc>>,
    /// The 5-hour pace delta (see [`chips::pace_text`]); `None` without a %.
    pub pace: Option<f64>,
    /// No 5-hour window is active: "5h · idle" in place of the countdown.
    pub idle: bool,
    /// The weekly windows for the tooltip: ("7-day all", %), ("Fable", %).
    pub weekly: Vec<(&'static str, Option<f64>)>,
}

impl PillSegment {
    /// The segment for one account's stat chips (5-hour first, see
    /// [`chips::chips`]); `None` without chips (nothing polled yet).
    pub fn from_chips(chips: &[Chip], color: Color32, label: &str) -> Option<PillSegment> {
        let five = chips.first()?;
        Some(PillSegment {
            color,
            label: label.to_owned(),
            pct: five.pct,
            level: analytics::color_for(five.pct),
            resets_at: five.resets_at,
            pace: five.pace.filter(|_| five.pct.is_some()),
            idle: five.idle,
            weekly: chips[1..].iter().map(|c| (c.label, c.pct)).collect(),
        })
    }
}

/// The pill's model: the first segment's 5-hour window (one account is
/// drawn, for now).
pub fn model(segments: &[PillSegment]) -> PillModel {
    segments
        .first()
        .map(|s| PillModel {
            pct: s.pct,
            pace: s.pace.filter(|_| s.pct.is_some()),
            resets_at: s.resets_at,
            idle: s.idle,
        })
        .unwrap_or(PillModel::EMPTY)
}

/// "5h · 1h59m": the countdown to the 5-hour reset, compact ("5h · 10m"
/// under an hour, "5h · —" when unknown, "5h · now" once reached).
pub fn countdown_text(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let full = analytics::humanize_until(resets_at, now).replace(' ', "");
    let short = match full.strip_prefix("0h") {
        Some(minutes) => match minutes.trim_start_matches('0') {
            "m" => "0m".to_owned(),
            m => m.to_owned(),
        },
        None => full,
    };
    format!("5h · {short}")
}

/// The hover text, two lines (the tooltip has to fit the 44-point window):
/// the 5-hour line with pace and reset, then the weekly windows (or the
/// last poll's failure).
pub fn tooltip_lines_in<Tz: TimeZone>(
    segments: &[PillSegment],
    failed: bool,
    now: DateTime<Utc>,
    tz: &Tz,
) -> [String; 2]
where
    Tz::Offset: Display,
{
    let Some(five) = segments.first() else {
        let second = if failed {
            "Last poll failed · click to see why"
        } else {
            "Waiting for the first poll"
        };
        return ["5-hour —".to_owned(), second.to_owned()];
    };
    let mut first = format!("5-hour {}", chips::pct_text(five.pct));
    if let Some(p) = five.pace.filter(|_| five.pct.is_some()) {
        first.push_str(&format!(" · {}", chips::pace_text(p)));
    }
    if let Some(r) = five.resets_at {
        first.push_str(&format!(" · {}", chips::reset_tip_in(r, now, tz)));
    }
    if five.idle {
        first.push_str(" · no active window");
    }
    let second = if failed {
        "Last poll failed · click to see why".to_owned()
    } else {
        five.weekly
            .iter()
            .map(|(label, pct)| format!("{label} {}", chips::pct_text(*pct)))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    [first, second]
}

/// The ring's arc for `pct`: (start, end) angles in radians, screen
/// coordinates (y down, so increasing angles run clockwise), starting at 12
/// o'clock. `None` when unknown or empty; a full circle from 100 %.
pub fn arc_angles(pct: Option<f64>) -> Option<(f32, f32)> {
    let share = (pct? / 100.0).clamp(0.0, 1.0) as f32;
    (share > 0.0).then(|| {
        let start = -std::f32::consts::FRAC_PI_2;
        (start, start + std::f32::consts::TAU * share)
    })
}

/// Points along the arc from `start` to `end` at `radius` around `center`,
/// about one per 6 degrees (at least two).
pub fn arc_points(center: Pos2, radius: f32, (start, end): (f32, f32)) -> Vec<Pos2> {
    let steps = (((end - start).abs() / 6f32.to_radians()).ceil() as usize).max(1);
    (0..=steps)
        .map(|i| {
            let a = start + (end - start) * i as f32 / steps as f32;
            center + radius * vec2(a.cos(), a.sin())
        })
        .collect()
}

/// The measured widths of the pill's text pieces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Widths {
    pub pct: f32,
    pub countdown: f32,
    pub pace: Option<f32>,
}

/// Where each piece goes (x from the pill's left edge). A piece that does
/// not fit before the dot is dropped: the pace first, then the countdown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slots {
    pub ring_center: f32,
    pub pct: f32,
    pub countdown: Option<f32>,
    pub pace: Option<f32>,
    pub dot_center: f32,
}

/// Lay the pieces out left to right in `width`, the dot at the right end.
pub fn slots(width: f32, w: &Widths) -> Slots {
    let ring_center = PAD_X + RING_R;
    let pct = ring_center + RING_R + GAP;
    let dot_center = width - PAD_X - DOT_R;
    let limit = dot_center - DOT_R - GAP;
    let countdown_x = pct + w.pct + GAP + 2.0;
    let countdown_end = countdown_x + w.countdown;
    let countdown = (countdown_end <= limit).then_some(countdown_x);
    let pace = match (countdown, w.pace) {
        (Some(_), Some(pw)) => {
            let x = countdown_end + GAP;
            (x + pw <= limit).then_some(x)
        }
        _ => None,
    };
    Slots {
        ring_center,
        pct,
        countdown,
        pace,
        dot_center,
    }
}

/// What the user asked the pill for this frame. Moving the window is done
/// in place ([`ViewportCommand::StartDrag`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillAction {
    None,
    Expand,
    Refresh,
    /// Expand and open the settings panel.
    Settings,
    Quit,
}

/// The pill's per-window state the app keeps.
#[derive(Debug, Default)]
pub struct PillState {
    /// The right-click action row is showing.
    pub menu_open: bool,
}

// --- Painting -----------------------------------------------------------------

/// Draw the pill into the whole of `ui` and handle its input. `segments`
/// are the accounts' 5-hour windows (empty before the first poll; only the
/// first is drawn, for now), `failed` whether
/// the last poll failed, `pulse` the near-cap pulse level, `refresh_ready`
/// whether a manual refresh is allowed now.
pub fn show_pill(
    ui: &mut Ui,
    state: &mut PillState,
    segments: &[PillSegment],
    failed: bool,
    pulse: f32,
    refresh_ready: bool,
) -> PillAction {
    let now = Utc::now();
    let rect = ui.max_rect();
    let palette = Palette::for_visuals(ui.visuals());
    let ctx = ui.ctx().clone();
    let response = ui.interact(rect, Id::new("pill"), Sense::click_and_drag());

    if response.drag_started() {
        // winit only starts a move loop for the focused window.
        if ctx.input(|i| i.viewport().focused) != Some(true) {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        state.menu_open = false;
    }
    let lost_focus = ctx.input(|i| i.viewport().focused) == Some(false);
    if state.menu_open && (lost_focus || ctx.input(|i| i.key_pressed(egui::Key::Escape))) {
        state.menu_open = false;
    }

    let mut action = PillAction::None;
    if response.secondary_clicked() {
        state.menu_open = !state.menu_open;
    } else if response.clicked() {
        if state.menu_open {
            state.menu_open = false;
        } else {
            action = PillAction::Expand;
        }
    }

    if state.menu_open {
        if let Some(a) = action_row(ui, rect, refresh_ready) {
            state.menu_open = false;
            action = a;
        }
        return action;
    }

    let model = model(segments);
    paint_stats(ui, rect, &model, failed, pulse, palette, now);
    if !response.dragged() {
        let lines = tooltip_lines_in(segments, failed, now, &Local);
        response.on_hover_ui(|ui| {
            for line in lines {
                ui.label(RichText::new(line).small());
            }
        });
    }
    action
}

/// The right-click actions, across the pill; the one clicked, if any.
fn action_row(ui: &mut Ui, rect: Rect, refresh_ready: bool) -> Option<PillAction> {
    let items = [
        ("Expand", PillAction::Expand, true),
        ("Refresh", PillAction::Refresh, refresh_ready),
        ("Settings", PillAction::Settings, true),
        ("Quit", PillAction::Quit, true),
    ];
    let inner = rect.shrink2(vec2(6.0, 7.0));
    let gap = 4.0;
    let w = (inner.width() - gap * (items.len() - 1) as f32) / items.len() as f32;
    let mut picked = None;
    for (i, (label, a, enabled)) in items.into_iter().enumerate() {
        let x = inner.left() + i as f32 * (w + gap);
        let slot = Rect::from_min_size(pos2(x, inner.top()), vec2(w, inner.height()));
        let button = egui::Button::new(RichText::new(label).size(12.5));
        // Disabled (dimmed, no click) while not allowed: the refresh cooldown.
        let clicked = ui
            .add_enabled_ui(enabled, |ui| ui.put(slot, button))
            .inner
            .clicked();
        if clicked {
            picked = Some(a);
        }
    }
    picked
}

fn paint_stats(
    ui: &Ui,
    rect: Rect,
    m: &PillModel,
    failed: bool,
    pulse: f32,
    palette: &Palette,
    now: DateTime<Utc>,
) {
    let ctx = ui.ctx();
    let painter = ui.painter();
    let level = analytics::color_for(m.pct);
    let color = motion::eased(ctx, Id::new("pill-color"), palette.level(level));
    let pct = m.pct.map(|p| motion::eased(ctx, Id::new("pill-pct"), p));

    let pct_font = theme::semibold(PCT_SIZE);
    let (pct_galley, pct_w) =
        digits::number_galley(painter, &chips::pct_text(pct), pct_font, color);
    let countdown = painter.layout_no_wrap(
        if m.idle {
            IDLE_COUNTDOWN.to_owned()
        } else {
            countdown_text(m.resets_at, now)
        },
        FontId::proportional(COUNTDOWN_SIZE),
        palette.text_muted,
    );
    let pace: Option<Arc<Galley>> = m.pace.map(|d| {
        painter.layout_no_wrap(
            chips::pace_text(d),
            FontId::proportional(PACE_SIZE),
            chips::pace_color(chips::pace_tone(d), palette),
        )
    });
    let s = slots(
        rect.width(),
        &Widths {
            pct: pct_w,
            countdown: countdown.size().x,
            pace: pace.as_ref().map(|g| g.size().x),
        },
    );
    let cy = rect.center().y;
    let at = |x: f32| rect.left() + x;

    // Ring: track, then the arc in the state color.
    let center = pos2(at(s.ring_center), cy);
    let r = RING_R - RING_W / 2.0;
    painter.circle_stroke(center, r, Stroke::new(RING_W, palette.track));
    if let Some(angles) = arc_angles(pct) {
        painter.line(arc_points(center, r, angles), Stroke::new(RING_W, color));
    }

    let left_mid = |x: f32, g: &Arc<Galley>| pos2(at(x), cy - g.size().y / 2.0);
    painter.galley(left_mid(s.pct, &pct_galley), pct_galley, color);
    if let Some(x) = s.countdown {
        painter.galley(left_mid(x, &countdown), countdown, palette.text_muted);
    }
    if let (Some(x), Some(g)) = (s.pace, pace) {
        painter.galley(left_mid(x, &g), g, palette.text_muted);
    }

    paint_dot(
        painter,
        pos2(at(s.dot_center), cy),
        color,
        level,
        failed,
        pulse,
    );
}

/// The state dot: filled in the state color, a hollow ring after a failed
/// poll, with a breathing halo while `pulse` (near the cap) is up.
fn paint_dot(
    painter: &egui::Painter,
    center: Pos2,
    color: Color32,
    level: Level,
    failed: bool,
    pulse: f32,
) {
    if pulse > 0.0 {
        let halo = Shadow {
            offset: [0, 0],
            blur: 10,
            spread: 1,
            color: color.gamma_multiply(0.6 * pulse.min(1.0)),
        };
        let r = Rect::from_center_size(center, vec2(2.0 * DOT_R, 2.0 * DOT_R));
        painter.add(halo.as_shape(r, DOT_R as u8));
    }
    if failed || level == Level::Dim {
        painter.circle_stroke(center, DOT_R - 0.75, Stroke::new(1.5, color));
    } else {
        let r = DOT_R * (1.0 + 0.15 * pulse.clamp(0.0, 1.0));
        painter.circle_filled(center, r, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use std::f32::consts::{FRAC_PI_2, PI, TAU};

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_791_000_000 + secs, 0).unwrap()
    }

    fn chip(label: &'static str, pct: Option<f64>, pace: Option<f64>) -> Chip {
        Chip {
            label,
            pct,
            pace,
            resets_at: Some(t(7_140)),
            pulses: label == "5-hour",
            idle: false,
        }
    }

    /// One account's segment from its chips.
    fn seg(chips: &[Chip]) -> Vec<PillSegment> {
        PillSegment::from_chips(chips, Color32::WHITE, "me")
            .into_iter()
            .collect()
    }

    #[test]
    fn segment_is_the_five_hour_chip_plus_weekly() {
        let chips = [
            chip("5-hour", Some(46.0), Some(6.2)),
            chip("7-day all", Some(13.0), None),
        ];
        let s = &seg(&chips)[0];
        assert_eq!(s.label, "me");
        assert_eq!(s.level, analytics::color_for(Some(46.0)));
        assert_eq!(s.weekly, [("7-day all", Some(13.0))]);
        assert!(PillSegment::from_chips(&[], Color32::WHITE, "me").is_none());
    }

    #[test]
    fn model_is_the_five_hour_chip() {
        let chips = [
            chip("5-hour", Some(46.0), Some(6.2)),
            chip("7-day all", Some(13.0), None),
        ];
        assert_eq!(
            model(&seg(&chips)),
            PillModel {
                pct: Some(46.0),
                pace: Some(6.2),
                resets_at: Some(t(7_140)),
                idle: false,
            }
        );
        assert_eq!(model(&[]), PillModel::EMPTY);
        // No % → no pace either.
        assert_eq!(model(&seg(&[chip("5-hour", None, Some(3.0))])).pace, None);
    }

    #[test]
    fn idle_pill_has_no_pace_or_reset() {
        let idle = Chip {
            pct: Some(0.0),
            pace: None,
            resets_at: None,
            idle: true,
            ..chip("5-hour", None, None)
        };
        let m = model(&seg(std::slice::from_ref(&idle)));
        assert!(m.idle);
        assert_eq!((m.pct, m.pace, m.resets_at), (Some(0.0), None, None));
        let utc = FixedOffset::east_opt(0).unwrap();
        let [a, _] = tooltip_lines_in(&seg(&[idle]), false, t(0), &utc);
        assert_eq!(a, "5-hour 0% · no active window");
    }

    #[test]
    fn countdown_is_compact() {
        assert_eq!(countdown_text(Some(t(7_140)), t(0)), "5h · 1h59m");
        assert_eq!(countdown_text(Some(t(600)), t(0)), "5h · 10m");
        assert_eq!(countdown_text(Some(t(300)), t(0)), "5h · 5m");
        assert_eq!(countdown_text(Some(t(30)), t(0)), "5h · 0m");
        assert_eq!(countdown_text(Some(t(3_600 * 30)), t(0)), "5h · 1d6h");
        assert_eq!(countdown_text(None, t(0)), "5h · —");
        assert_eq!(countdown_text(Some(t(0)), t(10)), "5h · now");
    }

    #[test]
    fn tooltip_has_two_lines_of_full_stats() {
        let utc = FixedOffset::east_opt(0).unwrap();
        let chips = [
            chip("5-hour", Some(46.0), Some(6.0)),
            chip("7-day all", Some(13.4), None),
            chip("Fable", Some(0.0), None),
        ];
        let segs = seg(&chips);
        let [a, b] = tooltip_lines_in(&segs, false, t(0), &utc);
        assert!(a.starts_with("5-hour 46% · ▲+6 · resets "), "{a}");
        assert!(a.ends_with("· in 1h 59m"), "{a}");
        assert_eq!(b, "7-day all 13% · Fable 0%");
        let [_, b] = tooltip_lines_in(&segs, true, t(0), &utc);
        assert_eq!(b, "Last poll failed · click to see why");
        let [a, b] = tooltip_lines_in(&[], false, t(0), &utc);
        assert_eq!(
            (a.as_str(), b.as_str()),
            ("5-hour —", "Waiting for the first poll")
        );
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn ring_arc_runs_clockwise_from_twelve() {
        assert_eq!(arc_angles(None), None);
        assert_eq!(arc_angles(Some(0.0)), None);
        let (s, e) = arc_angles(Some(25.0)).unwrap();
        assert!(close(s, -FRAC_PI_2) && close(e, 0.0), "{s} {e}");
        let (_, e) = arc_angles(Some(50.0)).unwrap();
        assert!(close(e, FRAC_PI_2));
        let (s, e) = arc_angles(Some(130.0)).unwrap();
        assert!(close(e - s, TAU), "clamped to a full circle");
        let (s, e) = arc_angles(Some(-5.0)).unwrap_or((0.0, 0.0));
        assert_eq!((s, e), (0.0, 0.0), "negative → no arc");

        // Points: from 12 o'clock to 3 o'clock for 25 % (y down).
        let pts = arc_points(Pos2::ZERO, 10.0, (-FRAC_PI_2, 0.0));
        assert!(pts.len() >= 15);
        let (first, last) = (pts[0], *pts.last().unwrap());
        assert!(close(first.x, 0.0) && close(first.y, -10.0), "{first:?}");
        assert!(close(last.x, 10.0) && close(last.y, 0.0), "{last:?}");
        // A tiny arc still has two points.
        assert_eq!(arc_points(Pos2::ZERO, 10.0, (0.0, 0.001)).len(), 2);
        assert!(arc_points(Pos2::ZERO, 10.0, (-FRAC_PI_2, -FRAC_PI_2 + PI)).len() >= 30);
    }

    #[test]
    fn slots_flow_left_and_drop_pace_then_countdown() {
        let w = Widths {
            pct: 46.0,
            countdown: 70.0,
            pace: Some(28.0),
        };
        let s = slots(300.0, &w);
        assert_eq!(s.ring_center, PAD_X + RING_R);
        assert_eq!(s.pct, PAD_X + 2.0 * RING_R + GAP);
        let cd = s.countdown.unwrap();
        assert_eq!(cd, s.pct + 46.0 + GAP + 2.0);
        assert_eq!(s.pace, Some(cd + 70.0 + GAP));
        assert_eq!(s.dot_center, 300.0 - PAD_X - DOT_R);
        assert!(s.pace.unwrap() + 28.0 <= s.dot_center - DOT_R - GAP);

        // Too narrow for the pace; narrower still, for the countdown.
        let s = slots(220.0, &w);
        assert!(s.countdown.is_some() && s.pace.is_none());
        let s = slots(150.0, &w);
        assert_eq!((s.countdown, s.pace), (None, None));
        // No pace to show.
        let s = slots(300.0, &Widths { pace: None, ..w });
        assert_eq!(s.pace, None);
    }

    /// The widest real strings at the real fonts fit the 300-point pill
    /// with every piece shown.
    #[test]
    fn widest_text_fits_the_pill() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut fitted = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let p = ui.painter();
            let (_, pct) =
                digits::number_galley(p, "100%", theme::semibold(PCT_SIZE), Color32::WHITE);
            let width = |text: &str, size: f32| {
                p.layout_no_wrap(text.to_owned(), FontId::proportional(size), Color32::WHITE)
                    .size()
                    .x
            };
            let w = Widths {
                pct,
                countdown: width("5h · 4h59m", COUNTDOWN_SIZE),
                pace: Some(width("▲+100", PACE_SIZE)),
            };
            fitted = Some(slots(super::super::window::PILL_SIZE.x, &w));
        });
        out.textures_delta.clear();
        let s = fitted.unwrap();
        assert!(s.countdown.is_some() && s.pace.is_some(), "{s:?}");
    }
}
