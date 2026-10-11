//! Motion: values that ease to new targets when a poll arrives, the
//! projection's draw-in, and the slow pulse near the 5-hour cap.
//!
//! Everything here is a small state machine driven by an [`Instant`] the
//! caller passes in, so the schedules are tested without egui. The egui
//! glue at the bottom keeps the states in the context's temp memory, reads
//! the "Reduce motion" preference the app sets each frame, and asks for
//! repaints only while something is moving: targets change only when a
//! poll arrives, so every animation settles and the repaints stop.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use eframe::egui::{Color32, Context, Id};

/// How long an eased value takes to reach a new target.
pub const EASE: Duration = Duration::from_millis(450);
/// How long the projection line takes to draw in.
pub const DRAW_IN: Duration = Duration::from_millis(600);
/// A projection end moving more than this many points draws the line in
/// again.
pub const REDRAW_POINTS: f64 = 5.0;
/// The 5-hour % at and above which the chip (and the pill's dot) pulse.
pub const PULSE_FROM_PCT: f64 = 90.0;
/// Once pulsed, the 5-hour % must drop below this (or the window change)
/// before crossing [`PULSE_FROM_PCT`] pulses again, so hovering around 90
/// doesn't replay the breaths.
pub const PULSE_REARM_BELOW_PCT: f64 = 88.0;
/// An eased value not drawn for longer than this (the window was hidden
/// or minimized, a section was collapsed) snaps to its new value instead
/// of gliding from a stale one. The 1 s tick keeps a shown value well
/// inside it.
pub const HIDDEN_GAP: Duration = Duration::from_secs(2);
/// One pulse, trough to trough.
pub const PULSE_PERIOD: Duration = Duration::from_millis(2_000);
/// Pulses shown when the threshold is first crossed, before holding steady.
pub const PULSE_COUNT: u32 = 3;
/// Repaint cadence while pulsing (~15 fps is plenty for a 2 s breath).
pub const PULSE_FRAME: Duration = Duration::from_millis(66);
/// The pulse's lowest emphasis; 1.0 is the steady "near cap" emphasis.
pub const PULSE_TROUGH: f32 = 0.25;

/// Ease-out cubic on `t` in 0..=1 (clamped): fast start, gentle landing.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Share of `duration` elapsed from `start` to `now`, 0..=1.
fn progress(start: Instant, now: Instant, duration: Duration) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }
    (now.saturating_duration_since(start).as_secs_f32() / duration.as_secs_f32()).min(1.0)
}

/// A value that can be blended between two of its kind.
pub trait Lerp: Copy + PartialEq {
    fn lerp(a: Self, b: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        a + (b - a) * t
    }
}

impl Lerp for f64 {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        a + (b - a) * f64::from(t)
    }
}

impl Lerp for Color32 {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        a.lerp_to_gamma(b, t)
    }
}

/// One eased value: snaps on its first value and after going undrawn for
/// more than [`HIDDEN_GAP`], otherwise glides from where it is to each new
/// target over [`EASE`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tween<T> {
    from: T,
    to: T,
    /// When the current glide began; `None` once settled.
    start: Option<Instant>,
    /// When this was last drawn.
    last_seen: Instant,
}

impl<T: Lerp> Tween<T> {
    /// A tween resting at `value`, drawn at `now`.
    pub fn new(value: T, now: Instant) -> Self {
        Self {
            from: value,
            to: value,
            start: None,
            last_seen: now,
        }
    }

    /// Point the tween at `target` at `now` and return the value to show.
    /// A new target starts a glide from the current value; the same target
    /// keeps going (or stays put). `animate == false` (reduce motion) or
    /// more than [`HIDDEN_GAP`] since the last draw (the window was hidden,
    /// a section collapsed) snaps.
    pub fn update(&mut self, target: T, now: Instant, animate: bool) -> T {
        let hidden = now.saturating_duration_since(self.last_seen) > HIDDEN_GAP;
        self.last_seen = now;
        if !animate || hidden {
            *self = Self::new(target, now);
        } else if target != self.to {
            self.from = self.value(now);
            self.to = target;
            self.start = Some(now);
        }
        self.value(now)
    }

    /// The value at `now`: exactly the target once the glide is over.
    pub fn value(&self, now: Instant) -> T {
        match self.start {
            Some(start) if progress(start, now, EASE) < 1.0 => {
                T::lerp(self.from, self.to, ease_out(progress(start, now, EASE)))
            }
            _ => self.to,
        }
    }

