//! Background polling: fetch, record, persist, and precompute everything the
//! GUI renders, off the UI thread. No egui here; the GUI wakes itself via the
//! `notify` callback.
//!
//! Port of the claude_usage_tui.py main loop, `record()`, and the inputs to
//! `render()`.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};

use crate::analytics::{BURN_WINDOW_MINUTES, HISTORY_LEN};
use crate::config::{Settings, MIN_INTERVAL_SECS};
use crate::fetch::{self, FetchError};
use crate::model::Snapshot;
use crate::paths::{self, Paths};
use crate::storage::{self, InstanceUsage};

/// Days shown in the 7-day daily charts (Python `daily_history(days=7)`).
const DAILY_DAYS: usize = 7;

/// One timestamped utilization sample for burn-rate math.
pub type Sample = (DateTime<Utc>, Option<f64>);

/// Everything the GUI renders for one successful poll.
#[derive(Debug, Clone)]
pub struct Dashboard {
    pub snapshot: Snapshot,
    /// In-memory 5-hour samples, oldest first.
    pub history: Vec<Sample>,
    /// In-memory Fable weekly samples, oldest first.
    pub fable_history: Vec<Sample>,
    pub five_hour_buckets: Vec<Sample>,
    pub fable_buckets: Vec<Sample>,
    pub daily_history: Vec<(NaiveDate, Option<f64>)>,
    pub fable_daily: Vec<(NaiveDate, Option<f64>)>,
    pub burners: Vec<InstanceUsage>,
    pub window_start: DateTime<Utc>,
}

/// Control messages into the poller thread.
#[derive(Debug, Clone)]
pub enum PollMsg {
    RefreshNow,
    /// Replace the settings and poll immediately.
    UpdateSettings(Settings),
    Shutdown,
}

/// A failed poll, ready for display.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchErrorInfo {
    pub message: String,
    /// The error looks like a stale anthropic-client-sha.
    pub stale_sha_hint: bool,
}

impl From<&FetchError> for FetchErrorInfo {
    fn from(e: &FetchError) -> Self {
        Self {
            message: e.to_string(),
            stale_sha_hint: e.stale_sha_hint(),
        }
    }
}

/// Results out of the poller thread.
#[derive(Debug, Clone)]
pub enum PollEvent {
    Ok(Box<Dashboard>),
    Err(FetchErrorInfo),
}

/// Where usage snapshots come from. The HTTP client in production; a fake in
/// tests.
pub trait UsageSource: Send + 'static {
    fn fetch(&mut self, paths: &Paths, settings: &Settings) -> Result<Snapshot, FetchError>;
}

impl UsageSource for fetch::Client {
    fn fetch(&mut self, paths: &Paths, settings: &Settings) -> Result<Snapshot, FetchError> {
        self.fetch_usage(paths, settings)
    }
}

/// The HTTP client, built on first use and rebuilt after a build failure, so
/// a failure surfaces as a poll error instead of preventing the poller.
#[derive(Default)]
pub struct HttpSource {
    client: Option<fetch::Client>,
}

impl UsageSource for HttpSource {
    fn fetch(&mut self, paths: &Paths, settings: &Settings) -> Result<Snapshot, FetchError> {
        if self.client.is_none() {
            self.client = Some(fetch::Client::new()?);
        }
        self.client
            .as_mut()
            .expect("client just built")
            .fetch(paths, settings)
    }
}

/// Local session directories scanned for per-instance token attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDirs {
    pub claude_code: PathBuf,
    pub cowork: PathBuf,
}

impl Default for SessionDirs {
    fn default() -> Self {
        Self {
            claude_code: paths::claude_code_dir(),
            cowork: paths::cowork_dir(),
        }
    }
}

/// In-memory burn histories (5-hour and Fable), capped at [`HISTORY_LEN`].
#[derive(Debug, Clone, Default)]
pub struct BurnHistory {
    five_hour: VecDeque<Sample>,
    fable: VecDeque<Sample>,
}

impl BurnHistory {
    /// Seed from history.jsonl rows of the last [`BURN_WINDOW_MINUTES`], so
    /// the burn rate is available right after a restart.
    pub fn seeded(paths: &Paths, now: DateTime<Utc>) -> Self {
        let mut h = Self::default();
        let since = now - ChronoDuration::minutes(BURN_WINDOW_MINUTES);
        for e in storage::read_recent(paths, since) {
            let Some(t) = DateTime::from_timestamp_micros((e.ts * 1e6).round() as i64) else {
                continue;
            };
            h.push(t, e.five_hour, e.fable_7d);
        }
        h
    }

