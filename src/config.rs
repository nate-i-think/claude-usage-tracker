//! User settings and poll-interval parsing.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::analytics::alerts::{AlertState, AlertToggles};

/// Smallest poll interval the app accepts, in seconds.
pub const MIN_INTERVAL_SECS: u64 = 5;

/// Persisted user settings. Missing fields take their [`Default`] values,
/// so an `app.ron` written by an older build still loads; fields it has
/// that were since removed (`cookie_file`, `client_sha`, `alert_state`) are
/// ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Plan label shown in the UI.
    pub plan: String,
    pub interval_secs: u64,
    pub show_all: bool,
    pub always_on_top: bool,
    /// The window mode to open in.
    pub ui_mode: UiMode,
    /// Last normal (not maximized) full-mode window rect. Units: see
    /// `ui::window`.
    pub full_rect: Option<SavedRect>,
    /// The full-mode window was maximized.
    pub full_maximized: bool,
    /// Last pill-mode window position (outer).
    pub pill_pos: Option<Pos>,
    /// Show the tray icon.
    pub tray_enabled: bool,
    /// The close button hides to the tray; only effective with
    /// [`Self::tray_enabled`].
    pub close_to_tray: bool,
    /// Open in pill mode regardless of the last mode.
    pub start_in_pill: bool,
    /// Which threshold alerts to deliver.
    pub alerts: AlertToggles,
    /// Alerts already delivered in the current 5-hour window, per profile id,
    /// persisted so a restart does not repeat them.
    pub alert_states: BTreeMap<String, AlertState>,
    /// The Details section is expanded.
    pub details_open: bool,
    /// Skip animations.
    pub reduce_motion: bool,
    /// Keep the high-usage pulse going instead of settling.
    pub keep_pulsing: bool,
    /// Claude Code profiles in display order: discovered ones merged in
    /// (see `profiles::merge_discovered`) plus any added by hand.
    pub profiles: Vec<ProfileSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            plan: "Max".to_owned(),
            interval_secs: 60,
            show_all: false,
            always_on_top: false,
            ui_mode: UiMode::Full,
            full_rect: None,
            full_maximized: false,
            pill_pos: None,
            tray_enabled: true,
            close_to_tray: true,
            start_in_pill: false,
            alerts: AlertToggles::default(),
            alert_states: BTreeMap::new(),
            details_open: false,
            reduce_motion: false,
            keep_pulsing: false,
            profiles: Vec::new(),
        }
    }
}

impl Settings {
    /// Whether the close button should hide to the tray instead of quitting.
    pub fn closes_to_tray(&self) -> bool {
        self.tray_enabled && self.close_to_tray
    }
}

/// One saved Claude Code profile. Missing fields take their [`Default`]
/// values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileSettings {
    /// Stable slug (`default`, `claude-secondary`); keys history and alerts.
    pub id: String,
    /// The Claude Code config directory (`~/.claude`, `~/.claude-secondary`).
    pub config_dir: PathBuf,
    /// User-chosen label; `None` derives one from the account.
    pub label: Option<String>,
    /// Series color index; `None` uses the profile's position.
    pub color: Option<u8>,
    /// `false` hides a discovered profile without forgetting it.
    pub enabled: bool,
    /// Keychain service override; `None` derives it from the config dir.
    pub keychain_service: Option<String>,
    /// Added by hand in Settings rather than discovered; only these can be
    /// forgotten (a discovered one would come straight back).
    pub manual: bool,
}

impl Default for ProfileSettings {
    fn default() -> Self {
        Self {
            id: String::new(),
            config_dir: PathBuf::new(),
            label: None,
            color: None,
            enabled: true,
            keychain_service: None,
            manual: false,
        }
    }
}

/// The window's layout: the full dashboard or the compact pill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiMode {
    #[default]
    Full,
    Pill,
}

/// A window's outer top-left corner in physical desktop pixels (see
/// `ui::window` for why pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    pub x: i32,
    pub y: i32,
}

/// A saved window: outer position in physical pixels, inner (client) size
/// in egui points (see `ui::window`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SavedRect {
    pub pos: Pos,
    pub w: f32,
    pub h: f32,
}

