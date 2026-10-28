//! The accounts the dashboard shows: one per polled Claude Code profile,
//! each with its latest poll results. Pure state built from
//! [`PollEvent`]s; no I/O and no painting.

use chrono::{DateTime, Utc};
use eframe::egui::Color32;

use crate::config::Settings;
use crate::fetch::ErrorKind;
use crate::poller::{Dashboard, FetchErrorInfo, PollEvent, PollOutcome};
use crate::profiles::{self, Discovered, Profile, ProfileId};

use super::theme::Palette;

/// One account's latest poll results.
#[derive(Debug, Default)]
pub struct PollState {
    /// The last successful poll; kept across failures.
    pub data: Option<Box<Dashboard>>,
    /// The most recent failure, cleared by the next success.
    pub error: Option<FetchErrorInfo>,
}

impl PollState {
    /// Fold one poll in: success replaces the data and clears the error;
    /// failure keeps the last good data and records the error.
    pub fn apply(&mut self, outcome: PollOutcome) {
        match outcome {
            PollOutcome::Ok(dashboard) => {
                self.data = Some(dashboard);
                self.error = None;
            }
            PollOutcome::Err(info) => self.error = Some(info),
        }
    }

    /// Showing data from before a failed poll.
    pub fn stale(&self) -> bool {
        self.error.is_some() && self.data.is_some()
    }
}

/// A polled profile and its results.
#[derive(Debug)]
pub struct AccountState {
    pub profile: Profile,
    pub poll: PollState,
}

/// Every polled account, in display order.
#[derive(Debug, Default)]
pub struct Accounts {
    list: Vec<AccountState>,
    /// The poller has said which profiles it polls (possibly none).
    known: bool,
}

impl Accounts {
    /// Fold one poller event in. A new profile list keeps each remaining
    /// account's results and merges newly seen profiles into
    /// `settings.profiles` (so they persist); returns whether that changed
    /// the settings.
    pub fn apply(&mut self, event: PollEvent, settings: &mut Settings) -> bool {
        match event {
            PollEvent::Profiles(profiles) => {
                let mut old = std::mem::take(&mut self.list);
                self.list = profiles
                    .into_iter()
                    .map(|profile| {
                        let poll = old
                            .iter()
                            .position(|a| a.profile.id == profile.id)
                            .map(|i| old.remove(i).poll)
                            .unwrap_or_default();
                        AccountState { profile, poll }
                    })
                    .collect();
                self.known = true;
                let found: Vec<Discovered> = self
                    .list
                    .iter()
                    .map(|a| Discovered {
                        id: a.profile.id.clone(),
                        config_dir: a.profile.config_dir.clone(),
                        config_dir_env: a.profile.config_dir_env.clone(),
                        account: a.profile.account.clone(),
                    })
                    .collect();
                profiles::merge_discovered(&mut settings.profiles, &found)
            }
            PollEvent::Profile { id, outcome } => {
                if let Some(a) = self.list.iter_mut().find(|a| a.profile.id == id) {
                    a.poll.apply(outcome);
                }
                false
            }
        }
    }

    pub fn list(&self) -> &[AccountState] {
        &self.list
    }

    /// The poller has reported its profile list.
    pub fn known(&self) -> bool {
        self.known
    }

    /// Some account has had a successful poll.
    pub fn any_data(&self) -> bool {
        self.list.iter().any(|a| a.poll.data.is_some())
    }

    /// Some account's latest poll failed.
    pub fn any_error(&self) -> bool {
        self.list.iter().any(|a| a.poll.error.is_some())
    }

    /// The account the single-account views show: the first with data,
    /// else the first.
    pub fn primary(&self) -> Option<&AccountState> {
        self.list
            .iter()
            .find(|a| a.poll.data.is_some())
            .or(self.list.first())
    }

    /// Every account as the views see it.
    pub fn views(&self, palette: &Palette) -> Vec<AccountView<'_>> {
        self.list
            .iter()
            .map(|a| AccountView {
                id: &a.profile.id,
                label: &a.profile.label,
                color: palette.series(a.profile.color),
                data: a.poll.data.as_deref(),
                error: a.poll.error.as_ref(),
                stale: a.poll.stale(),
            })
            .collect()
    }
}

