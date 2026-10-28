//! The tray icon. On Windows: the 5-hour % of the account with the most
//! headroom drawn as two digits on a state-colored rounded square, with a
//! tooltip listing every account. On macOS: a menu-bar item whose title is
//! every account's 5-hour % ("46 · 12"), no icon. Both carry the same menu
//! (Show / Hide, Pill mode, Refresh now, Quit); on macOS it opens on a left
//! click.
//!
//! The pure parts (icon pixels, title and tooltip text, event mapping, the
//! close decision) are testable on any platform; [`Tray`] owns the native
//! icon. Its event handlers only push a [`TrayAction`] into a channel and
//! wake the app; `App::logic` ([`eframe::App::logic`]) drains it, so the
//! tray works while the window is hidden.

use std::fmt::Display;
#[cfg(not(any(windows, target_os = "macos")))]
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use eframe::egui::Color32;

use crate::analytics::{self, alerts::fmt_clock_in, Level};

use super::accounts::{best_headroom, AccountView};
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

/// Whether a [`TrayAction::ToggleVisible`] at `now` should act, given the
/// last one that did. A double click on the icon reports two left-button
/// releases (down, up, double-click, up); the second, inside the system
/// double-click time `window`, is dropped so the window does not show and
/// hide again at once.
pub fn toggle_accepted(last: Option<Instant>, now: Instant, window: Duration) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= window)
}

/// The system double-click time (Windows), else 500 ms.
pub fn double_click_time() -> Duration {
    #[cfg(windows)]
    {
        // SAFETY: plain query, no pointers.
        let ms = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
        if ms > 0 {
            return Duration::from_millis(ms.into());
        }
    }
    Duration::from_millis(500)
}

/// Whether the app was just brought to the front (Dock icon, Cmd+Tab,
/// launched again): active now and inactive at the last look. `None` is a
/// look not taken (the status-item menu is open), so whatever happens while
/// the menu is open is never an edge.
pub fn became_active(was: Option<bool>, now: Option<bool>) -> bool {
    was == Some(false) && now == Some(true)
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

/// Everything the tray shows for the latest polls: the icon (Windows), the
/// menu-bar title (macOS) and the tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub face: IconFace,
    pub title: String,
    pub tooltip: String,
}

/// What the tray shows for `views` at `now` (see [`badge_face`],
/// [`menu_bar_title`] and [`tooltip_for`]). `poller_dead`: the poller has
/// stopped, so every figure is from before.
pub fn tray_view<Tz: TimeZone>(
    views: &[AccountView],
    now: DateTime<Utc>,
    poller_dead: bool,
    tz: &Tz,
) -> TrayView
where
    Tz::Offset: Display,
{
    let pcts: Vec<Option<f64>> = views.iter().map(|v| five_hour_pct(v, now)).collect();
    TrayView {
        face: badge_face(views, now),
        title: menu_bar_title(&pcts),
        tooltip: tooltip_for(views, now, poller_dead, tz),
    }
}

/// An account's live 5-hour % at `now` (a window that rolled over since the
/// poll reads 0%).
fn five_hour_pct(v: &AccountView, now: DateTime<Utc>) -> Option<f64> {
    v.data?.five_hour_now(now).pct
}

/// The menu-bar title's mark for an account without a figure.
const NO_FIGURE: &str = "–";