    /// Append this poll (Python `record()`).
    pub fn record(&mut self, snap: &Snapshot) {
        let fable = snap.window("Fable").and_then(|w| w.utilization);
        self.push(snap.fetched_at, snap.five_hour_pct(), fable);
    }

    fn push(&mut self, t: DateTime<Utc>, five_hour: Option<f64>, fable: Option<f64>) {
        for (q, v) in [(&mut self.five_hour, five_hour), (&mut self.fable, fable)] {
            if q.len() == HISTORY_LEN {
                q.pop_front();
            }
            q.push_back((t, v));
        }
    }

    pub fn five_hour(&self) -> Vec<Sample> {
        self.five_hour.iter().copied().collect()
    }

    pub fn fable(&self) -> Vec<Sample> {
        self.fable.iter().copied().collect()
    }
}

/// Start of the current five-hour window: `resets_at - 5h`, else `now - 5h`.
pub fn window_start(snap: &Snapshot, now: DateTime<Utc>) -> DateTime<Utc> {
    snap.five_hour_resets_at().unwrap_or(now) - ChronoDuration::hours(5)
}

/// The effective poll interval: the setting, at least [`MIN_INTERVAL_SECS`].
pub fn poll_interval(settings: &Settings) -> Duration {
    Duration::from_secs(settings.interval_secs.max(MIN_INTERVAL_SECS))
}

/// One poll: fetch, record into `history`, append to history.jsonl (which
/// also refreshes latest.json), and compute the dashboard. Shared by the
/// poller thread and the CLI `once` command.
pub fn poll_once(
    source: &mut dyn UsageSource,
    paths: &Paths,
    dirs: &SessionDirs,
    settings: &Settings,
    history: &mut BurnHistory,
) -> Result<Dashboard, FetchError> {
    let snap = source.fetch(paths, settings)?;
    history.record(&snap);
    // A disk problem must not hide fresh data; the next append retries.
    let _ = storage::append_snapshot(paths, &snap);
    Ok(build_dashboard(snap, history, paths, dirs, Utc::now()))
}

fn build_dashboard(
    snapshot: Snapshot,
    history: &BurnHistory,
    paths: &Paths,
    dirs: &SessionDirs,
    now: DateTime<Utc>,
) -> Dashboard {
    let ws = window_start(&snapshot, now);
    Dashboard {
        history: history.five_hour(),
        fable_history: history.fable(),
        five_hour_buckets: storage::five_hour_buckets(paths, ws, now),
        fable_buckets: storage::fable_five_hour_buckets(paths, ws, now),
        daily_history: storage::daily_history(paths, DAILY_DAYS, now),
        fable_daily: storage::fable_daily_history(paths, DAILY_DAYS, now),
        burners: storage::attribute_recent(&dirs.claude_code, &dirs.cowork, ws),
        window_start: ws,
        snapshot,
    }
}

/// Handle to the background poller thread. Dropping it shuts the thread down.
pub struct Poller {
    control: Sender<PollMsg>,
    events: Receiver<PollEvent>,
    handle: Option<JoinHandle<()>>,
}

impl Poller {
    /// Poll claude.ai over HTTP in the background. `notify` runs after each
    /// event is sent (the GUI requests a repaint; the CLI can pass a no-op).
    pub fn spawn(paths: Paths, settings: Settings, notify: impl Fn() + Send + 'static) -> Self {
        Self::spawn_with(
            HttpSource::default(),
            paths,
            SessionDirs::default(),
            settings,
            notify,
        )
    }

