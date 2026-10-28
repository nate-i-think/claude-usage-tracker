//! Window geometry we persist ourselves: the viewport builder for the saved
//! mode (built from `app.ron` read before eframe starts), the startup
//! placement, and the state recorded on save.
//!
//! eframe's own window persistence is off: it restores the window only
//! after the first paint (a visible flash when the mode differs) and clamps
//! restored sizes to 64 px, which a pill window is smaller than.
//!
//! # Units (the one place this is decided)
//! - **Positions** ([`Pos`], [`SavedRect::pos`]) are the window's outer
//!   top-left corner in **physical desktop pixels**. A point position
//!   depends on the scale of the monitor the window is on, and winit's
//!   builder converts it with whatever monitor the window happens to be
//!   created on (and egui converts it a second time after creation), so a
//!   window on a 100 % monitor next to a 225 % primary would come back in the
//!   wrong place. Pixels are unambiguous across monitors.
//! - **Sizes** ([`SavedRect::w`], [`SavedRect::h`]) are the inner (client)
//!   size in **egui points**, what the builder takes; winit keeps the
//!   logical size when the window moves between monitor scales.
//! - On Windows the builder gets no position: [`apply_placement`] moves the
//!   still-hidden window to the saved pixel position in `App::new` (the
//!   process is DPI-aware by then), keeps it on a monitor's work area, and
//!   maximizes it if it was. Elsewhere the builder gets the position as
//!   points (assumes scale 1) and `with_maximized`.
//! - At runtime (switching between full and pill, see `ui::mode`) a pixel
//!   position becomes an `OuterPosition` in points: divided by the window's
//!   current pixels-per-point, which egui-winit multiplies back by the same
//!   scale, so it is exact on any monitor.
//!
//! The pill window is built here too (borderless, fixed [`pill_size`],
//! always on top) when the app opens in pill mode, so the first frame is
//! already the pill: no flash.

use std::collections::HashMap;

use eframe::egui::{self, Pos2, Vec2, ViewportBuilder, ViewportInfo};

use crate::config::{Pos, SavedRect, Settings, UiMode};

use super::pill;
use super::settings::window_level;

/// eframe app id: names the storage directory and the window class.
pub const APP_ID: &str = "claude-usage";
pub const TITLE: &str = "Claude Usage Monitor";
/// eframe storage key for [`Settings`].
pub const SETTINGS_KEY: &str = "settings";
/// Full-mode window size on first run (inner, points).
pub const FULL_SIZE: Vec2 = Vec2::new(420.0, 760.0);
/// Smallest full-mode window (inner, points).
pub const FULL_MIN: Vec2 = Vec2::new(360.0, 320.0);
/// The one-account pill window's size (inner == outer: no decorations;
/// points).
const SINGLE_PILL: Vec2 = Vec2::new(300.0, 44.0);

/// The pill window's size for `segments` accounts (inner == outer: no
/// decorations; points): the one-account pill below two, else one
/// [`pill::SEGMENT_W`] per account, hairline gaps between them and the
/// state dot at the right end (see [`pill::multi_slots`]).
pub fn pill_size(segments: usize) -> Vec2 {
    if segments < 2 {
        return SINGLE_PILL;
    }
    let n = segments as f32;
    let fixed = 2.0 * pill::PAD_X + pill::GAP + 2.0 * pill::DOT_R;
    Vec2::new(
        fixed + n * pill::SEGMENT_W + (n - 1.0) * pill::DIVIDER_SPAN,
        SINGLE_PILL.y,
    )
}

/// How many segments the pill opens with: the enabled profiles (at least
/// one); the app resizes it once the poller reports the real list.
pub fn startup_pill_segments(settings: &Settings) -> usize {
    settings
        .profiles
        .iter()
        .filter(|p| p.enabled)
        .count()
        .max(1)
}
/// A saved position farther than this from the desktop origin (pixels,
/// either axis) is junk and dropped. Generous enough for three 4K monitors
/// side by side; on Windows the window is also kept on a real monitor's
/// work area afterwards.
pub const SANE_POS: i32 = 16_384;
/// How much of a restored window must stay on a monitor's work area, so it
/// can always be grabbed by its title bar (points).
pub const MIN_VISIBLE: Vec2 = Vec2::new(120.0, 48.0);

/// The persisted [`Settings`] in the text of eframe's `app.ron` (a RON map
/// of storage key to RON value); defaults if anything is missing or broken.
pub fn settings_from_app_ron(text: &str) -> Settings {
    ron::from_str::<HashMap<String, String>>(text)
        .ok()
        .and_then(|map| map.get(SETTINGS_KEY).cloned())
        .and_then(|value| ron::from_str(&value).ok())
        .unwrap_or_default()
}

/// The persisted [`Settings`], read from eframe's storage before
/// `run_native` so the first window already has the saved geometry.
pub fn load_settings_early() -> Settings {
    eframe::storage_dir(APP_ID)
        .and_then(|dir| std::fs::read_to_string(dir.join("app.ron")).ok())
        .map(|text| settings_from_app_ron(&text))
        .unwrap_or_default()
}

/// The mode the window opens in.
pub fn startup_mode(settings: &Settings) -> UiMode {
    if settings.start_in_pill {
        UiMode::Pill
    } else {
        settings.ui_mode
    }
}

