//! The settings panel: a buffered edit of [`Settings`] that is only
//! committed on Apply.
//!
//! The edit-buffer logic (validation, building the new [`Settings`], and
//! deciding what the poller must hear) is plain functions; [`SettingsPanel`]
//! only draws them.

use std::path::Path;

use chrono::{DateTime, Utc};
use eframe::egui::{self, Color32, RichText, Sense, Stroke, TextEdit, Ui};

use crate::analytics::alerts::AlertToggles;
use crate::analytics::Level;
use crate::config::{parse_interval, ProfileSettings, Settings, MIN_INTERVAL_SECS};
use crate::fetch::ErrorKind;
use crate::poller::PollMsg;
use crate::profiles;

use super::accounts::Accounts;
use super::{level_color, theme};

/// The panel's text fields, edited freely until Apply.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsDraft {
    pub plan: String,
    pub interval: String,
    pub show_all: bool,
    pub tray_enabled: bool,
    pub close_to_tray: bool,
    pub start_in_pill: bool,
    pub alerts: AlertToggles,
    pub reduce_motion: bool,
    pub keep_pulsing: bool,
    /// The Accounts rows, in display (and poll) order.
    pub profiles: Vec<ProfileDraft>,
}

/// One saved profile being edited; the text fields are buffered as typed.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileDraft {
    /// Everything but the label and Keychain service, which live in the
    /// text fields below.
    pub saved: ProfileSettings,
    /// Blank uses the default label.
    pub label: String,
    /// Blank derives the service from the config dir.
    pub keychain_service: String,
}

impl ProfileDraft {
    pub fn from_saved(p: &ProfileSettings) -> Self {
        Self {
            saved: p.clone(),
            label: p.label.clone().unwrap_or_default(),
            keychain_service: p.keychain_service.clone().unwrap_or_default(),
        }
    }

    /// The settings this row commits: blank text fields become `None`.
    pub fn build(&self) -> ProfileSettings {
        ProfileSettings {
            label: non_blank(&self.label),
            keychain_service: non_blank(&self.keychain_service),
            ..self.saved.clone()
        }
    }

    /// The Keychain service polling would use: the override if typed, else
    /// derived from the config dir as [`profiles::resolve`] does.
    pub fn effective_keychain_service(&self) -> String {
        non_blank(&self.keychain_service).unwrap_or_else(|| {
            let env = (self.saved.id != profiles::DEFAULT_ID)
                .then(|| self.saved.config_dir.display().to_string());
            profiles::keychain_service_for(env.as_deref())
        })
    }
}

fn non_blank(s: &str) -> Option<String> {
    Some(s.trim().to_owned()).filter(|s| !s.is_empty())
}

impl SettingsDraft {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            plan: s.plan.clone(),
            interval: format_interval(s.interval_secs),
            show_all: s.show_all,
            tray_enabled: s.tray_enabled,
            close_to_tray: s.close_to_tray,
            start_in_pill: s.start_in_pill,
            alerts: s.alerts,
            reduce_motion: s.reduce_motion,
            keep_pulsing: s.keep_pulsing,
            profiles: s.profiles.iter().map(ProfileDraft::from_saved).collect(),
        }
    }

    /// Fix each row's color at its current effective index, so moving or
    /// removing rows doesn't recolor accounts that use the default
    /// (position-based) color.
    fn pin_colors(&mut self) {
        for (i, p) in self.profiles.iter_mut().enumerate() {
            p.saved.color.get_or_insert(i as u8);
        }
    }

    /// Swap row `i` with its neighbour above (`up`) or below. Returns
    /// whether anything moved.
    pub fn move_profile(&mut self, i: usize, up: bool) -> bool {
        let j = if up { i.checked_sub(1) } else { Some(i + 1) };
        let Some(j) = j.filter(|&j| j < self.profiles.len() && i < self.profiles.len()) else {
            return false;
        };
        self.pin_colors();
        self.profiles.swap(i, j);
        true
    }

    /// Drop row `i` if it was added by hand; discovered rows can only be
    /// disabled. Returns whether it was removed.
    pub fn forget_profile(&mut self, i: usize) -> bool {
        if !self.profiles.get(i).is_some_and(|p| p.saved.manual) {
            return false;
        }
        self.pin_colors();
        self.profiles.remove(i);
        true
    }

    /// Add a hand-typed config dir as a new enabled row. The path is
    /// normalized first (see [`profiles::normalize_config_dir`]); a dir
    /// already listed is rejected. `~/.claude` is the default profile
    /// (`CLAUDE_CONFIG_DIR` unset, so its own Keychain service), added
    /// only while there is none.
    pub fn add_folder(&mut self, input: &str, home: &Path) -> Result<(), String> {
        let dir = profiles::normalize_config_dir(input, home)?;
        if profiles::is_default_dir(&dir, home) {
            return self.add_default(home);
        }
        if self
            .profiles
            .iter()
            .any(|p| profiles::same_config_dir(&p.saved.config_dir, &dir, home))
        {
            return Err(format!("{} is already listed", dir.display()));
        }
        let id = profiles::manual_id(&dir, |id| self.profiles.iter().any(|p| p.saved.id == id));
        self.profiles
            .push(ProfileDraft::from_saved(&ProfileSettings {
                id,
                config_dir: dir,
                enabled: true,
                manual: true,
                ..ProfileSettings::default()
            }));
        Ok(())
    }

    /// Add `~/.claude` as the default profile, unless it is already listed.
    fn add_default(&mut self, home: &Path) -> Result<(), String> {
        let dir = profiles::default_config_dir(home);
        let listed = self.profiles.iter().any(|p| {
            p.saved.id == profiles::DEFAULT_ID
                || profiles::same_config_dir(&p.saved.config_dir, &dir, home)
        });
        if listed {
            return Err(format!("{} is already the default profile", dir.display()));
        }
        self.profiles
            .push(ProfileDraft::from_saved(&ProfileSettings {
                id: profiles::DEFAULT_ID.to_owned(),
                config_dir: dir,
                enabled: true,
                manual: true,
                ..ProfileSettings::default()
            }));
        Ok(())
    }

    /// Append rows for discovered profiles in `live` the draft doesn't
    /// have yet (found by the poller while the panel was open). Hand-added
    /// entries missing from the draft were forgotten here and stay gone.
    pub fn absorb_discovered(&mut self, live: &[ProfileSettings]) {
        for p in live {
            if !p.manual
                && !self
                    .profiles
                    .iter()
                    .any(|d| d.saved.config_dir == p.config_dir)
            {
                self.profiles.push(ProfileDraft::from_saved(p));
            }
        }
    }
}