    /// [`Poller::spawn`] with an explicit source and session directories.
    pub fn spawn_with(
        source: impl UsageSource,
        paths: Paths,
        dirs: SessionDirs,
        settings: Settings,
        notify: impl Fn() + Send + 'static,
    ) -> Self {
        let (control, control_rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("usage-poller".into())
            .spawn(move || {
                run(
                    source,
                    paths,
                    dirs,
                    settings,
                    notify,
                    &control_rx,
                    &events_tx,
                )
            })
            .expect("spawning poller thread");
        Self {
            control,
            events,
            handle: Some(handle),
        }
    }

    /// The next pending event, if any. Never blocks.
    pub fn try_recv(&self) -> Option<PollEvent> {
        self.events.try_recv().ok()
    }

    /// Send a control message. Ignored if the thread has exited.
    pub fn send(&self, msg: PollMsg) {
        let _ = self.control.send(msg);
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        let _ = self.control.send(PollMsg::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(
    mut source: impl UsageSource,
    paths: Paths,
    dirs: SessionDirs,
    mut settings: Settings,
    notify: impl Fn(),
    control: &Receiver<PollMsg>,
    events: &Sender<PollEvent>,
) {
    let mut history = BurnHistory::seeded(&paths, Utc::now());
    loop {
        let result = catch_unwind(AssertUnwindSafe(|| {
            poll_once(&mut source, &paths, &dirs, &settings, &mut history)
        }));
        let event = match result {
            Ok(Ok(dashboard)) => PollEvent::Ok(Box::new(dashboard)),
            Ok(Err(e)) => PollEvent::Err(FetchErrorInfo::from(&e)),
            Err(panic) => PollEvent::Err(FetchErrorInfo {
                message: format!("internal error during poll: {}", panic_message(&*panic)),
                stale_sha_hint: false,
            }),
        };
        if events.send(event).is_err() {
            return;
        }
        notify();

        let deadline = Instant::now() + poll_interval(&settings);
        match control.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PollMsg::RefreshNow) | Err(RecvTimeoutError::Timeout) => {}
            Ok(PollMsg::UpdateSettings(s)) => settings = s,
            Ok(PollMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageWindow;
    use chrono::TimeZone;
    use std::fs;
    use std::sync::{Arc, Mutex};

    const WAIT: Duration = Duration::from_secs(1);

    fn snap(at: DateTime<Utc>, five: Option<f64>, fable: Option<f64>) -> Snapshot {
        let mut windows = vec![UsageWindow {
            label: "5-hour".into(),
            utilization: five,
            resets_at: Some(at + ChronoDuration::hours(2)),
        }];
        if let Some(f) = fable {
            windows.push(UsageWindow {
                label: "Fable".into(),
                utilization: Some(f),
                resets_at: None,
            });
        }
        Snapshot {
            fetched_at: at,
            windows,
            extra: None,
        }
    }

    /// Scripted source: pops the next result (an `Err(message)` becomes a
    /// 500) and records the settings plan seen by each call.
    #[derive(Clone, Default)]
    struct FakeSource {
        script: Arc<Mutex<VecDeque<Result<f64, String>>>>,
        seen_plans: Arc<Mutex<Vec<String>>>,
    }

    impl FakeSource {
        fn new(script: Vec<Result<f64, String>>) -> Self {
            Self {
                script: Arc::new(Mutex::new(script.into())),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<String> {
            self.seen_plans.lock().unwrap().clone()
        }
    }

    impl UsageSource for FakeSource {
        fn fetch(&mut self, _: &Paths, settings: &Settings) -> Result<Snapshot, FetchError> {
            self.seen_plans.lock().unwrap().push(settings.plan.clone());
            match self.script.lock().unwrap().pop_front().unwrap_or(Ok(1.0)) {
                Ok(pct) => Ok(snap(Utc::now(), Some(pct), None)),
                Err(body) => Err(FetchError::Status { status: 500, body }),
            }
        }
    }

    struct Rig {
        poller: Poller,
        source: FakeSource,
        notified: Receiver<()>,
        _tmp: tempfile::TempDir,
    }

    impl Rig {
        fn start(script: Vec<Result<f64, String>>) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::with_data_dir(tmp.path().join("data"));
            let dirs = SessionDirs {
                claude_code: tmp.path().join("cc"),
                cowork: tmp.path().join("cowork"),
            };
            let source = FakeSource::new(script);
            let (tx, notified) = mpsc::channel();
            let notify = move || {
                let _ = tx.send(());
            };
            let poller =
                Poller::spawn_with(source.clone(), paths, dirs, Settings::default(), notify);
            Self {
                poller,
                source,
                notified,
                _tmp: tmp,
            }
        }

        /// Wait for the next notification and return the event it announced.
        fn next(&self) -> PollEvent {
            self.notified.recv_timeout(WAIT).expect("poll within 1s");
            self.poller.try_recv().expect("event before notify")
        }
    }

    fn ok_pct(e: PollEvent) -> f64 {
        match e {
            PollEvent::Ok(d) => d.snapshot.five_hour_pct().unwrap(),
            PollEvent::Err(info) => panic!("unexpected error {info:?}"),
        }
    }

    #[test]
    fn first_poll_is_immediate_and_refresh_polls_again() {
        let rig = Rig::start(vec![Ok(10.0), Ok(20.0)]);
        assert_eq!(ok_pct(rig.next()), 10.0);
        assert!(rig.poller.try_recv().is_none());

        rig.poller.send(PollMsg::RefreshNow);
        let second = rig.next();
        let PollEvent::Ok(d) = second else {
            panic!("expected Ok")
        };
        assert_eq!(d.snapshot.five_hour_pct(), Some(20.0));
        // Both polls are in the in-memory history, oldest first.
        let pcts: Vec<_> = d.history.iter().map(|(_, p)| *p).collect();
        assert_eq!(pcts, [Some(10.0), Some(20.0)]);
        assert_eq!(rig.source.calls().len(), 2);
    }

    #[test]
    fn update_settings_applies_and_polls_now() {
        let rig = Rig::start(vec![]);
        rig.next();
        let new = Settings {
            plan: "Pro".into(),
            ..Settings::default()
        };
        rig.poller.send(PollMsg::UpdateSettings(new));
        rig.next();
        assert_eq!(rig.source.calls(), ["Max", "Pro"]);
    }

    #[test]
    fn errors_are_reported_and_polling_continues() {
        let rig = Rig::start(vec![Err("boom".into()), Ok(30.0)]);
        match rig.next() {
            PollEvent::Err(info) => {
                assert_eq!(info.message, "HTTP 500: boom");
                assert!(!info.stale_sha_hint);
            }
            PollEvent::Ok(_) => panic!("expected error"),
        }
        rig.poller.send(PollMsg::RefreshNow);
        assert_eq!(ok_pct(rig.next()), 30.0);
    }

    #[test]
    fn panic_in_source_is_reported_not_fatal() {
        struct Panicky(u32);
        impl UsageSource for Panicky {
            fn fetch(&mut self, _: &Paths, _: &Settings) -> Result<Snapshot, FetchError> {
                self.0 += 1;
                if self.0 == 1 {
                    panic!("kaboom");
                }
                Ok(snap(Utc::now(), Some(5.0), None))
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let poller = Poller::spawn_with(
            Panicky(0),
            Paths::with_data_dir(tmp.path().to_path_buf()),
            SessionDirs {
                claude_code: tmp.path().join("cc"),
                cowork: tmp.path().join("cw"),
            },
            Settings::default(),
            move || {
                let _ = tx.send(());
            },
        );
        rx.recv_timeout(WAIT).unwrap();
        match poller.try_recv().unwrap() {
            PollEvent::Err(info) => assert!(info.message.contains("kaboom"), "{info:?}"),
            PollEvent::Ok(_) => panic!("expected error"),
        }
        poller.send(PollMsg::RefreshNow);
        rx.recv_timeout(WAIT).unwrap();
        assert!(matches!(poller.try_recv(), Some(PollEvent::Ok(_))));
    }

    #[test]
    fn drop_shuts_down_promptly() {
        let rig = Rig::start(vec![]);
        rig.next();
        let started = Instant::now();
        drop(rig.poller);
        // The interval is 60 s; Drop must not wait for it.
        assert!(started.elapsed() < Duration::from_millis(500));
        // No further polls after shutdown.
        assert_eq!(rig.source.calls().len(), 1);
    }

    #[test]
    fn interval_is_clamped() {
        let s = |secs| Settings {
            interval_secs: secs,
            ..Settings::default()
        };
        assert_eq!(poll_interval(&s(0)), Duration::from_secs(MIN_INTERVAL_SECS));
        assert_eq!(poll_interval(&s(3)), Duration::from_secs(MIN_INTERVAL_SECS));
        assert_eq!(poll_interval(&s(90)), Duration::from_secs(90));
    }

    #[test]
    fn window_start_from_resets_at_or_now() {
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        let s = snap(now, Some(1.0), None); // resets at now + 2h
        assert_eq!(window_start(&s, now), now - ChronoDuration::hours(3));
        let no_reset = Snapshot {
            fetched_at: now,
            windows: vec![],
            extra: None,
        };
        assert_eq!(window_start(&no_reset, now), now - ChronoDuration::hours(5));
    }

    #[test]
    fn history_records_and_caps() {
        let t0 = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        let mut h = BurnHistory::default();
        for i in 0..(HISTORY_LEN + 3) {
            let t = t0 + ChronoDuration::seconds(i as i64);
            h.record(&snap(t, Some(i as f64), (i % 2 == 0).then_some(1.0)));
        }
        let five = h.five_hour();
        assert_eq!(five.len(), HISTORY_LEN);
        assert_eq!(five[0].1, Some(3.0));
        assert_eq!(five.last().unwrap().1, Some((HISTORY_LEN + 2) as f64));
        let fable = h.fable();
        assert_eq!(fable.len(), HISTORY_LEN);
        assert_eq!(fable[0].0, five[0].0);
    }

    #[test]
    fn history_seeded_from_recent_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(tmp.path().to_path_buf());
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        let ts = |mins_ago: i64| (now - ChronoDuration::minutes(mins_ago)).timestamp() as f64;
        let lines = [
            format!(r#"{{"ts": {}, "five_hour": 1.0, "fable_7d": 9.0}}"#, ts(30)),
            format!(
                r#"{{"ts": {}, "five_hour": 2.0, "fable_7d": 10.0}}"#,
                ts(10)
            ),
            format!(r#"{{"ts": {}.5, "five_hour": 3.0}}"#, ts(1)),
        ];
        fs::write(paths.history_file(), lines.join("\n") + "\n").unwrap();

        let h = BurnHistory::seeded(&paths, now);
        let five = h.five_hour();
        assert_eq!(
            five,
            [
                (now - ChronoDuration::minutes(10), Some(2.0)),
                (
                    now - ChronoDuration::minutes(1) + ChronoDuration::milliseconds(500),
                    Some(3.0)
                ),
            ]
        );
        let fable: Vec<_> = h.fable().iter().map(|(_, v)| *v).collect();
        assert_eq!(fable, [Some(10.0), None]);
    }

    #[test]
    fn history_seed_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let h = BurnHistory::seeded(&Paths::with_data_dir(tmp.path().join("x")), Utc::now());
        assert!(h.five_hour().is_empty() && h.fable().is_empty());
    }

    #[test]
    fn poll_once_persists_and_builds_dashboard() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(tmp.path().join("data"));
        let dirs = SessionDirs {
            claude_code: tmp.path().join("cc"),
            cowork: tmp.path().join("cw"),
        };
        let mut source = FakeSource::new(vec![Ok(42.0)]);
        let mut history = BurnHistory::default();
        let d = poll_once(
            &mut source,
            &paths,
            &dirs,
            &Settings::default(),
            &mut history,
        )
        .unwrap();

        assert_eq!(d.snapshot.five_hour_pct(), Some(42.0));
        assert_eq!(d.history.len(), 1);
        assert_eq!(d.fable_history, [(d.snapshot.fetched_at, None)]);
        assert!(paths.history_file().exists());
        assert!(paths.latest_file().exists());
        assert_eq!(
            d.window_start,
            d.snapshot.five_hour_resets_at().unwrap() - ChronoDuration::hours(5)
        );
        assert_eq!(d.five_hour_buckets.len(), 10);
        assert_eq!(d.fable_buckets.len(), 10);
        assert_eq!(d.daily_history.len(), DAILY_DAYS);
        assert_eq!(d.fable_daily.len(), DAILY_DAYS);
        assert!(d.burners.is_empty());
        // The just-appended sample lands in the current 30-minute bucket.
        assert!(d.five_hour_buckets.iter().any(|(_, v)| *v == Some(42.0)));
    }

    #[test]
    fn fetch_error_info_carries_stale_sha_hint() {
        let e = FetchError::Forbidden {
            body: r#"{"error":{"type":"permission_error"}}"#.into(),
        };
        let info = FetchErrorInfo::from(&e);
        assert!(info.stale_sha_hint);
        assert!(info.message.starts_with("403 — auth or Cloudflare: "));
    }
}
