//! Threshold alerts glue: after each new poll, decide which alerts fire
//! ([`crate::analytics::alerts::evaluate`]) and hand their copy to the
//! notifier thread ([`crate::ui::notify`]). The edge state lives in
//! [`Settings::alert_states`], keyed by profile id, so the app's usual
//! settings sync persists it and a restart does not repeat an alert. Every
//! account with fresh data is checked on its own; with more than one
//! account a toast names the one it is about.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use chrono::{DateTime, Utc};

use crate::analytics::alerts::{alert_copy, evaluate, AlertCopy, AlertState, AlertToggles};
use crate::analytics::{compute_rate, projection, Projection};
use crate::config::Settings;
use crate::poller::Dashboard;
use crate::profiles::ProfileId;

use super::accounts::AccountView;

/// The copy of the settings panel's "Send test notification".
pub fn test_copy() -> AlertCopy {
    AlertCopy {
        title: "Test notification".to_owned(),
        body: "Usage alerts from Claude Usage Monitor look like this.".to_owned(),
    }
}

/// The toasts one poll result asks for, recording them in `state`.
pub fn alerts_for(
    d: &Dashboard,
    state: &mut AlertState,
    toggles: AlertToggles,
    now: DateTime<Utc>,
) -> Vec<AlertCopy> {
    let key = d.snapshot.five_hour_resets_at();
    let pct = d.snapshot.five_hour_pct();
    let proj = match pct {
        Some(p) => projection(now, p, compute_rate(&d.history, now), d.window_end),
        None => Projection {
            end_t: now,
            end_pct: 0.0,
            cap_at: None,
        },
    };
    let fired = evaluate(state, key, pct, &proj, now, toggles);
    let resets_at = key.unwrap_or(d.window_end);
    fired
        .into_iter()
        .map(|kind| alert_copy(kind, resets_at, now))
        .collect()
}

/// Runs the alert check once per poll result and sends what fires.
pub struct AlertHook {
    toasts: Sender<AlertCopy>,
    /// Per account, `fetched_at` of the last poll checked.
    last_poll: HashMap<ProfileId, DateTime<Utc>>,
}

impl AlertHook {
    pub fn new(toasts: Sender<AlertCopy>) -> Self {
        Self {
            toasts,
            last_poll: HashMap::new(),
        }
    }

    /// Check each account's data that is a poll not seen yet: fired alerts
    /// go to the notifier (named after the account when there are several),
    /// the new edge state into `settings` (persisted by the app's settings
    /// sync since it now differs from what was saved). Stale accounts (the
    /// latest poll failed) are skipped: their figures are old. Never blocks.
    pub fn after_poll(
        &mut self,
        accounts: &[AccountView],
        settings: &mut Settings,
        now: DateTime<Utc>,
    ) {
        let named = accounts.len() >= 2;
        for a in accounts {
            let Some(d) = a.data.filter(|_| !a.stale) else {
                continue;
            };
            if self.last_poll.get(a.id) == Some(&d.snapshot.fetched_at) {
                continue;
            }
            self.last_poll.insert(a.id.clone(), d.snapshot.fetched_at);
            let toggles = settings.alerts;
            let state = settings
                .alert_states
                .entry(a.id.as_str().to_owned())
                .or_default();
            for copy in alerts_for(d, state, toggles, now) {
                let copy = if named {
                    copy.for_account(a.label)
                } else {
                    copy
                };
                // The notifier thread gone (it never exits on its own):
                // nothing to deliver to.
                let _ = self.toasts.send(copy);
            }
        }
    }

