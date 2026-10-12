//! Switching the one window between the full dashboard and the pill.
//!
//! [`switch`] is the state machine: it records the geometry of the mode
//! being left (full rect and maximized state, or pill position) in
//! [`Settings`], flips [`Settings::ui_mode`], and returns the viewport
//! commands that reshape the window, in the order the Task 0 spike proved
//! (each sequence takes effect in the frame after it is sent). Positions are
//! saved in pixels (see `ui::window`) and handed to egui as points at the
//! window's current scale.

use eframe::egui::{Pos2, Vec2, ViewportCommand, WindowLevel};

use crate::config::{Pos, Settings, UiMode};

use super::settings::window_level;
use super::window::{
    full_state_to_save, pill_pos_to_save, pill_start_pos, sane_pos, sane_size, TrackedWindow,
    FULL_MIN, PILL_SIZE,
};

/// How many passes after a switch the window may still report the old
/// mode's geometry (the pass that sends the commands paints before they
/// apply); see [`Settle`].
pub const SETTLE_PASSES: u8 = 1;

/// A pixel position as egui points at `px_per_point` (egui-winit multiplies
/// `OuterPosition` by the same scale, so the window lands on `p` exactly).
pub fn to_points(p: Pos, px_per_point: f32) -> Pos2 {
    let s = if px_per_point.is_finite() && px_per_point > 0.0 {
        px_per_point
    } else {
        1.0
    };
    Pos2::new(p.x as f32 / s, p.y as f32 / s)
}

/// Full → pill: un-maximize first if needed, drop the min size before
/// shrinking, then borderless, fixed, [`PILL_SIZE`], at `pos` (if known),
/// always on top.
pub fn to_pill_commands(pos: Option<Pos2>, unmaximize: bool) -> Vec<ViewportCommand> {
    let mut cmds = Vec::with_capacity(7);
    if unmaximize {
        cmds.push(ViewportCommand::Maximized(false));
    }
    cmds.extend([
        ViewportCommand::MinInnerSize(Vec2::ZERO),
        ViewportCommand::Decorations(false),
        ViewportCommand::Resizable(false),
        ViewportCommand::InnerSize(PILL_SIZE),
    ]);
    if let Some(p) = pos {
        cmds.push(ViewportCommand::OuterPosition(p));
    }
    cmds.push(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
    cmds
}

/// Pill → full: decorations, resizable, the Always on top window level, the full
/// min size, then the saved size and position (if known), and maximize
/// last so un-maximizing later returns to that normal rect.
pub fn to_full_commands(
    size: Vec2,
    pos: Option<Pos2>,
    maximized: bool,
    always_on_top: bool,
) -> Vec<ViewportCommand> {
    let mut cmds = vec![
        ViewportCommand::Decorations(true),
        ViewportCommand::Resizable(true),
        ViewportCommand::WindowLevel(window_level(always_on_top)),
        ViewportCommand::MinInnerSize(FULL_MIN),
        ViewportCommand::InnerSize(size),
    ];
    if let Some(p) = pos {
        cmds.push(ViewportCommand::OuterPosition(p));
    }
    if maximized {
        cmds.push(ViewportCommand::Maximized(true));
    }
    cmds
}

/// Switch `settings` to `to`, given the window as last seen in each mode
/// (`full`: the tracked full-mode window; `pill`: the last pill position)
/// at the window's current `px_per_point`. Returns the commands to send;
/// empty if already in `to`.
pub fn switch(
    settings: &mut Settings,
    to: UiMode,
    full: &TrackedWindow,
    pill: Option<Pos>,
    px_per_point: f32,
) -> Vec<ViewportCommand> {
    if settings.ui_mode == to {
        return Vec::new();
    }
    settings.ui_mode = to;
    match to {
        UiMode::Pill => {
            (settings.full_rect, settings.full_maximized) =
                full_state_to_save(UiMode::Full, full, settings);
            let pos = pill_start_pos(settings, px_per_point).map(|p| to_points(p, px_per_point));
            to_pill_commands(pos, full.maximized == Some(true))
        }
        UiMode::Full => {
            settings.pill_pos = pill_pos_to_save(UiMode::Pill, pill, settings);
            let pos = settings
                .full_rect
                .and_then(|r| sane_pos(r.pos))
                .map(|p| to_points(p, px_per_point));
            to_full_commands(
                sane_size(settings.full_rect),
                pos,
                settings.full_maximized,
                settings.always_on_top,
            )
        }
    }
}

/// The other mode.
pub fn other(mode: UiMode) -> UiMode {
    match mode {
        UiMode::Full => UiMode::Pill,
        UiMode::Pill => UiMode::Full,
    }
}

/// What a logic pass does with the window geometry it sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleStep {
    /// Still the old mode's geometry: don't record it.
    Skip,
    /// First pass in the new geometry: keep it on a monitor (Windows),
    /// don't record yet (the move lands next pass).
    Fixup,
    /// Normal: record it for the current mode.
    Track,
}

