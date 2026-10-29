//! The eframe app: owns the poller, drains its events, and lays out the
//! dashboard top to bottom.
//!
//! Work that must go on while the window is hidden (draining poll events,
//! telling the poller about settings changes, persisting settings, the 1 s
//! tick, tracking the window rect, the tray icon, threshold alerts) runs in
//! [`eframe::App::logic`], which eframe calls every pass, even when it skips
//! the UI. `ui()` only draws.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use eframe::egui::{
    self, style::ScrollStyle, Align, CentralPanel, Frame, Margin, RichText, ScrollArea, Ui,
    ViewportCommand,
};

use crate::config::{Pos, Settings, UiMode};
use crate::paths::Paths;
use crate::poller::{PollMsg, Poller};

use super::accounts::{self, AccountView, Accounts};
use super::alerts::AlertHook;
use super::banners::{banner, BannerKind};
use super::layout::{self, right_then_left, GAP};
use super::mode::{self, Settle, SettleStep};
use super::notify::{self, AppId};
use super::pill::{self, PillAction, PillSegment, PillState};
use super::settings::{self, PanelAction, SettingsPanel};
use super::tray::{self, Tray, TrayAction};
use super::window::{self, TrackedWindow, SETTINGS_KEY};

mod tray_glue;
use super::{chips, footer, motion, theme, timeline, week};

/// Key eframe stores egui's memory (including the theme preference) under
/// (private `STORAGE_EGUI_MEMORY_KEY` in eframe's native/epi_integration.rs).
const EGUI_MEMORY_KEY: &str = "egui";
/// Manual refreshes bypass the poller's minimum interval, so the button rests
/// this long after a click.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(5);
/// Repaint cadence so "last polled" and the countdowns tick while idle.
const TICK: Duration = Duration::from_secs(1);

/// Whether the refresh button is usable again, given its last click.
pub fn refresh_ready(last_click: Option<Instant>, now: Instant) -> bool {
    last_click.is_none_or(|t| now.saturating_duration_since(t) >= REFRESH_COOLDOWN)
}

/// The settle state the window opens with. Off Windows, a started one, so
/// the first frames' Fixup keeps a pill opened at a saved position (sized
/// for fewer accounts then) from passing its monitor's right edge; Windows
/// already did that in `window::apply_placement`.
fn startup_settle() -> Settle {
    let mut settle = Settle::default();
    if cfg!(not(windows)) {
        settle.start();
    }
    settle
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
    /// The pill position as of the last pass that showed it; saved into
    /// [`Settings::pill_pos`] when leaving the pill and by `save()`.
    pill_seen: Option<Pos>,
    /// Passes since the last full/pill switch (see [`mode::Settle`]).
    settle: Settle,
    pill: PillState,
    /// How many accounts the pill window is sized for (see
    /// [`window::pill_size`]).
    pill_accounts: usize,
    /// eframe's stale window key has been dropped from storage (first pass).
    storage_cleaned: bool,
    /// Every polled account and its latest results; every view draws all
    /// of them (the charts' freshness follows the first one with data).
    accounts: Accounts,
    /// The settings panel, while open.
    settings_panel: Option<SettingsPanel>,
    last_refresh: Option<Instant>,
    /// The near-cap pulse's schedule; drawn by the 5-hour chip (and the
    /// pill's state dot).
    pulse_tracker: motion::PulseTracker,
    /// This frame's pulse, from [`App::update_motion`].
    pulse: motion::Pulse,
    /// The tray icon, while [`Settings::tray_enabled`] and it could be
    /// created.
    tray: Option<Tray>,
    /// Clicks and menu picks from the tray icon (whichever one exists).
    tray_events: Receiver<TrayAction>,
    /// Creating the tray icon failed; not retried until the setting is
    /// turned off and on again.
    tray_failed: bool,
    /// Hidden to the tray or menu bar (invisible, and minimized on
    /// Windows; only `logic()` runs).
    hidden: bool,
    /// Quit was chosen: the close that follows is not turned into a hide.
    quitting: bool,
    /// When a tray toggle last acted (see [`tray::toggle_accepted`]).
    last_toggle: Option<Instant>,
    /// Whether the app was active at the last look (macOS; see
    /// [`tray::became_active`]).
    #[cfg(target_os = "macos")]
    was_active: Option<bool>,
    /// Threshold alerts, checked once per poll and sent to the notifier
    /// thread.
    alerts: AlertHook,
    /// A toast was clicked: bring the window forward.
    toast_clicks: Receiver<()>,
}

