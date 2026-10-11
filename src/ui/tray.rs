//! The tray icon: the live 5-hour % drawn as two digits on a state-colored
//! rounded square, a tooltip, and a menu (Show / Hide, Pill mode, Refresh
//! now, Quit).
//!
//! The pure parts (icon pixels, tooltip text, event mapping, the close
//! decision) are testable on any platform; [`Tray`] owns the native icon.
//! Its event handlers only push a [`TrayAction`] into a channel and wake
//! the app; [`crate::ui::App::logic`] drains it, so the tray works while the
//! window is hidden.

use std::fmt::Display;

use chrono::{DateTime, TimeZone, Utc};
use eframe::egui::Color32;

use crate::analytics::{self, alerts::fmt_clock_in, Level};

use super::chips::pct_text;
use super::theme;

/// Menu item ids.
pub const MENU_SHOW_HIDE: &str = "show_hide";
pub const MENU_PILL: &str = "pill";
pub const MENU_REFRESH: &str = "refresh";
pub const MENU_QUIT: &str = "quit";

/// Windows' tooltip buffer is 128 UTF-16 units including the terminator.
pub const TOOLTIP_MAX: usize = 127;

/// What the tray asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Left click or "Show / Hide".
    ToggleVisible,
    /// "Pill mode" was clicked; the item's check state (already flipped by
    /// the menu) is the mode wanted.
    PillMenu,
    Refresh,
    Quit,
}

/// The action for a menu item id.
pub fn menu_action(id: &str) -> Option<TrayAction> {
    match id {
        MENU_SHOW_HIDE => Some(TrayAction::ToggleVisible),
        MENU_PILL => Some(TrayAction::PillMenu),
        MENU_REFRESH => Some(TrayAction::Refresh),
        MENU_QUIT => Some(TrayAction::Quit),
        _ => None,
    }
}

/// What the window close button (or Alt+F4) does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// Let the app exit.
    Allow,
    /// Cancel the close and hide the window to the tray.
    HideToTray,
}

/// Close to the tray only while the tray icon exists, the user wants it
/// ([`crate::config::Settings::closes_to_tray`]), and Quit was not chosen.
pub fn close_decision(tray_shown: bool, closes_to_tray: bool, quitting: bool) -> CloseDecision {
    if tray_shown && closes_to_tray && !quitting {
        CloseDecision::HideToTray
    } else {
        CloseDecision::Allow
    }
}

/// The text on the icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconLabel {
    /// Whole percent, 0-99, as two digits.
    Pct(u8),
    /// 100% or more: "!!".
    Over,
    /// No figure: "–".
    Unknown,
}

/// Everything the icon shows; it is redrawn only when this changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconFace {
    pub label: IconLabel,
    pub level: Level,
}

impl IconFace {
    /// The face for a 5-hour utilization: whole percent rounded down, like
    /// the chips, so 99.6 never reads as the cap.
    pub fn for_pct(pct: Option<f64>) -> Self {
        let label = match pct {
            None => IconLabel::Unknown,
            Some(p) if p >= 100.0 => IconLabel::Over,
            Some(p) => IconLabel::Pct(p.floor().max(0.0) as u8),
        };
        Self {
            label,
            level: analytics::color_for(pct),
        }
    }
}

/// The square's color for a level. The icon sits on the taskbar, not in
/// the app's theme, so it always uses the light palette's deep state colors,
/// which carry white digits at 4.5:1 or better.
pub fn icon_background(level: Level) -> Color32 {
    theme::LIGHT.level(level)
}

/// White or near-black, whichever contrasts more with `bg`.
pub fn digit_color(bg: Color32) -> Color32 {
    let white = Color32::WHITE;
    let dark = theme::LIGHT.text;
    if contrast(white, bg) >= contrast(dark, bg) {
        white
    } else {
        dark
    }
}

