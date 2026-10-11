//! The eframe app: owns the poller, drains its events, and lays out the
//! dashboard top to bottom.

use std::time::{Duration, Instant};

use chrono::Utc;
use eframe::egui::{
    self, style::ScrollStyle, Align, CentralPanel, Color32, Frame, Layout, RichText, ScrollArea,
    Ui, ViewportCommand,
};

use crate::analytics::{self, Level};
use crate::config::Settings;
use crate::model::UsageWindow;
use crate::paths::Paths;
use crate::poller::{Dashboard, FetchErrorInfo, PollEvent, PollMsg, Poller};

use super::charts;
use super::rows::{self, BurnTag};
use super::settings::{self, PanelAction, SettingsPanel};
use super::{level_color, show_runs, theme, tone_color, Tone};

/// eframe storage key for [`Settings`].
const SETTINGS_KEY: &str = "settings";
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

/// True when eframe has no persisted egui memory yet (first launch), so no
/// theme preference has been saved.
pub fn first_run(storage: Option<&dyn eframe::Storage>) -> bool {
    storage.is_none_or(|s| s.get_string(EGUI_MEMORY_KEY).is_none())
}

pub struct App {
    /// The one poller for the app's lifetime; settings changes go to it as
    /// [`PollMsg::UpdateSettings`].
    poller: Poller,
    settings: Settings,
    state: PollState,
    /// The settings panel, while open.
    settings_panel: Option<SettingsPanel>,
    last_refresh: Option<Instant>,
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
        if settings.always_on_top {
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(settings::window_level(true)));
        }
        let notify_ctx = ctx.clone();
        let poller = Poller::spawn(Paths::platform_default(), settings.clone(), move || {
            notify_ctx.request_repaint()
        });
        Self {
            poller,
            settings,
            state: PollState::default(),
            settings_panel: None,
            last_refresh: None,
        }
    }

    fn drain_events(&mut self) {
        while let Some(event) = self.poller.try_recv() {
            self.state.apply(event);
        }
    }

    /// "claude.ai usage · {plan}" with refresh, pin, and settings buttons.
    fn header(&mut self, ui: &mut Ui) {
        let title = rows::header_runs(&self.settings.plan);
        // Buttons first (right to left), so the title wraps in what's left.
        right_then_left(
            ui,
            Align::Center,
            |ui| {
                if ui
                    .add(egui::Button::selectable(self.settings_panel.is_some(), "⚙"))
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.toggle_settings();
                }

                let pinned = self.settings.always_on_top;
                let pin_tip = if pinned {
                    "Stop keeping on top"
                } else {
                    "Keep on top"
                };
                if ui
                    .add(egui::Button::selectable(pinned, "📌"))
                    .on_hover_text(pin_tip)
                    .clicked()
                {
                    settings::set_always_on_top(ui.ctx(), &mut self.settings, !pinned);
                }

                let ready = refresh_ready(self.last_refresh, Instant::now());
                if ui
                    .add_enabled(ready, egui::Button::selectable(false, "🔄"))
                    .on_hover_text("Refresh now")
                    .on_disabled_hover_text("Refreshed just now")
                    .clicked()
                {
                    self.poller.send(PollMsg::RefreshNow);
                    self.last_refresh = Some(Instant::now());
                }
            },
            |ui| {
                show_runs(ui, &title);
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

    /// The settings panel, if open; Apply commits, saves, and tells the
    /// poller.
    fn settings_panel(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let Some(panel) = self.settings_panel.as_mut() else {
            return;
        };
        match panel.show(ui, &mut self.settings) {
            PanelAction::None => {}
            PanelAction::Cancel => self.settings_panel = None,
            PanelAction::Apply(new) => {
                if let Some(msg) = settings::poller_update(&self.settings, &new) {
                    self.poller.send(msg);
                }
                self.settings = new;
                self.settings_panel = None;
                if let Some(storage) = frame.storage_mut() {
                    eframe::set_value(storage, SETTINGS_KEY, &self.settings);
                    storage.flush();
                }
            }
        }
    }

    /// Poller-stopped, fetch-error, and persist-error notices.
    fn banners(&mut self, ui: &mut Ui) {
        let red = level_color(Level::Red, ui.visuals());
        if !self.poller.is_alive() {
            error_frame(ui, red, |ui| {
                ui.colored_label(red, "poller stopped — restart the app");
            });
        }
        if let Some(err) = &self.state.error {
            let hint = err.stale_sha_hint;
            let message = err.message.clone();
            let mut open_settings = false;
            error_frame(ui, red, |ui| {
                ui.colored_label(red, RichText::new("claude.ai usage — error").strong());
                ui.add(egui::Label::new(RichText::new(message).color(red)).wrap());
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
            let color = level_color(Level::Orange, ui.visuals());
            ui.add(egui::Label::new(RichText::new(persist).color(color)).wrap());
        }
    }
}

/// One row: `right` laid out first from the right edge, then `left` from the
/// left edge in the remaining width, so wrapping text on the left never runs
/// under the right-hand widgets.
fn right_then_left(
    ui: &mut Ui,
    align: Align,
    right: impl FnOnce(&mut Ui),
    left: impl FnOnce(&mut Ui),
) {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(align), |ui| {
            right(ui);
            ui.with_layout(Layout::left_to_right(align), left);
        });
    });
}