/// `"1h"`, `"2m"`, or `"45s"`: the largest unit that divides evenly.
pub fn format_interval(secs: u64) -> String {
    match secs {
        0 => "0s".to_owned(),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The interval field's text, checked.
#[derive(Debug, Clone, PartialEq)]
pub enum IntervalCheck {
    Ok(u64),
    /// Below [`MIN_INTERVAL_SECS`]; Apply uses the minimum instead.
    Clamped {
        entered: u64,
        used: u64,
    },
    Invalid(String),
}

pub fn check_interval(text: &str) -> IntervalCheck {
    match parse_interval(text) {
        Ok(secs) if secs < MIN_INTERVAL_SECS => IntervalCheck::Clamped {
            entered: secs,
            used: MIN_INTERVAL_SECS,
        },
        Ok(secs) => IntervalCheck::Ok(secs),
        Err(e) => IntervalCheck::Invalid(format!("{e}; use e.g. 30, 30s, 2m, 1h")),
    }
}

impl IntervalCheck {
    /// The seconds Apply would store, if the text is usable.
    pub fn secs(&self) -> Option<u64> {
        match self {
            Self::Ok(s) | Self::Clamped { used: s, .. } => Some(*s),
            Self::Invalid(_) => None,
        }
    }
}

/// The settings Apply would commit: the draft over `current`, whose
/// fields the panel does not buffer (`always_on_top`, edited live, and the
/// window/alert state the app maintains) are kept. `Err` carries the
/// interval error; nothing is applied then.
pub fn build_settings(draft: &SettingsDraft, current: &Settings) -> Result<Settings, String> {
    let interval_secs = match check_interval(&draft.interval) {
        IntervalCheck::Invalid(e) => return Err(e),
        check => check.secs().unwrap_or(MIN_INTERVAL_SECS),
    };
    let mut rows = draft.clone();
    rows.absorb_discovered(&current.profiles);
    Ok(Settings {
        plan: non_blank(&draft.plan).unwrap_or_else(|| Settings::default().plan),
        interval_secs,
        show_all: draft.show_all,
        tray_enabled: draft.tray_enabled,
        close_to_tray: draft.close_to_tray,
        start_in_pill: draft.start_in_pill,
        alerts: draft.alerts,
        reduce_motion: draft.reduce_motion,
        keep_pulsing: draft.keep_pulsing,
        profiles: rows.profiles.iter().map(ProfileDraft::build).collect(),
        ..current.clone()
    })
}

/// The message the poller needs after `old` becomes `new`: the interval and
/// the profiles are the settings it uses. The poller applies
/// [`PollMsg::UpdateSettings`] at once and polls immediately.
pub fn poller_update(old: &Settings, new: &Settings) -> Option<PollMsg> {
    let changed = old.interval_secs != new.interval_secs || old.profiles != new.profiles;
    changed.then(|| PollMsg::UpdateSettings(new.clone()))
}

/// A polled account's status for its Accounts row.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountStatus {
    /// The profile id ([`ProfileSettings::id`]).
    pub id: String,
    /// "me@x.com · token valid 3h", "me@x.com · token expired", "waiting".
    pub text: String,
    /// The last poll failed; the full message, for the hover.
    pub error: Option<String>,
}

/// An account's status line from its latest poll: an error's few words if
/// the last poll failed, else the token's remaining life once polled, else
/// "waiting"; prefixed with the email when known.
pub fn account_status(
    id: &str,
    email: Option<&str>,
    polled: bool,
    token_expires_at: Option<DateTime<Utc>>,
    error: Option<(ErrorKind, &str)>,
    now: DateTime<Utc>,
) -> AccountStatus {
    let state = match (error, polled) {
        (Some((kind, _)), _) => super::accounts::short_error(kind).to_owned(),
        (None, true) => match token_expires_at {
            Some(at) => token_validity(at, now),
            None => "signed in".to_owned(),
        },
        (None, false) => "waiting".to_owned(),
    };
    AccountStatus {
        id: id.to_owned(),
        text: match email {
            Some(e) => format!("{e} · {state}"),
            None => state,
        },
        error: error.map(|(_, m)| m.to_owned()),
    }
}

/// "token valid 3h", "token valid 12m", or "token expired".
fn token_validity(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (expires_at - now).num_seconds();
    match secs {
        s if s <= 0 => "token expired".to_owned(),
        s if s >= 3600 => format!("token valid {}h", s / 3600),
        s => format!("token valid {}m", (s / 60).max(1)),
    }
}

/// Window level for the always-on-top flag.
pub fn window_level(always_on_top: bool) -> egui::WindowLevel {
    if always_on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    }
}

