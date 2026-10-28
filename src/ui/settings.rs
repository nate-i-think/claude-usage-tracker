//! The settings panel: a buffered edit of [`Settings`] that is only
//! committed on Apply.
//!
//! The edit-buffer logic (validation, building the new [`Settings`], and
//! deciding what the poller must hear) is plain functions; [`SettingsPanel`]
//! only draws them.

use eframe::egui::{self, Color32, RichText, TextEdit, Ui};

use crate::analytics::alerts::AlertToggles;
use crate::analytics::Level;
use crate::config::{parse_interval, Settings, MIN_INTERVAL_SECS};
use crate::poller::PollMsg;

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
    let non_blank = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
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
}

/// The open settings panel: the draft being edited.
pub struct SettingsPanel {
    draft: SettingsDraft,
}

impl SettingsPanel {
    pub fn new(current: &Settings) -> Self {
        Self {
            draft: SettingsDraft::from_settings(current),
        }
    }

    /// Draw the panel. Always on top (edited in `live`) and Theme (egui's
    /// own preference) apply immediately; everything else waits for Apply.
    pub fn show(&mut self, ui: &mut Ui, live: &mut Settings) -> PanelAction {
        let red = level_color(Level::Red, ui.visuals());
        let orange = level_color(Level::Orange, ui.visuals());
        let mut action = PanelAction::None;
        let palette = theme::Palette::for_visuals(ui.visuals());
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

            group_heading(ui, "Window");
            let d = &mut self.draft;
            theme::checkbox(ui, &mut d.tray_enabled, "Show tray icon");
            ui.add_enabled_ui(d.tray_enabled, |ui| {
                theme::checkbox(ui, &mut d.close_to_tray, "Close button hides to tray")
                    .on_disabled_hover_text("Needs the tray icon");
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
            details_open: true,
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
        // ...then Details was toggled, the window moved and Always on top flipped
        // before Apply. Apply builds over the live settings, not the
        // snapshot the draft came from.
        let live = Settings {
            details_open: true,
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
}
