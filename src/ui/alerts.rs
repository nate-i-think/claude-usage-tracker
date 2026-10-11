//! Threshold alerts glue: after each new poll, decide which alerts fire
//! ([`crate::analytics::alerts::evaluate`]) and hand their copy to the
//! notifier thread ([`crate::ui::notify`]). The edge state lives in
//! [`Settings::alert_state`], so the app's usual settings sync persists it
//! and a restart does not repeat an alert.

use std::sync::mpsc::Sender;

use chrono::{DateTime, Utc};

use crate::analytics::alerts::{alert_copy, evaluate, AlertCopy, AlertState, AlertToggles};
use crate::analytics::{compute_rate, projection, Projection};
use crate::config::Settings;
use crate::poller::Dashboard;

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
    /// `fetched_at` of the last poll checked.
    last_poll: Option<DateTime<Utc>>,
}

impl AlertHook {
    pub fn new(toasts: Sender<AlertCopy>) -> Self {
        Self {
            toasts,
            last_poll: None,
        }
    }

    /// Check `data` if it is a poll not seen yet: fired alerts go to the
    /// notifier, the new edge state into `settings` (persisted by the app's
    /// settings sync since it now differs from what was saved). Never
    /// blocks.
    pub fn after_poll(
        &mut self,
        data: Option<&Dashboard>,
        settings: &mut Settings,
        now: DateTime<Utc>,
    ) {
        let Some(d) = data else { return };
        if self.last_poll == Some(d.snapshot.fetched_at) {
            return;
        }
        self.last_poll = Some(d.snapshot.fetched_at);
        for copy in alerts_for(d, &mut settings.alert_state, settings.alerts, now) {
            // The notifier thread gone (it never exits on its own): nothing
            // to deliver to.
            let _ = self.toasts.send(copy);
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
    use crate::model::{Snapshot, UsageWindow};
    use chrono::{Duration, TimeZone};
    use std::sync::mpsc::{channel, Receiver};

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
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            daily_history: Vec::new(),
            fable_daily: Vec::new(),
            burners: Vec::new(),
            window_start: resets_at - Duration::hours(5),
            window_end: resets_at,
            window_samples: Vec::new(),
            persist_error: None,
        }
    }

    fn titles(rx: &Receiver<AlertCopy>) -> Vec<String> {
        rx.try_iter().map(|c| c.title).collect()
    }

    fn run(hook: &mut AlertHook, settings: &mut Settings, d: &Dashboard) {
        hook.after_poll(Some(d), settings, d.snapshot.fetched_at);
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
        assert_ne!(settings.alert_state, AlertState::default());
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
        settings.alert_state = AlertState::default();
        run(&mut hook, &mut settings, &d);
        assert_eq!(titles(&rx), ["5-hour usage at 80%"]);
        hook.after_poll(None, &mut settings, t0());
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
    fn unknown_reset_fires_nothing() {
        let (tx, rx) = channel();
        let mut hook = AlertHook::new(tx);
        let mut settings = Settings::default();
        let mut d = poll(1, 95.0, 0);
        d.snapshot.windows[0].resets_at = None;
        run(&mut hook, &mut settings, &d);
        assert!(titles(&rx).is_empty());
        assert_eq!(settings.alert_state, AlertState::default());
    }

    #[test]
    fn test_notification_goes_through_the_same_channel() {
        let (tx, rx) = channel();
        let hook = AlertHook::new(tx);
        hook.send_test();
        assert_eq!(titles(&rx), [test_copy().title]);
    }
}