/// Set always-on-top in `settings` and on the window right away.
pub fn set_always_on_top(ctx: &egui::Context, settings: &mut Settings, on: bool) {
    settings.always_on_top = on;
    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(window_level(on)));
}

/// The Theme row's choices, in order. egui persists the pick itself (its
/// memory's theme preference), so it is not a [`Settings`] field.
pub const THEME_CHOICES: [(egui::ThemePreference, &str); 3] = [
    (egui::ThemePreference::System, "System"),
    (egui::ThemePreference::Dark, "Dark"),
    (egui::ThemePreference::Light, "Light"),
];

/// The Theme row: radio buttons over egui's theme preference, applied the
/// moment one is picked.
fn theme_row(ui: &mut Ui) {
    let mut pref = ui.ctx().options(|o| o.theme_preference);
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Theme");
        for (value, text) in THEME_CHOICES {
            changed |= ui.radio_value(&mut pref, value, text).changed();
        }
    })
    .response
    .on_hover_text("Applies immediately");
    if changed {
        ui.ctx().set_theme(pref);
    }
}

/// What the user did in the panel this frame.
#[derive(Debug, PartialEq)]
pub enum PanelAction {
    None,
    /// Commit these settings (already validated).
    Apply(Settings),
    /// Discard the draft.
    Cancel,
    /// Send a sample toast; the panel stays open.
    TestNotification,
    /// Poll now, which rediscovers profiles; the panel stays open.
    Rescan,
}

/// Every polled account's [`AccountStatus`], from its latest poll.
pub fn account_statuses(accounts: &Accounts, now: DateTime<Utc>) -> Vec<AccountStatus> {
    accounts
        .list()
        .iter()
        .map(|a| {
            let data = a.poll.data.as_deref();
            account_status(
                a.profile.id.as_str(),
                a.profile.account.as_ref().and_then(|x| x.email.as_deref()),
                data.is_some(),
                data.and_then(|d| d.token_expires_at),
                a.poll.error.as_ref().map(|e| (e.kind, e.message.as_str())),
                now,
            )
        })
        .collect()
}

/// The open settings panel: the draft being edited.
pub struct SettingsPanel {
    draft: SettingsDraft,
    /// The "Add folder" field.
    add_path: String,
    /// Why the last Add was refused.
    add_error: Option<String>,
}

impl SettingsPanel {
    pub fn new(current: &Settings) -> Self {
        Self {
            draft: SettingsDraft::from_settings(current),
            add_path: String::new(),
            add_error: None,
        }
    }

    /// Draw the panel. Always on top (edited in `live`) and Theme (egui's
    /// own preference) apply immediately; everything else waits for Apply.
    /// `statuses` are the polled accounts' status lines, by profile id.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        live: &mut Settings,
        statuses: &[AccountStatus],
    ) -> PanelAction {
        let red = level_color(Level::Red, ui.visuals());
        let orange = level_color(Level::Orange, ui.visuals());
        let mut action = PanelAction::None;
        let palette = theme::Palette::for_visuals(ui.visuals());
        self.draft.absorb_discovered(&live.profiles);
        theme::card(palette).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Settings");
            ui.add_space(4.0);

            field(ui, "Plan label", |ui| {
                ui.add(TextEdit::singleline(&mut self.draft.plan).desired_width(f32::INFINITY));
            });

            field(ui, "Poll interval", |ui| {
                ui.add(
                    TextEdit::singleline(&mut self.draft.interval)
                        .hint_text("30, 30s, 2m, 1h")
                        .desired_width(f32::INFINITY),
                );
                match check_interval(&self.draft.interval) {
                    IntervalCheck::Ok(_) => {}
                    IntervalCheck::Clamped { entered, used } => note(
                        ui,
                        orange,
                        &format!("{entered} s is below the minimum; {used} s will be used"),
                    ),
                    IntervalCheck::Invalid(e) => note(ui, red, &e),
                }
            });

            theme::checkbox(ui, &mut self.draft.show_all, "Show all buckets");
            let mut on_top = live.always_on_top;
            if theme::checkbox(ui, &mut on_top, "Always on top")
                .on_hover_text("Applies immediately")
                .changed()
            {
                set_always_on_top(ui.ctx(), live, on_top);
            }
            theme_row(ui);
            ui.add_space(4.0);

            group_heading(ui, "Accounts");
            if accounts_group(
                ui,
                palette,
                &mut self.draft,
                statuses,
                &mut self.add_path,
                &mut self.add_error,
            ) {
                action = PanelAction::Rescan;
            }

            group_heading(ui, "Window");
            let d = &mut self.draft;
            let (show_text, hide_text, needs) = tray_labels();
            theme::checkbox(ui, &mut d.tray_enabled, show_text);
            ui.add_enabled_ui(d.tray_enabled, |ui| {
                theme::checkbox(ui, &mut d.close_to_tray, hide_text).on_disabled_hover_text(needs);
            });
            theme::checkbox(ui, &mut d.start_in_pill, "Start in pill mode");

            group_heading(ui, "Alerts");
            theme::checkbox(ui, &mut d.alerts.crossed_75, "5-hour window reaches 75%");
            theme::checkbox(ui, &mut d.alerts.crossed_90, "5-hour window reaches 90%");
            theme::checkbox(
                ui,
                &mut d.alerts.projected_cap,
                "Projected to hit the cap before reset",
            );
            if ui
                .button("Send test notification")
                .on_hover_text("Shows a sample alert now, as real alerts are sent")
                .clicked()
            {
                action = PanelAction::TestNotification;
            }

            group_heading(ui, "Motion");
            theme::checkbox(ui, &mut d.reduce_motion, "Reduce motion");
            theme::checkbox(ui, &mut d.keep_pulsing, "Keep pulsing near cap");
            ui.add_space(4.0);

            ui.horizontal(|ui| {
                let built = build_settings(&self.draft, live);
                if ui
                    .add_enabled(built.is_ok(), egui::Button::new("Apply"))
                    .on_disabled_hover_text("Fix the poll interval first")
                    .clicked()
                {
                    if let Ok(new) = built {
                        action = PanelAction::Apply(new);
                    }
                }
                if ui.button("Cancel").clicked() {
                    action = PanelAction::Cancel;
                }
            });
        });
        ui.add_space(4.0);
        action
    }
}