/// WCAG relative luminance.
fn luminance(c: Color32) -> f64 {
    let lin = |v: u8| {
        let s = v as f64 / 255.0;
        if s <= 0.040_45 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio.
pub fn contrast(a: Color32, b: Color32) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// 3x5 pixel glyphs, one row per byte, high bit on the left.
const GLYPH_W: u32 = 3;
const GLYPH_H: u32 = 5;
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];
const BANG: [u8; 5] = [0b010, 0b010, 0b010, 0b000, 0b010];
const DASH: [u8; 5] = [0b000, 0b000, 0b111, 0b000, 0b000];

fn glyphs(label: IconLabel) -> Vec<[u8; 5]> {
    match label {
        IconLabel::Pct(p) => vec![DIGITS[(p / 10 % 10) as usize], DIGITS[(p % 10) as usize]],
        IconLabel::Over => vec![BANG, BANG],
        IconLabel::Unknown => vec![DASH],
    }
}

/// The icon as straight-alpha RGBA, `size` x `size` (render at the system's
/// small-icon size). Glyph pixels are whole `k` x `k` blocks, `k = size / 8`,
/// so the digits stay crisp at every size; the square's corners are
/// anti-aliased.
pub fn render_icon_rgba(size: u32, face: IconFace) -> Vec<u8> {
    let bg = icon_background(face.level);
    let fg = digit_color(bg);
    let mut px = vec![0u8; (size * size * 4) as usize];
    // Rounded square, 4x4 supersampled coverage.
    let s = size as f32;
    let rad = s * 0.25;
    const SS: u32 = 4;
    for y in 0..size {
        for x in 0..size {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let (cx, cy) = (fx.clamp(rad, s - rad), fy.clamp(rad, s - rad));
                    if (fx - cx).powi(2) + (fy - cy).powi(2) <= rad * rad {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                let a = (hits * 255 / (SS * SS)) as u8;
                let i = ((y * size + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&[bg.r(), bg.g(), bg.b(), a]);
            }
        }
    }
    // Glyphs: 3k wide, k apart, centered.
    let k = (size / 8).max(1);
    let glyphs = glyphs(face.label);
    let n = glyphs.len() as u32;
    let width = n * GLYPH_W * k + (n - 1) * k;
    let (x0, y0) = (
        size.saturating_sub(width) / 2,
        size.saturating_sub(GLYPH_H * k) / 2,
    );
    for (g, rows) in glyphs.iter().enumerate() {
        let gx = x0 + g as u32 * (GLYPH_W + 1) * k;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..GLYPH_W {
                if bits & (0b100 >> col) == 0 {
                    continue;
                }
                for yy in 0..k {
                    for xx in 0..k {
                        let (x, y) = (gx + col * k + xx, y0 + row as u32 * k + yy);
                        if x < size && y < size {
                            let i = ((y * size + x) * 4) as usize;
                            px[i..i + 4].copy_from_slice(&[fg.r(), fg.g(), fg.b(), 255]);
                        }
                    }
                }
            }
        }
    }
    px
}

/// "Claude usage · 5h 46% · resets 3:42 PM", the reset left out when
/// unknown and " · last poll failed" added while polls fail; at most
/// [`TOOLTIP_MAX`] UTF-16 units.
pub fn tooltip_in<Tz: TimeZone>(
    pct: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
    failed: bool,
    tz: &Tz,
) -> String
where
    Tz::Offset: Display,
{
    let mut text = format!("Claude usage · 5h {}", pct_text(pct));
    if let Some(at) = resets_at {
        text.push_str(&format!(" · resets {}", fmt_clock_in(at, tz)));
    }
    if failed {
        text.push_str(" · last poll failed");
    }
    cap_utf16(text, TOOLTIP_MAX)
}

/// `text` cut to at most `max` UTF-16 units, ending in "…" when cut.
fn cap_utf16(text: String, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text;
    }
    let mut out = String::new();
    let mut units = 0;
    for c in text.chars() {
        if units + c.len_utf16() > max - 1 {
            break;
        }
        units += c.len_utf16();
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(windows)]
pub use native::Tray;

/// No tray outside Windows: [`Tray::create`] always fails.
#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn create(_ctx: &eframe::egui::Context, _pill: bool) -> Result<Self, String> {
        Err("no tray icon on this platform".to_owned())
    }
    pub fn try_recv(&self) -> Option<TrayAction> {
        None
    }
    pub fn pill_checked(&self) -> bool {
        false
    }
    pub fn set_pill_checked(&self, _on: bool) {}
    pub fn update(&mut self, _face: IconFace, _tooltip: String) {}
}

#[cfg(windows)]
mod native {
    use std::sync::mpsc::{channel, Receiver};

    use eframe::egui;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

    use super::{menu_action, render_icon_rgba, IconFace, IconLabel, TrayAction};