    /// A sample toast through the same path as real alerts.
    pub fn send_test(&self) {
        let _ = self.toasts.send(test_copy());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::ErrorKind;
    use crate::model::{Snapshot, UsageWindow};
    use crate::poller::FetchErrorInfo;
    use crate::profiles::DEFAULT_ID;
    use chrono::{Duration, TimeZone};
    use std::sync::mpsc::{channel, Receiver};

    /// The default profile's alert state; absent reads as fresh.
    fn default_state(s: &Settings) -> AlertState {
        s.alert_states.get(DEFAULT_ID).cloned().unwrap_or_default()
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    /// A poll `minutes` after t0 at `pct`, in the window resetting at 15:00
    /// (plus `jitter_ms`, as the API does).
    fn poll(minutes: i64, pct: f64, jitter_ms: i64) -> Dashboard {
        let fetched_at = t0() + Duration::minutes(minutes);
        let resets_at = Utc.with_ymd_and_hms(2026, 10, 11, 15, 0, 0).unwrap()
            + Duration::milliseconds(jitter_ms);
        Dashboard {
            snapshot: Snapshot {
                fetched_at,
                windows: vec![UsageWindow {
                    label: "5-hour".to_owned(),
                    utilization: Some(pct),
                    resets_at: Some(resets_at),
                }],
                extra: None,
                breakdown: None,
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            seven_day_recent: Vec::new(),
            fable_recent: Vec::new(),
            burners: Vec::new(),
            window_start: resets_at - Duration::hours(5),
            window_end: resets_at,
            window_samples: Vec::new(),
            week_start: resets_at - Duration::days(7),
            week_end: resets_at,
            week_samples: Vec::new(),
            persist_error: None,
            plan: None,
            token_expires_at: None,
        }
    }

    fn titles(rx: &Receiver<AlertCopy>) -> Vec<String> {
        rx.try_iter().map(|c| c.title).collect()
    }

    fn view<'a>(id: &'a ProfileId, label: &'a str, d: Option<&'a Dashboard>) -> AccountView<'a> {
        AccountView {
            id,
            label,
            color: eframe::egui::Color32::WHITE,
            data: d,
            error: None,
            stale: false,
        }
    }

    fn run(hook: &mut AlertHook, settings: &mut Settings, d: &Dashboard) {
        let id = ProfileId::new(DEFAULT_ID);
        hook.after_poll(&[view(&id, "me", Some(d))], settings, d.snapshot.fetched_at);
    }

    #[test]
    fn polls_60_76_91_fire_75_then_90_once() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        run(&mut hook, &mut settings, &poll(0, 60.0, 0));
        assert!(titles(&rx).is_empty());
        run(&mut hook, &mut settings, &poll(1, 76.0, 107));
        assert_eq!(titles(&rx), ["5-hour usage at 76%"]);
        run(&mut hook, &mut settings, &poll(2, 91.0, -400));
        assert_eq!(titles(&rx), ["5-hour usage at 91%"]);
        run(&mut hook, &mut settings, &poll(3, 93.0, 2_000));
        assert!(titles(&rx).is_empty(), "each edge fires once");
        assert_ne!(default_state(&settings), AlertState::default());
    }

