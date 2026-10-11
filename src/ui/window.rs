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

use std::collections::HashMap;

use eframe::egui::{Pos2, Vec2, ViewportBuilder, ViewportInfo};

use crate::config::{Pos, SavedRect, Settings, UiMode};

use super::settings::window_level;

/// eframe app id: names the storage directory and the window class.
pub const APP_ID: &str = "claude-usage";
pub const TITLE: &str = "Claude Usage Monitor";
/// eframe storage key for [`Settings`].
pub const SETTINGS_KEY: &str = "settings";
/// Full-mode window size on first run (inner, points).
pub const FULL_SIZE: Vec2 = Vec2::new(420.0, 460.0);
/// Smallest full-mode window (inner, points).
pub const FULL_MIN: Vec2 = Vec2::new(360.0, 320.0);
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

/// The startup placement for `settings`.
pub fn startup_placement(settings: &Settings) -> Placement {
    match startup_mode(settings) {
        // Task 8 places the pill window here; until then it opens full.
        UiMode::Pill | UiMode::Full => Placement {
            pos: settings.full_rect.map(|r| r.pos),
            maximized: settings.full_maximized,
        },
    }
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
        .with_app_id(APP_ID)
        .with_window_level(window_level(settings.always_on_top));
    let builder = match startup_mode(settings) {
        // Task 8 builds the pill window here; until then it opens full.
        UiMode::Pill | UiMode::Full => base
            .with_inner_size(sane_size(settings.full_rect))
            .with_min_inner_size(FULL_MIN)
            .with_decorations(true)
            .with_resizable(true),
    };
    match place_by {
        PlaceBy::Native => builder,
        PlaceBy::Builder => {
            let placement = startup_placement(settings);
            let builder = builder.with_maximized(placement.maximized);
            match placement.pos {
                Some(p) => builder.with_position(Pos2::new(p.x as f32, p.y as f32)),
                None => builder,
            }
        }
    }
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
/// it as little as possible. A work area smaller than `visible` pins it to
/// the work area's left/top instead of panicking.
pub fn keep_visible(window: PxRect, work: PxRect, visible: (i32, i32)) -> Pos {
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

/// The full-mode rect to save for this frame's viewport: outer position in
/// pixels (`zoom` is egui's zoom factor, part of its points-to-pixels
/// scale) and inner size in points. `None` while minimized or maximized
/// (the normal rect is what restores) or when egui has no usable rect.
pub fn full_rect_of(viewport: &ViewportInfo, zoom: f32) -> Option<SavedRect> {
    if viewport.minimized == Some(true) || viewport.maximized == Some(true) {
        return None;
    }
    let (outer, inner) = (viewport.outer_rect?, viewport.inner_rect?);
    let px_per_point = viewport.native_pixels_per_point.unwrap_or(1.0) * zoom;
    let to_px = |v: f32| {
        let px = (v * px_per_point).round();
        (px.is_finite() && px.abs() < 1e9).then_some(px as i32)
    };
    let (w, h) = (inner.width(), inner.height());
    let size_ok = w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0;
    size_ok.then_some(())?;
    Some(SavedRect {
        pos: Pos {
            x: to_px(outer.min.x)?,
            y: to_px(outer.min.y)?,
        },
        w,
        h,
    })
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
/// then maximize it if it was. Call from `App::new` with its
/// `CreationContext`.
#[cfg(windows)]
pub fn apply_placement(handle: &impl raw_window_handle::HasWindowHandle, placement: Placement) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
        SW_MAXIMIZE,
    };

    let to_px = |r: RECT| PxRect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    };
    let to_rect = |r: PxRect| RECT {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    };

    let Some(hwnd) = hwnd_of(handle) else {
        return;
    };
    if let Some(pos) = placement.pos {
        let mut current = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: hwnd is our live top-level window; the out-pointers point
        // at locals of the right types.
        unsafe {
            if GetWindowRect(hwnd, &mut current) != 0 {
                let size = to_px(current);
                let window = PxRect {
                    left: pos.x,
                    top: pos.y,
                    right: pos.x + size.width(),
                    bottom: pos.y + (size.bottom - size.top),
                };
                let dpi = GetDpiForWindow(hwnd);
                let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
                let visible = (
                    (MIN_VISIBLE.x * scale).round() as i32,
                    (MIN_VISIBLE.y * scale).round() as i32,
                );
                let strip = to_rect(grab_strip(window, visible.1));
                let monitor = MonitorFromRect(&strip, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    rcMonitor: current,
                    rcWork: current,
                    dwFlags: 0,
                };
                let target = if GetMonitorInfoW(monitor, &mut info) != 0 {
                    keep_visible(window, to_px(info.rcWork), visible)
                } else {
                    pos
                };
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    target.x,
                    target.y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
    if placement.maximized {
        // SAFETY: hwnd is our live top-level window.
        unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
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
            startup_placement(&Settings::default()),
            Placement {
                pos: None,
                maximized: false
            }
        );
        assert_eq!(
            (FULL_SIZE, FULL_MIN),
            (Vec2::new(420.0, 460.0), Vec2::new(360.0, 320.0))
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
            startup_placement(&s),
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
    fn pill_mode_opens_full_until_task_8() {
        let rect = saved(10, 20, 400.0, 400.0);
        for s in [
            Settings {
                ui_mode: UiMode::Pill,
                full_rect: Some(rect),
                pill_pos: Some(Pos { x: 1, y: 2 }),
                ..Settings::default()
            },
            Settings {
                start_in_pill: true,
                full_rect: Some(rect),
                ..Settings::default()
            },
        ] {
            assert_eq!(startup_mode(&s), UiMode::Pill);
            assert_eq!(startup_placement(&s).pos, Some(Pos { x: 10, y: 20 }));
            assert_eq!(
                viewport_builder(&s, PlaceBy::Native).decorations,
                Some(true)
            );
        }
        assert_eq!(startup_mode(&Settings::default()), UiMode::Full);
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