/// Where the first window goes and whether it starts maximized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Outer top-left in physical pixels; `None` lets the OS place it.
    pub pos: Option<Pos>,
    pub maximized: bool,
}

/// The startup placement for `settings`; `px_per_point` is the window's
/// scale (used only to place a pill that has no saved position).
pub fn startup_placement(settings: &Settings, px_per_point: f32) -> Placement {
    match startup_mode(settings) {
        UiMode::Full => Placement {
            pos: settings.full_rect.map(|r| r.pos),
            maximized: settings.full_maximized,
        },
        UiMode::Pill => Placement {
            pos: pill_start_pos(
                settings,
                px_per_point,
                pill_size(startup_pill_segments(settings)),
            ),
            maximized: false,
        },
    }
}

/// `p` unless it is farther than [`SANE_POS`] from the origin.
pub fn sane_pos(p: Pos) -> Option<Pos> {
    let ok = |v: i32| (-SANE_POS..=SANE_POS).contains(&v);
    (ok(p.x) && ok(p.y)).then_some(p)
}

/// Where the pill goes: its saved position if sane, else
/// [`default_pill_pos`] for the saved full rect and a pill of `size`, else
/// `None` (the OS places it).
pub fn pill_start_pos(settings: &Settings, px_per_point: f32, size: Vec2) -> Option<Pos> {
    settings
        .pill_pos
        .and_then(sane_pos)
        .or_else(|| default_pill_pos(settings.full_rect, px_per_point, size))
}

/// A pill of `size` with no saved position sits in the bottom-right corner
/// of the full window (`full`: position in pixels, size in points, at
/// `px_per_point`), where the eye already is. `None` without a sane full
/// rect.
pub fn default_pill_pos(full: Option<SavedRect>, px_per_point: f32, size: Vec2) -> Option<Pos> {
    let full = full?;
    let pos = sane_pos(full.pos)?;
    let inset = |free: f32| {
        let px = (free * px_per_point).round();
        if px.is_finite() && px > 0.0 {
            px.min(SANE_POS as f32) as i32
        } else {
            0
        }
    };
    Some(Pos {
        x: pos.x + inset(full.w - size.x),
        y: pos.y + inset(full.h - size.y),
    })
}

/// Who applies the [`Placement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceBy {
    /// The viewport builder (position as points).
    Builder,
    /// [`apply_placement`] after the window exists (Windows).
    Native,
}

impl PlaceBy {
    pub fn for_platform() -> Self {
        if cfg!(windows) {
            Self::Native
        } else {
            Self::Builder
        }
    }
}

/// The root viewport for `settings`.
pub fn viewport_builder(settings: &Settings, place_by: PlaceBy) -> ViewportBuilder {
    let base = ViewportBuilder::default()
        .with_title(TITLE)
        .with_app_id(APP_ID);
    let builder = match startup_mode(settings) {
        UiMode::Full => base
            .with_inner_size(sane_size(settings.full_rect))
            .with_min_inner_size(FULL_MIN)
            .with_decorations(true)
            .with_resizable(true)
            .with_window_level(window_level(settings.always_on_top)),
        // No min size: the runtime switch to full sets one.
        UiMode::Pill => base
            .with_inner_size(pill_size(startup_pill_segments(settings)))
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(egui::WindowLevel::AlwaysOnTop),
    };
    match place_by {
        PlaceBy::Native => builder,
        PlaceBy::Builder => {
            let placement = startup_placement(settings, 1.0);
            let builder = builder.with_maximized(placement.maximized);
            match placement.pos {
                Some(p) => builder.with_position(Pos2::new(p.x as f32, p.y as f32)),
                None => builder,
            }
        }
    }
}

/// Storage key of eframe's own window geometry (private
/// `STORAGE_WINDOW_KEY` in eframe's native/epi_integration.rs). eframe reads
/// it even with `persist_window: false`, and builds before the redesign
/// wrote it.
pub const EFRAME_WINDOW_KEY: &str = "window";

/// For `NativeOptions::window_builder`: put our geometry (`ours`, from
/// [`viewport_builder`]) back over what eframe derived from a stale
/// [`EFRAME_WINDOW_KEY`] (position, size, maximized, fullscreen).
pub fn keep_our_geometry(mut eframe: ViewportBuilder, ours: &ViewportBuilder) -> ViewportBuilder {
    eframe.position = ours.position;
    eframe.inner_size = ours.inner_size;
    eframe.maximized = ours.maximized;
    eframe.fullscreen = ours.fullscreen;
    eframe
}

/// Drop eframe's stale window geometry from storage; true if there was
/// one (so the caller flushes).
pub fn remove_stale_window_key(storage: &mut dyn eframe::Storage) -> bool {
    let stale = storage.get_string(EFRAME_WINDOW_KEY).is_some();
    if stale {
        storage.remove_string(EFRAME_WINDOW_KEY);
    }
    stale
}

/// Inner size for a saved full rect: [`FULL_SIZE`] if none or non-finite,
/// never below [`FULL_MIN`].
pub fn sane_size(saved: Option<SavedRect>) -> Vec2 {
    saved
        .map(|r| Vec2::new(r.w, r.h))
        .filter(|s| s.x.is_finite() && s.y.is_finite())
        .unwrap_or(FULL_SIZE)
        .max(FULL_MIN)
}

