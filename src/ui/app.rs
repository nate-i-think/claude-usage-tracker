//! The eframe app: owns the poller, drains its events, and lays out the
//! dashboard top to bottom.
//!
//! Work that must go on while the window is hidden (draining poll events,
//! telling the poller about settings changes, persisting settings, the 1 s
//! tick, tracking the window rect) runs in [`eframe::App::logic`], which
//! eframe calls every pass, even when it skips the UI. `ui()` only draws.

use std::time::{Duration, Instant};

use chrono::Utc;
use eframe::egui::{
    self, style::ScrollStyle, Align, CentralPanel, Frame, Margin, RichText, ScrollArea, Ui,
};

use crate::config::{Settings, UiMode};
use crate::paths::Paths;
use crate::poller::{Dashboard, FetchErrorInfo, PollEvent, PollMsg, Poller};

use super::banners::{banner, BannerKind};
use super::layout::{self, right_then_left, GAP};
use super::settings::{self, PanelAction, SettingsPanel};
use super::window::{self, TrackedWindow, SETTINGS_KEY};
use super::{chips, details, footer, motion, theme, timeline};

/// Key eframe stores egui's memory (including the theme preference) under
/// (private `STORAGE_EGUI_MEMORY_KEY` in eframe's native/epi_integration.rs).
const EGUI_MEMORY_KEY: &str = "egui";
/// Manual refreshes bypass the poller's minimum interval, so the button rests
/// this long after a click.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(5);
/// Repaint cadence so "last polled" and the countdowns tick while idle.
const TICK: Duration = Duration::from_secs(1);

/// The latest poll results as the dashboard sees them.
#[derive(Debug, Default)]
pub struct PollState {
    /// The last successful poll; kept across failures.
    pub data: Option<Box<Dashboard>>,
    /// The most recent failure, cleared by the next success.
    pub error: Option<FetchErrorInfo>,
}

impl PollState {
    /// Fold one poller event in: success replaces the data and clears the
    /// error; failure keeps the last good data and records the error.
    pub fn apply(&mut self, event: PollEvent) {
        match event {
            PollEvent::Ok(dashboard) => {
                self.data = Some(dashboard);
                self.error = None;
            }
            PollEvent::Err(info) => self.error = Some(info),
        }
    }
}

/// Whether the refresh button is usable again, given its last click.
pub fn refresh_ready(last_click: Option<Instant>, now: Instant) -> bool {
    last_click.is_none_or(|t| now.saturating_duration_since(t) >= REFRESH_COOLDOWN)
}

/// What one logic pass must do with the settings.
#[derive(Debug)]
pub struct SettingsSync {
    /// Tell the poller: what it uses differs from what it was last sent.
    pub poller: Option<PollMsg>,
    /// Write the settings to storage now (they differ from what was last
    /// persisted).
    pub persist: bool,
}

/// Compare the live settings with what the poller and storage last got.
pub fn settings_sync(current: &Settings, sent: &Settings, persisted: &Settings) -> SettingsSync {
    SettingsSync {
        poller: settings::poller_update(sent, current),
        persist: current != persisted,
    }
}

/// True when eframe has no persisted egui memory yet (first launch), so no
/// theme preference has been saved.
pub fn first_run(storage: Option<&dyn eframe::Storage>) -> bool {
    storage.is_none_or(|s| s.get_string(EGUI_MEMORY_KEY).is_none())
}