impl App {
    /// `toast_app` is the identity [`notify::register`] got in `main`.
    pub fn new(cc: &eframe::CreationContext<'_>, toast_app: AppId) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        // The window was built for this mode ("Start in pill mode" wins).
        settings.ui_mode = window::startup_mode(&settings);
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
        {
            window::apply_placement(
                cc,
                window::startup_placement(&settings, ctx.pixels_per_point()),
            );
            // Rounds the borderless pill; decorated windows already are.
            window::round_corners(cc);
        }
        let notify_ctx = ctx.clone();
        let poller = Poller::spawn(Paths::platform_default(), settings.clone(), move || {
            notify_ctx.request_repaint()
        });
        let (click_tx, toast_clicks) = channel();
        let click_ctx = ctx.clone();
        let toasts = notify::spawn_notifier(
            toast_app,
            Arc::new(move || {
                let _ = click_tx.send(());
                click_ctx.request_repaint();
            }),
        );
        let pill_accounts = window::startup_pill_segments(&settings);
        let mut app = Self {
            poller,
            poller_settings: settings.clone(),
            persisted: settings.clone(),
            settings,
            tracked: TrackedWindow::default(),
            pill_seen: None,
            settle: startup_settle(),
            pill: PillState::default(),
            pill_accounts,
            storage_cleaned: false,
            accounts: Accounts::default(),
            settings_panel: None,
            last_refresh: None,
            pulse_tracker: motion::PulseTracker::default(),
            pulse: motion::Pulse::NONE,
            tray: None,
            tray_events: tray::install_event_handlers(&ctx),
            tray_failed: false,
            hidden: false,
            quitting: false,
            last_toggle: None,
            #[cfg(target_os = "macos")]
            was_active: None,
            alerts: AlertHook::new(toasts),
            toast_clicks,
        };
        app.sync_tray_presence();
        app
    }

    /// An account's last poll failed or the poller has stopped (the pill's
    /// state dot says so).
    fn poll_failing(&self) -> bool {
        self.accounts.any_error() || !self.poller.is_alive()
    }

    fn drain_events(&mut self) {
        while let Some(event) = self.poller.try_recv() {
            if self.accounts.apply(event, &mut self.settings) {
                // Newly discovered profiles, merged into the settings to
                // persist; the poller polls them already, so this is no
                // reason to tell it.
                self.poller_settings.profiles = self.settings.profiles.clone();
            }
        }
    }

    /// Remember the window rect for the current mode while egui reports one
    /// (not while minimized or hidden). Right after a switch the old mode's
    /// geometry is skipped, then the window is kept on a monitor once.
    fn track_window(&mut self, ctx: &egui::Context, _frame: &eframe::Frame) {
        let zoom = ctx.zoom_factor();
        match self.settle.step() {
            SettleStep::Skip => {}
            SettleStep::Fixup => {
                #[cfg(windows)]
                window::keep_on_screen(_frame);
                #[cfg(not(windows))]
                if self.settings.ui_mode == UiMode::Pill {
                    self.pill_right_edge_on_monitor(ctx, _frame, None);
                }
                ctx.request_repaint();
            }
            SettleStep::Track => match self.settings.ui_mode {
                UiMode::Full => ctx.input(|i| self.tracked.observe(i.viewport(), zoom)),
                UiMode::Pill => {
                    if let Some(p) = ctx.input(|i| window::pill_pos_of(i.viewport(), zoom)) {
                        self.pill_seen = Some(p);
                    }
                }
            },
        }
    }