/// Parse `"30"`, `"30s"`, `"2m"`, `"1h"` into seconds.
pub fn parse_interval(s: &str) -> Result<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        bail!("empty interval");
    }
    let (digits, multiplier) = match s.as_bytes()[s.len() - 1] {
        b's' => (&s[..s.len() - 1], 1),
        b'm' => (&s[..s.len() - 1], 60),
        b'h' => (&s[..s.len() - 1], 3600),
        _ => (s.as_str(), 1),
    };
    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid interval {s:?}"))?;
    n.checked_mul(multiplier)
        .with_context(|| format!("interval {s:?} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_values() {
        let s = Settings::default();
        assert_eq!(s.plan, "Max");
        assert_eq!(s.interval_secs, 60);
        assert!(!s.show_all);
        assert!(!s.always_on_top);
        assert_eq!(s.ui_mode, UiMode::Full);
        assert_eq!(s.full_rect, None);
        assert!(!s.full_maximized);
        assert_eq!(s.pill_pos, None);
        assert!(s.tray_enabled && s.close_to_tray && s.closes_to_tray());
        assert!(!s.start_in_pill);
        assert_eq!(s.alerts, AlertToggles::default());
        assert!(s.alerts.crossed_75 && s.alerts.crossed_90 && s.alerts.projected_cap);
        assert!(s.alert_states.is_empty());
        assert!(!s.details_open && !s.reduce_motion && !s.keep_pulsing);
        assert!(s.profiles.is_empty());
    }

    #[test]
    fn close_to_tray_needs_the_tray() {
        let s = Settings {
            tray_enabled: false,
            ..Settings::default()
        };
        assert!(s.close_to_tray && !s.closes_to_tray());
    }

    /// The exact RON eframe stored for the pre-redesign Settings (as
    /// `eframe::set_value` writes it) still loads; the removed cookie and
    /// SHA fields are ignored and new fields defaulted.
    #[test]
    fn settings_old_app_ron_value_loads_with_defaults() {
        let old = r#"(plan:"Max 5x",interval_secs:120,show_all:true,always_on_top:true,cookie_file:Some("C:/x/cookies.sqlite"),client_sha:None)"#;
        let s: Settings = ron::from_str(old).unwrap();
        assert_eq!(
            s,
            Settings {
                plan: "Max 5x".into(),
                interval_secs: 120,
                show_all: true,
                always_on_top: true,
                ..Settings::default()
            }
        );
    }

    /// The full Settings RON of the last cookie-era build (e9dff54), with
    /// `cookie_file`, `client_sha` and a populated `alert_state`, loads; the
    /// kept fields survive and the removed ones are dropped.
    #[test]
    fn settings_cookie_era_app_ron_loads() {
        let old = r#"(plan:"Max",interval_secs:90,show_all:false,always_on_top:false,cookie_file:Some("/Users/x/Library/Application Support/Firefox/Profiles/a.default/cookies.sqlite"),client_sha:Some("abcdef1234567890"),ui_mode:Pill,full_rect:Some((pos:(x:-1200,y:40),w:430.0,h:470.25)),full_maximized:true,pill_pos:Some((x:10,y:20)),tray_enabled:true,close_to_tray:false,start_in_pill:false,alerts:(crossed_75:true,crossed_90:false,projected_cap:true),alert_state:(window:Some("2026-10-01T10:00:00Z"),crossed_75:true,crossed_90:false,projected_cap:false),details_open:true,reduce_motion:false,keep_pulsing:true)"#;
        let s: Settings = ron::from_str(old).unwrap();
        assert_eq!(
            s,
            Settings {
                plan: "Max".into(),
                interval_secs: 90,
                ui_mode: UiMode::Pill,
                full_rect: Some(SavedRect {
                    pos: Pos { x: -1200, y: 40 },
                    w: 430.0,
                    h: 470.25,
                }),
                full_maximized: true,
                pill_pos: Some(Pos { x: 10, y: 20 }),
                close_to_tray: false,
                alerts: AlertToggles {
                    crossed_90: false,
                    ..AlertToggles::default()
                },
                details_open: true,
                keep_pulsing: true,
                ..Settings::default()
            }
        );
        assert!(s.alert_states.is_empty());
    }

    #[test]
    fn settings_ron_round_trip_with_new_fields() {
        let s = Settings {
            ui_mode: UiMode::Pill,
            full_rect: Some(SavedRect {
                pos: Pos { x: -1200, y: 40 },
                w: 430.0,
                h: 470.25,
            }),
            full_maximized: true,
            pill_pos: Some(Pos { x: 10, y: 20 }),
            tray_enabled: false,
            start_in_pill: true,
            alerts: AlertToggles {
                crossed_90: false,
                ..AlertToggles::default()
            },
            details_open: true,
            reduce_motion: true,
            keep_pulsing: true,
            ..Settings::default()
        };
        let text = ron::ser::to_string(&s).unwrap();
        assert_eq!(ron::from_str::<Settings>(&text).unwrap(), s);
    }

    #[test]
    fn settings_ron_profiles_round_trip_and_default_enabled() {
        let s = Settings {
            profiles: vec![
                ProfileSettings {
                    id: "default".into(),
                    config_dir: PathBuf::from("/h/.claude"),
                    ..ProfileSettings::default()
                },
                ProfileSettings {
                    id: "claude-secondary".into(),
                    config_dir: PathBuf::from("/h/.claude-secondary"),
                    label: Some("work".into()),
                    color: Some(3),
                    enabled: false,
                    keychain_service: Some("svc".into()),
                    manual: true,
                },
            ],
            ..Settings::default()
        };
        let text = ron::ser::to_string(&s).unwrap();
        assert_eq!(ron::from_str::<Settings>(&text).unwrap(), s);

        let p: ProfileSettings = ron::from_str(r#"(id:"x",config_dir:"/d")"#).unwrap();
        assert!(p.enabled, "enabled defaults to true");
    }

    #[test]
    fn settings_deserialize_empty_object_is_default() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn settings_deserialize_partial_keeps_other_defaults() {
        let s: Settings =
            serde_json::from_str(r#"{"interval_secs": 30, "cookie_file": "c.sqlite", "client_sha": "abcdef123", "alert_state": {"window": null}, "extra": 1}"#)
                .unwrap();
        assert_eq!(
            s,
            Settings {
                interval_secs: 30,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn settings_round_trip() {
        let s = Settings {
            plan: "Max 5x".into(),
            alert_states: BTreeMap::from([("default".to_owned(), AlertState::default())]),
            ..Settings::default()
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn parse_interval_units() {
        assert_eq!(parse_interval("30").unwrap(), 30);
        assert_eq!(parse_interval("30s").unwrap(), 30);
        assert_eq!(parse_interval("2m").unwrap(), 120);
        assert_eq!(parse_interval("1h").unwrap(), 3600);
        assert_eq!(parse_interval("  2M \n").unwrap(), 120);
        assert_eq!(parse_interval("0").unwrap(), 0);
    }

    #[test]
    fn parse_interval_rejects_bad_input() {
        for bad in [
            "",
            "   ",
            "s",
            "m",
            "abc",
            "1x",
            "1.5m",
            "-5",
            "10d",
            "99999999999999999999h",
        ] {
            assert!(parse_interval(bad).is_err(), "{bad:?} should fail");
        }
    }
}