    /// The action for a click on the icon: only a left button release
    /// (right click opens the menu; moves, enters and leaves are dropped).
    pub fn click_action(event: &TrayIconEvent) -> Option<TrayAction> {
        match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => Some(TrayAction::ToggleVisible),
            _ => None,
        }
    }

    /// The live tray icon and its menu. Created and dropped on the main
    /// thread (the menu items are not `Send`); dropping it removes the icon.
    pub struct Tray {
        icon: TrayIcon,
        pill_item: CheckMenuItem,
        rx: Receiver<TrayAction>,
        /// Icon edge in pixels (the system small-icon size).
        size: u32,
        face: IconFace,
        tooltip: String,
    }

    impl Tray {
        /// Show the icon; its events wake `ctx`. Never opens the menu
        /// itself: without a user gesture the popup gets stuck.
        pub fn create(ctx: &egui::Context, pill: bool) -> Result<Self, String> {
            let show_hide = MenuItem::with_id(super::MENU_SHOW_HIDE, "Show / Hide", true, None);
            let pill_item = CheckMenuItem::with_id(super::MENU_PILL, "Pill mode", true, pill, None);
            let refresh = MenuItem::with_id(super::MENU_REFRESH, "Refresh now", true, None);
            let quit = MenuItem::with_id(super::MENU_QUIT, "Quit", true, None);
            let menu = Menu::new();
            menu.append_items(&[
                &show_hide,
                &pill_item,
                &refresh,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .map_err(|e| e.to_string())?;
            // SAFETY: plain query, no pointers.
            let size = (unsafe { GetSystemMetrics(SM_CXSMICON) }).clamp(16, 256) as u32;
            let face = IconFace {
                label: IconLabel::Unknown,
                level: crate::analytics::Level::Dim,
            };
            let tooltip = super::tooltip_in(None, None, false, &chrono::Local);
            let icon = TrayIconBuilder::new()
                .with_id("claude-usage")
                .with_tooltip(&tooltip)
                .with_icon(render(size, face)?)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .map_err(|e| e.to_string())?;

            let (tx, rx) = channel();
            {
                let (tx, ctx) = (tx.clone(), ctx.clone());
                MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                    if let Some(action) = menu_action(e.id.0.as_str()) {
                        let _ = tx.send(action);
                        ctx.request_repaint();
                    }
                }));
            }
            {
                let ctx = ctx.clone();
                TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                    if let Some(action) = click_action(&e) {
                        let _ = tx.send(action);
                        ctx.request_repaint();
                    }
                }));
            }
            Ok(Self {
                icon,
                pill_item,
                rx,
                size,
                face,
                tooltip,
            })
        }

        /// The next queued action, if any.
        pub fn try_recv(&self) -> Option<TrayAction> {
            self.rx.try_recv().ok()
        }

        /// The "Pill mode" check (the menu flips it before reporting).
        pub fn pill_checked(&self) -> bool {
            self.pill_item.is_checked()
        }

        /// Keep "Pill mode" in step with the window's mode.
        pub fn set_pill_checked(&self, on: bool) {
            if self.pill_item.is_checked() != on {
                self.pill_item.set_checked(on);
            }
        }

        /// Redraw the icon and tooltip, each only when it changed.
        pub fn update(&mut self, face: IconFace, tooltip: String) {
            if face != self.face {
                if let Ok(icon) = render(self.size, face) {
                    if self.icon.set_icon(Some(icon)).is_ok() {
                        self.face = face;
                    }
                }
            }
            if tooltip != self.tooltip && self.icon.set_tooltip(Some(&tooltip)).is_ok() {
                self.tooltip = tooltip;
            }
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            // Release the handlers' context and sender with the icon.
            MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
            TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        }
    }

    fn render(size: u32, face: IconFace) -> Result<Icon, String> {
        Icon::from_rgba(render_icon_rgba(size, face), size, size).map_err(|e| e.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tray_icon::dpi::PhysicalPosition;
        use tray_icon::{Rect, TrayIconId};

        fn click(button: MouseButton, button_state: MouseButtonState) -> TrayIconEvent {
            TrayIconEvent::Click {
                id: TrayIconId::new("t"),
                position: PhysicalPosition::new(0.0, 0.0),
                rect: Rect::default(),
                button,
                button_state,
            }
        }

        #[test]
        fn only_left_button_release_toggles() {
            assert_eq!(
                click_action(&click(MouseButton::Left, MouseButtonState::Up)),
                Some(TrayAction::ToggleVisible)
            );
            assert_eq!(
                click_action(&click(MouseButton::Left, MouseButtonState::Down)),
                None
            );
            assert_eq!(
                click_action(&click(MouseButton::Right, MouseButtonState::Up)),
                None
            );
            assert_eq!(
                click_action(&click(MouseButton::Middle, MouseButtonState::Up)),
                None
            );
            let id = TrayIconId::new("t");
            let pos = PhysicalPosition::new(1.0, 2.0);
            for e in [
                TrayIconEvent::Move {
                    id: id.clone(),
                    position: pos,
                    rect: Rect::default(),
                },
                TrayIconEvent::Enter {
                    id: id.clone(),
                    position: pos,
                    rect: Rect::default(),
                },
                TrayIconEvent::Leave {
                    id: id.clone(),
                    position: pos,
                    rect: Rect::default(),
                },
                TrayIconEvent::DoubleClick {
                    id,
                    position: pos,
                    rect: Rect::default(),
                    button: MouseButton::Left,
                },
            ] {
                assert_eq!(click_action(&e), None, "{e:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const SIZES: [u32; 6] = [16, 20, 24, 32, 36, 48];

    fn face(pct: Option<f64>) -> IconFace {
        IconFace::for_pct(pct)
    }

    /// The glyph pixels as rows of '#' and '.', cropped to their bounding box.
    fn glyph_mask(size: u32, f: IconFace) -> Vec<String> {
        let px = render_icon_rgba(size, f);
        let fg = digit_color(icon_background(f.level));
        let on = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            px[i..i + 4] == [fg.r(), fg.g(), fg.b(), 255]
        };
        let pts: Vec<(u32, u32)> = (0..size)
            .flat_map(|y| (0..size).map(move |x| (x, y)))
            .filter(|&(x, y)| on(x, y))
            .collect();
        let (x0, x1) = (
            pts.iter().map(|p| p.0).min().unwrap(),
            pts.iter().map(|p| p.0).max().unwrap(),
        );
        let (y0, y1) = (
            pts.iter().map(|p| p.1).min().unwrap(),
            pts.iter().map(|p| p.1).max().unwrap(),
        );
        (y0..=y1)
            .map(|y| {
                (x0..=x1)
                    .map(|x| if on(x, y) { '#' } else { '.' })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn face_from_pct() {
        assert_eq!(face(Some(46.7)).label, IconLabel::Pct(46));
        assert_eq!(face(Some(46.7)).level, Level::Green);
        assert_eq!(face(Some(0.0)).label, IconLabel::Pct(0));
        assert_eq!(face(Some(99.9)).label, IconLabel::Pct(99));
        assert_eq!(face(Some(99.9)).level, Level::Red);
        assert_eq!(face(Some(100.0)).label, IconLabel::Over);
        assert_eq!(face(Some(130.0)).label, IconLabel::Over);
        assert_eq!(
            face(None),
            IconFace {
                label: IconLabel::Unknown,
                level: Level::Dim
            }
        );
        assert_eq!(face(Some(76.0)).level, Level::Orange);
        // Same whole number and level: same face, so no redraw.
        assert_eq!(face(Some(46.1)), face(Some(46.9)));
        assert_ne!(face(Some(74.9)), face(Some(75.0)));
    }

    #[test]
    fn icon_has_the_requested_size_and_transparent_corners() {
        for size in SIZES {
            let px = render_icon_rgba(size, face(Some(42.0)));
            assert_eq!(px.len(), (size * size * 4) as usize);
            assert_eq!(px[3], 0, "top-left corner transparent at {size}");
            let last = px.len() - 1;
            assert_eq!(px[last], 0, "bottom-right corner transparent at {size}");
            // Edge midpoints are opaque square.
            let mid = ((size / 2) * size * 4 + 3) as usize;
            assert_eq!(px[mid], 255, "left edge middle opaque at {size}");
        }
    }

    #[test]
    fn square_color_follows_the_level() {
        let size = 32;
        let at = |px: &[u8], x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            Color32::from_rgba_unmultiplied(px[i], px[i + 1], px[i + 2], px[i + 3])
        };
        for (pct, level) in [
            (Some(10.0), Level::Green),
            (Some(55.0), Level::Yellow),
            (Some(80.0), Level::Orange),
            (Some(95.0), Level::Red),
            (None, Level::Dim),
        ] {
            let px = render_icon_rgba(size, face(pct));
            // Just inside the left edge, clear of the glyphs.
            assert_eq!(at(&px, 1, size / 2), icon_background(level), "{pct:?}");
        }
        let levels = [
            Level::Green,
            Level::Yellow,
            Level::Orange,
            Level::Red,
            Level::Dim,
        ];
        for (i, a) in levels.iter().enumerate() {
            for b in &levels[i + 1..] {
                assert_ne!(icon_background(*a), icon_background(*b));
            }
        }
    }

    #[test]
    fn digits_read_on_every_background() {
        for level in [
            Level::Green,
            Level::Yellow,
            Level::Orange,
            Level::Red,
            Level::Dim,
        ] {
            let bg = icon_background(level);
            assert!(contrast(digit_color(bg), bg) >= 4.5, "{level:?}");
        }
    }

    #[test]
    fn digit_mask_at_16px_is_the_pixel_font_doubled() {
        let mask = glyph_mask(16, face(Some(42.0)));
        let expected = [
            "##..##..######",
            "##..##..######",
            "##..##......##",
            "##..##......##",
            "######..######",
            "######..######",
            "....##..##....",
            "....##..##....",
            "....##..######",
            "....##..######",
        ];
        assert_eq!(mask, expected);
    }

    #[test]
    fn glyphs_scale_in_whole_blocks_and_stay_centered() {
        for size in SIZES {
            let k = (size / 8).max(1) as usize;
            let mask = glyph_mask(size, face(Some(88.0)));
            // "8 8": 7 cells wide (3 + gap + 3), 5 tall, each cell k px.
            assert_eq!(mask.len(), 5 * k, "height at {size}");
            assert_eq!(mask[0].len(), 7 * k, "width at {size}");
            let px = render_icon_rgba(size, face(Some(88.0)));
            let fg = digit_color(icon_background(Level::Orange));
            let first_x = (0..size)
                .find(|&x| {
                    (0..size).any(|y| {
                        let i = ((y * size + x) * 4) as usize;
                        px[i..i + 4] == [fg.r(), fg.g(), fg.b(), 255]
                    })
                })
                .unwrap();
            let margin_right = size - first_x - 7 * k as u32;
            assert!(first_x.abs_diff(margin_right) <= 1, "centered at {size}");
        }
    }

    #[test]
    fn over_and_unknown_glyphs() {
        assert_eq!(
            glyph_mask(16, face(Some(100.0))),
            [
                "##......##",
                "##......##",
                "##......##",
                "##......##",
                "##......##",
                "##......##",
                "..........",
                "..........",
                "##......##",
                "##......##",
            ]
        );
        assert_eq!(glyph_mask(16, face(None)), ["######", "######"]);
    }

    #[test]
    fn tooltip_text() {
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let reset = DateTime::parse_from_rfc3339("2026-10-11T19:42:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            tooltip_in(Some(46.9), Some(reset), false, &tz),
            "Claude usage · 5h 46% · resets 3:42 PM"
        );
        assert_eq!(tooltip_in(None, None, false, &tz), "Claude usage · 5h —");
        assert_eq!(
            tooltip_in(Some(12.0), None, true, &tz),
            "Claude usage · 5h 12% · last poll failed"
        );
    }

    #[test]
    fn tooltip_is_capped_in_utf16_units() {
        let long = "é".repeat(100) + &"😀".repeat(40);
        let cut = cap_utf16(long, TOOLTIP_MAX);
        assert!(cut.encode_utf16().count() <= TOOLTIP_MAX);
        assert!(cut.ends_with('…'));
        let short = "Claude usage".to_owned();
        assert_eq!(cap_utf16(short.clone(), TOOLTIP_MAX), short);
        let exact = "a".repeat(TOOLTIP_MAX);
        assert_eq!(cap_utf16(exact.clone(), TOOLTIP_MAX), exact);
    }

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(menu_action(MENU_SHOW_HIDE), Some(TrayAction::ToggleVisible));
        assert_eq!(menu_action(MENU_PILL), Some(TrayAction::PillMenu));
        assert_eq!(menu_action(MENU_REFRESH), Some(TrayAction::Refresh));
        assert_eq!(menu_action(MENU_QUIT), Some(TrayAction::Quit));
        assert_eq!(menu_action("something else"), None);
    }

    #[test]
    fn close_hides_to_tray_only_with_tray_setting_and_not_quitting() {
        use CloseDecision::*;
        for tray in [false, true] {
            for closes in [false, true] {
                for quitting in [false, true] {
                    let want = if tray && closes && !quitting {
                        HideToTray
                    } else {
                        Allow
                    };
                    assert_eq!(
                        close_decision(tray, closes, quitting),
                        want,
                        "{tray} {closes} {quitting}"
                    );
                }
            }
        }
    }
}