/// The menu-bar title: each account's 5-hour whole percent, rounded down
/// like the chips, joined by " · " ("46 · 12"); "–" for an account without
/// a figure, and for no accounts at all.
pub fn menu_bar_title(pcts: &[Option<f64>]) -> String {
    if pcts.is_empty() {
        return NO_FIGURE.to_owned();
    }
    pcts.iter()
        .map(|p| match p {
            Some(p) => format!("{:.0}", p.floor()),
            None => NO_FIGURE.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The Windows badge: the account with the most headroom
/// ([`best_headroom`]), so the icon says whether there is an account left
/// to work in.
pub fn badge_face(views: &[AccountView], now: DateTime<Utc>) -> IconFace {
    IconFace::for_pct(best_headroom(views, now).and_then(|i| five_hour_pct(&views[i], now)))
}

/// The tooltip for every account. One account (or none) reads as
/// [`tooltip_in`]; more read "Claude usage" then a line per account,
/// "Work 46% · resets 3:42 PM", marked " · stale" when its last poll failed
/// (" · last poll failed" when it has no figure at all). At most
/// [`TOOLTIP_MAX`] UTF-16 units.
pub fn tooltip_for<Tz: TimeZone>(
    views: &[AccountView],
    now: DateTime<Utc>,
    poller_dead: bool,
    tz: &Tz,
) -> String
where
    Tz::Offset: Display,
{
    let failed = |v: &AccountView| v.error.is_some() || poller_dead;
    if views.len() <= 1 {
        let five = views.first().and_then(|v| Some(v.data?.five_hour_now(now)));
        let failed = views.first().map_or(poller_dead, failed);
        return tooltip_in(
            five.and_then(|f| f.pct),
            five.and_then(|f| f.resets_at),
            failed,
            tz,
        );
    }
    let mut text = "Claude usage".to_owned();
    for v in views {
        let five = v.data.map(|d| d.five_hour_now(now));
        text.push_str(&format!(
            "\n{} {}",
            v.label,
            pct_text(five.and_then(|f| f.pct))
        ));
        if let Some(at) = five.and_then(|f| f.resets_at) {
            text.push_str(&format!(" · resets {}", fmt_clock_in(at, tz)));
        }
        if failed(v) {
            text.push_str(if v.data.is_some() {
                " · stale"
            } else {
                " · last poll failed"
            });
        }
    }
    cap_utf16(text, TOOLTIP_MAX)
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
    if theme::contrast(white, bg) >= theme::contrast(dark, bg) {
        white
    } else {
        dark
    }
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
const PERCENT: [u8; 5] = [0b101, 0b001, 0b010, 0b100, 0b101];

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
    render_badge_rgba(size, bg, digit_color(bg), &glyphs(face.label))
}

/// The app's own icon (the toast's app icon): Claude coral with a "%".
pub fn render_app_icon_rgba(size: u32) -> Vec<u8> {
    let bg = theme::DARK.accent;
    render_badge_rgba(size, bg, digit_color(bg), &[PERCENT])
}

/// A rounded square in `bg` with `glyphs` centered on it in `fg`.
fn render_badge_rgba(size: u32, bg: Color32, fg: Color32, glyphs: &[[u8; 5]]) -> Vec<u8> {
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

#[cfg(target_os = "macos")]
pub use native::{app_active, deactivate_app};
#[cfg(any(windows, target_os = "macos"))]
pub use native::{install_event_handlers, Tray};

/// No tray outside Windows and macOS: nothing is ever sent.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn install_event_handlers(_ctx: &eframe::egui::Context) -> Receiver<TrayAction> {
    std::sync::mpsc::channel().1
}

/// No tray outside Windows and macOS: [`Tray::create`] always fails.
#[cfg(not(any(windows, target_os = "macos")))]
pub struct Tray;

#[cfg(not(any(windows, target_os = "macos")))]
impl Tray {
    pub fn create(_pill: bool) -> Result<Self, String> {
        Err("no tray icon on this platform".to_owned())
    }
    pub fn pill_checked(&self) -> bool {
        false
    }
    pub fn set_pill_checked(&self, _on: bool) {}
    pub fn update(&mut self, _view: &TrayView) {}
}

#[cfg(any(windows, target_os = "macos"))]
mod native {
    use std::sync::mpsc::{channel, Receiver};

    use eframe::egui;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    #[cfg(windows)]
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconEvent};
    use tray_icon::{TrayIcon, TrayIconBuilder};
    #[cfg(windows)]
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

    use super::{menu_action, TrayAction, TrayView};
    #[cfg(windows)]
    use super::{render_icon_rgba, IconFace, IconLabel};

    /// The action for a click on the icon: only a left button release
    /// (right click opens the menu; moves, enters and leaves are dropped).
    /// A double click yields two releases; the app drops the second
    /// ([`super::toggle_accepted`]).
    #[cfg(windows)]
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

    /// Route menu picks (and, on Windows, clicks on the icon) into the
    /// returned channel, waking `ctx` for each. Call once per process:
    /// tray-icon and muda keep only the first handler ever set, so the
    /// handlers outlive any one [`Tray`] (the icon is dropped and re-created
    /// with the setting).
    ///
    /// On macOS a left click opens the menu, so clicks on the item itself
    /// are not routed: the mouse-up that follows the menu would toggle the
    /// window as well.
    pub fn install_event_handlers(ctx: &egui::Context) -> Receiver<TrayAction> {
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
        #[cfg(windows)]
        {
            let ctx = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let Some(action) = click_action(&e) {
                    let _ = tx.send(action);
                    ctx.request_repaint();
                }
            }));
        }
        rx
    }

    /// The live tray icon (Windows) or menu-bar item (macOS) and its menu.
    /// Created and dropped on the main thread (the menu items are not
    /// `Send`; on macOS tray-icon refuses any other thread); dropping it
    /// removes the icon.
    pub struct Tray {
        icon: TrayIcon,
        pill_item: CheckMenuItem,
        /// Icon edge in pixels (the system small-icon size).
        #[cfg(windows)]
        size: u32,
        #[cfg(windows)]
        face: IconFace,
        #[cfg(target_os = "macos")]
        title: String,
        tooltip: String,
    }

    impl Tray {
        /// Show the icon; its events go to [`install_event_handlers`]'
        /// channel. Never opens the menu itself: without a user gesture the
        /// popup gets stuck.
        pub fn create(pill: bool) -> Result<Self, String> {
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
            let tooltip = super::tooltip_in(None, None, false, &chrono::Local);
            let builder = TrayIconBuilder::new()
                .with_id("claude-usage")
                .with_tooltip(&tooltip)
                .with_menu(Box::new(menu));
            Self::build(builder, pill_item, tooltip)
        }

        /// Windows: the badge icon; left click toggles the window and right
        /// click opens the menu.
        #[cfg(windows)]
        fn build(
            builder: TrayIconBuilder,
            pill_item: CheckMenuItem,
            tooltip: String,
        ) -> Result<Self, String> {
            // SAFETY: plain query, no pointers.
            let size = (unsafe { GetSystemMetrics(SM_CXSMICON) }).clamp(16, 256) as u32;
            let face = IconFace {
                label: IconLabel::Unknown,
                level: crate::analytics::Level::Dim,
            };
            let icon = builder
                .with_icon(render(size, face)?)
                .with_menu_on_left_click(false)
                .build()
                .map_err(|e| e.to_string())?;
            Ok(Self {
                icon,
                pill_item,
                size,
                face,
                tooltip,
            })
        }

        /// macOS: a title-only menu-bar item (tray-icon draws no image
        /// without an icon) whose menu opens on either click. The autosave
        /// name keeps the item where the user drags it.
        #[cfg(target_os = "macos")]
        fn build(
            builder: TrayIconBuilder,
            pill_item: CheckMenuItem,
            tooltip: String,
        ) -> Result<Self, String> {
            let title = super::menu_bar_title(&[]);
            let icon = builder
                .with_title(&title)
                .with_autosave_name("claude-usage")
                .with_menu_on_left_click(true)
                .build()
                .map_err(|e| e.to_string())?;
            Ok(Self {
                icon,
                pill_item,
                title,
                tooltip,
            })
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

        /// Redraw the icon (Windows) or title (macOS) and the tooltip, each
        /// only when it changed.
        pub fn update(&mut self, view: &TrayView) {
            #[cfg(windows)]
            if view.face != self.face {
                if let Ok(icon) = render(self.size, view.face) {
                    if self.icon.set_icon(Some(icon)).is_ok() {
                        self.face = view.face;
                    }
                }
            }
            #[cfg(target_os = "macos")]
            if view.title != self.title {
                self.icon.set_title(Some(&view.title));
                self.title.clone_from(&view.title);
            }
            if view.tooltip != self.tooltip && self.icon.set_tooltip(Some(&view.tooltip)).is_ok() {
                self.tooltip.clone_from(&view.tooltip);
            }
        }
    }

    /// Whether the app is the active (frontmost) one; `None` off the main
    /// thread or while a menu is being tracked (the status-item menu runs
    /// AppKit's event-tracking run-loop mode, and eframe may run a pass in
    /// it), so opening that menu never reads as the app being reopened.
    #[cfg(target_os = "macos")]
    pub fn app_active() -> Option<bool> {
        use objc2::MainThreadMarker;
        use objc2_app_kit::{NSApplication, NSEventTrackingRunLoopMode};
        use objc2_foundation::NSRunLoop;

        let mtm = MainThreadMarker::new()?;
        let mode = NSRunLoop::currentRunLoop().currentMode();
        // SAFETY: an AppKit constant, valid for the life of the process.
        let tracking = unsafe { NSEventTrackingRunLoopMode };
        if mode.is_some_and(|m| &*m == tracking) {
            return None;
        }
        Some(NSApplication::sharedApplication(mtm).isActive())
    }

    /// Hand the focus to the next app, as hiding the last window does not:
    /// a later click on the Dock icon then activates the app again, which
    /// is how the window comes back ([`super::became_active`]).
    #[cfg(target_os = "macos")]
    pub fn deactivate_app() {
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            objc2_app_kit::NSApplication::sharedApplication(mtm).deactivate();
        }
    }

    #[cfg(windows)]
    fn render(size: u32, face: IconFace) -> Result<Icon, String> {
        Icon::from_rgba(render_icon_rgba(size, face), size, size).map_err(|e| e.to_string())
    }

    #[cfg(windows)]
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
    use crate::fetch::ErrorKind;
    use crate::model::{Snapshot, UsageWindow};
    use crate::poller::{Dashboard, FetchErrorInfo};
    use crate::profiles::ProfileId;
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
    fn toggle_debounces_the_double_clicks_second_release() {
        let w = Duration::from_millis(500);
        let t = Instant::now();
        assert!(toggle_accepted(None, t, w));
        // Second release of a double click: dropped.
        assert!(!toggle_accepted(Some(t), t + Duration::from_millis(180), w));
        assert!(!toggle_accepted(Some(t), t + Duration::from_millis(499), w));
        // A separate click later: acts.
        assert!(toggle_accepted(Some(t), t + w, w));
        assert!(toggle_accepted(Some(t), t + Duration::from_secs(3), w));
        // A clock oddity (now before last) never panics; it is dropped.
        assert!(!toggle_accepted(Some(t + w), t, w));
    }

    #[test]
    fn double_click_time_is_sane() {
        let d = double_click_time();
        assert!(
            d >= Duration::from_millis(100) && d <= Duration::from_secs(5),
            "{d:?}"
        );
    }

    #[test]
    fn app_icon_is_a_coral_square_with_a_percent_sign() {
        let size = 256;
        let px = render_app_icon_rgba(size);
        assert_eq!(px.len(), (size * size * 4) as usize);
        let at = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        let bg = theme::DARK.accent;
        assert_eq!(at(0, 0)[3], 0, "rounded corner is transparent");
        assert_eq!(at(size / 2, 4), [bg.r(), bg.g(), bg.b(), 255]);
        // The "%" glyph: k = 32 px blocks, 3x5 cells centered.
        let fg = digit_color(bg);
        let (x0, y0, k) = ((size - 3 * 32) / 2, (size - 5 * 32) / 2, 32);
        let cell = |col: u32, row: u32| at(x0 + col * k + k / 2, y0 + row * k + k / 2);
        let ink = [fg.r(), fg.g(), fg.b(), 255];
        let rows: Vec<String> = (0..5)
            .map(|r| {
                (0..3)
                    .map(|c| if cell(c, r) == ink { '#' } else { '.' })
                    .collect()
            })
            .collect();
        assert_eq!(rows, ["#.#", "..#", ".#.", "#..", "#.#"]);
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
            assert!(theme::contrast(digit_color(bg), bg) >= 4.5, "{level:?}");
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

    fn t() -> DateTime<Utc> {
        DateTime::from_timestamp(1_791_000_000, 0).unwrap()
    }

    fn dashboard(pct: f64) -> Dashboard {
        Dashboard {
            snapshot: Snapshot {
                fetched_at: t(),
                windows: vec![UsageWindow {
                    label: "5-hour".to_owned(),
                    utilization: Some(pct),
                    resets_at: Some(t() + chrono::Duration::hours(2)),
                }],
                extra: None,
                breakdown: None,
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            seven_day_recent: Vec::new(),
            fable_recent: Vec::new(),
            burners: Vec::new(),
            window_start: t() - chrono::Duration::hours(3),
            window_end: t() + chrono::Duration::hours(2),
            window_samples: Vec::new(),
            week_start: t(),
            week_end: t(),
            week_samples: Vec::new(),
            persist_error: None,
            plan: None,
            token_expires_at: None,
        }
    }

    fn error() -> FetchErrorInfo {
        FetchErrorInfo {
            kind: ErrorKind::Transport,
            message: "down".to_owned(),
        }
    }

    /// Account views over `accounts`: (label, data, failed).
    fn views<'a>(
        id: &'a ProfileId,
        err: &'a FetchErrorInfo,
        accounts: &'a [(&'a str, Option<Dashboard>, bool)],
    ) -> Vec<AccountView<'a>> {
        accounts
            .iter()
            .map(|(label, data, failed)| AccountView {
                id,
                label,
                color: Color32::WHITE,
                data: data.as_ref(),
                error: failed.then_some(err),
                stale: *failed && data.is_some(),
            })
            .collect()
    }

    #[test]
    fn menu_bar_title_lists_every_account() {
        assert_eq!(menu_bar_title(&[Some(46.9), Some(12.0)]), "46 · 12");
        assert_eq!(menu_bar_title(&[Some(46.9)]), "46");
        assert_eq!(menu_bar_title(&[Some(99.6), None]), "99 · –");
        assert_eq!(menu_bar_title(&[Some(0.0), Some(130.0)]), "0 · 130");
        assert_eq!(menu_bar_title(&[None]), "–");
        assert_eq!(menu_bar_title(&[]), "–");
        // Rounds down, like the chips.
        assert_eq!(
            menu_bar_title(&[Some(46.9)]),
            pct_text(Some(46.9)).trim_end_matches('%')
        );
    }

    #[test]
    fn tray_view_title_keeps_a_stale_accounts_last_value() {
        let (id, err) = (ProfileId::new("p"), error());
        let accounts = [
            ("Work", Some(dashboard(46.0)), true),
            ("Home", Some(dashboard(12.0)), false),
            ("New", None, true),
        ];
        let view = tray_view(&views(&id, &err, &accounts), t(), false, &Utc);
        assert_eq!(view.title, "46 · 12 · –");
    }

    #[test]
    fn badge_shows_the_account_with_the_most_headroom() {
        let (id, err) = (ProfileId::new("p"), error());
        let accounts = [
            ("Work", Some(dashboard(80.0)), false),
            ("Home", Some(dashboard(30.0)), false),
            ("New", None, false),
        ];
        assert_eq!(
            badge_face(&views(&id, &err, &accounts), t()),
            IconFace::for_pct(Some(30.0))
        );
        // The lower one goes stale: the fresh one wins.
        let accounts = [
            ("Work", Some(dashboard(80.0)), false),
            ("Home", Some(dashboard(30.0)), true),
        ];
        assert_eq!(
            badge_face(&views(&id, &err, &accounts), t()),
            IconFace::for_pct(Some(80.0))
        );
        assert_eq!(badge_face(&[], t()), IconFace::for_pct(None));
        let accounts = [("New", None, true)];
        assert_eq!(
            badge_face(&views(&id, &err, &accounts), t()),
            IconFace::for_pct(None)
        );
    }

    #[test]
    fn tooltip_for_one_account_reads_as_before() {
        let (id, err) = (ProfileId::new("p"), error());
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let reset = fmt_clock_in(t() + chrono::Duration::hours(2), &tz);
        let accounts = [("Work", Some(dashboard(46.9)), false)];
        assert_eq!(
            tooltip_for(&views(&id, &err, &accounts), t(), false, &tz),
            format!("Claude usage · 5h 46% · resets {reset}")
        );
        let accounts = [("Work", Some(dashboard(46.9)), true)];
        assert_eq!(
            tooltip_for(&views(&id, &err, &accounts), t(), false, &tz),
            format!("Claude usage · 5h 46% · resets {reset} · last poll failed")
        );
        assert_eq!(tooltip_for(&[], t(), false, &tz), "Claude usage · 5h —");
        assert_eq!(
            tooltip_for(&[], t(), true, &tz),
            "Claude usage · 5h — · last poll failed"
        );
    }

    #[test]
    fn tooltip_for_several_accounts_has_a_line_each() {
        let (id, err) = (ProfileId::new("p"), error());
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let reset = fmt_clock_in(t() + chrono::Duration::hours(2), &tz);
        let accounts = [
            ("Work", Some(dashboard(46.9)), false),
            ("Home", Some(dashboard(12.0)), true),
            ("New", None, true),
        ];
        assert_eq!(
            tooltip_for(&views(&id, &err, &accounts), t(), false, &tz),
            format!(
                "Claude usage\nWork 46% · resets {reset}\nHome 12% · resets {reset} · stale\nNew — · last poll failed"
            )
        );
        // A stopped poller marks every account.
        let accounts = [
            ("Work", Some(dashboard(46.9)), false),
            ("Home", Some(dashboard(12.0)), false),
        ];
        assert_eq!(
            tooltip_for(&views(&id, &err, &accounts), t(), true, &tz),
            format!(
                "Claude usage\nWork 46% · resets {reset} · stale\nHome 12% · resets {reset} · stale"
            )
        );
    }

    #[test]
    fn tooltip_for_two_long_accounts_is_capped() {
        let (id, err) = (ProfileId::new("p"), error());
        let long_a = "Ä".repeat(60) + "😀";
        let long_b = "B".repeat(60);
        let accounts = [
            (long_a.as_str(), Some(dashboard(46.0)), true),
            (long_b.as_str(), Some(dashboard(12.0)), false),
        ];
        let text = tooltip_for(&views(&id, &err, &accounts), t(), false, &Utc);
        assert!(text.encode_utf16().count() <= TOOLTIP_MAX, "{text}");
        assert!(text.ends_with('…'));
        assert!(text.starts_with(&format!("Claude usage\n{long_a} 46%")));
    }

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(menu_action(MENU_SHOW_HIDE), Some(TrayAction::ToggleVisible));
        assert_eq!(menu_action(MENU_PILL), Some(TrayAction::PillMenu));
        assert_eq!(menu_action(MENU_REFRESH), Some(TrayAction::Refresh));
        assert_eq!(menu_action(MENU_QUIT), Some(TrayAction::Quit));
        assert_eq!(menu_action("something else"), None);
        // The ids are what the menu items are built with; keep them stable.
        assert_eq!(
            [MENU_SHOW_HIDE, MENU_PILL, MENU_REFRESH, MENU_QUIT],
            ["show_hide", "pill", "refresh", "quit"]
        );
    }

    #[test]
    fn became_active_only_on_an_inactive_to_active_edge() {
        assert!(became_active(Some(false), Some(true)));
        // Already active (the window was just closed) or still inactive.
        assert!(!became_active(Some(true), Some(true)));
        assert!(!became_active(Some(false), Some(false)));
        assert!(!became_active(Some(true), Some(false)));
        // First look, or a look skipped while the menu was open.
        assert!(!became_active(None, Some(true)));
        assert!(!became_active(Some(false), None));
        assert!(!became_active(None, None));
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