/// The tray toggles' texts: "Show tray icon", "Close button hides to
/// tray", and the disabled hover; macOS calls the tray the menu bar.
fn tray_labels() -> (&'static str, &'static str, &'static str) {
    if cfg!(target_os = "macos") {
        (
            "Show menu-bar item",
            "Close button hides to menu bar",
            "Needs the menu-bar item",
        )
    } else {
        (
            "Show tray icon",
            "Close button hides to tray",
            "Needs the tray icon",
        )
    }
}

/// A row edit that reshapes the list, applied after the rows are drawn.
enum RowOp {
    Move(usize, bool),
    Forget(usize),
}

/// The Accounts group: a card per profile, then the Add folder field and
/// Rescan. Returns whether Rescan was clicked.
fn accounts_group(
    ui: &mut Ui,
    palette: &theme::Palette,
    draft: &mut SettingsDraft,
    statuses: &[AccountStatus],
    add_path: &mut String,
    add_error: &mut Option<String>,
) -> bool {
    let red = level_color(Level::Red, ui.visuals());
    let muted = palette.text_muted;
    let n = draft.profiles.len();
    let series_len = palette.series.len() as u8;
    let mut op = None;
    if n == 0 {
        note(ui, muted, "No Claude Code folders found yet.");
    }
    for (i, p) in draft.profiles.iter_mut().enumerate() {
        ui.push_id(p.saved.id.clone(), |ui| {
            egui::Frame::new()
                .stroke(Stroke::new(1.0, palette.border))
                .corner_radius(6)
                .inner_margin(6)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let current = p.saved.color.unwrap_or(i as u8) % series_len;
                        for c in 0..series_len {
                            if swatch(ui, palette.series(c), c == current)
                                .on_hover_text("Account color")
                                .clicked()
                            {
                                p.saved.color = Some(c);
                            }
                        }
                        ui.add_space(4.0);
                        theme::checkbox(ui, &mut p.saved.enabled, "Enabled")
                            .on_hover_text("Unchecked: not polled or shown");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if p.saved.manual
                                && ui
                                    .small_button("Forget")
                                    .on_hover_text("Remove this hand-added folder")
                                    .clicked()
                            {
                                op = Some(RowOp::Forget(i));
                            }
                            let down = egui::Button::new("▼").small();
                            if ui
                                .add_enabled(i + 1 < n, down)
                                .on_hover_text("Move down")
                                .clicked()
                            {
                                op = Some(RowOp::Move(i, false));
                            }
                            let up = egui::Button::new("▲").small();
                            if ui.add_enabled(i > 0, up).on_hover_text("Move up").clicked() {
                                op = Some(RowOp::Move(i, true));
                            }
                        });
                    });
                    ui.add(
                        TextEdit::singleline(&mut p.label)
                            .hint_text("Label (blank: from the account)")
                            .desired_width(f32::INFINITY),
                    );
                    let dim = |ui: &mut Ui, text: String| {
                        ui.add(egui::Label::new(RichText::new(text).small().color(muted)).wrap());
                    };
                    dim(ui, p.saved.config_dir.display().to_string());
                    dim(ui, format!("Keychain: {}", p.effective_keychain_service()));
                    let status = statuses.iter().find(|s| s.id == p.saved.id);
                    match status {
                        Some(s) => {
                            let color = if s.error.is_some() { red } else { palette.text };
                            let r = ui
                                .add(egui::Label::new(RichText::new(&s.text).color(color)).wrap());
                            if let Some(msg) = &s.error {
                                r.on_hover_text(msg);
                            }
                        }
                        None if p.saved.enabled => note(ui, muted, "waiting"),
                        None => note(ui, muted, "disabled"),
                    }
                    egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
                        ui.label("Keychain service override");
                        ui.add(
                            TextEdit::singleline(&mut p.keychain_service)
                                .hint_text("blank: derived from the folder")
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
        });
        ui.add_space(4.0);
    }
    match op {
        Some(RowOp::Move(i, up)) => {
            draft.move_profile(i, up);
        }
        Some(RowOp::Forget(i)) => {
            draft.forget_profile(i);
        }
        None => {}
    }

    let mut rescan = false;
    ui.label("Add folder");
    ui.horizontal(|ui| {
        let buttons = 110.0;
        let field = ui.add(
            TextEdit::singleline(add_path)
                .hint_text("~/.claude-work")
                .desired_width((ui.available_width() - buttons).max(80.0)),
        );
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Add").clicked() || entered {
            match draft.add_folder(add_path, &crate::paths::home_dir()) {
                Ok(()) => {
                    add_path.clear();
                    *add_error = None;
                }
                Err(e) => *add_error = Some(e),
            }
        }
        rescan = ui
            .button("Rescan")
            .on_hover_text("Look for new Claude Code folders now")
            .clicked();
    });
    if let Some(e) = add_error {
        note(ui, red, e);
    }
    rescan
}