    /// Still gliding at `now` (so another frame is needed).
    pub fn animating(&self, now: Instant) -> bool {
        self.start
            .is_some_and(|start| progress(start, now, EASE) < 1.0)
    }
}

/// The projection line's draw-in: from nothing to full over [`DRAW_IN`]
/// the first time a projection appears, and again when a new poll moves
/// its end by more than [`REDRAW_POINTS`] from where the previous poll put
/// it. Between polls the end drifts with the clock; that never redraws.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DrawIn {
    /// The poll the end was last taken from (its identity, e.g. fetch
    /// time in ms).
    last_poll: Option<i64>,
    last_end: Option<f64>,
    start: Option<Instant>,
}

impl DrawIn {
    /// Feed the projection's end % as of `poll` (an identity that changes
    /// with each poll) and get the share of the line to draw, 0..=1, eased.
    /// The end is compared only when `poll` changes. `animate == false`
    /// always draws it whole.
    pub fn update(&mut self, poll: i64, end_pct: f64, now: Instant, animate: bool) -> f32 {
        let new_poll = self.last_poll != Some(poll);
        let moved = new_poll
            && self
                .last_end
                .is_none_or(|last| (end_pct - last).abs() > REDRAW_POINTS);
        if new_poll {
            self.last_poll = Some(poll);
            self.last_end = Some(end_pct);
        }
        if !animate {
            self.start = None;
        } else if moved {
            self.start = Some(now);
        }
        self.share(now)
    }

    /// The eased share drawn at `now`.
    pub fn share(&self, now: Instant) -> f32 {
        self.start
            .map_or(1.0, |start| ease_out(progress(start, now, DRAW_IN)))
    }

    /// Still drawing at `now`.
    pub fn animating(&self, now: Instant) -> bool {
        self.start
            .is_some_and(|start| progress(start, now, DRAW_IN) < 1.0)
    }
}

/// The near-cap emphasis to draw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pulse {
    /// 0 = none (below the threshold); [`PULSE_TROUGH`]..=1 while pulsing;
    /// 1 = the steady "near cap" emphasis.
    pub level: f32,
    /// Changing over time, so frames are needed to show it.
    pub moving: bool,
}

impl Pulse {
    pub const NONE: Pulse = Pulse {
        level: 0.0,
        moving: false,
    };
    pub const STEADY: Pulse = Pulse {
        level: 1.0,
        moving: false,
    };
}

/// The pulse `elapsed` after the threshold was crossed: [`PULSE_COUNT`]
/// breaths (each trough → peak → trough over [`PULSE_PERIOD`]) rising from
/// the trough, then the steady emphasis; or breathing for good with
/// `keep_pulsing`. The last pulse ends on its peak, so the hold continues
/// it without a jump.
pub fn pulse_at(elapsed: Duration, keep_pulsing: bool) -> Pulse {
    let cycles = elapsed.as_secs_f32() / PULSE_PERIOD.as_secs_f32();
    // Peaks fall at 0.5, 1.5, 2.5 … cycles.
    let last_peak = PULSE_COUNT as f32 - 0.5;
    if !keep_pulsing && cycles >= last_peak {
        return Pulse::STEADY;
    }
    let breath = 0.5 - 0.5 * (std::f32::consts::TAU * cycles).cos();
    Pulse {
        level: PULSE_TROUGH + (1.0 - PULSE_TROUGH) * breath,
        moving: true,
    }
}

/// Tracks when the 5-hour % crossed [`PULSE_FROM_PCT`], so the pulse runs
/// once per crossing. Owned by the app; the chip and the pill's state dot
/// draw the same [`Pulse`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PulseTracker {
    /// When the current crossing began; kept until re-armed.
    crossed_at: Option<Instant>,
    /// The 5-hour window (its reset time) last seen.
    window: Option<DateTime<Utc>>,
}

