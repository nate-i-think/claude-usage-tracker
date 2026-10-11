//! The settings panel: a buffered edit of [`Settings`] that is only
//! committed on Apply.
//!
//! The edit-buffer logic (validation, building the new [`Settings`], and
//! deciding what the poller must hear) is plain functions; [`SettingsPanel`]
//! only draws them.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align, Color32, Layout, RichText, TextEdit, Ui};

use crate::analytics::alerts::AlertToggles;
use crate::analytics::Level;
use crate::config::{
    parse_interval, resolve_client_sha_with_source, ClientShaSource, Settings, CLIENT_SHA_ENV,
    CLIENT_SHA_FILE, MIN_CLIENT_SHA_LEN, MIN_INTERVAL_SECS,
};
use crate::poller::PollMsg;

use super::{level_color, theme};

/// The panel's text fields, edited freely until Apply.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsDraft {
    pub plan: String,
    pub interval: String,
    pub show_all: bool,
    pub cookie_file: String,
    pub client_sha: String,
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
            cookie_file: s
                .cookie_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            client_sha: s.client_sha.clone().unwrap_or_default(),
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
        cookie_file: non_blank(&draft.cookie_file).map(PathBuf::from),
        client_sha: non_blank(&draft.client_sha),
        tray_enabled: draft.tray_enabled,
        close_to_tray: draft.close_to_tray,
        start_in_pill: draft.start_in_pill,
        alerts: draft.alerts,
        reduce_motion: draft.reduce_motion,
        keep_pulsing: draft.keep_pulsing,
        ..current.clone()
    })
}

/// The message the poller needs after `old` becomes `new`: the interval,
/// cookie file, and client SHA are the settings it uses. The poller applies
/// [`PollMsg::UpdateSettings`] at once and polls immediately.
pub fn poller_update(old: &Settings, new: &Settings) -> Option<PollMsg> {
    let changed = old.interval_secs != new.interval_secs
        || old.cookie_file != new.cookie_file
        || old.client_sha != new.client_sha;
    changed.then(|| PollMsg::UpdateSettings(new.clone()))
}

