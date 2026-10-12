//! Visibility and tray glue for [`App`]: hide to the tray, show, quit,
//! the close button, and keeping the tray icon in step with the settings
//! and the latest poll. A child module of `app` so it can reach the app's
//! private state.

use std::time::Instant;

use chrono::Local;
use eframe::egui;

use crate::config::UiMode;
use crate::crash;
use crate::paths::Paths;

use super::{refresh_ready, App};
use crate::ui::tray::{self, CloseDecision, IconFace, Tray, TrayAction};

impl App {
    /// Hide to the tray. Minimizing as well as hiding is what makes eframe
    /// skip `ui()` (it only checks the minimized state); `logic()` goes on.
    fn hide(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.hidden = true;
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
        let snap = self.state.data.as_ref().map(|d| &d.snapshot);
        let pct = snap.and_then(|s| s.five_hour_pct());
        let resets_at = snap.and_then(|s| s.five_hour_resets_at());
        let failed = self.poll_failing();
        let pill = self.settings.ui_mode == UiMode::Pill;
        if let Some(t) = self.tray.as_mut() {
            t.update(
                IconFace::for_pct(pct),
                tray::tooltip_in(pct, resets_at, failed, &Local),
            );
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
                let minimized = ctx.input(|i| i.viewport().minimized) == Some(true);
                if self.hidden || minimized {
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