    /// Switch the window to `to` (full dashboard or pill), recording the
    /// geometry of the mode being left. The header button, the pill and the
    /// tray menu all come through here.
    pub fn set_mode(&mut self, ctx: &egui::Context, to: UiMode) {
        let accounts = self.pill_accounts_now();
        let cmds = mode::switch(
            &mut self.settings,
            to,
            &self.tracked,
            self.pill_seen,
            ctx.pixels_per_point(),
            window::pill_size(accounts),
        );
        if cmds.is_empty() {
            return;
        }
        self.pill_accounts = accounts;
        for cmd in cmds {
            ctx.send_viewport_cmd(cmd);
        }
        self.settle.start();
        self.pill.menu_open = false;
        if to == UiMode::Pill {
            // The pill can't show the panel; don't leave a draft behind it.
            self.settings_panel = None;
        }
        ctx.request_repaint();
    }

    /// How many accounts the pill shows: the polled ones once the poller
    /// has said (at least one), else what the window is sized for.
    fn pill_accounts_now(&self) -> usize {
        if self.accounts.known() {
            self.accounts.list().len().max(1)
        } else {
            self.pill_accounts
        }
    }

    /// Resize the pill when its account count changed (the window is kept
    /// on a monitor afterwards, as after a switch; off Windows a widened
    /// pill also moves left as it grows, if it would pass the monitor's
    /// right edge).
    fn fit_pill(&mut self, ctx: &egui::Context, _frame: &eframe::Frame) {
        let accounts = self.pill_accounts_now();
        if self.settings.ui_mode != UiMode::Pill || accounts == self.pill_accounts {
            return;
        }
        self.pill_accounts = accounts;
        let size = window::pill_size(accounts);
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        #[cfg(not(windows))]
        {
            let width_px = (size.x * ctx.pixels_per_point()).round() as i32;
            self.pill_right_edge_on_monitor(ctx, _frame, Some(width_px));
        }
        self.settle.start();
        ctx.request_repaint();
    }