pub struct App {
    /// The one poller for the app's lifetime; settings changes go to it as
    /// [`PollMsg::UpdateSettings`].
    poller: Poller,
    /// The live settings; the UI edits them, `logic()` propagates changes.
    settings: Settings,
    /// The settings the poller was last given.
    poller_settings: Settings,
    /// The settings last written to storage.
    persisted: Settings,
    /// The full-mode window as of the last pass that showed it; saved into
    /// [`Settings::full_rect`] / [`Settings::full_maximized`] by `save()`.
    tracked: TrackedWindow,
    /// eframe's stale window key has been dropped from storage (first pass).
    storage_cleaned: bool,
    state: PollState,
    /// The settings panel, while open.
    settings_panel: Option<SettingsPanel>,
    last_refresh: Option<Instant>,
    /// The near-cap pulse's schedule; drawn by the 5-hour chip (and the
    /// pill's state dot).
    pulse_tracker: motion::PulseTracker,
    /// This frame's pulse, from [`App::update_motion`].
    pulse: motion::Pulse,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        let ctx = cc.egui_ctx.clone();
        // Dark by default; afterwards egui restores the persisted preference.
        if first_run(cc.storage) {
            ctx.set_theme(egui::ThemePreference::Dark);
        }
        theme::install(&ctx);
        // Window level and size come from the viewport builder
        // (window::viewport_builder); on Windows the saved position and
        // maximized state are applied here, while the window is hidden.
        #[cfg(windows)]
        window::apply_placement(
            cc,
            window::startup_placement(&settings, ctx.pixels_per_point()),
        );
        let notify_ctx = ctx.clone();
        let poller = Poller::spawn(Paths::platform_default(), settings.clone(), move || {
            notify_ctx.request_repaint()
        });
        Self {
            poller,
            poller_settings: settings.clone(),
            persisted: settings.clone(),
            settings,
            tracked: TrackedWindow::default(),
            storage_cleaned: false,
            state: PollState::default(),
            settings_panel: None,
            last_refresh: None,
            pulse_tracker: motion::PulseTracker::default(),
            pulse: motion::Pulse::NONE,
        }
    }

    fn drain_events(&mut self) {
        while let Some(event) = self.poller.try_recv() {
            self.state.apply(event);
        }
    }

    /// Remember the window rect for the current mode while egui reports one
    /// (not while minimized or hidden).
    fn track_window(&mut self, ctx: &egui::Context) {
        match self.settings.ui_mode {
            UiMode::Full => {
                let zoom = ctx.zoom_factor();
                ctx.input(|i| self.tracked.observe(i.viewport(), zoom));
            }
            // Task 8 tracks the pill position here.
            UiMode::Pill => {}
        }
    }

    /// Hand settings changes (from the panel, the pin, later the tray) to
    /// the poller and to storage.
    fn sync_settings(&mut self, frame: &mut eframe::Frame) {
        if !self.storage_cleaned {
            self.storage_cleaned = true;
            if let Some(storage) = frame.storage_mut() {
                if window::remove_stale_window_key(storage) {
                    storage.flush();
                }
            }
        }
        let sync = settings_sync(&self.settings, &self.poller_settings, &self.persisted);
        if let Some(msg) = sync.poller {
            self.poller.send(msg);
            self.poller_settings = self.settings.clone();
        }
        if sync.persist {
            if let Some(storage) = frame.storage_mut() {
                eframe::set_value(storage, SETTINGS_KEY, &self.settings);
                storage.flush();
            }
            self.persisted = self.settings.clone();
        }
    }

    /// Runs every pass after the poll results are in, visible or not: the
    /// tray icon (Task 9) and threshold alerts (Task 10) hook in here.
    fn background_hooks(&mut self, _now: Instant) {}

    /// Hand this frame's motion preferences to the widgets and advance the
    /// near-cap pulse; asks for the next pulse frame only while it moves
    /// and the window is visible. Called from `ui()`, which only runs while
    /// the window is shown, so the three pulses play where they are seen.
    fn update_motion(&mut self, ctx: &egui::Context) {
        motion::set_reduce_motion(ctx, self.settings.reduce_motion);
        let snap = self.state.data.as_ref().map(|d| &d.snapshot);
        self.pulse = self.pulse_tracker.update(
            snap.and_then(|s| s.five_hour_pct()),
            snap.and_then(|s| s.five_hour_resets_at()),
            Instant::now(),
            self.settings.keep_pulsing,
            self.settings.reduce_motion,
        );
        let visible = ctx.input(|i| i.viewport().visible()) != Some(false);
        if let Some(after) = motion::pulse_repaint(self.pulse, visible) {
            ctx.request_repaint_after(after);
        }
    }

    /// Stat chips on the left (wrapping as needed), refresh, pill and
    /// settings buttons on the right.
    fn header(&mut self, ui: &mut Ui) {
        let now = Utc::now();
        let chips = self
            .state
            .data
            .as_ref()
            .map(|d| chips::chips(&d.snapshot, d.window_start, d.window_end, now));
        let pulse = self.pulse.level;
        right_then_left(
            ui,
            Align::Min,
            |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.spacing_mut().button_padding = egui::vec2(4.0, 4.0);
                if icon_button(ui, self.settings_panel.is_some(), true, "⚙")
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.toggle_settings();
                }
                // Task 8 wires pill mode; until then the button is a stub.
                icon_button(ui, false, false, "◱")
                    .on_disabled_hover_text("Compact pill mode — coming soon");
                let ready = refresh_ready(self.last_refresh, Instant::now());
                if icon_button(ui, false, ready, "⟳")
                    .on_hover_text("Refresh now")
                    .on_disabled_hover_text("Refreshed just now")
                    .clicked()
                {
                    self.poller.send(PollMsg::RefreshNow);
                    self.last_refresh = Some(Instant::now());
                }
            },
            |ui| {
                if let Some(chips) = &chips {
                    chips::show_chips(ui, chips, now, pulse);
                }
            },
        );
    }

    /// Open the settings panel with a fresh draft, or close it (discarding
    /// the draft).
    fn toggle_settings(&mut self) {
        self.settings_panel = match self.settings_panel {
            Some(_) => None,
            None => Some(SettingsPanel::new(&self.settings)),
        };
    }

    /// The settings panel, if open; Apply commits to the live settings, and
    /// `logic()` saves them and tells the poller.
    fn settings_panel(&mut self, ui: &mut Ui) {
        let Some(panel) = self.settings_panel.as_mut() else {
            return;
        };
        match panel.show(ui, &mut self.settings) {
            PanelAction::None => {}
            PanelAction::Cancel => self.settings_panel = None,
            PanelAction::Apply(new) => {
                // logic() tells the poller and persists on the next pass.
                self.settings = new;
                self.settings_panel = None;
                ui.ctx().request_repaint();
            }
        }
    }

    /// Poller-stopped, fetch-error, persist-error and loading notices.
    fn banners(&mut self, ui: &mut Ui) {
        let palette = theme::Palette::for_visuals(ui.visuals());
        let body = egui::TextStyle::Body.resolve(ui.style());
        let title = |text: &str, color| {
            RichText::new(text)
                .font(theme::semibold(body.size))
                .color(color)
        };
        if !self.poller.is_alive() {
            banner(ui, BannerKind::Error, |ui| {
                ui.label(title("Poller stopped", palette.danger));
                ui.label(RichText::new("Restart the app to resume polling.").color(palette.text));
            });
        }
        if let Some(err) = &self.state.error {
            let hint = err.stale_sha_hint;
            let message = err.message.clone();
            let mut open_settings = false;
            banner(ui, BannerKind::Error, |ui| {
                ui.label(title("claude.ai usage — error", palette.danger));
                ui.add(egui::Label::new(RichText::new(message).color(palette.text)).wrap());
                if hint {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Your client SHA may be stale — update it in Settings");
                        open_settings = ui.button("Open Settings").clicked();
                    });
                }
            });
            if open_settings && self.settings_panel.is_none() {
                self.toggle_settings();
            }
        }
        if let Some(persist) = self
            .state
            .data
            .as_ref()
            .and_then(|d| d.persist_error.as_ref())
        {
            banner(ui, BannerKind::Warning, |ui| {
                ui.label(title("History not saved", palette.high));
                ui.add(egui::Label::new(RichText::new(persist).color(palette.text)).wrap());
            });
        }
        // While the first poll is failing, the error banner says it all.
        if self.state.data.is_none() && self.state.error.is_none() {
            banner(ui, BannerKind::Loading, |ui| {
                ui.label(RichText::new("Loading usage…").color(palette.text_muted));
            });
        }
    }
}