/// A rect in physical pixels, edges like Win32's `RECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PxRect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
}

/// The band along the top of `window` the user grabs it by, `height` tall:
/// the monitor it overlaps most is where the window belongs.
pub fn grab_strip(window: PxRect, height: i32) -> PxRect {
    PxRect {
        bottom: window.top + height,
        ..window
    }
}

/// The top-left for `window` (outer rect, pixels) that keeps at least
/// `visible` (width, height) of it on `work` with its top edge inside, moving
/// it as little as possible. `visible` is capped at the window's own size,
/// so a window smaller than it (the pill) can sit flush on an edge. A work
/// area smaller than `visible` pins it to the work area's left/top instead
/// of panicking.
pub fn keep_visible(window: PxRect, work: PxRect, visible: (i32, i32)) -> Pos {
    let visible = capped_visible(window, visible);
    let clamp = |v: i32, lo: i32, hi: i32| v.min(hi).max(lo);
    Pos {
        x: clamp(
            window.left,
            work.left - window.width() + visible.0,
            work.right - visible.0,
        ),
        y: clamp(window.top, work.top, work.bottom - visible.1),
    }
}

/// `visible` (width, height) no larger than `window` itself.
pub fn capped_visible(window: PxRect, visible: (i32, i32)) -> (i32, i32) {
    (
        visible.0.min(window.width().max(0)),
        visible.1.min((window.bottom - window.top).max(0)),
    )
}

/// The full-mode rect to save for this frame's viewport: outer position in
/// pixels (`zoom` is egui's zoom factor, part of its points-to-pixels
/// scale) and inner size in points. `None` while minimized or maximized
/// (the normal rect is what restores) or when egui has no usable rect.
pub fn full_rect_of(viewport: &ViewportInfo, zoom: f32) -> Option<SavedRect> {
    if viewport.minimized == Some(true) || viewport.maximized == Some(true) {
        return None;
    }
    let inner = viewport.inner_rect?;
    let (w, h) = (inner.width(), inner.height());
    let size_ok = w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0;
    size_ok.then_some(())?;
    Some(SavedRect {
        pos: outer_pos_px(viewport, zoom)?,
        w,
        h,
    })
}

/// The viewport's outer top-left in pixels (`zoom` as in
/// [`full_rect_of`]); `None` without a usable outer rect.
fn outer_pos_px(viewport: &ViewportInfo, zoom: f32) -> Option<Pos> {
    let outer = viewport.outer_rect?;
    let px_per_point = viewport.native_pixels_per_point.unwrap_or(1.0) * zoom;
    let to_px = |v: f32| {
        let px = (v * px_per_point).round();
        (px.is_finite() && px.abs() < 1e9).then_some(px as i32)
    };
    Some(Pos {
        x: to_px(outer.min.x)?,
        y: to_px(outer.min.y)?,
    })
}

/// The pill position to remember for this frame's viewport (outer
/// top-left, pixels); `None` while minimized or without a rect.
pub fn pill_pos_of(viewport: &ViewportInfo, zoom: f32) -> Option<Pos> {
    if viewport.minimized == Some(true) {
        return None;
    }
    outer_pos_px(viewport, zoom)
}

/// The pill position to persist: in Pill mode the last one seen (else the
/// saved one); in Full mode the saved one, untouched.
pub fn pill_pos_to_save(mode: UiMode, seen: Option<Pos>, settings: &Settings) -> Option<Pos> {
    match mode {
        UiMode::Pill => seen.or(settings.pill_pos),
        UiMode::Full => settings.pill_pos,
    }
}

/// The full-mode window as `logic()` last saw it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackedWindow {
    /// The last normal (not maximized, not minimized) rect.
    pub rect: Option<SavedRect>,
    /// Whether it is maximized, as of the last frame that said.
    pub maximized: Option<bool>,
}

impl TrackedWindow {
    /// Fold in one frame's viewport info. Minimized frames are ignored, so
    /// hiding or minimizing keeps the last real state.
    pub fn observe(&mut self, viewport: &ViewportInfo, zoom: f32) {
        if viewport.minimized == Some(true) {
            return;
        }
        if viewport.maximized.is_some() {
            self.maximized = viewport.maximized;
        }
        if let Some(rect) = full_rect_of(viewport, zoom) {
            self.rect = Some(rect);
        }
    }
}

/// `(full_rect, full_maximized)` to persist: in Full mode what was tracked
/// (falling back to the saved values for anything not seen yet); in Pill
/// mode the saved full-mode values are kept untouched.
pub fn full_state_to_save(
    mode: UiMode,
    tracked: &TrackedWindow,
    settings: &Settings,
) -> (Option<SavedRect>, bool) {
    match mode {
        UiMode::Full => (
            tracked.rect.or(settings.full_rect),
            tracked.maximized.unwrap_or(settings.full_maximized),
        ),
        UiMode::Pill => (settings.full_rect, settings.full_maximized),
    }
}

/// The Win32 window handle behind `h` (eframe's `CreationContext` or
/// `Frame`).
#[cfg(windows)]
fn hwnd_of(
    h: &impl raw_window_handle::HasWindowHandle,
) -> Option<windows_sys::Win32::Foundation::HWND> {
    match h.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(w) => Some(w.hwnd.get() as _),
        _ => None,
    }
}