    /// Move the pill left if its right edge would pass its monitor's at
    /// `width_px` wide (its current width if `None`). Off Windows only:
    /// Windows keeps it on screen natively ([`window::keep_on_screen`]).
    #[cfg(not(windows))]
    fn pill_right_edge_on_monitor(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        width_px: Option<i32>,
    ) {
        if let Some(p) = window::keep_right_edge_on_monitor(frame, width_px) {
            let at = mode::to_points(p, ctx.pixels_per_point());
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(at));
            self.pill_seen = Some(p);
        }
    }

    /// Full ↔ pill.
    pub fn toggle_mode(&mut self, ctx: &egui::Context) {
        self.set_mode(ctx, mode::other(self.settings.ui_mode));
    }

    /// Ask for a poll now (the refresh button and the pill's menu).
    fn refresh_now(&mut self) {
        self.poller.send(PollMsg::RefreshNow);
        self.last_refresh = Some(Instant::now());
    }

    /// Hand settings changes (from the panel, Always on top, the alerts'
    /// edge state) to the poller and to storage.
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

    /// Runs every pass after the poll results are in, visible or not, and
    /// before the settings sync (which persists the alerts' edge state):
    /// threshold alerts for a new poll, and a clicked toast showing the
    /// window.
    fn background_hooks(&mut self, ctx: &egui::Context) {
        // Colors play no part in the alerts.
        let views = self.accounts.views(&theme::DARK);
        self.alerts
            .after_poll(&views, &mut self.settings, Utc::now());
        if self.toast_clicks.try_iter().count() > 0 {
            self.show(ctx);
        }
    }

    /// Hand this frame's motion preferences to the widgets and advance the
    /// near-cap pulse, which follows the fresh account nearest its cap
    /// ([`accounts::hottest`]); asks for the next pulse frame only while it
    /// moves and the window is visible. Called from `ui()`, which only runs
    /// while the window is shown, so the three pulses play where they are
    /// seen.
    fn update_motion(&mut self, ctx: &egui::Context) {
        motion::set_reduce_motion(ctx, self.settings.reduce_motion);
        let now = Utc::now();
        let views = self.accounts.views(&theme::DARK);
        let five = accounts::hottest(&views, now)
            .and_then(|i| views[i].data)
            .map(|d| d.five_hour_now(now));
        self.pulse = self.pulse_tracker.update(
            five.and_then(|f| f.pct),
            five.and_then(|f| f.resets_at),
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
        let pulse = self.pulse.level;
        let settings_open = self.settings_panel.is_some();
        let ready = refresh_ready(self.last_refresh, Instant::now());
        let views = self
            .accounts
            .views(theme::Palette::for_visuals(ui.visuals()));
        // The buttons' clicks, acted on once the accounts are no longer
        // borrowed for the chips.
        let (mut settings, mut pill, mut refresh) = (false, false, false);
        right_then_left(
            ui,
            Align::Min,
            |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.spacing_mut().button_padding = egui::vec2(4.0, 4.0);
                settings = icon_button(ui, settings_open, true, "⚙")
                    .on_hover_text("Settings")
                    .clicked();
                pill = icon_button(ui, false, true, "◱")
                    .on_hover_text("Compact pill (click it to come back)")
                    .clicked();
                refresh = icon_button(ui, false, ready, "⟳")
                    .on_hover_text("Refresh now")
                    .on_disabled_hover_text("Refreshed just now")
                    .clicked();
            },
            |ui| chips::show_account_chips(ui, &views, now, pulse),
        );
        if settings {
            self.toggle_settings();
        }
        if pill {
            self.set_mode(ui.ctx(), UiMode::Pill);
        }
        if refresh {
            self.refresh_now();
        }
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
        let statuses = settings::account_statuses(&self.accounts, Utc::now());
        match panel.show(ui, &mut self.settings, &statuses) {
            PanelAction::None => {}
            PanelAction::Cancel => self.settings_panel = None,
            PanelAction::TestNotification => self.alerts.send_test(),
            PanelAction::Rescan => self.refresh_now(),
            PanelAction::Apply(new) => {
                // logic() tells the poller and persists on the next pass.
                self.settings = new;
                self.settings_panel = None;
                ui.ctx().request_repaint();
            }
        }
    }

    /// The pill view, filling the window; acts on what it asks for.
    fn pill_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let palette = theme::Palette::for_visuals(ui.visuals());
        let segments = pill_segments(&self.accounts.views(palette), Utc::now());
        let failed = self.poll_failing();
        let ready = refresh_ready(self.last_refresh, Instant::now());
        let frame = Frame::NONE.fill(palette.surface);
        let action = CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| {
                pill::show_pill(
                    ui,
                    &mut self.pill,
                    &segments,
                    failed,
                    self.pulse.level,
                    ready,
                )
            })
            .inner;
        match action {
            PillAction::None => {}
            PillAction::Expand => self.set_mode(&ctx, UiMode::Full),
            PillAction::Refresh => self.refresh_now(),
            PillAction::Settings => {
                self.set_mode(&ctx, UiMode::Full);
                if self.settings_panel.is_none() {
                    self.toggle_settings();
                }
            }
            PillAction::Quit => self.quit(&ctx),
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
        for a in self.accounts.list() {
            if let Some(err) = &a.poll.error {
                let heading = format!("{} — {}", a.profile.label, accounts::short_error(err.kind));
                banner(ui, BannerKind::Error, |ui| {
                    ui.label(title(&heading, palette.danger));
                    ui.add(
                        egui::Label::new(RichText::new(&err.message).color(palette.text)).wrap(),
                    );
                });
            }
            if let Some(persist) = a.poll.data.as_ref().and_then(|d| d.persist_error.as_ref()) {
                let heading = format!("{} — history not saved", a.profile.label);
                banner(ui, BannerKind::Warning, |ui| {
                    ui.label(title(&heading, palette.high));
                    ui.add(egui::Label::new(RichText::new(persist).color(palette.text)).wrap());
                });
            }
        }
        if self.accounts.known() && self.accounts.list().is_empty() {
            banner(ui, BannerKind::Error, |ui| {
                ui.label(title("No Claude Code login found", palette.danger));
                ui.label(
                    RichText::new("Run `claude` and sign in; accounts appear here on their own.")
                        .color(palette.text),
                );
            });
        } else if !self.accounts.any_data() && !self.accounts.any_error() {
            // While a first poll is failing, its error banner says it all.
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

/// The pill's segments: one per account, in order; the pulse's account
/// ([`accounts::hottest`]) marked hot.
fn pill_segments(views: &[AccountView], now: DateTime<Utc>) -> Vec<PillSegment> {
    let hot = accounts::hottest(views, now);
    views
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let segment = v.data.and_then(|d| {
                let chips = chips::chips(&d.snapshot, d.window_start, d.window_end, now);
                PillSegment::from_chips(&chips, v.color, v.label)
            });
            PillSegment {
                stale: v.stale,
                hot: hot == Some(i),
                ..segment.unwrap_or_else(|| PillSegment::waiting(v.color, v.label))
            }
        })
        .collect()
}