/// A red-outlined frame across the full width.
fn error_frame(ui: &mut Ui, red: Color32, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .stroke((1.0, red))
        .fill(red.gamma_multiply(0.08))
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(4.0);
}

/// The dashboard body for one successful poll.
fn dashboard(ui: &mut Ui, d: &Dashboard, settings: &Settings) {
    let now = Utc::now();
    let snap = &d.snapshot;
    let order = analytics::ordered_windows(snap);
    let show_rows = |ui: &mut Ui, windows: &[&UsageWindow]| {
        for w in windows {
            if analytics::window_visible(w, settings.show_all) {
                rows::show_window_row(ui, &rows::window_row(w, now));
            }
        }
    };

    show_rows(ui, &order.primary);
    ui.add_space(4.0);
    let all = analytics::burn_line(
        &d.history,
        snap.five_hour_pct(),
        snap.five_hour_resets_at(),
        now,
    );
    rows::show_burn_row(ui, BurnTag::All, &rows::burn_body(all.as_ref()));
    if let Some(fable) = snap.window("Fable") {
        let line = analytics::burn_line(&d.fable_history, fable.utilization, fable.resets_at, now);
        rows::show_burn_row(ui, BurnTag::Fable, &rows::burn_body(line.as_ref()));
    }
    if !order.rest.is_empty() {
        ui.add_space(4.0);
        show_rows(ui, &order.rest);
    }

    if let Some(extra) = analytics::visible_extra(snap) {
        ui.add_space(6.0);
        show_runs(ui, &rows::extra_runs(extra));
    }

    ui.add_space(8.0);
    charts::show_charts(ui, d);
    ui.add_space(8.0);

    show_runs(ui, &rows::burners_runs(&d.burners));
    // "last polled" first (right), so projected use wraps in what's left.
    right_then_left(
        ui,
        Align::Min,
        |ui| {
            let dim = tone_color(Tone::Dim, ui.visuals());
            ui.colored_label(dim, rows::last_polled(snap.fetched_at, now));
        },
        |ui| {
            let rate = analytics::compute_rate(&d.history, now);
            if let Some(h) = analytics::headroom(snap, rate, now) {
                show_runs(ui, &rows::headroom_runs(&h));
            }
        },
    );
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        self.drain_events();
        ui.ctx().request_repaint_after(TICK);

        CentralPanel::default().show(ui, |ui| {
            // A solid scrollbar takes its own space instead of overlaying
            // content at narrow widths.
            ui.spacing_mut().scroll = ScrollStyle::solid();
            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                self.header(ui);
                ui.separator();
                self.settings_panel(ui, frame);
                self.banners(ui);
                match &self.state.data {
                    Some(d) => dashboard(ui, d, &self.settings),
                    // While the first poll is failing, the error banner says it all.
                    None if self.state.error.is_some() => {}
                    None => {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| ui.weak("loading…"));
                    }
                }
            });
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;
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
            five_hour_buckets: Vec::new(),
            fable_buckets: Vec::new(),
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
    fn refresh_cooldown() {
        let t = Instant::now();
        assert!(refresh_ready(None, t));
        assert!(!refresh_ready(Some(t), t));
        assert!(!refresh_ready(Some(t), t + Duration::from_millis(4_900)));
        assert!(refresh_ready(Some(t), t + REFRESH_COOLDOWN));
    }
}