/// "In use: …" text for the client SHA resolution result, and whether a
/// SHA was found at all. Never includes the SHA itself.
pub fn describe_sha_source(resolved: &anyhow::Result<(String, ClientShaSource)>) -> (String, bool) {
    match resolved {
        Ok((_, ClientShaSource::Env)) => (
            format!("In use: environment variable {CLIENT_SHA_ENV} (overrides this field)"),
            true,
        ),
        Ok((_, ClientShaSource::Settings)) => ("In use: the value saved here".to_owned(), true),
        Ok((_, ClientShaSource::File(path))) => (format!("In use: file {}", path.display()), true),
        Err(_) => (
            format!("In use: none found; enter one here or restore {CLIENT_SHA_FILE}"),
            false,
        ),
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

/// What the user did in the panel this frame.
#[derive(Debug, PartialEq)]
pub enum PanelAction {
    None,
    /// Commit these settings (already validated).
    Apply(Settings),
    /// Discard the draft.
    Cancel,
}

/// Adapted from the README section "Refreshing client_sha.txt".
const HELP_SHA: &str = "Anthropic rotates the web client's build hash every couple of \
weeks; when it goes stale, polls fail with permission_error even with a valid session. \
To get the current one: open claude.ai/settings/usage in your browser, open DevTools \
(F12) \u{2192} Network \u{2192} filter Fetch/XHR, reload the page, select the request to \
/api/organizations/\u{2026}/usage, and in its Headers copy the anthropic-client-sha \
request header. Leave this field blank to use client_sha.txt.";

/// The open settings panel: the draft plus the client SHA source as of
/// opening.
pub struct SettingsPanel {
    draft: SettingsDraft,
    sha_source: (String, bool),
    cookie_check: CookieFileCheck,
}

/// Whether the cookie path is an existing file, cached per path text so the
/// open panel does not stat the disk every frame.
#[derive(Debug, Default)]
struct CookieFileCheck {
    /// The path text last checked, and its result.
    last: Option<(String, bool)>,
}

impl CookieFileCheck {
    fn is_file(&mut self, path: &str) -> bool {
        self.is_file_with(path, |p| Path::new(p).is_file())
    }

    /// Like [`Self::is_file`] with an injectable check (for tests); `check`
    /// runs only when `path` differs from the last one.
    fn is_file_with(&mut self, path: &str, check: impl FnOnce(&str) -> bool) -> bool {
        match &self.last {
            Some((p, found)) if p == path => *found,
            _ => {
                let found = check(path);
                self.last = Some((path.to_owned(), found));
                found
            }
        }
    }
}

impl SettingsPanel {
    pub fn new(current: &Settings) -> Self {
        Self {
            draft: SettingsDraft::from_settings(current),
            sha_source: describe_sha_source(&resolve_client_sha_with_source(
                current.client_sha.as_deref(),
            )),
            cookie_check: CookieFileCheck::default(),
        }
    }

    /// Draw the panel. Always-on-top edits `live` immediately (it is the
    /// header pin's state); everything else waits for Apply.
    pub fn show(&mut self, ui: &mut Ui, live: &mut Settings) -> PanelAction {
        let red = level_color(Level::Red, ui.visuals());
        let green = level_color(Level::Green, ui.visuals());
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
            ui.add_space(4.0);

            field(ui, "Cookie file", |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Clear").clicked() {
                            self.draft.cookie_file.clear();
                        }
                        ui.add(
                            TextEdit::singleline(&mut self.draft.cookie_file)
                                .hint_text("auto-detect")
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
                let path = self.draft.cookie_file.trim();
                if path.is_empty() {
                    ui.weak("Blank: the browser profile is detected automatically");
                } else if self.cookie_check.is_file(path) {
                    note(ui, green, "File exists");
                } else {
                    note(ui, red, "File not found");
                }
            });

            field(ui, "Client SHA", |ui| {
                ui.add(
                    TextEdit::singleline(&mut self.draft.client_sha)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(format!("blank: use {CLIENT_SHA_FILE}"))
                        .desired_width(f32::INFINITY),
                );
                let len = self.draft.client_sha.trim().len();
                if (1..MIN_CLIENT_SHA_LEN).contains(&len) {
                    note(
                        ui,
                        orange,
                        &format!(
                            "Shorter than {MIN_CLIENT_SHA_LEN} characters; it will be ignored"
                        ),
                    );
                }
                let (text, found) = &self.sha_source;
                if *found {
                    ui.add(egui::Label::new(RichText::new(text).weak()).wrap());
                } else {
                    note(ui, red, text);
                }
                egui::CollapsingHeader::new("How to get it")
                    .id_salt("client_sha_help")
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(HELP_SHA).wrap());
                    });
            });

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

            group_heading(ui, "Motion");
            theme::checkbox(ui, &mut d.reduce_motion, "Reduce motion");
            theme::checkbox(ui, &mut d.keep_pulsing, "Keep pulsing at high usage");
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
    fn sha_source_descriptions() {
        let ok = |src| Ok(("abcdef123".to_owned(), src));
        let (text, found) = describe_sha_source(&ok(ClientShaSource::Env));
        assert!(found && text.contains(CLIENT_SHA_ENV), "{text}");
        let (text, found) = describe_sha_source(&ok(ClientShaSource::Settings));
        assert!(found && text.contains("saved here"), "{text}");
        let path = PathBuf::from("C:/app/client_sha.txt");
        let (text, found) = describe_sha_source(&ok(ClientShaSource::File(path.clone())));
        assert!(
            found && text.contains(&path.display().to_string()),
            "{text}"
        );
        let (text, found) = describe_sha_source(&Err(anyhow::anyhow!("missing")));
        assert!(!found && text.contains(CLIENT_SHA_FILE), "{text}");
        assert!(!text.contains("abcdef123"));
    }

    #[test]
    fn cookie_file_check_stats_only_when_the_text_changes() {
        let mut c = CookieFileCheck::default();
        let mut calls = 0;
        let mut check = |path: &str, result: bool| {
            c.is_file_with(path, |_| {
                calls += 1;
                result
            })
        };
        assert!(check("a.json", true));
        assert!(check("a.json", false)); // cached: the check is not rerun
        assert!(!check("b.json", false));
        assert!(check("a.json", true));
        assert_eq!(calls, 3);

        let real = tempfile::NamedTempFile::new().unwrap();
        let mut c = CookieFileCheck::default();
        assert!(c.is_file(real.path().to_str().unwrap()));
        assert!(!c.is_file("definitely/not/here.json"));
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
            cookie_file: Some(PathBuf::from("C:/x/cookies.sqlite")),
            client_sha: Some("abcdef123".into()),
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
                cookie_file: "C:/x/cookies.sqlite".into(),
                client_sha: "abcdef123".into(),
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
        assert_eq!(d.cookie_file, "");
        assert_eq!(d.client_sha, "");
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
            cookie_file: Some(PathBuf::from("c.sqlite")),
            client_sha: Some("abcdef123".into()),
            ..Settings::default()
        };
        let d = SettingsDraft::from_settings(&current);
        assert_eq!(build_settings(&d, &current).unwrap(), current);
    }

    #[test]
    fn build_applies_trimmed_fields_and_keeps_live_pin() {
        let current = Settings {
            always_on_top: true,
            ..Settings::default()
        };
        let d = SettingsDraft {
            plan: "  Pro ".into(),
            interval: "2s".into(),
            show_all: true,
            cookie_file: " C:/p/cookies.sqlite ".into(),
            client_sha: " abcdef123\n".into(),
            ..draft()
        };
        assert_eq!(
            build_settings(&d, &current).unwrap(),
            Settings {
                plan: "Pro".into(),
                interval_secs: MIN_INTERVAL_SECS,
                show_all: true,
                always_on_top: true,
                cookie_file: Some(PathBuf::from("C:/p/cookies.sqlite")),
                client_sha: Some("abcdef123".into()),
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
        // ...then Details was toggled, the window moved and the pin flipped
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
    fn build_blank_optionals_become_none() {
        let current = Settings {
            cookie_file: Some(PathBuf::from("c.sqlite")),
            client_sha: Some("abcdef123".into()),
            ..Settings::default()
        };
        let d = SettingsDraft {
            cookie_file: "   ".into(),
            client_sha: "".into(),
            ..SettingsDraft::from_settings(&current)
        };
        let s = build_settings(&d, &current).unwrap();
        assert_eq!(s.cookie_file, None);
        assert_eq!(s.client_sha, None);

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
                cookie_file: Some(PathBuf::from("c.sqlite")),
                ..old.clone()
            },
            Settings {
                client_sha: Some("abcdef123".into()),
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
