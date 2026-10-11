//! Window geometry we persist ourselves: the viewport builder for the saved
//! mode (built from `app.ron` read before eframe starts) and the rect
//! recorded on save.
//!
//! eframe's own window persistence is off: it restores the window only
//! after the first paint (a visible flash when the mode differs) and clamps
//! restored sizes to 64 px, which a pill window is smaller than.
//!
//! Every rect here is in egui points, as `ViewportInfo` reports them and
//! `ViewportBuilder` takes them.

use std::collections::HashMap;

use eframe::egui::{Pos2, Rect, Vec2, ViewportBuilder, ViewportInfo};

use crate::config::{SavedRect, Settings, UiMode};

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
/// How much of a restored window must stay on screen, so it can always be
/// grabbed by its title bar.
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

/// The root viewport for `settings`. `screen` is the desktop's bounding
/// rect, if known; a saved rect is kept inside it.
pub fn viewport_builder(settings: &Settings, screen: Option<Rect>) -> ViewportBuilder {
    let base = ViewportBuilder::default()
        .with_title(TITLE)
        .with_app_id(APP_ID)
        .with_window_level(window_level(settings.always_on_top));
    match startup_mode(settings) {
        // Task 8 builds the pill window here; until then it opens full.
        UiMode::Pill => full_builder(base, settings, screen),
        UiMode::Full => full_builder(base, settings, screen),
    }
}

fn full_builder(
    base: ViewportBuilder,
    settings: &Settings,
    screen: Option<Rect>,
) -> ViewportBuilder {
    let (pos, size) = sane_full_rect(settings.full_rect, screen);
    let builder = base
        .with_inner_size(size)
        .with_min_inner_size(FULL_MIN)
        .with_decorations(true)
        .with_resizable(true);
    match pos {
        Some(p) => builder.with_position(p),
        None => builder,
    }
}

/// Position (`None`: let the OS place it) and inner size for a saved full
/// rect: non-finite values are dropped, the size is at least [`FULL_MIN`]
/// and at most the screen, and at least [`MIN_VISIBLE`] of the window stays
/// on `screen` with its top edge inside it.
pub fn sane_full_rect(saved: Option<SavedRect>, screen: Option<Rect>) -> (Option<Pos2>, Vec2) {
    let finite = |v: f32| v.is_finite().then_some(v);
    let size = saved
        .and_then(|r| Some(Vec2::new(finite(r.w)?, finite(r.h)?)))
        .unwrap_or(FULL_SIZE);
    let size = match screen {
        Some(s) => size.min(s.size()),
        None => size,
    }
    .max(FULL_MIN);
    let pos = saved.and_then(|r| Some(Pos2::new(finite(r.x)?, finite(r.y)?)));
    let pos = match (pos, screen) {
        (Some(p), Some(s)) => Some(Pos2::new(
            clamp_lenient(
                p.x,
                s.min.x - size.x + MIN_VISIBLE.x,
                s.max.x - MIN_VISIBLE.x,
            ),
            clamp_lenient(p.y, s.min.y, s.max.y - MIN_VISIBLE.y),
        )),
        (pos, _) => pos,
    };
    (pos, size)
}

/// `v.clamp(lo, hi)`, preferring `lo` when the range is empty (a screen
/// smaller than [`MIN_VISIBLE`]) instead of panicking.
fn clamp_lenient(v: f32, lo: f32, hi: f32) -> f32 {
    v.min(hi).max(lo)
}

/// The full-mode rect to save for this frame's viewport: outer position
/// and inner size. `None` while minimized or when egui has no usable rect.
pub fn full_rect_of(viewport: &ViewportInfo) -> Option<SavedRect> {
    if viewport.minimized == Some(true) {
        return None;
    }
    let (outer, inner) = (viewport.outer_rect?, viewport.inner_rect?);
    let rect = SavedRect {
        x: outer.min.x,
        y: outer.min.y,
        w: inner.width(),
        h: inner.height(),
    };
    let usable = [rect.x, rect.y, rect.w, rect.h]
        .iter()
        .all(|v| v.is_finite())
        && rect.w >= 1.0
        && rect.h >= 1.0;
    usable.then_some(rect)
}