/// Move the (still hidden) window to `placement`, kept on the work area of
/// the monitor its title band overlaps most (or the nearest one, so it
/// can't land in a gap of an L-shaped layout or on an unplugged monitor),
/// then maximize it if it was. Call from `App::new`.
///
/// Maximizing goes through winit so its window state stays in sync. winit
/// 0.30.13's `apply_diff` (`window_state.rs`) calls `ShowWindow(SW_MAXIMIZE)`
/// and then, because the window is not yet marked visible,
/// `ShowWindow(SW_HIDE)` in the same pass, so the window stays hidden until
/// eframe shows it after the first paint.
#[cfg(windows)]
pub fn apply_placement(cc: &eframe::CreationContext<'_>, placement: Placement) {
    if let (Some(pos), Some(hwnd)) = (placement.pos, hwnd_of(cc)) {
        // Second pass: moving across monitor scales resizes the window
        // (winit keeps its logical size), so re-check with the new size.
        let mut at = pos;
        for _ in 0..2 {
            if let Some(target) = win32::visible_target(hwnd, at) {
                win32::move_to(hwnd, target);
                at = target;
            }
        }
    }
    if placement.maximized {
        if let Some(w) = cc.winit_window() {
            w.set_maximized(true);
        }
    }
}

/// Ask Windows 11 to round the window's corners (the borderless pill has
/// none otherwise). The preference survives later style changes, so once
/// at startup is enough. Harmless where unsupported.
#[cfg(windows)]
pub fn round_corners(h: &impl raw_window_handle::HasWindowHandle) {
    if let Some(hwnd) = hwnd_of(h) {
        win32::round_corners(hwnd);
    }
}

/// Pull the window back onto a monitor's work area if it is off screen
/// (after a full/pill switch to a position saved on a monitor that is gone).
#[cfg(windows)]
pub fn keep_on_screen(frame: &eframe::Frame) {
    let Some(hwnd) = hwnd_of(frame) else { return };
    if let Some(at) = win32::current_pos(hwnd) {
        if let Some(target) = win32::visible_target(hwnd, at).filter(|t| *t != at) {
            win32::move_to(hwnd, target);
        }
    }
}

#[cfg(windows)]
mod win32 {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    use super::{capped_visible, grab_strip, keep_visible, PxRect, MIN_VISIBLE};
    use crate::config::Pos;

    fn to_px(r: RECT) -> PxRect {
        PxRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    fn to_rect(r: PxRect) -> RECT {
        RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    /// Where the window (at its current outer size and DPI) should go if its
    /// top-left were `pos`, kept visible on the best monitor.
    pub(super) fn visible_target(hwnd: HWND, pos: Pos) -> Option<Pos> {
        let mut current = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: hwnd is our live top-level window; the out-pointers point
        // at locals of the right types.
        unsafe {
            if GetWindowRect(hwnd, &mut current) == 0 {
                return None;
            }
            let size = to_px(current);
            let window = PxRect {
                left: pos.x,
                top: pos.y,
                right: pos.x + size.width(),
                bottom: pos.y + (size.bottom - size.top),
            };
            let dpi = GetDpiForWindow(hwnd);
            let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
            let visible = capped_visible(
                window,
                (
                    (MIN_VISIBLE.x * scale).round() as i32,
                    (MIN_VISIBLE.y * scale).round() as i32,
                ),
            );
            let strip = to_rect(grab_strip(window, visible.1));
            let monitor = MonitorFromRect(&strip, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                rcMonitor: current,
                rcWork: current,
                dwFlags: 0,
            };
            Some(if GetMonitorInfoW(monitor, &mut info) != 0 {
                keep_visible(window, to_px(info.rcWork), visible)
            } else {
                pos
            })
        }
    }

    pub(super) fn round_corners(hwnd: HWND) {
        let pref = DWMWCP_ROUND;
        // SAFETY: hwnd is our live top-level window; the attribute is a
        // DWM_WINDOW_CORNER_PREFERENCE (i32) passed by pointer with its size.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const _ as *const core::ffi::c_void,
                std::mem::size_of_val(&pref) as u32,
            )
        };
    }

    /// The window's outer top-left now.
    pub(super) fn current_pos(hwnd: HWND) -> Option<Pos> {
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: hwnd is our live top-level window; r is a local RECT.
        (unsafe { GetWindowRect(hwnd, &mut r) } != 0).then_some(Pos {
            x: r.left,
            y: r.top,
        })
    }