    #[test]
    fn the_same_poll_is_checked_once() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        let d = poll(1, 80.0, 0);
        run(&mut hook, &mut settings, &d);
        // Later passes before the next poll: no re-check (the state would
        // stop it anyway; this keeps logic() cheap).
        settings.alert_states.clear();
        run(&mut hook, &mut settings, &d);
        assert_eq!(titles(&rx), ["5-hour usage at 80%"]);
        hook.after_poll(&[], &mut settings, t0());
        let id = ProfileId::new(DEFAULT_ID);
        hook.after_poll(&[view(&id, "me", None)], &mut settings, t0());
        assert!(titles(&rx).is_empty());
    }

    #[test]
    fn restart_with_persisted_state_does_not_refire() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        run(&mut hook, &mut settings, &poll(1, 76.0, 0));
        assert_eq!(titles(&rx).len(), 1);

        // Saved and restored (the app persists Settings with serde).
        let saved = serde_json::to_string(&settings).unwrap();
        let mut restored: Settings = serde_json::from_str(&saved).unwrap();
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        run(&mut hook, &mut restored, &poll(30, 78.0, 3_000));
        assert!(titles(&rx).is_empty(), "75% already delivered this window");
        run(&mut hook, &mut restored, &poll(31, 90.0, 0));
        assert_eq!(titles(&rx), ["5-hour usage at 90%"]);
    }

    #[test]
    fn toggles_off_send_nothing() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings {
            alerts: AlertToggles {
                crossed_75: false,
                crossed_90: false,
                projected_cap: false,
            },
            ..Settings::default()
        };
        for (i, pct) in [60.0, 76.0, 91.0].into_iter().enumerate() {
            run(&mut hook, &mut settings, &poll(i as i64, pct, 0));
        }
        assert!(titles(&rx).is_empty());
    }

    #[test]
    fn projected_cap_fires_from_the_burn_rate() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        // 1%/min over the last 15 minutes from 30% at 12:15: the cap lands
        // at 13:25, before the 15:00 reset.
        let mut d = poll(15, 30.0, 0);
        d.history = (0..=15)
            .map(|m| (t0() + Duration::minutes(m), Some(15.0 + m as f64)))
            .collect();
        run(&mut hook, &mut settings, &d);
        let got = titles(&rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].starts_with("On pace to hit the 5-hour cap at "));
    }

    #[test]
    fn idle_window_between_windows_keeps_the_key_and_the_next_fires_fresh() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        run(&mut hook, &mut settings, &poll(1, 91.0, 0));
        assert_eq!(titles(&rx), ["5-hour usage at 91%"]);
        let consumed = default_state(&settings);
        // 15:00 reset: idle polls report 0% with no reset (a null key).
        for m in [190, 200, 230] {
            let mut idle = poll(m, 0.0, 0);
            idle.snapshot.windows[0].resets_at = None;
            idle.window_start = idle.snapshot.fetched_at - Duration::hours(5);
            idle.window_end = idle.snapshot.fetched_at;
            run(&mut hook, &mut settings, &idle);
        }
        assert!(titles(&rx).is_empty(), "nothing fires while idle");
        assert_eq!(
            default_state(&settings),
            consumed,
            "the null window keys nothing"
        );
        // The next window (resetting 20:00) crosses 75%: a fresh window.
        let mut next = poll(400, 76.0, 0);
        next.snapshot.windows[0].resets_at = Some(t0() + Duration::hours(8));
        run(&mut hook, &mut settings, &next);
        assert_eq!(titles(&rx), ["5-hour usage at 76%"]);
    }

    #[test]
    fn unknown_reset_fires_nothing() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        let mut d = poll(1, 95.0, 0);
        d.snapshot.windows[0].resets_at = None;
        run(&mut hook, &mut settings, &d);
        assert!(titles(&rx).is_empty());
        assert_eq!(default_state(&settings), AlertState::default());
    }

    #[test]
    fn each_account_alerts_on_its_own_and_is_named() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        let (a, b) = (ProfileId::new(DEFAULT_ID), ProfileId::new("claude-b"));
        let (a76, b60) = (poll(1, 76.0, 0), poll(1, 60.0, 0));
        let both = |da, db| [view(&a, "me", Some(da)), view(&b, "work", Some(db))];
        hook.after_poll(&both(&a76, &b60), &mut settings, t0());
        assert_eq!(titles(&rx), ["me — 5-hour usage at 76%"]);
        // Same fetched_at for both: each account remembers its own poll.
        let (a77, b80) = (poll(2, 77.0, 0), poll(2, 80.0, 0));
        hook.after_poll(&both(&a77, &b80), &mut settings, t0());
        assert_eq!(titles(&rx), ["work — 5-hour usage at 80%"]);
        assert_ne!(settings.alert_states["claude-b"], AlertState::default());
        assert_ne!(settings.alert_states[DEFAULT_ID], AlertState::default());
        // One account alone: no name.
        let b91 = poll(3, 91.0, 0);
        hook.after_poll(&[view(&b, "work", Some(&b91))], &mut settings, t0());
        assert_eq!(titles(&rx), ["5-hour usage at 91%"]);
    }

    #[test]
    fn stale_accounts_do_not_alert() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        let id = ProfileId::new(DEFAULT_ID);
        let d = poll(1, 95.0, 0);
        let error = FetchErrorInfo {
            kind: ErrorKind::Transport,
            message: "down".into(),
        };
        let stale = AccountView {
            error: Some(&error),
            stale: true,
            ..view(&id, "me", Some(&d))
        };
        hook.after_poll(&[stale], &mut settings, t0());
        assert!(titles(&rx).is_empty());
        assert!(settings.alert_states.is_empty());
        // Fresh again with the same data: checked now.
        hook.after_poll(&[view(&id, "me", Some(&d))], &mut settings, t0());
        assert_eq!(titles(&rx), ["5-hour usage at 95%"]);
    }

    #[test]
    fn test_notification_goes_through_the_same_channel() {
        let (tx, rx) = channel();
        let hook = AlertHook::new(tx);
        hook.send_test();
        assert_eq!(titles(&rx), [test_copy().title]);
    }
}
