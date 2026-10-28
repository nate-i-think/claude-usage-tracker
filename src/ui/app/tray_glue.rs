//! Visibility and tray glue for [`App`]: hide to the tray, show, quit,
//! the close button, and keeping the tray icon in step with the settings
//! and the latest poll. A child module of `app` so it can reach the app's
//! private state.

use std::time::Instant;

use chrono::{Local, Utc};
use eframe::egui;

use crate::config::UiMode;
use crate::crash;
use crate::paths::Paths;

use super::{refresh_ready, App};
use crate::ui::theme;
use crate::ui::tray::{self, CloseDecision, Tray, TrayAction};

impl App {
    /// Hide to the tray (the menu bar on macOS); `logic()` goes on either
    /// way, as eframe runs it for an invisible window on a timer instead of
    /// waiting for a redraw. On Windows, minimizing as well as hiding is
    /// what makes eframe skip `ui()` (it reads the minimized state). On
    /// macOS hiding alone does it (the window reports itself occluded), and
    /// minimizing would animate into the Dock.
    fn hide(&mut self, ctx: &egui::Context) {
        #[cfg(not(target_os = "macos"))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        // Otherwise the app stays active with no window, and a click on the
        // Dock icon would not count as reopening it.
        #[cfg(target_os = "macos")]
        tray::deactivate_app();
        self.hidden = true;
    }

    /// macOS: bring a window hidden to the menu bar back when the app is
    /// reopened (Dock icon, Cmd+Tab, launched again), so it is never stuck
    /// hidden when the menu-bar item cannot be seen (a crowded menu bar,
    /// the notch, a menu-bar manager). winit has no reopen event, so this
    /// watches for the app becoming active.
    #[cfg(target_os = "macos")]
    fn show_on_reopen(&mut self, ctx: &egui::Context) {
        let now = tray::app_active();
        let was = std::mem::replace(&mut self.was_active, now);
        if self.hidden && tray::became_active(was, now) {
            self.show(ctx);
        }
    }

    /// Bring the window back from the tray (or from the taskbar).
    pub(super) fn show(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.hidden = false;
        ctx.request_repaint();
    }

    /// Exit for real: the close request this sends is not turned into a
    /// hide. `save()` still runs.
    pub(super) fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// The close button (or Alt+F4) hides to the tray when the settings say
    /// so.
    pub(super) fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        let decision = tray::close_decision(
            self.tray.is_some(),
            self.settings.closes_to_tray(),
            self.quitting,
        );
        if decision == CloseDecision::HideToTray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(ctx);
        }
    }

    /// Create or drop the tray icon to match the setting, act on its clicks
    /// and menu, and keep its icon, tooltip and "Pill mode" check current.
    pub(super) fn sync_tray(&mut self, ctx: &egui::Context) {
        self.sync_tray_presence();
        if self.tray.is_none() {
            // Left over from a dropped icon.
            while self.tray_events.try_recv().is_ok() {}
            // Nothing could bring the window back.
            if self.hidden {
                self.show(ctx);
            }
            return;
        }
        while let Ok(action) = self.tray_events.try_recv() {
            self.tray_action(ctx, action);
        }
        #[cfg(target_os = "macos")]
        self.show_on_reopen(ctx);
        // As of now: a window that rolled over since the poll reads 0%
        // with no reset.
        let view = tray::tray_view(
            &self.accounts.views(&theme::DARK),
            Utc::now(),
            !self.poller.is_alive(),
            &Local,
        );
        let pill = self.settings.ui_mode == UiMode::Pill;
        if let Some(t) = self.tray.as_mut() {
            t.update(&view);
            t.set_pill_checked(pill);
        }
    }

    /// Create the tray icon (on the main thread) when the setting is on and
    /// it does not exist yet; drop it when the setting is off.
    pub(super) fn sync_tray_presence(&mut self) {
        if !self.settings.tray_enabled {
            self.tray_failed = false;
            self.tray = None;
        } else if self.tray.is_none() && !self.tray_failed {
            match Tray::create(self.settings.ui_mode == UiMode::Pill) {
                Ok(t) => self.tray = Some(t),
                Err(e) => {
                    self.tray_failed = true;
                    let _ = crash::append(
                        &Paths::platform_default(),
                        Local::now(),
                        &format!("Tray icon unavailable: {e}"),
                    );
                }
            }
        }
    }

    fn tray_action(&mut self, ctx: &egui::Context, action: TrayAction) {
        match action {
            TrayAction::ToggleVisible => {
                let now = Instant::now();
                if !tray::toggle_accepted(self.last_toggle, now, tray::double_click_time()) {
                    return;
                }
                self.last_toggle = Some(now);
                // macOS reads the minimized state only at startup, so there
                // a window that is minimized to the Dock or behind other
                // apps (not focused) is brought forward rather than hidden.
                let backgrounded = if cfg!(target_os = "macos") {
                    ctx.input(|i| i.viewport().focused) == Some(false)
                } else {
                    ctx.input(|i| i.viewport().minimized) == Some(true)
                };
                if self.hidden || backgrounded {
                    self.show(ctx);
                } else {
                    self.hide(ctx);
                }
            }
            TrayAction::PillMenu => {
                let pill = self.tray.as_ref().is_some_and(Tray::pill_checked);
                if self.hidden {
                    self.show(ctx);
                }
                self.set_mode(ctx, if pill { UiMode::Pill } else { UiMode::Full });
            }
            TrayAction::Refresh => {
                if refresh_ready(self.last_refresh, Instant::now()) {
                    self.refresh_now();
                }
            }
            TrayAction::Quit => self.quit(ctx),
        }
    }
}