    pub(super) fn move_to(hwnd: HWND, pos: Pos) {
        // SAFETY: hwnd is our live top-level window.
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                pos.x,
                pos.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{self, Rect};

    fn saved(x: i32, y: i32, w: f32, h: f32) -> SavedRect {
        SavedRect {
            pos: Pos { x, y },
            w,
            h,
        }
    }

    fn px(left: i32, top: i32, right: i32, bottom: i32) -> PxRect {
        PxRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn first_run_opens_full_at_default_size() {
        for place_by in [PlaceBy::Native, PlaceBy::Builder] {
            let b = viewport_builder(&Settings::default(), place_by);
            assert_eq!(b.inner_size, Some(FULL_SIZE));
            assert_eq!(b.min_inner_size, Some(FULL_MIN));
            assert_eq!(b.position, None);
            assert_eq!(b.decorations, Some(true));
            assert_eq!(b.resizable, Some(true));
            assert_eq!(b.window_level, Some(egui::WindowLevel::Normal));
            assert_eq!(b.title.as_deref(), Some(TITLE));
            assert_eq!(b.app_id.as_deref(), Some(APP_ID));
        }
        assert_eq!(
            startup_placement(&Settings::default(), 1.0),
            Placement {
                pos: None,
                maximized: false
            }
        );
        assert_eq!(
            (FULL_SIZE, FULL_MIN),
            (Vec2::new(420.0, 760.0), Vec2::new(360.0, 320.0))
        );
    }

    #[test]
    fn saved_window_is_restored() {
        let s = Settings {
            full_rect: Some(saved(-1500, 120, 500.0, 600.0)),
            full_maximized: true,
            always_on_top: true,
            ..Settings::default()
        };
        assert_eq!(
            startup_placement(&s, 1.0),
            Placement {
                pos: Some(Pos { x: -1500, y: 120 }),
                maximized: true
            }
        );
        // Windows: size and level in the builder, placement done natively.
        let b = viewport_builder(&s, PlaceBy::Native);
        assert_eq!(b.inner_size, Some(Vec2::new(500.0, 600.0)));
        assert_eq!(b.window_level, Some(egui::WindowLevel::AlwaysOnTop));
        assert_eq!((b.position, b.maximized), (None, None));
        // Elsewhere: the builder places it.
        let b = viewport_builder(&s, PlaceBy::Builder);
        assert_eq!(b.position, Some(Pos2::new(-1500.0, 120.0)));
        assert_eq!(b.maximized, Some(true));
    }

    #[test]
    fn pill_mode_opens_as_the_pill_at_its_position() {
        let rect = saved(10, 20, 400.0, 400.0);
        let pill = Settings {
            ui_mode: UiMode::Pill,
            full_rect: Some(rect),
            pill_pos: Some(Pos { x: -1800, y: 900 }),
            always_on_top: false,
            ..Settings::default()
        };
        assert_eq!(startup_mode(&pill), UiMode::Pill);
        assert_eq!(
            startup_placement(&pill, 2.0),
            Placement {
                pos: Some(Pos { x: -1800, y: 900 }),
                maximized: false
            }
        );
        let b = viewport_builder(&pill, PlaceBy::Native);
        assert_eq!(b.inner_size, Some(pill_size(1)));
        assert_eq!(b.min_inner_size, None);
        assert_eq!(b.decorations, Some(false));
        assert_eq!(b.resizable, Some(false));
        assert_eq!(b.window_level, Some(egui::WindowLevel::AlwaysOnTop));
        assert_eq!((b.position, b.maximized), (None, None));
        let b = viewport_builder(&pill, PlaceBy::Builder);
        assert_eq!(b.position, Some(Pos2::new(-1800.0, 900.0)));
        assert_eq!(b.maximized, Some(false));

        // "Start in pill mode" wins over a saved Full mode, even maximized;
        // without a pill position it goes to the full rect's bottom right.
        let start = Settings {
            start_in_pill: true,
            full_rect: Some(rect),
            full_maximized: true,
            ..Settings::default()
        };
        assert_eq!(startup_mode(&start), UiMode::Pill);
        assert_eq!(
            startup_placement(&start, 2.0),
            Placement {
                pos: Some(Pos {
                    x: 10 + 200,
                    y: 20 + 712
                }),
                maximized: false
            }
        );
        assert_eq!(
            viewport_builder(&start, PlaceBy::Native).decorations,
            Some(false)
        );
        assert_eq!(startup_mode(&Settings::default()), UiMode::Full);
    }

    #[test]
    fn pill_widens_per_account() {
        assert_eq!(pill_size(0), Vec2::new(300.0, 44.0));
        assert_eq!(pill_size(1), Vec2::new(300.0, 44.0));
        let (two, three) = (pill_size(2), pill_size(3));
        assert_eq!((two.y, three.y), (44.0, 44.0));
        // Measured: each segment fits ring, "100%" and "5h · 4h59m".
        assert_eq!(two.x, 401.0);
        // Each further account adds the same width.
        assert_eq!(three.x - two.x, pill::SEGMENT_W + pill::DIVIDER_SPAN);
    }

    #[test]
    fn pill_opens_sized_for_the_enabled_profiles() {
        let profile = |id: &str, enabled: bool| crate::config::ProfileSettings {
            id: id.to_owned(),
            enabled,
            ..Default::default()
        };
        let mut s = Settings {
            ui_mode: UiMode::Pill,
            ..Settings::default()
        };
        assert_eq!(startup_pill_segments(&s), 1);
        s.profiles = vec![profile("a", true), profile("b", false)];
        assert_eq!(startup_pill_segments(&s), 1);
        s.profiles.push(profile("c", true));
        assert_eq!(startup_pill_segments(&s), 2);
        let b = viewport_builder(&s, PlaceBy::Native);
        assert_eq!(b.inner_size, Some(pill_size(2)));
        // With no pill position, the wider pill still sits flush in the full
        // window's bottom-right corner.
        s.full_rect = Some(saved(0, 0, 600.0, 400.0));
        let size = pill_size(2);
        assert_eq!(
            startup_placement(&s, 1.0).pos,
            Some(Pos {
                x: (600.0 - size.x) as i32,
                y: 400 - 44
            })
        );
    }

    #[test]
    fn pill_start_pos_drops_wild_positions() {
        let s = |pill: Option<Pos>, full: Option<SavedRect>| Settings {
            pill_pos: pill,
            full_rect: full,
            ..Settings::default()
        };
        let full = saved(1000, 500, 420.0, 460.0);
        // Sane, including monitors left of / above the primary.
        for p in [
            Pos { x: -3840, y: -2160 },
            Pos { x: 0, y: 0 },
            Pos { x: 11_000, y: 4000 },
        ] {
            assert_eq!(
                pill_start_pos(&s(Some(p), Some(full)), 1.0, pill_size(1)),
                Some(p)
            );
        }
        // Wild: fall back to the full rect's bottom right (1x: 120, 416 in).
        let fallback = Some(Pos { x: 1120, y: 916 });
        for p in [
            Pos { x: -40_000, y: 10 },
            Pos { x: 10, y: 99_999 },
            Pos {
                x: i32::MIN,
                y: i32::MAX,
            },
        ] {
            assert_eq!(
                pill_start_pos(&s(Some(p), Some(full)), 1.0, pill_size(1)),
                fallback
            );
        }
        assert_eq!(
            pill_start_pos(&s(None, Some(full)), 1.0, pill_size(1)),
            fallback
        );
        // Nothing usable: the OS places it.
        assert_eq!(pill_start_pos(&s(None, None), 1.0, pill_size(1)), None);
        let wild_full = saved(50_000, 0, 420.0, 460.0);
        assert_eq!(
            pill_start_pos(&s(None, Some(wild_full)), 1.0, pill_size(1)),
            None
        );
        // A full rect smaller than the pill: its top-left.
        let tiny = saved(5, 6, 100.0, 20.0);
        assert_eq!(
            default_pill_pos(Some(tiny), 2.0, pill_size(1)),
            Some(Pos { x: 5, y: 6 })
        );
        // Garbage scale doesn't produce garbage positions.
        assert_eq!(
            default_pill_pos(Some(full), f32::NAN, pill_size(1)),
            Some(full.pos)
        );
    }

    #[test]
    fn pill_pos_tracked_in_pixels_and_saved_only_in_pill_mode() {
        let mut v = viewport(2.25);
        v.outer_rect = Some(Rect::from_min_size(Pos2::new(100.0, 50.0), pill_size(1)));
        v.inner_rect = v.outer_rect;
        assert_eq!(pill_pos_of(&v, 1.0), Some(Pos { x: 225, y: 113 }));
        let mut min = v.clone();
        min.minimized = Some(true);
        assert_eq!(pill_pos_of(&min, 1.0), None);
        let mut gone = v.clone();
        gone.outer_rect = None;
        assert_eq!(pill_pos_of(&gone, 1.0), None);

        let s = Settings {
            pill_pos: Some(Pos { x: 1, y: 2 }),
            ..Settings::default()
        };
        let seen = Some(Pos { x: 30, y: 40 });
        assert_eq!(pill_pos_to_save(UiMode::Pill, seen, &s), seen);
        assert_eq!(
            pill_pos_to_save(UiMode::Pill, None, &s),
            Some(Pos { x: 1, y: 2 })
        );
        assert_eq!(
            pill_pos_to_save(UiMode::Full, seen, &s),
            Some(Pos { x: 1, y: 2 })
        );
    }

    #[test]
    fn sizes_are_sane() {
        assert_eq!(sane_size(None), FULL_SIZE);
        assert_eq!(sane_size(Some(saved(0, 0, 300.0, 48.0))), FULL_MIN);
        assert_eq!(
            sane_size(Some(saved(0, 0, 900.0, 700.0))),
            Vec2::new(900.0, 700.0)
        );
        assert_eq!(sane_size(Some(saved(0, 0, f32::NAN, 500.0))), FULL_SIZE);
        assert_eq!(
            sane_size(Some(saved(0, 0, 500.0, f32::INFINITY))),
            FULL_SIZE
        );
    }

    #[test]
    fn keep_visible_pulls_an_off_screen_window_back() {
        let work = px(0, 0, 3840, 2100);
        let vis = (270, 108);
        let win = |x: i32, y: i32| px(x, y, x + 973, y + 1114);
        // Already visible, even partly off the left edge: untouched.
        assert_eq!(
            keep_visible(win(200, 300), work, vis),
            Pos { x: 200, y: 300 }
        );
        assert_eq!(keep_visible(win(-500, 0), work, vis), Pos { x: -500, y: 0 });
        // Far right / bottom: `vis` stays on the work area.
        assert_eq!(
            keep_visible(win(9000, 5000), work, vis),
            Pos {
                x: 3840 - 270,
                y: 2100 - 108
            }
        );
        // Far left: the window's right `vis.0` pixels stay on; above the top:
        // the title bar comes back inside. Uses the outer width (973).
        assert_eq!(
            keep_visible(win(-5000, -400), work, vis),
            Pos { x: 270 - 973, y: 0 }
        );
    }

    #[test]
    fn keep_visible_leaves_a_pill_flush_in_the_corner() {
        // 300x44 pt at 225 % = 675x99 px; MIN_VISIBLE asks for 270x108 px,
        // more than the pill is tall: capped at the window's own size.
        let work = px(0, 0, 3840, 2100);
        let pill = px(3840 - 675, 2100 - 99, 3840, 2100);
        assert_eq!(
            keep_visible(pill, work, (270, 108)),
            Pos {
                x: 3840 - 675,
                y: 2100 - 99
            }
        );
        // Below the bottom edge it comes back fully, flush, not 9 px higher.
        let low = px(100, 2050, 775, 2149);
        assert_eq!(
            keep_visible(low, work, (270, 108)),
            Pos {
                x: 100,
                y: 2100 - 99
            }
        );
        // A window narrower than the visible width keeps its whole width on.
        let narrow = px(-500, 0, -400, 50);
        assert_eq!(keep_visible(narrow, work, (270, 108)), Pos { x: 0, y: 0 });
    }

    #[test]
    fn keep_visible_on_a_monitor_left_of_the_primary() {
        let work = px(-1920, -200, 0, 880);
        assert_eq!(
            keep_visible(px(-1800, -100, -1380, 400), work, (120, 48)),
            Pos { x: -1800, y: -100 }
        );
    }

    #[test]
    fn keep_visible_tiny_work_area_does_not_panic() {
        let p = keep_visible(px(500, 500, 900, 900), px(0, 0, 50, 20), (120, 48));
        assert_eq!(p, Pos { x: 50 - 120, y: 0 });
    }

    #[test]
    fn grab_strip_is_the_top_band() {
        assert_eq!(grab_strip(px(10, 20, 410, 520), 108), px(10, 20, 410, 128));
    }

    fn viewport(scale: f32) -> ViewportInfo {
        ViewportInfo {
            outer_rect: Some(Rect::from_min_size(
                Pos2::new(100.0, 50.0),
                Vec2::new(436.0, 499.0),
            )),
            inner_rect: Some(Rect::from_min_size(
                Pos2::new(108.0, 81.0),
                Vec2::new(420.0, 460.0),
            )),
            native_pixels_per_point: Some(scale),
            maximized: Some(false),
            minimized: Some(false),
            ..ViewportInfo::default()
        }
    }

    #[test]
    fn full_rect_position_in_pixels_size_in_points() {
        // 225 %: points * 2.25, rounded.
        assert_eq!(
            full_rect_of(&viewport(2.25), 1.0),
            Some(saved(225, 113, 420.0, 460.0))
        );
        // 100 % monitor: pixels == points.
        assert_eq!(
            full_rect_of(&viewport(1.0), 1.0),
            Some(saved(100, 50, 420.0, 460.0))
        );
        // egui zoom is part of the points-to-pixels scale.
        assert_eq!(
            full_rect_of(&viewport(1.5), 2.0),
            Some(saved(300, 150, 420.0, 460.0))
        );
        // Negative desktop coordinates (monitor left of the primary).
        let mut v = viewport(1.25);
        v.outer_rect = Some(Rect::from_min_size(
            Pos2::new(-1536.0, -8.0),
            Vec2::new(436.0, 499.0),
        ));
        assert_eq!(full_rect_of(&v, 1.0).unwrap().pos, Pos { x: -1920, y: -10 });
    }

    #[test]
    fn full_rect_skips_minimized_maximized_and_unusable() {
        let mut v = viewport(1.0);
        v.minimized = Some(true);
        assert_eq!(full_rect_of(&v, 1.0), None);
        let mut v = viewport(1.0);
        v.maximized = Some(true);
        assert_eq!(full_rect_of(&v, 1.0), None);
        let mut v = viewport(1.0);
        v.inner_rect = None;
        assert_eq!(full_rect_of(&v, 1.0), None);
        let mut v = viewport(1.0);
        v.inner_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
        assert_eq!(full_rect_of(&v, 1.0), None);
        let mut v = viewport(1.0);
        v.outer_rect = Some(Rect::from_min_size(
            Pos2::new(f32::NAN, 0.0),
            Vec2::splat(400.0),
        ));
        assert_eq!(full_rect_of(&v, 1.0), None);
    }

    #[test]
    fn tracking_keeps_the_normal_rect_while_maximized_or_minimized() {
        let mut t = TrackedWindow::default();
        t.observe(&viewport(1.0), 1.0);
        let normal = saved(100, 50, 420.0, 460.0);
        assert_eq!(
            t,
            TrackedWindow {
                rect: Some(normal),
                maximized: Some(false)
            }
        );

        // Maximized: the flag flips, the normal rect stays.
        let mut max = viewport(1.0);
        max.maximized = Some(true);
        max.outer_rect = Some(Rect::from_min_size(
            Pos2::new(-8.0, -8.0),
            Vec2::splat(2000.0),
        ));
        t.observe(&max, 1.0);
        assert_eq!(
            t,
            TrackedWindow {
                rect: Some(normal),
                maximized: Some(true)
            }
        );

        // Minimized (or hidden to the tray) while maximized: nothing changes.
        let mut min = viewport(1.0);
        min.minimized = Some(true);
        min.maximized = Some(false);
        t.observe(&min, 1.0);
        assert_eq!(t.maximized, Some(true));
        assert_eq!(t.rect, Some(normal));
    }

    #[test]
    fn full_state_to_save_only_full_mode_copies_tracking() {
        let old = saved(1, 2, 400.0, 400.0);
        let new = saved(30, 40, 500.0, 450.0);
        let s = Settings {
            full_rect: Some(old),
            full_maximized: true,
            ..Settings::default()
        };
        let seen = TrackedWindow {
            rect: Some(new),
            maximized: Some(false),
        };
        assert_eq!(
            full_state_to_save(UiMode::Full, &seen, &s),
            (Some(new), false)
        );
        assert_eq!(
            full_state_to_save(UiMode::Pill, &seen, &s),
            (Some(old), true)
        );
        // Nothing tracked yet (started maximized, or hidden all along).
        let unseen = TrackedWindow::default();
        assert_eq!(
            full_state_to_save(UiMode::Full, &unseen, &s),
            (Some(old), true)
        );
        // Started maximized, never restored: the saved normal rect survives.
        let max_only = TrackedWindow {
            rect: None,
            maximized: Some(true),
        };
        assert_eq!(
            full_state_to_save(UiMode::Full, &max_only, &s),
            (Some(old), true)
        );
    }

    /// A pre-redesign app.ron: eframe's "window" key says maximized at
    /// 800x900 somewhere else; ours says a 440x500 normal window.
    const STALE_APP_RON: &str = r#"{
    "window": "(inner_position_pixels:Some((x:120.0,y:150.0)),outer_position_pixels:Some((x:100.0,y:100.0)),fullscreen:false,maximized:true,inner_size_points:Some((x:800.0,y:900.0)))",
    "settings": "(plan:\"Max\",full_rect:Some((pos:(x:1350,y:338),w:440.0,h:500.0)),full_maximized:false)",
    "egui": "()",
}"#;

    #[test]
    fn stale_eframe_window_key_does_not_win() {
        let s = settings_from_app_ron(STALE_APP_RON);
        assert_eq!(s.full_rect, Some(saved(1350, 338, 440.0, 500.0)));
        assert!(!s.full_maximized);

        for place_by in [PlaceBy::Native, PlaceBy::Builder] {
            let ours = viewport_builder(&s, place_by);
            // What eframe's WindowSettings::initialize_viewport_builder does
            // with the stale key.
            let eframe_view = ours
                .clone()
                .with_position(Pos2::new(44.4, 44.4))
                .with_inner_size(Vec2::new(800.0, 900.0))
                .with_fullscreen(false)
                .with_maximized(true);
            let b = keep_our_geometry(eframe_view, &ours);
            assert_eq!(b.position, ours.position);
            assert_eq!(b.inner_size, Some(Vec2::new(440.0, 500.0)));
            assert_eq!(b.maximized, ours.maximized);
            assert_eq!(b.fullscreen, None);
            // Everything else stays as eframe had it (ours, here).
            assert_eq!(b.min_inner_size, ours.min_inner_size);
            assert_eq!(b.title, ours.title);
        }
        assert_eq!(viewport_builder(&s, PlaceBy::Native).position, None);
        assert_eq!(viewport_builder(&s, PlaceBy::Native).maximized, None);
    }

    #[derive(Default)]
    struct MapStorage(HashMap<String, String>, usize);

    impl eframe::Storage for MapStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.1 += 1;
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn stale_window_key_is_removed_once() {
        let mut storage = MapStorage(ron::from_str(STALE_APP_RON).unwrap(), 0);
        assert!(remove_stale_window_key(&mut storage));
        assert!(!storage.0.contains_key(EFRAME_WINDOW_KEY));
        assert!(storage.0.contains_key(SETTINGS_KEY) && storage.0.contains_key("egui"));
        // Already gone: no remove (which would mark eframe's storage dirty).
        assert!(!remove_stale_window_key(&mut storage));
        assert_eq!(storage.1, 1);
    }

    #[test]
    fn settings_read_from_app_ron_text() {
        let stored = Settings {
            plan: "Pro".into(),
            full_rect: Some(saved(1, 2, 430.0, 470.0)),
            full_maximized: true,
            ..Settings::default()
        };
        let mut map = HashMap::new();
        map.insert(
            SETTINGS_KEY.to_owned(),
            ron::ser::to_string(&stored).unwrap(),
        );
        map.insert("egui".to_owned(), "(whatever)".to_owned());
        let text = ron::ser::to_string_pretty(&map, Default::default()).unwrap();
        assert_eq!(settings_from_app_ron(&text), stored);

        // Pre-redesign value: loads, new fields default.
        let old = r#"{"settings": "(plan:\"Max\",interval_secs:60,show_all:true,always_on_top:false,cookie_file:None,client_sha:None)", "window": "()"}"#;
        let s = settings_from_app_ron(old);
        assert!(s.show_all);
        assert_eq!(s.full_rect, None);

        for broken in ["", "not ron", "{}", r#"{"settings": "(plan:"}"#] {
            assert_eq!(
                settings_from_app_ron(broken),
                Settings::default(),
                "{broken:?}"
            );
        }
    }
}