/// A square header button; the theme gives it a frame only when hovered or
/// selected.
fn icon_button(ui: &mut Ui, selected: bool, enabled: bool, icon: &str) -> egui::Response {
    let font = egui::FontId::proportional(17.0);
    let button = egui::Button::selectable(selected, RichText::new(icon).font(font))
        .min_size(egui::vec2(26.0, 28.0));
    ui.add_enabled(enabled, button)
}

/// The dashboard body for one successful poll: hero timeline, footer
/// strip, Details.
fn dashboard(ui: &mut Ui, d: &Dashboard, settings: &mut Settings) {
    let now = Utc::now();
    let palette = theme::Palette::for_visuals(ui.visuals());
    theme::card(palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        timeline::show_timeline(ui, d, now, palette, timeline::FULL_HEIGHT);
    });
    footer::show_footer(ui, d, now);
    details::show_details(ui, d, settings, now);
}

/// Lay `add` out in the content column: at most
/// [`layout::MAX_CONTENT_WIDTH`] wide, centered, sections [`GAP`] apart.
fn content_column(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let available = ui.available_width();
    let width = layout::content_width(available);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(layout::centered_inset(available));
        ui.vertical(|ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
            add(ui);
        });
    });
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.track_window(ctx);
        self.drain_events();
        self.sync_settings(frame);
        self.background_hooks(now);
        // Keeps "last polled" and the countdowns ticking; eframe throttles
        // it while the window is hidden.
        ctx.request_repaint_after(TICK);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.update_motion(ui.ctx());
        let frame = Frame::central_panel(ui.style()).inner_margin(Margin::same(layout::OUTER_PAD));
        CentralPanel::default().frame(frame).show(ui, |ui| {
            // A solid scrollbar takes its own space instead of overlaying
            // content at narrow widths.
            ui.spacing_mut().scroll = ScrollStyle::solid();
            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                content_column(ui, |ui| {
                    self.header(ui);
                    self.settings_panel(ui);
                    self.banners(ui);
                    if let Some(d) = &self.state.data {
                        dashboard(ui, d, &mut self.settings);
                    }
                });
            });
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        (self.settings.full_rect, self.settings.full_maximized) =
            window::full_state_to_save(self.settings.ui_mode, &self.tracked, &self.settings);
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
        self.persisted = self.settings.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Snapshot, UsageWindow};
    use chrono::DateTime;
    use eframe::Storage;

    fn dashboard(pct: f64) -> Box<Dashboard> {
        let t = DateTime::from_timestamp(1_791_000_000, 0).unwrap();
        Box::new(Dashboard {
            snapshot: Snapshot {
                fetched_at: t,
                windows: vec![UsageWindow {
                    label: "5-hour".to_owned(),
                    utilization: Some(pct),
                    resets_at: None,
                }],
                extra: None,
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            daily_history: Vec::new(),
            fable_daily: Vec::new(),
            burners: Vec::new(),
            window_start: t,
            window_end: t,
            window_samples: Vec::new(),
            persist_error: None,
        })
    }

    fn err(message: &str) -> PollEvent {
        PollEvent::Err(FetchErrorInfo {
            message: message.to_owned(),
            stale_sha_hint: false,
        })
    }

    #[test]
    fn error_keeps_last_good_data_and_success_clears_error() {
        let mut state = PollState::default();
        state.apply(err("request failed: timed out"));
        assert!(state.data.is_none());
        assert_eq!(
            state.error.as_ref().unwrap().message,
            "request failed: timed out"
        );

        state.apply(PollEvent::Ok(dashboard(10.0)));
        assert!(state.error.is_none());

        state.apply(err("HTTP 500: oops"));
        assert_eq!(
            state.data.as_ref().unwrap().snapshot.five_hour_pct(),
            Some(10.0)
        );
        assert!(state.error.is_some());

        state.apply(PollEvent::Ok(dashboard(20.0)));
        assert_eq!(
            state.data.as_ref().unwrap().snapshot.five_hour_pct(),
            Some(20.0)
        );
        assert!(state.error.is_none());
    }

    #[derive(Default)]
    struct FakeStorage(std::collections::HashMap<String, String>);

    impl eframe::Storage for FakeStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn first_run_only_without_persisted_egui_memory() {
        assert!(first_run(None));
        let mut storage = FakeStorage::default();
        storage.set_string(SETTINGS_KEY, "{}".to_owned());
        assert!(first_run(Some(&storage)));
        storage.set_string(EGUI_MEMORY_KEY, "{}".to_owned());
        assert!(!first_run(Some(&storage)));
    }

    #[test]
    fn settings_sync_tells_poller_and_storage_only_about_changes() {
        let base = Settings::default();
        let idle = settings_sync(&base, &base, &base);
        assert!(idle.poller.is_none() && !idle.persist);

        // UI-only change (pin, a toggle): persist, poller not bothered.
        let pinned = Settings {
            always_on_top: true,
            reduce_motion: true,
            ..base.clone()
        };
        let sync = settings_sync(&pinned, &base, &base);
        assert!(sync.poller.is_none() && sync.persist);

        // Interval change: both.
        let faster = Settings {
            interval_secs: 30,
            ..base.clone()
        };
        let sync = settings_sync(&faster, &base, &base);
        assert!(sync.persist);
        match sync.poller {
            Some(PollMsg::UpdateSettings(sent)) => assert_eq!(sent, faster),
            other => panic!("expected UpdateSettings, got {other:?}"),
        }

        // Already sent and persisted: nothing more.
        let done = settings_sync(&faster, &faster, &faster);
        assert!(done.poller.is_none() && !done.persist);

        // save() wrote a new rect: persisted matches, the poller ignores it.
        let moved = Settings {
            full_rect: Some(crate::config::SavedRect {
                pos: crate::config::Pos { x: 5, y: 6 },
                w: 420.0,
                h: 460.0,
            }),
            ..base.clone()
        };
        let sync = settings_sync(&moved, &base, &moved);
        assert!(sync.poller.is_none() && !sync.persist);
    }

    #[test]
    fn refresh_cooldown() {
        let t = Instant::now();
        assert!(refresh_ready(None, t));
        assert!(!refresh_ready(Some(t), t));
        assert!(!refresh_ready(Some(t), t + Duration::from_millis(4_900)));
        assert!(refresh_ready(Some(t), t + REFRESH_COOLDOWN));
    }
}