/// The dashboard body once some account has data: the charts and the
/// footer strip.
fn dashboard(ui: &mut Ui, views: &[AccountView]) {
    if accounts::first_data(views).is_none() {
        return;
    }
    let now = Utc::now();
    charts(ui, views, now);
    footer::show_footer(ui, views);
}

/// The 5-hour and the 7-day timelines, each on its titled card, both
/// [`timeline::FULL_HEIGHT`] tall: side by side when the column is wide
/// enough (see [`layout::chart_widths`]), levelled to the taller card's
/// content (measured the frame before); else stacked, 5-hour first.
fn charts(ui: &mut Ui, views: &[AccountView], now: DateTime<Utc>) {
    let palette = theme::Palette::for_visuals(ui.visuals());
    let Some((left, right)) = layout::chart_widths(ui.available_width()) else {
        let size = layout::CardSize {
            chart: timeline::FULL_HEIGHT,
            min_content: 0.0,
        };
        timeline::show_timeline_card(ui, views, now, palette, size);
        week::show_week_card(ui, views, now, palette, size);
        return;
    };
    let id = egui::Id::new("charts-level-height");
    let level: f32 = ui.ctx().data(|m| m.get_temp(id)).unwrap_or(0.0);
    let size = layout::CardSize {
        chart: timeline::FULL_HEIGHT,
        min_content: level,
    };
    let natural = ui
        .horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            let column = egui::Layout::top_down(egui::Align::Min);
            let a = ui.allocate_ui_with_layout(egui::vec2(left, 0.0), column, |ui| {
                timeline::show_timeline_card(ui, views, now, palette, size)
            });
            let b = ui.allocate_ui_with_layout(egui::vec2(right, 0.0), column, |ui| {
                week::show_week_card(ui, views, now, palette, size)
            });
            [a.inner, b.inner]
        })
        .inner;
    let next = layout::level_height(&natural);
    if (next - level).abs() > 0.5 {
        ui.ctx().data_mut(|m| m.insert_temp(id, next));
        ui.ctx().request_repaint();
    }
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
        self.track_window(ctx, frame);
        self.handle_close_request(ctx);
        self.drain_events();
        self.fit_pill(ctx, frame);
        self.sync_tray(ctx);
        self.background_hooks(ctx);
        self.sync_settings(frame);
        // Keeps "last polled" and the countdowns ticking; eframe throttles
        // it while the window is hidden.
        ctx.request_repaint_after(TICK);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.update_motion(ui.ctx());
        if self.settings.ui_mode == UiMode::Pill {
            self.pill_ui(ui);
            return;
        }
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
                    let views = self
                        .accounts
                        .views(theme::Palette::for_visuals(ui.visuals()));
                    dashboard(ui, &views);
                });
            });
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        (self.settings.full_rect, self.settings.full_maximized) =
            window::full_state_to_save(self.settings.ui_mode, &self.tracked, &self.settings);
        self.settings.pill_pos =
            window::pill_pos_to_save(self.settings.ui_mode, self.pill_seen, &self.settings);
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
        self.persisted = self.settings.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::Storage;

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

        // UI-only change (Always on top, a toggle): persist, poller not bothered.
        let on_top = Settings {
            always_on_top: true,
            reduce_motion: true,
            ..base.clone()
        };
        let sync = settings_sync(&on_top, &base, &base);
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