/// A small clickable color square, ringed when `selected`.
fn swatch(ui: &mut Ui, color: Color32, selected: bool) -> egui::Response {
    let size = egui::vec2(14.0, 14.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect.shrink(2.0), 3.0, color);
    if selected || response.hovered() {
        let ring = ui.visuals().strong_text_color();
        painter.rect_stroke(rect, 4.0, Stroke::new(1.5, ring), egui::StrokeKind::Inside);
    }
    response
}

/// A labeled field: the label above, then `add`.
fn field(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(label);
    add(ui);
    ui.add_space(4.0);
}

/// A small semibold heading over a group of toggles.
fn group_heading(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    let body = egui::TextStyle::Body.resolve(ui.style());
    ui.label(RichText::new(text).font(theme::semibold(body.size)));
}

/// A wrapping colored note under a field.
fn note(ui: &mut Ui, color: Color32, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_choices_cover_every_preference_once() {
        let prefs: Vec<_> = THEME_CHOICES.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            prefs,
            [
                egui::ThemePreference::System,
                egui::ThemePreference::Dark,
                egui::ThemePreference::Light
            ]
        );
        let labels: Vec<_> = THEME_CHOICES.iter().map(|(_, t)| *t).collect();
        assert_eq!(labels, ["System", "Dark", "Light"]);
    }

    #[test]
    fn theme_row_applies_the_pick_to_the_context() {
        // set_theme is what the row calls; the row reads the same option back.
        let ctx = egui::Context::default();
        for (pref, _) in THEME_CHOICES {
            ctx.set_theme(pref);
            assert_eq!(ctx.options(|o| o.theme_preference), pref);
        }
    }

    #[test]
    fn window_level_follows_flag() {
        assert_eq!(window_level(true), egui::WindowLevel::AlwaysOnTop);
        assert_eq!(window_level(false), egui::WindowLevel::Normal);
    }

    fn draft() -> SettingsDraft {
        SettingsDraft::from_settings(&Settings::default())
    }

    #[test]
    fn draft_from_settings() {
        let s = Settings {
            plan: "Max 5x".into(),
            interval_secs: 120,
            show_all: true,
            always_on_top: true,
            tray_enabled: false,
            start_in_pill: true,
            reduce_motion: true,
            ..Settings::default()
        };
        assert_eq!(
            SettingsDraft::from_settings(&s),
            SettingsDraft {
                plan: "Max 5x".into(),
                interval: "2m".into(),
                show_all: true,
                tray_enabled: false,
                close_to_tray: true,
                start_in_pill: true,
                alerts: AlertToggles::default(),
                reduce_motion: true,
                keep_pulsing: false,
                profiles: Vec::new(),
            }
        );
        let d = draft();
        assert_eq!(d.interval, "1m");
    }

    #[test]
    fn format_interval_largest_even_unit() {
        assert_eq!(format_interval(45), "45s");
        assert_eq!(format_interval(60), "1m");
        assert_eq!(format_interval(90), "90s");
        assert_eq!(format_interval(7200), "2h");
        assert_eq!(format_interval(5400), "90m");
        assert_eq!(format_interval(0), "0s");
    }

    #[test]
    fn format_interval_round_trips_through_parse() {
        for secs in [5, 45, 60, 90, 3600, 5400] {
            assert_eq!(parse_interval(&format_interval(secs)).unwrap(), secs);
        }
    }

    #[test]
    fn check_interval_cases() {
        assert_eq!(check_interval("30"), IntervalCheck::Ok(30));
        assert_eq!(check_interval(" 2m "), IntervalCheck::Ok(120));
        assert_eq!(
            check_interval(&MIN_INTERVAL_SECS.to_string()),
            IntervalCheck::Ok(MIN_INTERVAL_SECS)
        );
        assert_eq!(
            check_interval("2s"),
            IntervalCheck::Clamped {
                entered: 2,
                used: MIN_INTERVAL_SECS
            }
        );
        assert!(matches!(check_interval("0"), IntervalCheck::Clamped { .. }));
        for bad in ["", "abc", "1x", "-5", "1.5m"] {
            assert!(
                matches!(check_interval(bad), IntervalCheck::Invalid(_)),
                "{bad:?}"
            );
        }
        assert_eq!(check_interval("abc").secs(), None);
        assert_eq!(check_interval("1s").secs(), Some(MIN_INTERVAL_SECS));
    }

    #[test]
    fn build_unchanged_draft_is_identity() {
        let current = Settings {
            always_on_top: true,
            alert_states: [("default".to_owned(), Default::default())].into(),
            ..Settings::default()
        };
        let d = SettingsDraft::from_settings(&current);
        assert_eq!(build_settings(&d, &current).unwrap(), current);
    }

    #[test]
    fn build_applies_trimmed_fields_and_keeps_live_always_on_top() {
        let current = Settings {
            always_on_top: true,
            ..Settings::default()
        };
        let d = SettingsDraft {
            plan: "  Pro ".into(),
            interval: "2s".into(),
            show_all: true,
            ..draft()
        };
        assert_eq!(
            build_settings(&d, &current).unwrap(),
            Settings {
                plan: "Pro".into(),
                interval_secs: MIN_INTERVAL_SECS,
                show_all: true,
                always_on_top: true,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn build_applies_toggles_and_keeps_app_maintained_state() {
        use crate::config::{Pos, SavedRect, UiMode};
        let current = Settings {
            ui_mode: UiMode::Pill,
            full_rect: Some(SavedRect {
                pos: Pos { x: 1, y: 2 },
                w: 400.0,
                h: 450.0,
            }),
            full_maximized: true,
            pill_pos: Some(Pos { x: 5, y: 6 }),
            ..Settings::default()
        };
        let d = SettingsDraft {
            tray_enabled: false,
            close_to_tray: false,
            start_in_pill: true,
            alerts: AlertToggles {
                crossed_75: false,
                crossed_90: true,
                projected_cap: false,
            },
            reduce_motion: true,
            keep_pulsing: true,
            ..SettingsDraft::from_settings(&current)
        };
        assert_eq!(
            build_settings(&d, &current).unwrap(),
            Settings {
                tray_enabled: false,
                close_to_tray: false,
                start_in_pill: true,
                alerts: d.alerts,
                reduce_motion: true,
                keep_pulsing: true,
                ..current.clone()
            }
        );
    }

    #[test]
    fn apply_keeps_ui_state_changed_while_the_panel_was_open() {
        use crate::config::{Pos, SavedRect, UiMode};
        // The panel opened on these settings...
        let opened = Settings::default();
        let d = SettingsDraft::from_settings(&opened);
        // ...then the window moved and Always on top flipped before Apply.
        // Apply builds over the live settings, not the snapshot the draft
        // came from.
        let live = Settings {
            always_on_top: true,
            ui_mode: UiMode::Pill,
            full_rect: Some(SavedRect {
                pos: Pos { x: 3, y: 4 },
                w: 420.0,
                h: 460.0,
            }),
            full_maximized: true,
            pill_pos: Some(Pos { x: 7, y: 8 }),
            ..opened.clone()
        };
        assert_eq!(build_settings(&d, &live).unwrap(), live);
    }

    #[test]
    fn build_blank_plan_becomes_default() {
        let current = Settings::default();
        let blank_plan = SettingsDraft {
            plan: "  ".into(),
            ..draft()
        };
        assert_eq!(
            build_settings(&blank_plan, &current).unwrap().plan,
            Settings::default().plan
        );
    }

    #[test]
    fn build_rejects_invalid_interval() {
        let d = SettingsDraft {
            interval: "soon".into(),
            plan: "Changed".into(),
            ..draft()
        };
        let err = build_settings(&d, &Settings::default()).unwrap_err();
        assert!(err.contains("soon"), "{err}");
    }

    #[test]
    fn poller_hears_only_about_what_it_uses() {
        let old = Settings::default();
        let ui_only = Settings {
            plan: "Pro".into(),
            show_all: true,
            always_on_top: true,
            ..old.clone()
        };
        assert!(poller_update(&old, &old).is_none());
        assert!(poller_update(&old, &ui_only).is_none());

        for new in [
            Settings {
                interval_secs: 30,
                ..old.clone()
            },
            Settings {
                profiles: vec![crate::config::ProfileSettings {
                    id: "default".into(),
                    ..Default::default()
                }],
                ..old.clone()
            },
        ] {
            match poller_update(&old, &new) {
                Some(PollMsg::UpdateSettings(sent)) => assert_eq!(sent, new),
                other => panic!("expected UpdateSettings, got {other:?}"),
            }
        }
    }

    fn profile(id: &str, dir: &Path) -> ProfileSettings {
        ProfileSettings {
            id: id.into(),
            config_dir: dir.to_path_buf(),
            ..ProfileSettings::default()
        }
    }

    /// Settings with `default` (~/.claude) and `claude-b` (~/.claude-b)
    /// under `home`.
    fn two_profiles(home: &Path) -> Settings {
        Settings {
            profiles: vec![
                profile("default", &home.join(".claude")),
                profile("claude-b", &home.join(".claude-b")),
            ],
            ..Settings::default()
        }
    }

    fn built_profiles(d: &SettingsDraft, current: &Settings) -> Vec<ProfileSettings> {
        build_settings(d, current).unwrap().profiles
    }

    #[test]
    fn profiles_round_trip_through_the_draft() {
        let home = Path::new("/h");
        let mut current = two_profiles(home);
        current.profiles[1].label = Some("work".into());
        current.profiles[1].color = Some(4);
        current.profiles[1].enabled = false;
        current.profiles[1].keychain_service = Some("svc".into());
        let d = SettingsDraft::from_settings(&current);
        assert_eq!(d.profiles[1].label, "work");
        assert_eq!(d.profiles[1].keychain_service, "svc");
        assert_eq!(build_settings(&d, &current).unwrap(), current);
    }

    #[test]
    fn rename_sets_the_label_and_blank_restores_the_default() {
        let current = two_profiles(Path::new("/h"));
        let mut d = SettingsDraft::from_settings(&current);
        d.profiles[0].label = "  Personal ".into();
        d.profiles[1].keychain_service = "  ".into();
        let built = built_profiles(&d, &current);
        assert_eq!(built[0].label.as_deref(), Some("Personal"));
        assert_eq!(built[1].keychain_service, None);

        d.profiles[0].label = " ".into();
        assert_eq!(built_profiles(&d, &current)[0].label, None);
    }

    #[test]
    fn recolor_and_disable_reach_the_settings() {
        let current = two_profiles(Path::new("/h"));
        let mut d = SettingsDraft::from_settings(&current);
        d.profiles[0].saved.color = Some(5);
        d.profiles[1].saved.enabled = false;
        let built = built_profiles(&d, &current);
        assert_eq!(built[0].color, Some(5));
        assert!(built[0].enabled);
        assert!(!built[1].enabled);
    }

    #[test]
    fn reorder_swaps_rows_and_keeps_their_colors() {
        let home = Path::new("/h");
        let mut current = two_profiles(home);
        current
            .profiles
            .push(profile("claude-c", &home.join(".claude-c")));
        let mut d = SettingsDraft::from_settings(&current);
        assert!(!d.move_profile(0, true), "first row can't move up");
        assert!(!d.move_profile(2, false), "last row can't move down");
        assert!(!d.move_profile(7, true), "no such row");
        assert_eq!(d.profiles, SettingsDraft::from_settings(&current).profiles);

        assert!(d.move_profile(2, true));
        let built = built_profiles(&d, &current);
        let ids: Vec<_> = built.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["default", "claude-c", "claude-b"]);
        // Default colors were by position; each account keeps its own.
        let colors: Vec<_> = built.iter().map(|p| p.color).collect();
        assert_eq!(colors, [Some(0), Some(2), Some(1)]);

        assert!(d.move_profile(0, false));
        let ids: Vec<_> = built_profiles(&d, &current)
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["claude-c", "default", "claude-b"]);
    }

    #[test]
    fn add_folder_normalizes_and_adds_an_enabled_manual_row() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let current = two_profiles(home);
        let mut d = SettingsDraft::from_settings(&current);
        let typed = format!("{}/work-claude/", home.display());
        d.add_folder(&typed, home).unwrap();
        let added = &built_profiles(&d, &current)[2];
        assert_eq!(added.id, "work-claude");
        assert_eq!(added.config_dir, home.join("work-claude"));
        assert!(added.enabled && added.manual);
        assert_eq!(added.label, None);

        // `~` expands, and the id is unique.
        d.add_folder("~/x/.claude-b", home).unwrap();
        let added = &built_profiles(&d, &current)[3];
        assert_eq!(added.id, "claude-b-2");
        assert_eq!(added.config_dir, home.join("x").join(".claude-b"));
    }

    #[test]
    fn add_folder_maps_the_home_claude_dir_to_the_default_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // ~/.claude didn't exist at discovery, so there is no default row.
        let current = Settings {
            profiles: vec![profile("claude-b", &home.join(".claude-b"))],
            ..Settings::default()
        };
        let mut d = SettingsDraft::from_settings(&current);
        d.add_folder(&format!("{}/.claude/", home.display()), home)
            .unwrap();
        let row = d.profiles.last().unwrap();
        assert_eq!(row.saved.id, profiles::DEFAULT_ID);
        assert_eq!(row.saved.config_dir, home.join(".claude"));
        assert_eq!(row.effective_keychain_service(), "Claude Code-credentials");
        // Polled as the default profile: no CLAUDE_CONFIG_DIR.
        let built = built_profiles(&d, &current);
        let polled = profiles::resolve(&built, &[]);
        let default = polled.iter().find(|p| p.id.is_default()).unwrap();
        assert_eq!(default.config_dir_env, None);
        assert_eq!(default.keychain_service(), "Claude Code-credentials");
        // Once is enough.
        let err = d.add_folder("~/.claude", home).unwrap_err();
        assert!(err.contains("already the default profile"), "{err}");
    }

    #[test]
    fn add_folder_rejects_duplicates_and_bad_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude-b")).unwrap();
        let current = two_profiles(home);
        let mut d = SettingsDraft::from_settings(&current);
        let before = d.clone();
        for dup in [
            "~/.claude-b".to_owned(),
            "~/.claude-b/".to_owned(),
            format!("{}/.claude-b", home.display()),
        ] {
            let err = d.add_folder(&dup, home).unwrap_err();
            assert!(err.contains("already listed"), "{dup}: {err}");
        }
        for default in ["~/.claude", "~/.claude/"] {
            let err = d.add_folder(default, home).unwrap_err();
            assert!(err.contains("already the default profile"), "{err}");
        }
        assert!(d.add_folder("", home).is_err());
        assert!(d.add_folder("relative/dir", home).is_err());
        assert_eq!(d, before, "nothing was added");

        d.add_folder("~/.claude-new", home).unwrap();
        let err = d.add_folder("~/.claude-new//", home).unwrap_err();
        assert!(err.contains("already listed"), "{err}");
    }

    #[test]
    fn only_hand_added_rows_can_be_forgotten() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let current = two_profiles(home);
        let mut d = SettingsDraft::from_settings(&current);
        assert!(!d.forget_profile(1), "discovered rows only disable");
        assert!(!d.forget_profile(9));
        d.add_folder("~/manual", home).unwrap();
        d.add_folder("~/other", home).unwrap();
        assert!(d.forget_profile(2));
        let built = built_profiles(&d, &current);
        let ids: Vec<_> = built.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["default", "claude-b", "other"]);
        assert_eq!(built[2].color, Some(3), "keeps the color it had");
    }

    #[test]
    fn apply_keeps_profiles_discovered_while_the_panel_was_open() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let opened = Settings {
            profiles: vec![
                profile("default", &home.join(".claude")),
                ProfileSettings {
                    manual: true,
                    ..profile("manual", &home.join("manual"))
                },
            ],
            ..Settings::default()
        };
        let mut d = SettingsDraft::from_settings(&opened);
        d.forget_profile(1);
        // The poller found a new folder meanwhile; App merged it in.
        let mut live = opened.clone();
        live.profiles
            .push(profile("claude-new", &home.join(".claude-new")));
        let ids: Vec<_> = built_profiles(&d, &live)
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["default", "claude-new"], "forgotten stays gone");

        d.absorb_discovered(&live.profiles);
        assert_eq!(d.profiles.len(), 2);
        d.absorb_discovered(&live.profiles);
        assert_eq!(d.profiles.len(), 2, "absorbs once");
    }

    #[test]
    fn poller_hears_about_any_profile_edit() {
        let old = two_profiles(Path::new("/h"));
        let edits: [fn(&mut SettingsDraft); 4] = [
            |d| d.profiles[0].label = "me".into(),
            |d| d.profiles[0].saved.color = Some(3),
            |d| d.profiles[1].saved.enabled = false,
            |d| {
                d.move_profile(1, true);
            },
        ];
        for edit in edits {
            let mut d = SettingsDraft::from_settings(&old);
            edit(&mut d);
            let new = build_settings(&d, &old).unwrap();
            assert!(
                matches!(poller_update(&old, &new), Some(PollMsg::UpdateSettings(_))),
                "{d:?}"
            );
        }
    }

    #[test]
    fn effective_keychain_service_follows_dir_or_override() {
        let home = Path::new("/h");
        let s = two_profiles(home);
        let d = SettingsDraft::from_settings(&s);
        assert_eq!(
            d.profiles[0].effective_keychain_service(),
            profiles::keychain_service_for(None)
        );
        let env = home.join(".claude-b").display().to_string();
        assert_eq!(
            d.profiles[1].effective_keychain_service(),
            profiles::keychain_service_for(Some(&env))
        );
        let mut row = d.profiles[1].clone();
        row.keychain_service = " custom ".into();
        assert_eq!(row.effective_keychain_service(), "custom");
    }

    #[test]
    fn account_status_lines() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let h = chrono::Duration::hours(1);
        let status = |email, polled, exp, err| account_status("b", email, polled, exp, err, now);
        let s = status(
            Some("me@x.com"),
            true,
            Some(now + h * 3 + chrono::Duration::minutes(20)),
            None,
        );
        assert_eq!(s.text, "me@x.com · token valid 3h");
        assert_eq!((s.id.as_str(), s.error), ("b", None));
        assert_eq!(
            status(None, true, Some(now + chrono::Duration::seconds(30)), None).text,
            "token valid 1m"
        );
        assert_eq!(
            status(None, true, Some(now - h), None).text,
            "token expired"
        );
        assert_eq!(status(None, true, None, None).text, "signed in");
        assert_eq!(
            status(Some("me@x.com"), false, None, None).text,
            "me@x.com · waiting"
        );
        let s = status(
            Some("me@x.com"),
            true,
            Some(now + h),
            Some((ErrorKind::NoCredentials, "no credential in ~/.claude-b")),
        );
        assert_eq!(s.text, "me@x.com · not signed in");
        assert_eq!(s.error.as_deref(), Some("no credential in ~/.claude-b"));
    }

    #[test]
    fn tray_labels_name_the_menu_bar_on_macos() {
        let (show, hide, _) = tray_labels();
        if cfg!(target_os = "macos") {
            assert_eq!(
                (show, hide),
                ("Show menu-bar item", "Close button hides to menu bar")
            );
        } else {
            assert_eq!(
                (show, hide),
                ("Show tray icon", "Close button hides to tray")
            );
        }
    }
}