impl PulseTracker {
    /// The pulse for the 5-hour `pct` in `window` (its reset time) at
    /// `now`. Re-arms once the % drops below [`PULSE_REARM_BELOW_PCT`] or
    /// the window changes, so wobbling around 90 doesn't replay the
    /// breaths. `reduce_motion` holds the steady emphasis without pulsing.
    pub fn update(
        &mut self,
        pct: Option<f64>,
        window: Option<DateTime<Utc>>,
        now: Instant,
        keep_pulsing: bool,
        reduce_motion: bool,
    ) -> Pulse {
        if window != self.window || pct.is_none_or(|p| p < PULSE_REARM_BELOW_PCT) {
            self.crossed_at = None;
        }
        self.window = window;
        if pct.is_none_or(|p| p < PULSE_FROM_PCT) {
            return Pulse::NONE;
        }
        let crossed = *self.crossed_at.get_or_insert(now);
        if reduce_motion {
            return Pulse::STEADY;
        }
        pulse_at(now.saturating_duration_since(crossed), keep_pulsing)
    }
}

/// When to paint the next pulse frame: only while it moves and the window
/// is visible.
pub fn pulse_repaint(pulse: Pulse, visible: bool) -> Option<Duration> {
    (pulse.moving && visible).then_some(PULSE_FRAME)
}

// --- egui glue -----------------------------------------------------------------

/// The motion preferences, kept in egui's temp memory by the app each frame.
#[derive(Debug, Clone, Copy, Default)]
struct Prefs {
    reduce_motion: bool,
}

fn prefs_id() -> Id {
    Id::new("motion::prefs")
}

/// Record the "Reduce motion" setting for this frame's widgets.
pub fn set_reduce_motion(ctx: &Context, reduce: bool) {
    ctx.data_mut(|d| {
        d.insert_temp(
            prefs_id(),
            Prefs {
                reduce_motion: reduce,
            },
        )
    });
}

/// Whether eased values and draw-ins may move this frame.
pub fn animate_enabled(ctx: &Context) -> bool {
    !ctx.data(|d| d.get_temp::<Prefs>(prefs_id()))
        .unwrap_or_default()
        .reduce_motion
}

/// `target` eased per `id`: snaps the first time `id` is drawn (and after
/// [`HIDDEN_GAP`] undrawn), glides to each new target, and keeps frames
/// coming only until it lands.
pub fn eased<T>(ctx: &Context, id: Id, target: T) -> T
where
    T: Lerp + Send + Sync + 'static,
{
    let now = Instant::now();
    let animate = animate_enabled(ctx);
    let (value, moving) = ctx.data_mut(|d| {
        let tween = d.get_temp_mut_or_insert_with(id, || Tween::new(target, now));
        let value = tween.update(target, now, animate);
        (value, tween.animating(now))
    });
    if moving {
        ctx.request_repaint();
    }
    value
}