/// Counts the passes after a switch, so neither mode records the other's
/// geometry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settle {
    skip: u8,
    fixup: bool,
}

impl Settle {
    /// A switch was just sent.
    pub fn start(&mut self) {
        *self = Self {
            skip: SETTLE_PASSES,
            fixup: true,
        };
    }

    /// The step for this pass.
    pub fn step(&mut self) -> SettleStep {
        if self.skip > 0 {
            self.skip -= 1;
            SettleStep::Skip
        } else if std::mem::take(&mut self.fixup) {
            SettleStep::Fixup
        } else {
            SettleStep::Track
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SavedRect;
    use crate::ui::window::FULL_SIZE;

    fn rect(x: i32, y: i32, w: f32, h: f32) -> SavedRect {
        SavedRect {
            pos: Pos { x, y },
            w,
            h,
        }
    }

    #[test]
    fn to_pill_sequence_is_the_spike_order() {
        let p = Pos2::new(10.0, 20.0);
        assert_eq!(
            to_pill_commands(Some(p), false),
            vec![
                ViewportCommand::MinInnerSize(Vec2::ZERO),
                ViewportCommand::Decorations(false),
                ViewportCommand::Resizable(false),
                ViewportCommand::InnerSize(Vec2::new(300.0, 44.0)),
                ViewportCommand::OuterPosition(p),
                ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop),
            ]
        );
        let cmds = to_pill_commands(None, true);
        assert_eq!(cmds[0], ViewportCommand::Maximized(false));
        assert_eq!(cmds[1], ViewportCommand::MinInnerSize(Vec2::ZERO));
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, ViewportCommand::OuterPosition(_))));
        assert_eq!(cmds.len(), 6);
    }

    #[test]
    fn to_full_sequence_restores_decorations_level_and_rect() {
        let p = Pos2::new(-5.0, 7.0);
        let size = Vec2::new(500.0, 600.0);
        assert_eq!(
            to_full_commands(size, Some(p), false, false),
            vec![
                ViewportCommand::Decorations(true),
                ViewportCommand::Resizable(true),
                ViewportCommand::WindowLevel(WindowLevel::Normal),
                ViewportCommand::MinInnerSize(Vec2::new(360.0, 320.0)),
                ViewportCommand::InnerSize(size),
                ViewportCommand::OuterPosition(p),
            ]
        );
        let cmds = to_full_commands(size, None, true, true);
        assert_eq!(
            cmds[2],
            ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop)
        );
        assert_eq!(cmds.last(), Some(&ViewportCommand::Maximized(true)));
        assert_eq!(cmds.len(), 6);
    }

    #[test]
    fn pixels_become_points_at_the_current_scale() {
        assert_eq!(
            to_points(Pos { x: 450, y: -225 }, 2.25),
            Pos2::new(200.0, -100.0)
        );
        assert_eq!(to_points(Pos { x: 3, y: 4 }, 0.0), Pos2::new(3.0, 4.0));
        assert_eq!(to_points(Pos { x: 3, y: 4 }, f32::NAN), Pos2::new(3.0, 4.0));
    }

    #[test]
    fn entering_the_pill_records_the_full_window() {
        let mut s = Settings {
            full_rect: Some(rect(1, 2, 400.0, 400.0)),
            pill_pos: Some(Pos { x: 900, y: 450 }),
            ..Settings::default()
        };
        let seen = TrackedWindow {
            rect: Some(rect(100, 200, 500.0, 520.0)),
            maximized: Some(false),
        };
        let cmds = switch(&mut s, UiMode::Pill, &seen, None, 2.25);
        assert_eq!(s.ui_mode, UiMode::Pill);
        assert_eq!(s.full_rect, seen.rect);
        assert!(!s.full_maximized);
        assert_eq!(cmds, to_pill_commands(Some(Pos2::new(400.0, 200.0)), false));
        // Again: already there, nothing to do or record.
        let before = s.clone();
        assert!(switch(&mut s, UiMode::Pill, &TrackedWindow::default(), None, 1.0).is_empty());
        assert_eq!(s, before);
    }

    #[test]
    fn entering_the_pill_while_maximized_unmaximizes_and_remembers() {
        let normal = rect(100, 200, 500.0, 520.0);
        let mut s = Settings::default();
        let seen = TrackedWindow {
            rect: Some(normal),
            maximized: Some(true),
        };
        let cmds = switch(&mut s, UiMode::Pill, &seen, None, 1.0);
        assert_eq!((s.full_rect, s.full_maximized), (Some(normal), true));
        // No pill position yet: the full window's bottom-right corner.
        assert_eq!(cmds, to_pill_commands(Some(Pos2::new(300.0, 676.0)), true));
    }

    #[test]
    fn leaving_the_pill_records_its_position_and_restores_full() {
        let full = rect(100, 200, 500.0, 520.0);
        let mut s = Settings {
            ui_mode: UiMode::Pill,
            full_rect: Some(full),
            full_maximized: false,
            pill_pos: Some(Pos { x: 1, y: 1 }),
            always_on_top: true,
            ..Settings::default()
        };
        let pill_seen = Some(Pos { x: 2000, y: 1000 });
        let cmds = switch(
            &mut s,
            UiMode::Full,
            &TrackedWindow::default(),
            pill_seen,
            2.0,
        );
        assert_eq!(s.ui_mode, UiMode::Full);
        assert_eq!(s.pill_pos, pill_seen);
        assert_eq!(s.full_rect, Some(full));
        assert_eq!(
            cmds,
            to_full_commands(
                Vec2::new(500.0, 520.0),
                Some(Pos2::new(50.0, 100.0)),
                false,
                true
            )
        );

        // Pill never seen (hidden all along): the saved position stays; no
        // full rect yet: default size, wherever the window is.
        let mut s = Settings {
            ui_mode: UiMode::Pill,
            full_maximized: true,
            pill_pos: Some(Pos { x: 1, y: 1 }),
            ..Settings::default()
        };
        let cmds = switch(&mut s, UiMode::Full, &TrackedWindow::default(), None, 1.0);
        assert_eq!(s.pill_pos, Some(Pos { x: 1, y: 1 }));
        assert_eq!(cmds, to_full_commands(FULL_SIZE, None, true, false));
    }

    #[test]
    fn round_trip_keeps_both_geometries() {
        let mut s = Settings::default();
        let full_seen = TrackedWindow {
            rect: Some(rect(10, 10, 420.0, 460.0)),
            maximized: Some(false),
        };
        switch(&mut s, UiMode::Pill, &full_seen, None, 1.0);
        switch(
            &mut s,
            UiMode::Full,
            &full_seen,
            Some(Pos { x: 70, y: 80 }),
            1.0,
        );
        let cmds = switch(&mut s, UiMode::Pill, &full_seen, None, 1.0);
        assert_eq!(cmds, to_pill_commands(Some(Pos2::new(70.0, 80.0)), false));
        assert_eq!(s.full_rect, full_seen.rect);
        assert_eq!(other(other(UiMode::Pill)), UiMode::Pill);
    }

    #[test]
    fn settle_skips_the_old_geometry_then_fixes_up_once() {
        let mut settle = Settle::default();
        assert_eq!(settle.step(), SettleStep::Track);
        settle.start();
        assert_eq!(settle.step(), SettleStep::Skip);
        assert_eq!(settle.step(), SettleStep::Fixup);
        assert_eq!(settle.step(), SettleStep::Track);
        assert_eq!(settle.step(), SettleStep::Track);
    }
}