/// One account, borrowed for drawing.
#[derive(Debug, Clone, Copy)]
pub struct AccountView<'a> {
    pub id: &'a ProfileId,
    pub label: &'a str,
    pub color: Color32,
    pub data: Option<&'a Dashboard>,
    pub error: Option<&'a FetchErrorInfo>,
    pub stale: bool,
}

/// The account with the most 5-hour headroom at `now`: the lowest live
/// 5-hour %, fresh accounts before stale ones. `None` without data.
pub fn best_headroom(views: &[AccountView], now: DateTime<Utc>) -> Option<usize> {
    views
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let pct = v.data?.five_hour_now(now).pct.unwrap_or(0.0);
            Some((v.stale, pct, i))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
        .map(|(_, _, i)| i)
}

/// A banner title's few words for an error: "token expired", ....
pub fn short_error(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::TokenExpired | ErrorKind::Unauthorized => "token expired",
        ErrorKind::NoCredentials => "not signed in",
        ErrorKind::Forbidden => "access denied",
        ErrorKind::RateLimited => "rate limited",
        ErrorKind::Transport => "network error",
        ErrorKind::Status | ErrorKind::Json => "usage endpoint error",
        ErrorKind::Internal => "internal error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProfileSettings;
    use crate::model::{Snapshot, UsageWindow};
    use crate::profiles::AccountInfo;
    use std::path::PathBuf;

    fn t() -> DateTime<Utc> {
        DateTime::from_timestamp(1_791_000_000, 0).unwrap()
    }

    fn profile(id: &str) -> Profile {
        Profile {
            id: ProfileId::new(id),
            config_dir: PathBuf::from(format!("/h/.{id}")),
            config_dir_env: (id != "default").then(|| format!("/h/.{id}")),
            label: format!("{id}-label"),
            color: 0,
            keychain_override: None,
            account: Some(AccountInfo {
                account_uuid: Some(format!("uuid-{id}")),
                ..AccountInfo::default()
            }),
        }
    }

    fn dashboard(pct: f64) -> Box<Dashboard> {
        Box::new(Dashboard {
            snapshot: Snapshot {
                fetched_at: t(),
                windows: vec![UsageWindow {
                    label: "5-hour".to_owned(),
                    utilization: Some(pct),
                    resets_at: Some(t() + chrono::Duration::hours(2)),
                }],
                extra: None,
                breakdown: None,
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            seven_day_recent: Vec::new(),
            fable_recent: Vec::new(),
            burners: Vec::new(),
            window_start: t() - chrono::Duration::hours(3),
            window_end: t() + chrono::Duration::hours(2),
            window_samples: Vec::new(),
            week_start: t(),
            week_end: t(),
            week_samples: Vec::new(),
            persist_error: None,
            plan: None,
            token_expires_at: None,
        })
    }

    fn ok(id: &str, pct: f64) -> PollEvent {
        PollEvent::Profile {
            id: ProfileId::new(id),
            outcome: PollOutcome::Ok(dashboard(pct)),
        }
    }

    fn err(id: &str, message: &str) -> PollEvent {
        PollEvent::Profile {
            id: ProfileId::new(id),
            outcome: PollOutcome::Err(FetchErrorInfo {
                kind: ErrorKind::Status,
                message: message.to_owned(),
            }),
        }
    }

    fn pct(a: &AccountState) -> Option<f64> {
        a.poll.data.as_ref()?.snapshot.five_hour_pct()
    }

    #[test]
    fn error_keeps_last_good_data_and_success_clears_error() {
        let mut state = PollState::default();
        state.apply(PollOutcome::Err(FetchErrorInfo {
            kind: ErrorKind::Transport,
            message: "request failed: timed out".into(),
        }));
        assert!(state.data.is_none() && !state.stale());
        assert_eq!(
            state.error.as_ref().unwrap().message,
            "request failed: timed out"
        );
        state.apply(PollOutcome::Ok(dashboard(10.0)));
        assert!(state.error.is_none() && !state.stale());
        state.apply(PollOutcome::Err(FetchErrorInfo {
            kind: ErrorKind::Status,
            message: "HTTP 500: oops".into(),
        }));
        assert_eq!(
            state.data.as_ref().unwrap().snapshot.five_hour_pct(),
            Some(10.0)
        );
        assert!(state.stale());
        state.apply(PollOutcome::Ok(dashboard(20.0)));
        assert!(!state.stale());
    }

    #[test]
    fn apply_tracks_each_account_and_merges_profiles_into_settings() {
        let mut accounts = Accounts::default();
        let mut settings = Settings::default();
        assert!(!accounts.known() && accounts.primary().is_none());

        let changed = accounts.apply(
            PollEvent::Profiles(vec![profile("default"), profile("claude-b")]),
            &mut settings,
        );
        assert!(changed);
        let saved: Vec<_> = settings.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(saved, ["default", "claude-b"]);
        assert!(accounts.known() && !accounts.any_data());
        // No data yet: the first account.
        assert_eq!(accounts.primary().unwrap().profile.id.as_str(), "default");

        assert!(!accounts.apply(err("default", "boom"), &mut settings));
        assert!(!accounts.apply(ok("claude-b", 30.0), &mut settings));
        assert!(accounts.any_data() && accounts.any_error());
        // The first account with data.
        assert_eq!(accounts.primary().unwrap().profile.id.as_str(), "claude-b");

        // Stale: the error after good data keeps the data.
        accounts.apply(err("claude-b", "HTTP 500: oops"), &mut settings);
        let b = &accounts.list()[1];
        assert_eq!(pct(b), Some(30.0));
        assert!(b.poll.stale());
        let views = accounts.views(&crate::ui::theme::DARK);
        assert!(views[1].stale && views[1].data.is_some());
        assert_eq!(views[1].label, "claude-b-label");
        assert!(!views[0].stale && views[0].error.is_some());
        assert_eq!(views[0].color, crate::ui::theme::DARK.series(0));

        // The same list again: settings untouched, results kept.
        let again = accounts.apply(
            PollEvent::Profiles(vec![profile("claude-b"), profile("default")]),
            &mut settings,
        );
        assert!(!again);
        assert_eq!(accounts.list()[0].profile.id.as_str(), "claude-b");
        assert_eq!(pct(&accounts.list()[0]), Some(30.0));

        // Events for unknown profiles are ignored.
        assert!(!accounts.apply(ok("gone", 1.0), &mut settings));
        assert_eq!(accounts.list().len(), 2);
    }

    #[test]
    fn apply_never_re_enables_a_disabled_profile() {
        let mut settings = Settings {
            profiles: vec![ProfileSettings {
                id: "claude-b".into(),
                config_dir: PathBuf::from("/h/.claude-b"),
                enabled: false,
                ..ProfileSettings::default()
            }],
            ..Settings::default()
        };
        let mut accounts = Accounts::default();
        assert!(!accounts.apply(
            PollEvent::Profiles(vec![profile("claude-b")]),
            &mut settings
        ));
        assert!(!settings.profiles[0].enabled);
    }

    #[test]
    fn best_headroom_prefers_fresh_then_lowest() {
        let mut accounts = Accounts::default();
        let mut settings = Settings::default();
        accounts.apply(
            PollEvent::Profiles(vec![profile("a"), profile("b"), profile("c"), profile("d")]),
            &mut settings,
        );
        let now = t();
        assert_eq!(
            best_headroom(&accounts.views(&crate::ui::theme::DARK), now),
            None
        );
        accounts.apply(ok("a", 60.0), &mut settings);
        accounts.apply(ok("b", 20.0), &mut settings);
        accounts.apply(ok("c", 40.0), &mut settings);
        // "d" has no data: never picked.
        let views = accounts.views(&crate::ui::theme::DARK);
        assert_eq!(best_headroom(&views, now), Some(1));
        // "b" goes stale: the lowest fresh one wins over it.
        accounts.apply(err("b", "down"), &mut settings);
        let views = accounts.views(&crate::ui::theme::DARK);
        assert_eq!(best_headroom(&views, now), Some(2));
        // All stale: lowest again.
        accounts.apply(err("a", "down"), &mut settings);
        accounts.apply(err("c", "down"), &mut settings);
        let views = accounts.views(&crate::ui::theme::DARK);
        assert_eq!(best_headroom(&views, now), Some(1));
    }

    #[test]
    fn short_errors() {
        assert_eq!(short_error(ErrorKind::TokenExpired), "token expired");
        assert_eq!(short_error(ErrorKind::Unauthorized), "token expired");
        assert_eq!(short_error(ErrorKind::NoCredentials), "not signed in");
        assert_eq!(short_error(ErrorKind::RateLimited), "rate limited");
    }
}