/// The desktop's bounding rect (all monitors) in points, read before eframe
/// makes the process DPI-aware, so Windows reports it scaled to the system
/// DPI: the same logical units winit uses for the builder's position.
#[cfg(windows)]
pub fn screen_bounds() -> Option<Rect> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    // SAFETY: GetSystemMetrics has no preconditions.
    let [x, y, w, h] = [
        SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
        SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN,
    ]
    .map(|m| unsafe { GetSystemMetrics(m) } as f32);
    (w > 0.0 && h > 0.0).then(|| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)))
}

#[cfg(not(windows))]
pub fn screen_bounds() -> Option<Rect> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Pos;
    use eframe::egui;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> SavedRect {
        SavedRect { x, y, w, h }
    }

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1700.0, 960.0))
    }

    #[test]
    fn first_run_opens_full_at_default_size() {
        let b = viewport_builder(&Settings::default(), Some(screen()));
        assert_eq!(b.inner_size, Some(FULL_SIZE));
        assert_eq!(b.min_inner_size, Some(FULL_MIN));
        assert_eq!(b.position, None);
        assert_eq!(b.decorations, Some(true));
        assert_eq!(b.resizable, Some(true));
        assert_eq!(b.window_level, Some(egui::WindowLevel::Normal));
        assert_eq!(b.title.as_deref(), Some(TITLE));
        assert_eq!(b.app_id.as_deref(), Some(APP_ID));
        assert_eq!(
            (FULL_SIZE, FULL_MIN),
            (Vec2::new(420.0, 460.0), Vec2::new(360.0, 320.0))
        );
    }

    #[test]
    fn saved_rect_and_pin_are_restored() {
        let s = Settings {
            full_rect: Some(rect(300.0, 120.5, 500.0, 600.0)),
            always_on_top: true,
            ..Settings::default()
        };
        let b = viewport_builder(&s, Some(screen()));
        assert_eq!(b.position, Some(Pos2::new(300.0, 120.5)));
        assert_eq!(b.inner_size, Some(Vec2::new(500.0, 600.0)));
        assert_eq!(b.window_level, Some(egui::WindowLevel::AlwaysOnTop));
    }

    #[test]
    fn pill_mode_opens_full_until_task_8() {
        let saved = rect(10.0, 20.0, 400.0, 400.0);
        for s in [
            Settings {
                ui_mode: UiMode::Pill,
                full_rect: Some(saved),
                pill_pos: Some(Pos { x: 1.0, y: 2.0 }),
                ..Settings::default()
            },
            Settings {
                start_in_pill: true,
                full_rect: Some(saved),
                ..Settings::default()
            },
        ] {
            assert_eq!(startup_mode(&s), UiMode::Pill);
            let b = viewport_builder(&s, Some(screen()));
            assert_eq!(b.position, Some(Pos2::new(10.0, 20.0)));
            assert_eq!(b.decorations, Some(true));
        }
        assert_eq!(startup_mode(&Settings::default()), UiMode::Full);
    }

    #[test]
    fn tiny_saved_size_is_raised_to_the_minimum() {
        let (_, size) = sane_full_rect(Some(rect(0.0, 0.0, 300.0, 48.0)), Some(screen()));
        assert_eq!(size, FULL_MIN);
        let (_, size) = sane_full_rect(Some(rect(0.0, 0.0, 300.0, 48.0)), None);
        assert_eq!(size, FULL_MIN);
    }

    #[test]
    fn oversized_saved_size_fits_the_screen() {
        let (_, size) = sane_full_rect(Some(rect(0.0, 0.0, 5000.0, 400.0)), Some(screen()));
        assert_eq!(size, Vec2::new(1700.0, 400.0));
    }

    #[test]
    fn off_screen_position_is_pulled_back_to_a_visible_strip() {
        let size = Vec2::new(420.0, 460.0);
        let off = |x, y| sane_full_rect(Some(rect(x, y, size.x, size.y)), Some(screen())).0;
        // Far right / bottom (monitor unplugged): MIN_VISIBLE stays on screen.
        assert_eq!(
            off(5000.0, 3000.0),
            Some(Pos2::new(1700.0 - MIN_VISIBLE.x, 960.0 - MIN_VISIBLE.y))
        );
        // Far left: the right MIN_VISIBLE.x of the window stays on screen.
        // Above the top: the title bar is brought back inside.
        assert_eq!(
            off(-3000.0, -500.0),
            Some(Pos2::new(MIN_VISIBLE.x - size.x, 0.0))
        );
        // Already visible: untouched, including partly off the left edge.
        assert_eq!(off(-100.0, 50.0), Some(Pos2::new(-100.0, 50.0)));
    }

    #[test]
    fn negative_desktop_origin_is_respected() {
        // A second monitor left of the primary.
        let desk = Rect::from_min_max(Pos2::new(-1920.0, -200.0), Pos2::new(1700.0, 960.0));
        let (pos, _) = sane_full_rect(Some(rect(-1800.0, -100.0, 420.0, 460.0)), Some(desk));
        assert_eq!(pos, Some(Pos2::new(-1800.0, -100.0)));
    }

    #[test]
    fn non_finite_values_fall_back() {
        let (pos, size) = sane_full_rect(
            Some(rect(f32::NAN, 10.0, f32::INFINITY, 500.0)),
            Some(screen()),
        );
        assert_eq!((pos, size), (None, FULL_SIZE));
        // Unknown screen: the position is kept as saved.
        let (pos, _) = sane_full_rect(Some(rect(-9000.0, 10.0, 420.0, 460.0)), None);
        assert_eq!(pos, Some(Pos2::new(-9000.0, 10.0)));
    }

    #[test]
    fn screen_smaller_than_the_visible_strip_does_not_panic() {
        let tiny = Rect::from_min_size(Pos2::ZERO, Vec2::new(50.0, 20.0));
        let (pos, size) = sane_full_rect(Some(rect(500.0, 500.0, 420.0, 460.0)), Some(tiny));
        assert_eq!(size, FULL_MIN);
        assert!(pos.is_some_and(|p| p.x.is_finite() && p.y.is_finite()));
    }

    #[test]
    fn full_rect_from_viewport() {
        let mut v = ViewportInfo {
            outer_rect: Some(Rect::from_min_size(
                Pos2::new(100.0, 50.0),
                Vec2::new(436.0, 499.0),
            )),
            inner_rect: Some(Rect::from_min_size(
                Pos2::new(108.0, 81.0),
                Vec2::new(420.0, 460.0),
            )),
            ..ViewportInfo::default()
        };
        assert_eq!(full_rect_of(&v), Some(rect(100.0, 50.0, 420.0, 460.0)));
        v.minimized = Some(true);
        assert_eq!(full_rect_of(&v), None);
        v.minimized = Some(false);
        v.inner_rect = None;
        assert_eq!(full_rect_of(&v), None);
        v.inner_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
        assert_eq!(full_rect_of(&v), None);
    }

    #[test]
    fn settings_read_from_app_ron_text() {
        let saved = Settings {
            plan: "Pro".into(),
            full_rect: Some(rect(1.0, 2.0, 430.0, 470.0)),
            ..Settings::default()
        };
        let mut map = HashMap::new();
        map.insert(
            SETTINGS_KEY.to_owned(),
            ron::ser::to_string(&saved).unwrap(),
        );
        map.insert("egui".to_owned(), "(whatever)".to_owned());
        let text = ron::ser::to_string_pretty(&map, Default::default()).unwrap();
        assert_eq!(settings_from_app_ron(&text), saved);

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