/// The share of the projection line to draw for its end `end_pct` as of
/// `poll` (see [`DrawIn`]).
pub fn draw_in(ctx: &Context, id: Id, poll: i64, end_pct: f64) -> f32 {
    let now = Instant::now();
    let animate = animate_enabled(ctx);
    let (share, moving) = ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_insert_with(id, DrawIn::default);
        let share = state.update(poll, end_pct, now, animate);
        (share, state.animating(now))
    });
    if moving {
        ctx.request_repaint();
    }
    share
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn ease_out_ends_and_shape() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_out(-1.0), 0.0, "clamped below");
        assert_eq!(ease_out(2.0), 1.0, "clamped above");
        // Ease-out: ahead of linear in the middle, monotonic.
        assert!(ease_out(0.5) > 0.5);
        let mut last = 0.0;
        for i in 1..=100 {
            let v = ease_out(i as f32 / 100.0);
            assert!(v >= last);
            last = v;
        }
    }

    #[test]
    fn tween_snaps_on_its_first_value() {
        let t0 = Instant::now();
        let mut tw = Tween::new(46.0_f64, t0);
        assert_eq!(tw.update(46.0, t0, true), 46.0);
        assert!(!tw.animating(t0));
    }

    #[test]
    fn tween_same_target_never_animates() {
        let t0 = Instant::now();
        let mut tw = Tween::new(46.0_f64, t0);
        for i in 1..10 {
            let now = t0 + ms(16 * i);
            assert_eq!(tw.update(46.0, now, true), 46.0);
            assert!(!tw.animating(now));
        }
    }

    #[test]
    fn tween_glides_and_settles_exactly_on_target() {
        let t0 = Instant::now();
        let mut tw = Tween::new(40.0_f64, t0);
        // A poll moves the target: starts at the old value.
        assert_eq!(tw.update(52.3, t0, true), 40.0);
        assert!(tw.animating(t0));
        let mid = tw.update(52.3, t0 + ms(225), true);
        assert!(
            mid > 40.0 + 6.15,
            "ease-out is past halfway at half time: {mid}"
        );
        assert!(mid < 52.3);
        let end = t0 + EASE;
        assert_eq!(tw.update(52.3, end, true), 52.3, "exactly the target");
        assert!(!tw.animating(end));
        assert_eq!(tw.update(52.3, end + ms(1_000), true), 52.3);
    }

    #[test]
    fn tween_retargets_from_where_it_is() {
        let t0 = Instant::now();
        let mut tw = Tween::new(0.0_f32, t0);
        tw.update(100.0, t0, true);
        let at = tw.update(100.0, t0 + ms(100), true);
        // New target mid-glide: continues from the current value.
        assert_eq!(tw.update(50.0, t0 + ms(100), true), at);
        assert_eq!(tw.update(50.0, t0 + ms(100) + EASE, true), 50.0);
    }

    #[test]
    fn tween_snaps_with_reduce_motion() {
        let t0 = Instant::now();
        let mut tw = Tween::new(10.0_f64, t0);
        assert_eq!(tw.update(80.0, t0, false), 80.0);
        assert!(!tw.animating(t0));
    }

    #[test]
    fn tween_snaps_after_being_hidden_and_glides_while_shown() {
        let t0 = Instant::now();
        let mut tw = Tween::new(10.0_f64, t0);
        // Drawn every second (the idle tick): a new value glides.
        assert_eq!(tw.update(10.0, t0 + ms(1_000), true), 10.0);
        assert_eq!(tw.update(20.0, t0 + ms(2_000), true), 10.0);
        assert!(tw.animating(t0 + ms(2_000)));
        // Undrawn for longer than HIDDEN_GAP (hidden to the tray): snaps.
        let back = t0 + ms(2_000) + HIDDEN_GAP + ms(1);
        assert_eq!(tw.update(70.0, back, true), 70.0);
        assert!(!tw.animating(back));
        // Exactly HIDDEN_GAP still glides.
        assert_eq!(tw.update(80.0, back + HIDDEN_GAP, true), 70.0);
        assert!(tw.animating(back + HIDDEN_GAP));
    }

    #[test]
    fn tween_colors_land_on_the_target() {
        let t0 = Instant::now();
        let mut tw = Tween::new(Color32::RED, t0);
        tw.update(Color32::GREEN, t0, true);
        assert_ne!(tw.value(t0 + ms(200)), Color32::GREEN);
        assert_eq!(tw.value(t0 + EASE), Color32::GREEN);
    }

    #[test]
    fn draw_in_on_first_projection_and_on_polls_that_move_it() {
        let t0 = Instant::now();
        let mut d = DrawIn::default();
        assert_eq!(
            d.update(1, 70.0, t0, true),
            0.0,
            "first projection draws in"
        );
        assert!(d.animating(t0));
        let half = d.update(1, 70.5, t0 + ms(300), true);
        assert!(half > 0.5 && half < 1.0, "{half}");
        assert_eq!(d.update(1, 71.0, t0 + DRAW_IN, true), 1.0);
        assert!(!d.animating(t0 + DRAW_IN));
        // The same poll drifting with the clock never redraws, however far.
        for i in 1..=40 {
            let end = 70.0 + 0.5 * i as f64;
            assert_eq!(d.update(1, end, t0 + DRAW_IN + ms(i * 16), true), 1.0);
        }
        // A new poll more than 5 points from the last poll's end redraws.
        let t1 = t0 + ms(10_000);
        assert_eq!(d.update(2, 75.5, t1, true), 0.0);
        assert_eq!(d.update(2, 75.5, t1 + DRAW_IN, true), 1.0);
        // A new poll exactly 5 points away does not.
        assert_eq!(d.update(3, 80.5, t1 + DRAW_IN + ms(16), true), 1.0);
        // Nor does a new poll that only matches the drift.
        assert_eq!(d.update(4, 82.0, t1 + DRAW_IN + ms(32), true), 1.0);
    }

    #[test]
    fn draw_in_whole_with_reduce_motion() {
        let t0 = Instant::now();
        let mut d = DrawIn::default();
        assert_eq!(d.update(1, 70.0, t0, false), 1.0);
        assert!(!d.animating(t0));
        assert_eq!(d.update(2, 90.0, t0 + ms(16), false), 1.0);
    }

    #[test]
    fn pulse_three_breaths_then_steady() {
        assert_eq!(pulse_at(Duration::ZERO, false).level, PULSE_TROUGH);
        let peaks: Vec<f32> = [1_000, 3_000]
            .iter()
            .map(|&m| pulse_at(ms(m), false).level)
            .collect();
        for p in peaks {
            assert!((p - 1.0).abs() < 1e-5, "peak {p}");
        }
        let trough = pulse_at(ms(2_000), false);
        assert!((trough.level - PULSE_TROUGH).abs() < 1e-5);
        assert!(trough.moving);
        // The third peak (5 s) ends it: steady from there, no more frames.
        assert!(pulse_at(ms(4_999), false).moving);
        assert_eq!(pulse_at(ms(5_000), false), Pulse::STEADY);
        assert_eq!(pulse_at(ms(60_000), false), Pulse::STEADY);
    }

    #[test]
    fn pulse_keeps_breathing_when_asked() {
        let p = pulse_at(ms(60_000), true);
        assert!(p.moving);
        assert!(
            (p.level - PULSE_TROUGH).abs() < 1e-4,
            "trough at 30 periods"
        );
        assert!((pulse_at(ms(61_000), true).level - 1.0).abs() < 1e-4);
    }

    fn window(h: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(1_791_000_000 + h * 3_600, 0)
    }

    #[test]
    fn tracker_pulses_once_per_crossing() {
        let t0 = Instant::now();
        let w = window(0);
        let mut tr = PulseTracker::default();
        assert_eq!(tr.update(Some(89.9), w, t0, false, false), Pulse::NONE);
        assert_eq!(tr.update(None, w, t0, false, false), Pulse::NONE);
        // Crossing starts the breaths from the trough.
        let first = tr.update(Some(90.0), w, t0 + ms(1), false, false);
        assert!(first.moving);
        assert_eq!(first.level, PULSE_TROUGH);
        // Higher polls do not restart it.
        assert!(
            tr.update(Some(95.0), w, t0 + ms(2_001), false, false)
                .moving
        );
        assert_eq!(
            tr.update(Some(96.0), w, t0 + ms(5_001), false, false),
            Pulse::STEADY
        );
    }

    #[test]
    fn tracker_rearms_below_88_or_in_a_new_window() {
        let t0 = Instant::now();
        let w = window(0);
        let mut tr = PulseTracker::default();
        tr.update(Some(91.0), w, t0, false, false);
        assert_eq!(
            tr.update(Some(91.0), w, t0 + ms(6_000), false, false),
            Pulse::STEADY
        );
        // Wobbling down to 89 and back: no emphasis below 90, no replay.
        assert_eq!(
            tr.update(Some(89.0), w, t0 + ms(7_000), false, false),
            Pulse::NONE
        );
        assert_eq!(
            tr.update(Some(88.0), w, t0 + ms(8_000), false, false),
            Pulse::NONE
        );
        assert_eq!(
            tr.update(Some(90.5), w, t0 + ms(9_000), false, false),
            Pulse::STEADY
        );
        // Below 88: re-armed, the next crossing pulses again.
        tr.update(Some(87.9), w, t0 + ms(10_000), false, false);
        assert!(
            tr.update(Some(90.0), w, t0 + ms(11_000), false, false)
                .moving
        );
        // A new window re-arms even without dropping below 88.
        assert_eq!(
            tr.update(Some(92.0), w, t0 + ms(20_000), false, false),
            Pulse::STEADY
        );
        let next = tr.update(Some(92.0), window(5), t0 + ms(21_000), false, false);
        assert!(next.moving);
        assert_eq!(next.level, PULSE_TROUGH);
    }

    #[test]
    fn tracker_reduce_motion_holds_steady() {
        let t0 = Instant::now();
        let w = window(0);
        let mut tr = PulseTracker::default();
        assert_eq!(tr.update(Some(93.0), w, t0, false, true), Pulse::STEADY);
        assert_eq!(tr.update(Some(93.0), w, t0, true, true), Pulse::STEADY);
        assert_eq!(tr.update(Some(50.0), w, t0, true, true), Pulse::NONE);
    }

    #[test]
    fn repaint_only_while_pulsing_and_visible() {
        let moving = pulse_at(ms(500), false);
        assert_eq!(pulse_repaint(moving, true), Some(PULSE_FRAME));
        assert_eq!(pulse_repaint(moving, false), None);
        assert_eq!(pulse_repaint(Pulse::STEADY, true), None);
        assert_eq!(pulse_repaint(Pulse::NONE, true), None);
    }
}
