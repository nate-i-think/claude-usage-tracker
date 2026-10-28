//! Background polling: fetch, record, persist, and precompute everything the
//! GUI renders, off the UI thread. No egui here; the GUI wakes itself via the
//! `notify` callback.
//!
//! One poller thread serves every enabled Claude Code profile. Each fetch
//! (credential read and HTTP request) runs on a thread of its own, since a
//! Keychain read can sit on a macOS prompt indefinitely; the poller thread
//! records each result and emits its event as soon as it arrives, so one
//! blocked profile never holds up another.
//!
//! Ported from the Python claude_usage_tui.py (removed; see git history):
//! its main loop, `record()`, and the inputs to `render()`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::analytics::{self, WeekSample, WindowSample, BURN_WINDOW_MINUTES, HISTORY_LEN};
use crate::config::{Settings, MIN_INTERVAL_SECS};
use crate::crash::catch_recoverable;
use crate::credentials::{self, CredError, CredentialLocator, CredentialStore, OAuthCredential};
use crate::fetch::{self, ErrorKind, FetchError};
use crate::model::Snapshot;
use crate::paths::{self, Paths};
use crate::profiles::{self, Discovered, Profile, ProfileId, ProfileIndexEntry, DEFAULT_ID};
use crate::storage::{self, CoworkFilter, InstanceUsage, ProfileIndex};

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
    /// 7-day-all samples of the last [`crate::analytics::WEEKLY_RATE_HOURS`]
    /// after their anchor, oldest first, including this poll's, for the
    /// weekly projection.
    pub seven_day_recent: Vec<Sample>,
    /// The same for Fable's 7-day limit.
    pub fable_recent: Vec<Sample>,
    pub burners: Vec<InstanceUsage>,
    pub window_start: DateTime<Utc>,
    /// End of the current five-hour window: its `resets_at`, else
    /// `window_start + 5h`.
    pub window_end: DateTime<Utc>,
    /// Every history sample in `[window_start, window_end)` from the last
    /// reset on ([`analytics::since_last_reset`]), oldest first, including
    /// this poll's. Empty while no window is active
    /// ([`analytics::FiveHourState::Idle`]).
    pub window_samples: Vec<WindowSample>,
    /// The current weekly window (see [`analytics::week_window`]).
    pub week_start: DateTime<Utc>,
    pub week_end: DateTime<Utc>,
    /// The weekly window's samples, thinned, oldest first, including this
    /// poll's.
    pub week_samples: Vec<WeekSample>,
    /// Saving this poll to history.jsonl failed; the data above is still
    /// fresh. Shown as a warning.
    pub persist_error: Option<String>,
    /// The credential's `subscriptionType` (`max`, `pro`, ...).
    pub plan: Option<String>,
    /// When the access token used for this poll expires.
    pub token_expires_at: Option<DateTime<Utc>>,
}

impl Dashboard {
    /// The 5-hour window's state and figures at `now` (see
    /// [`analytics::five_hour_now`]): a window that rolled over since this
    /// poll is idle until the next one.
    pub fn five_hour_now(&self, now: DateTime<Utc>) -> analytics::FiveHourNow {
        analytics::five_hour_now(
            self.snapshot.five_hour_resets_at(),
            self.snapshot.five_hour_pct(),
            now,
        )
    }
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
    pub kind: ErrorKind,
    pub message: String,
}

impl FetchErrorInfo {
    /// `e` for `profile`: credential problems name its config dir (`~` for
    /// the home directory) and the fix.
    pub fn for_profile(e: &FetchError, profile: &Profile) -> Self {
        Self::for_dir(e, &profile.config_dir, &paths::home_dir())
    }

    /// [`Self::for_profile`] for the config dir `dir`, shortening `home`.
    pub fn for_dir(e: &FetchError, dir: &Path, home: &Path) -> Self {
        let dir = tilde(dir, home);
        let message = match e {
            FetchError::TokenExpired | FetchError::Unauthorized => {
                format!("token expired — run Claude Code in {dir} to refresh")
            }
            FetchError::NoCredentials(why) if why.is_empty() => {
                format!("no Claude Code login found in {dir} — run `claude` and sign in")
            }
            FetchError::NoCredentials(why) => {
                format!("no Claude Code login found in {dir} — run `claude` and sign in ({why})")
            }
            FetchError::StoreUnavailable(why) => format!(
                "couldn't read the Claude Code login for {dir} — {}{}",
                credentials::UNAVAILABLE_HINT,
                if why.is_empty() {
                    String::new()
                } else {
                    format!(" ({why})")
                }
            ),
            other => other.to_string(),
        };
        Self {
            kind: e.kind(),
            message,
        }
    }

    /// A poll that panicked.
    fn internal(panic: &(dyn std::any::Any + Send)) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: format!("internal error during poll: {}", panic_message(panic)),
        }
    }
}

/// `dir` with a leading `home` written as `~`.
pub fn tilde(dir: &Path, home: &Path) -> String {
    if home.as_os_str().is_empty() {
        return dir.display().to_string();
    }
    match dir.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

/// One profile's poll result.
#[derive(Debug, Clone)]
pub enum PollOutcome {
    Ok(Box<Dashboard>),
    Err(FetchErrorInfo),
}

/// Results out of the poller thread.
#[derive(Debug, Clone)]
pub enum PollEvent {
    /// The profiles being polled, in display order; sent first and whenever
    /// the set (or anything about one) changes.
    Profiles(Vec<Profile>),
    /// One profile's poll.
    Profile { id: ProfileId, outcome: PollOutcome },
}

/// A successful fetch: the snapshot and what the credential says.
#[derive(Debug, Clone)]
pub struct FetchOk {
    pub snapshot: Snapshot,
    /// The credential's `subscriptionType`.
    pub plan: Option<String>,
    pub token_expires_at: Option<DateTime<Utc>>,
}

/// Where one profile's usage snapshots come from: [`OAuthSource`] in
/// production; fakes in tests. Each profile has its own.
pub trait UsageSource: Send + 'static {
    fn fetch(&mut self, profile: &Profile) -> Result<FetchOk, FetchError>;
}

/// The usage endpoint for one bearer token.
pub trait UsageApi: Send + 'static {
    fn get_usage(&mut self, bearer: &str) -> Result<Snapshot, FetchError>;
}

/// The HTTP client, built on first use and rebuilt after a build failure,
/// so a failure surfaces as a poll error instead of preventing the poller.
#[derive(Default)]
pub struct HttpApi {
    client: Option<fetch::Client>,
}

impl UsageApi for HttpApi {
    fn get_usage(&mut self, bearer: &str) -> Result<Snapshot, FetchError> {
        if self.client.is_none() {
            self.client = Some(fetch::Client::new()?);
        }
        self.client
            .as_ref()
            .expect("client just built")
            .fetch_usage(bearer)
    }
}

/// The usage endpoint over a profile's Claude Code OAuth credential. The
/// credential is cached and re-read only once it has expired, after a 401,
/// or when the profile's account changed (`/login` or `/logout` in the same
/// config dir rewrites the same Keychain item); an expired one fails
/// without a request. Never refreshes or writes a credential.
pub struct OAuthSource<A: UsageApi = HttpApi> {
    api: A,
    store: Arc<dyn CredentialStore>,
    /// The credential last read, with where from and the account the
    /// profile was signed in to then (a changed locator, e.g. a new
    /// Keychain override, or a changed account forces a re-read).
    cached: Option<(CacheKey, OAuthCredential)>,
}

/// What an [`OAuthSource`]'s cached credential is good for: a locator and
/// the profile's `accountUuid` (`None` while signed out or unknown).
type CacheKey = (CredentialLocator, Option<String>);

impl OAuthSource {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self::with_api(HttpApi::default(), store)
    }
}

impl<A: UsageApi> OAuthSource<A> {
    pub fn with_api(api: A, store: Arc<dyn CredentialStore>) -> Self {
        Self {
            api,
            store,
            cached: None,
        }
    }

    /// Read the credential from the store and cache it under `key`; a
    /// failed read clears the cache.
    fn load(&mut self, key: &CacheKey) -> Result<OAuthCredential, FetchError> {
        self.cached = None;
        let cred = self.store.load(&key.0).map_err(|e| match e {
            CredError::NotFound(_) => FetchError::NoCredentials(String::new()),
            CredError::Unavailable(why) => FetchError::StoreUnavailable(why),
            other => FetchError::NoCredentials(other.to_string()),
        })?;
        self.cached = Some((key.clone(), cred.clone()));
        Ok(cred)
    }

    /// The cached credential while it is current, else a fresh read.
    fn credential(
        &mut self,
        key: &CacheKey,
        now: DateTime<Utc>,
    ) -> Result<OAuthCredential, FetchError> {
        match &self.cached {
            Some((at, cred)) if at == key && !cred.is_expired(now) => Ok(cred.clone()),
            _ => self.load(key),
        }
    }
}

impl<A: UsageApi> UsageSource for OAuthSource<A> {
    fn fetch(&mut self, profile: &Profile) -> Result<FetchOk, FetchError> {
        let key = (profile.locator(), profile.account_uuid().map(str::to_owned));
        let mut cred = self.credential(&key, Utc::now())?;
        if cred.is_expired(Utc::now()) {
            return Err(FetchError::TokenExpired);
        }
        let snapshot = match self.api.get_usage(cred.bearer()) {
            // Claude Code may have refreshed the token since it was read:
            // read it once more and retry only if it changed.
            Err(FetchError::Unauthorized) => {
                let fresh = self.load(&key)?;
                if fresh.fingerprint() == cred.fingerprint() {
                    return Err(FetchError::Unauthorized);
                }
                if fresh.is_expired(Utc::now()) {
                    return Err(FetchError::TokenExpired);
                }
                cred = fresh;
                self.api.get_usage(cred.bearer())?
            }
            other => other?,
        };
        Ok(FetchOk {
            snapshot,
            plan: cred.subscription_type.clone(),
            token_expires_at: cred.expires_at,
        })
    }
}

/// Local session directories scanned for per-instance token attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDirs {
    /// A profile's Claude Code projects dir (`<config_dir>/projects`).
    pub claude_code: PathBuf,
    pub cowork: PathBuf,
}

impl SessionDirs {
    /// `profile`'s projects and the Cowork sessions under `cowork`.
    pub fn for_profile(profile: &Profile, cowork: &Path) -> Self {
        Self {
            claude_code: profile.projects_dir(),
            cowork: cowork.to_path_buf(),
        }
    }
}

/// Which Cowork sessions count toward `profile`, given every polled
/// profile: Cowork files sessions under the account's `accountUuid`, so a
/// non-default profile takes its own (none while its account is unknown),
/// and the default profile takes every session no other profile claims.
pub fn cowork_filter(profile: &Profile, all: &[Profile]) -> CoworkFilter {
    if profile.id.is_default() {
        return CoworkFilter::Unmatched(
            all.iter()
                .filter(|p| !p.id.is_default())
                .filter_map(|p| p.account_uuid().map(str::to_owned))
                .collect(),
        );
    }
    match profile.account_uuid() {
        Some(uuid) => CoworkFilter::Account(uuid.to_owned()),
        None => CoworkFilter::None,
    }
}

/// One profile's polling state: its files, burn history and source.
pub struct ProfileRuntime {
    pub profile: Profile,
    /// The profile's own data dir (`<data>/profiles/<id>/`).
    pub paths: Paths,
    pub dirs: SessionDirs,
    pub history: BurnHistory,
    /// `None` while a fetch is in flight on its own thread.
    source: Option<Box<dyn UsageSource>>,
    /// When the next fetch is due; `None` = now.
    next_due: Option<Instant>,
    /// `next_due` is a 429 backoff, which a refresh does not override.
    backoff: bool,
    /// Tells this runtime's fetch results from those of an earlier runtime
    /// for the same id (removed and re-added while a fetch was in flight),
    /// or of an earlier account in the same config dir.
    generation: u64,
    /// The account uuid this runtime last polled for (the last one known:
    /// a transiently unreadable `.claude.json` does not clear it).
    account: Option<String>,
}

impl ProfileRuntime {
    /// `profile` polled through `source`, its files under `root`, its burn
    /// history seeded from its history.jsonl.
    pub fn new(
        profile: Profile,
        root: &Paths,
        cowork_dir: &Path,
        source: Box<dyn UsageSource>,
    ) -> Self {
        let paths = root.for_profile(&profile.id);
        let history =
            catch_recoverable(|| BurnHistory::seeded(&paths, Utc::now())).unwrap_or_default();
        Self {
            dirs: SessionDirs::for_profile(&profile, cowork_dir),
            paths,
            history,
            source: Some(source),
            next_due: None,
            backoff: false,
            generation: 0,
            account: profile_account(&profile),
            profile,
        }
    }
}

/// `profile`'s account uuid, lowercased for comparison.
fn profile_account(profile: &Profile) -> Option<String> {
    profile.account_uuid().map(str::to_ascii_lowercase)
}

/// One profile's poll, start to end: fetch, record, persist, build the
/// dashboard. The CLI `once` command's path; the poller thread splits it
/// (the fetch runs on a thread of its own).
pub fn poll_profile(
    rt: &mut ProfileRuntime,
    cowork: &CoworkFilter,
) -> Result<Dashboard, FetchError> {
    let source = rt.source.as_mut().expect("no fetch in flight");
    let fetched = source.fetch(&rt.profile)?;
    Ok(record(rt, fetched, cowork))
}

/// Record a successful fetch into `rt`'s burn history, append it to its
/// history.jsonl (which also refreshes latest.json), and compute the
/// dashboard.
fn record(rt: &mut ProfileRuntime, fetched: FetchOk, cowork: &CoworkFilter) -> Dashboard {
    let FetchOk {
        snapshot,
        plan,
        token_expires_at,
    } = fetched;
    rt.history.record(&snapshot);
    // A disk problem must not hide fresh data: report it alongside.
    let persist_error = storage::append_snapshot(&rt.paths, &snapshot)
        .err()
        .map(|e| {
            format!(
                "couldn't save history to {}: {e}",
                rt.paths.history_file().display()
            )
        });
    let mut dashboard = build_dashboard(
        snapshot,
        &rt.history,
        &rt.paths,
        &rt.dirs,
        cowork,
        Utc::now(),
    );
    dashboard.persist_error = persist_error;
    dashboard.plan = plan;
    dashboard.token_expires_at = token_expires_at;
    dashboard
}

fn build_dashboard(
    snapshot: Snapshot,
    history: &BurnHistory,
    paths: &Paths,
    dirs: &SessionDirs,
    cowork: &CoworkFilter,
    now: DateTime<Utc>,
) -> Dashboard {
    let ws = window_start(&snapshot, now);
    let window_end = snapshot
        .five_hour_resets_at()
        .unwrap_or(ws + ChronoDuration::hours(5));
    let (week_start, week_end) = analytics::week_window(snapshot.seven_day_resets_at(), now);
    // One history read feeds the rollups, the window samples and the week.
    let rollups = storage::rollups(paths, (ws, window_end), (week_start, week_end), now);
    let window_samples = five_hour_samples(
        with_current_sample(rollups.window_samples, &snapshot, ws, window_end),
        &snapshot,
        now,
    );
    let week_samples = with_current_week_sample(rollups.week_samples, &snapshot);
    Dashboard {
        history: history.five_hour(),
        fable_history: history.fable(),
        seven_day_recent: with_current(
            rollups.seven_day_recent,
            snapshot.fetched_at,
            snapshot.seven_day_pct(),
        ),
        fable_recent: with_current(
            rollups.fable_recent,
            snapshot.fetched_at,
            snapshot.fable_pct(),
        ),
        burners: storage::attribute_recent(&dirs.claude_code, &dirs.cowork, ws, cowork),
        window_start: ws,
        window_end,
        window_samples,
        week_start,
        week_end,
        week_samples,
        persist_error: None,
        plan: None,
        token_expires_at: None,
        snapshot,
    }
}

/// The 5-hour chart's samples: none while no window is active (the chart
/// starts over, empty, like the old TUI after a reset), else those from
/// the last reset on, so the previous window's climb never shows (a
/// trailing view, or `resets_at` jitter at a window's start edge).
fn five_hour_samples(
    samples: Vec<WindowSample>,
    snap: &Snapshot,
    now: DateTime<Utc>,
) -> Vec<WindowSample> {
    match analytics::five_hour_state(snap.five_hour_resets_at(), snap.five_hour_pct(), now) {
        analytics::FiveHourState::Idle => Vec::new(),
        _ => analytics::since_last_reset(&samples).to_vec(),
    }
}

/// `samples` with this poll's weekly sample at its exact fetch time in
/// place of its persisted copy (if any), kept sorted.
fn with_current_week_sample(mut samples: Vec<WeekSample>, snap: &Snapshot) -> Vec<WeekSample> {
    let t = snap.fetched_at;
    samples.retain(|s| (s.t - t).abs() >= ChronoDuration::milliseconds(1));
    samples.push(WeekSample {
        t,
        seven_day: snap.seven_day_pct(),
        fable: snap.fable_pct(),
    });
    samples.sort_by_key(|s| s.t);
    samples
}

/// `samples` with this poll's value at its exact fetch time `t` in place
/// of its persisted copy (if any), kept sorted.
fn with_current(mut samples: Vec<Sample>, t: DateTime<Utc>, value: Option<f64>) -> Vec<Sample> {
    samples.retain(|(s, _)| (*s - t).abs() >= ChronoDuration::milliseconds(1));
    samples.push((t, value));
    samples.sort_by_key(|(s, _)| *s);
    samples
}

/// `samples` with this poll's sample at its exact fetch time, whether or not
/// it reached history.jsonl (the persist can fail, and the file keeps only
/// microseconds). Kept only when it falls inside the half-open window
/// `[window_start, window_end)`, like the persisted samples.
fn with_current_sample(
    mut samples: Vec<WindowSample>,
    snap: &Snapshot,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<WindowSample> {
    let t = snap.fetched_at;
    if t < window_start || t >= window_end {
        return samples;
    }
    // The persisted copy of this poll, if any, rounded to microseconds.
    samples.retain(|s| (s.t - t).abs() >= ChronoDuration::milliseconds(1));
    samples.push(WindowSample {
        t,
        five_hour: snap.five_hour_pct(),
        fable: snap.window("Fable").and_then(|w| w.utilization),
    });
    samples.sort_by_key(|s| s.t);
    samples
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

/// What a one-shot setup does when a config dir is now signed in to a
/// different account than `profiles.json` last recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountChange {
    /// Set the old account's history aside, as the poller thread does.
    Archive,
    /// Leave the history and `profiles.json` alone, so the GUI's poller
    /// still notices the change and archives it itself (the CLI).
    Leave,
}

/// One-shot setup for polling outside the poller thread (the CLI `once`
/// and `debug` commands), the same as the poller's start: migrate the
/// legacy history, discover and resolve the profiles, handle an account
/// change per `on_change`, refresh `profiles.json`. Returns a ready
/// runtime per polled profile, in display order.
pub fn prepare_runtimes(
    deps: PollerDeps,
    root: Paths,
    settings: Settings,
    on_change: AccountChange,
) -> Vec<ProfileRuntime> {
    // Nothing fetches on a thread here; results have nowhere to go.
    let (fetched, _) = mpsc::channel();
    let mut engine = Engine::new(deps, root, settings, fetched);
    engine.on_change = on_change;
    engine.reconcile(Instant::now());
    engine.runtimes
}

/// Makes the [`UsageSource`] for a profile seen for the first time.
pub type SourceFactory = Box<dyn Fn(&Profile) -> Box<dyn UsageSource> + Send>;

/// What the poller thread needs from the outside world, swappable in tests.
pub struct PollerDeps {
    /// A new source for a profile seen for the first time.
    pub source_for: SourceFactory,
    /// The Claude Code profiles on disk (see [`profiles::discover`]).
    pub discover: Box<dyn Fn() -> Vec<Discovered> + Send>,
    /// The Cowork sessions dir (see [`paths::cowork_dir`]).
    pub cowork_dir: PathBuf,
}

impl PollerDeps {
    /// The real thing: OAuth over the platform credential store, discovery
    /// under the home directory.
    pub fn platform() -> Self {
        let store = credentials::platform_store();
        Self {
            source_for: Box::new(move |_: &Profile| {
                Box::new(OAuthSource::new(Arc::clone(&store))) as Box<dyn UsageSource>
            }),
            discover: Box::new(|| profiles::discover(&paths::home_dir())),
            cowork_dir: paths::cowork_dir(),
        }
    }
}

/// State shared between the [`Poller`] handle and its thread.
#[derive(Default)]
struct Flags {
    /// Set by `Drop` before it sends `Shutdown`.
    shutdown: AtomicBool,
    /// True while the thread is working (reconciling, recording, sending,
    /// notifying), false while it waits for the next tick or message.
    /// Fetches run on their own threads and never make it busy.
    busy: AtomicBool,
}

/// Everything into the poller thread: control messages from the handle,
/// fetch results from the fetch threads.
enum Inbox {
    Control(PollMsg),
    Fetched(Fetched),
}

/// A fetch thread's result, handing the source back.
struct Fetched {
    id: ProfileId,
    generation: u64,
    source: Box<dyn UsageSource>,
    result: thread::Result<Result<FetchOk, FetchError>>,
}

/// Handle to the background poller thread. Dropping it shuts the thread down.
pub struct Poller {
    control: Sender<Inbox>,
    events: Receiver<PollEvent>,
    flags: Arc<Flags>,
    handle: Option<JoinHandle<()>>,
}

impl Poller {
    /// Poll every enabled Claude Code profile in the background, keeping
    /// their files under `root`. `notify` runs after each event is sent
    /// (the GUI requests a repaint; the CLI can pass a no-op).
    pub fn spawn(root: Paths, settings: Settings, notify: impl Fn() + Send + 'static) -> Self {
        Self::spawn_with(PollerDeps::platform(), root, settings, notify)
    }

    /// [`Poller::spawn`] with explicit sources, discovery and Cowork dir.
    pub fn spawn_with(
        deps: PollerDeps,
        root: Paths,
        settings: Settings,
        notify: impl Fn() + Send + 'static,
    ) -> Self {
        let (control, inbox) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let flags = Arc::new(Flags::default());
        let thread_flags = Arc::clone(&flags);
        let fetched = control.clone();
        let handle = thread::Builder::new()
            .name("usage-poller".into())
            .spawn(move || {
                let ctx = LoopCtx {
                    notify,
                    inbox,
                    events: events_tx,
                    flags: thread_flags,
                };
                run(Engine::new(deps, root, settings, fetched), &ctx);
            })
            .expect("spawning poller thread");
        Self {
            control,
            events,
            flags,
            handle: Some(handle),
        }
    }

    /// The next pending event, if any. Never blocks.
    pub fn try_recv(&self) -> Option<PollEvent> {
        self.events.try_recv().ok()
    }

    /// Send a control message. Ignored if the thread has exited.
    pub fn send(&self, msg: PollMsg) {
        let _ = self.control.send(Inbox::Control(msg));
    }

    /// False once the poller thread has exited (after `Shutdown`, or if it
    /// died), so the UI can show "poller stopped" instead of idling.
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for Poller {
    /// Never blocks for a poll. Fetches run on their own threads, which are
    /// abandoned (a blocked one finishes whenever it does and its result is
    /// dropped). If the poller thread is waiting, `Shutdown` wakes it and
    /// the join is immediate; if it is busy recording a result, it is
    /// detached: it sees the shutdown flag and exits without sending.
    fn drop(&mut self) {
        self.flags.shutdown.store(true, Ordering::SeqCst);
        let _ = self.control.send(Inbox::Control(PollMsg::Shutdown));
        if let Some(handle) = self.handle.take() {
            // SeqCst pairs with `run`: if `busy` reads false here, the
            // thread's next `busy = true` is ordered after our `shutdown =
            // true`, so it sees the flag and exits before working.
            if !self.flags.busy.load(Ordering::SeqCst) {
                let _ = handle.join();
            }
        }
    }
}

/// The poller thread's channels and flags.
struct LoopCtx<N> {
    notify: N,
    inbox: Receiver<Inbox>,
    events: Sender<PollEvent>,
    flags: Arc<Flags>,
}

impl<N: Fn()> LoopCtx<N> {
    /// Send `event` and notify; false once shutting down (or nobody is
    /// listening), and the loop should exit.
    fn emit(&self, event: PollEvent) -> bool {
        if self.flags.shutdown.load(Ordering::SeqCst) || self.events.send(event).is_err() {
            return false;
        }
        let _ = catch_recoverable(&self.notify);
        true
    }

    /// Mark the thread busy; false if it should exit instead.
    fn start_work(&self) -> bool {
        self.flags.busy.store(true, Ordering::SeqCst);
        !self.flags.shutdown.load(Ordering::SeqCst)
    }
}

fn run(mut engine: Engine, ctx: &LoopCtx<impl Fn()>) {
    let mut rediscover = true;
    loop {
        if !ctx.start_work() {
            return;
        }
        let now = Instant::now();
        let mut events = Vec::new();
        if rediscover || engine.discovery_due(now) {
            events.extend(engine.reconcile(now));
        }
        events.extend(engine.dispatch(now));
        for event in events {
            if !ctx.emit(event) {
                return;
            }
        }
        ctx.flags.busy.store(false, Ordering::SeqCst);

        let first = match ctx.inbox.recv_timeout(engine.wait(Instant::now())) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
                rediscover = false;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if !ctx.start_work() {
            return;
        }
        let mut fetched = Vec::new();
        let wake = drain(first, &ctx.inbox, &mut fetched);
        if wake == Wake::Exit {
            return;
        }
        for f in fetched {
            if let Some(event) = engine.complete(f, Instant::now()) {
                if !ctx.emit(event) {
                    return;
                }
            }
        }
        rediscover = match wake {
            Wake::Exit => return,
            Wake::Poll(settings) => {
                if let Some(new) = settings {
                    engine.settings = new;
                }
                engine.refresh_all();
                true
            }
            Wake::Results => false,
        };
    }
}

/// The poller thread's state: every polled profile's runtime and what was
/// last told to the GUI and written to `profiles.json`.
struct Engine {
    deps: PollerDeps,
    root: Paths,
    settings: Settings,
    /// The polled profiles' runtimes, in display order.
    runtimes: Vec<ProfileRuntime>,
    /// The profile list last sent as [`PollEvent::Profiles`].
    sent: Option<Vec<Profile>>,
    /// The entries last written to (or read from) `profiles.json`.
    indexed: Vec<ProfileIndexEntry>,
    /// The account uuid last seen per profile id (seeded from the index),
    /// to notice a config dir signed in to another account.
    accounts: BTreeMap<ProfileId, String>,
    discovered_at: Option<Instant>,
    next_generation: u64,
    /// Where fetch threads send their results.
    fetched: Sender<Inbox>,
    on_change: AccountChange,
    /// An account change was left alone ([`AccountChange::Leave`]):
    /// `profiles.json` must keep the old account so the GUI notices it.
    change_left: bool,
}

impl Engine {
    /// Copies the pre-profiles history into the default profile's dir
    /// first, so its runtime seeds from it.
    fn new(deps: PollerDeps, root: Paths, settings: Settings, fetched: Sender<Inbox>) -> Self {
        let default = root.for_profile(&ProfileId::new(DEFAULT_ID));
        // Best effort: a failure leaves the old history where it was.
        let _ = catch_recoverable(|| storage::migrate_legacy(&root, &default));
        let indexed = storage::read_profiles_index(&root)
            .map(|i| i.profiles)
            .unwrap_or_default();
        let accounts = indexed
            .iter()
            .filter_map(|e| Some((e.id.clone(), e.account_uuid.clone()?)))
            .collect();
        Self {
            deps,
            root,
            settings,
            runtimes: Vec::new(),
            sent: None,
            indexed,
            accounts,
            discovered_at: None,
            next_generation: 0,
            fetched,
            on_change: AccountChange::Archive,
            change_left: false,
        }
    }

    fn interval(&self) -> Duration {
        poll_interval(&self.settings)
    }

    fn discovery_due(&self, now: Instant) -> bool {
        self.discovered_at
            .is_none_or(|t| now >= t + self.interval())
    }

    /// Rediscover profiles and bring the runtimes in line: new ones start
    /// (due now), dropped ones stop, kept ones take the latest settings.
    /// Returns a [`PollEvent::Profiles`] when the list changed.
    fn reconcile(&mut self, now: Instant) -> Option<PollEvent> {
        self.discovered_at = Some(now);
        let found = catch_recoverable(|| (self.deps.discover)()).unwrap_or_default();
        // Sticky: a profile once discovered stays polled (unaccounted) when
        // a later discovery misses it, e.g. its `.claude.json` mid-write.
        profiles::merge_discovered(&mut self.settings.profiles, &found);
        let profiles = profiles::resolve(&self.settings.profiles, &found);
        let mut old = std::mem::take(&mut self.runtimes);
        for p in &profiles {
            let kept = old
                .iter()
                .position(|r| r.profile.id == p.id && r.profile.config_dir == p.config_dir);
            let mut rt = match kept {
                Some(i) => {
                    let mut rt = old.remove(i);
                    rt.profile = p.clone();
                    rt
                }
                None => {
                    let source = (self.deps.source_for)(p);
                    let mut rt =
                        ProfileRuntime::new(p.clone(), &self.root, &self.deps.cowork_dir, source);
                    rt.generation = self.next_generation;
                    self.next_generation += 1;
                    rt
                }
            };
            self.check_account(&mut rt);
            self.runtimes.push(rt);
        }
        // Runtimes left in `old` are dropped; their in-flight results, if
        // any, are discarded on arrival.

        let entries: Vec<ProfileIndexEntry> = profiles.iter().map(Profile::index_entry).collect();
        if entries != self.indexed && !self.change_left {
            // Best effort: the index only helps the CLI and MCP readers.
            let _ = storage::write_profiles_index(&self.root, &ProfileIndex::new(entries.clone()));
            self.indexed = entries;
        }
        if self.sent.as_ref() == Some(&profiles) {
            return None;
        }
        self.sent = Some(profiles.clone());
        Some(PollEvent::Profiles(profiles))
    }

    /// When `rt`'s config dir is now signed in to a different account than
    /// last seen: poll it on a fresh source (the old one caches the old
    /// account's token, and its result, if in flight, is dropped), and set
    /// the old account's history aside for a fresh one (unless
    /// [`AccountChange::Leave`]).
    fn check_account(&mut self, rt: &mut ProfileRuntime) {
        // Signed out, or `.claude.json` unreadable for now: the source
        // re-reads its credential once the account it cached under is gone,
        // and the history stays the last account's.
        let Some(uuid) = rt.profile.account_uuid().map(str::to_owned) else {
            return;
        };
        let now = uuid.to_ascii_lowercase();
        if rt.account.as_ref().is_some_and(|was| *was != now) {
            self.restart_source(rt);
        }
        rt.account = Some(now);
        let id = rt.profile.id.clone();
        if let Some(old) = self.accounts.get(&id) {
            if !old.eq_ignore_ascii_case(&uuid) {
                if self.on_change == AccountChange::Leave {
                    self.change_left = true;
                    return;
                }
                let old8: String = old.chars().take(8).collect();
                // Best effort: if the rename fails the new account's samples
                // follow the old ones, as before profiles existed.
                let _ = storage::archive_history(&rt.paths, &old8);
                rt.history = BurnHistory::default();
            }
        }
        self.accounts.insert(id, uuid);
    }

    /// Give `rt` a new source and generation, due now: a fetch in flight on
    /// the old source is discarded when it returns (generation mismatch),
    /// old source and all.
    fn restart_source(&mut self, rt: &mut ProfileRuntime) {
        rt.source = Some((self.deps.source_for)(&rt.profile));
        rt.generation = self.next_generation;
        self.next_generation += 1;
        rt.next_due = None;
        rt.backoff = false;
    }

    /// Start a fetch thread for every idle profile that is due.
    fn dispatch(&mut self, now: Instant) -> Vec<PollEvent> {
        let interval = self.interval();
        let mut events = Vec::new();
        for rt in &mut self.runtimes {
            if rt.next_due.is_some_and(|t| t > now) {
                continue;
            }
            let Some(source) = rt.source.take() else {
                continue; // Still in flight from an earlier tick.
            };
            rt.next_due = Some(now + interval);
            let (id, generation, profile) =
                (rt.profile.id.clone(), rt.generation, rt.profile.clone());
            let tx = self.fetched.clone();
            let spawned = thread::Builder::new()
                .name(format!("fetch-{id}"))
                .spawn(move || {
                    let mut source = source;
                    let result = catch_recoverable(|| source.fetch(&profile));
                    // The poller gone (shut down): nobody wants the result.
                    let _ = tx.send(Inbox::Fetched(Fetched {
                        id,
                        generation,
                        source,
                        result,
                    }));
                });
            if let Err(e) = spawned {
                // The source went down with the closure; start over.
                rt.source = Some((self.deps.source_for)(&rt.profile));
                events.push(PollEvent::Profile {
                    id: rt.profile.id.clone(),
                    outcome: PollOutcome::Err(FetchErrorInfo {
                        kind: ErrorKind::Internal,
                        message: format!("couldn't start a fetch: {e}"),
                    }),
                });
            }
        }
        events
    }

    /// Fold a fetch result in: record and persist it, schedule the next
    /// fetch, and return the profile's event. `None` for a result from a
    /// profile no longer polled.
    fn complete(&mut self, f: Fetched, now: Instant) -> Option<PollEvent> {
        let i = self
            .runtimes
            .iter()
            .position(|r| r.profile.id == f.id && r.generation == f.generation)?;
        let interval = self.interval();
        let all: Vec<Profile> = self.runtimes.iter().map(|r| r.profile.clone()).collect();
        let rt = &mut self.runtimes[i];
        let cowork = cowork_filter(&rt.profile, &all);
        rt.source = Some(f.source);
        rt.backoff = false;
        // `None` means a refresh came in while the fetch was in flight: due
        // now. Otherwise the next fetch is an interval after this result.
        if rt.next_due.is_some() {
            rt.next_due = Some(now + interval);
        }
        let outcome = match f.result {
            Ok(Ok(fetched)) => match catch_recoverable(|| record(rt, fetched, &cowork)) {
                Ok(d) => PollOutcome::Ok(Box::new(d)),
                Err(panic) => PollOutcome::Err(FetchErrorInfo::internal(&*panic)),
            },
            Ok(Err(e)) => {
                if let FetchError::RateLimited { retry_after } = &e {
                    rt.next_due = Some(now + retry_after.unwrap_or(interval * 2));
                    rt.backoff = true;
                }
                PollOutcome::Err(FetchErrorInfo::for_profile(&e, &rt.profile))
            }
            Err(panic) => PollOutcome::Err(FetchErrorInfo::internal(&*panic)),
        };
        Some(PollEvent::Profile {
            id: rt.profile.id.clone(),
            outcome,
        })
    }

    /// A refresh: every profile is due now, except one backing off after a
    /// 429.
    fn refresh_all(&mut self) {
        for rt in &mut self.runtimes {
            if !rt.backoff {
                rt.next_due = None;
            }
        }
    }

    /// How long to wait for the next due fetch or rediscovery.
    fn wait(&self, now: Instant) -> Duration {
        let discovery = self.discovered_at.map_or(now, |t| t + self.interval());
        let wake = self
            .runtimes
            .iter()
            .filter(|r| r.source.is_some())
            .map(|r| r.next_due.unwrap_or(now))
            .fold(discovery, Instant::min);
        wake.saturating_duration_since(now)
    }
}

/// What to do after waking on the inbox.
#[derive(Debug, PartialEq)]
enum Wake {
    Exit,
    /// A refresh: poll every profile now, with replacement settings if any
    /// arrived.
    Poll(Option<Settings>),
    /// Only fetch results arrived.
    Results,
}

/// Fold `first` and everything already queued into one decision: any
/// `Shutdown` (or a disconnected channel) exits, the last `UpdateSettings`
/// wins, and any number of refreshes become a single poll. Fetch results
/// are collected into `fetched`.
fn drain(first: Inbox, inbox: &Receiver<Inbox>, fetched: &mut Vec<Fetched>) -> Wake {
    let mut settings = None;
    let mut refresh = false;
    let mut next = Some(first);
    while let Some(msg) = next {
        match msg {
            Inbox::Control(PollMsg::Shutdown) => return Wake::Exit,
            Inbox::Control(PollMsg::UpdateSettings(s)) => {
                settings = Some(s);
                refresh = true;
            }
            Inbox::Control(PollMsg::RefreshNow) => refresh = true,
            Inbox::Fetched(f) => fetched.push(f),
        }
        next = match inbox.try_recv() {
            Ok(msg) => Some(msg),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => return Wake::Exit,
        };
    }
    if refresh {
        Wake::Poll(settings)
    } else {
        Wake::Results
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
    use crate::config::ProfileSettings;
    use crate::model::UsageWindow;
    use crate::profiles::AccountInfo;
    use chrono::TimeZone;
    use std::fs;
    use std::sync::Mutex;

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
            breakdown: None,
        }
    }

    fn fetch_ok(pct: f64) -> FetchOk {
        FetchOk {
            snapshot: snap(Utc::now(), Some(pct), None),
            plan: Some("max".into()),
            token_expires_at: None,
        }
    }

    /// A discovered profile under `home`: `default` is `~/.claude`, any
    /// other id `~/.<id>`; signed in to account `uuid-<id>`.
    fn discovered(home: &Path, id: &str) -> Discovered {
        let (dir, env) = if id == DEFAULT_ID {
            (home.join(".claude"), None)
        } else {
            let dir = home.join(format!(".{id}"));
            let env = Some(dir.display().to_string());
            (dir, env)
        };
        Discovered {
            id: ProfileId::new(id),
            config_dir: dir,
            config_dir_env: env,
            account: Some(AccountInfo {
                email: Some(format!("{id}@example.com")),
                account_uuid: Some(format!("uuid-{id}")),
                ..AccountInfo::default()
            }),
        }
    }

    fn profile(home: &Path, id: &str) -> Profile {
        profiles::polled(&[], &[discovered(home, id)]).remove(0)
    }

    /// Scripted source: pops the next result (an `Err(message)` becomes a
    /// 500, `RateLimited` a 429) and records the profile seen by each call.
    #[derive(Clone, Default)]
    struct FakeSource {
        script: Arc<Mutex<VecDeque<Result<f64, FetchError>>>>,
        seen: Arc<Mutex<Vec<Profile>>>,
    }

    impl FakeSource {
        fn new(script: Vec<Result<f64, &str>>) -> Self {
            let script = script
                .into_iter()
                .map(|r| {
                    r.map_err(|body| FetchError::Status {
                        status: 500,
                        body: body.into(),
                    })
                })
                .collect();
            Self {
                script: Arc::new(Mutex::new(script)),
                ..Self::default()
            }
        }

        fn rate_limited_then(retry_after: Duration, pct: f64) -> Self {
            let s = Self::default();
            s.script.lock().unwrap().extend([
                Err(FetchError::RateLimited {
                    retry_after: Some(retry_after),
                }),
                Ok(pct),
            ]);
            s
        }

        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn labels(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.label.clone())
                .collect()
        }
    }

    impl UsageSource for FakeSource {
        fn fetch(&mut self, profile: &Profile) -> Result<FetchOk, FetchError> {
            self.seen.lock().unwrap().push(profile.clone());
            let next = self.script.lock().unwrap().pop_front().unwrap_or(Ok(1.0));
            next.map(fetch_ok)
        }
    }

    /// Each fetch signals `started`, then blocks until `gate` yields a token.
    struct GatedSource {
        started: Sender<()>,
        gate: Receiver<()>,
        calls: Arc<Mutex<u32>>,
    }

    impl UsageSource for GatedSource {
        fn fetch(&mut self, _: &Profile) -> Result<FetchOk, FetchError> {
            *self.calls.lock().unwrap() += 1;
            let _ = self.started.send(());
            let _ = self.gate.recv_timeout(Duration::from_secs(5));
            Ok(fetch_ok(1.0))
        }
    }

    /// Hands out the queued source for each profile id once.
    type Sources = Arc<Mutex<BTreeMap<String, Box<dyn UsageSource>>>>;

    struct Rig {
        poller: Poller,
        notified: Receiver<()>,
        /// What discovery finds; tests can change it mid-run.
        found: Arc<Mutex<Vec<Discovered>>>,
        sources: Sources,
        root: Paths,
        tmp: tempfile::TempDir,
    }

    impl Rig {
        /// A poller over the profiles `ids` (the first discovered first),
        /// each fetching from its source.
        fn start(sources: Vec<(&str, Box<dyn UsageSource>)>) -> Self {
            Self::start_in(tempfile::tempdir().unwrap(), sources, Settings::default())
        }

        fn start_in(
            tmp: tempfile::TempDir,
            sources: Vec<(&str, Box<dyn UsageSource>)>,
            settings: Settings,
        ) -> Self {
            let home = tmp.path().join("home");
            let found: Vec<Discovered> = sources
                .iter()
                .map(|(id, _)| discovered(&home, id))
                .collect();
            let found = Arc::new(Mutex::new(found));
            let sources: Sources = Arc::new(Mutex::new(
                sources
                    .into_iter()
                    .map(|(id, s)| (id.to_owned(), s))
                    .collect(),
            ));
            let root = Paths::with_data_dir(tmp.path().join("data"));
            let deps = PollerDeps {
                source_for: {
                    let sources = Arc::clone(&sources);
                    Box::new(move |p: &Profile| {
                        sources
                            .lock()
                            .unwrap()
                            .remove(p.id.as_str())
                            .unwrap_or_else(|| Box::new(FakeSource::default()))
                    })
                },
                discover: {
                    let found = Arc::clone(&found);
                    Box::new(move || found.lock().unwrap().clone())
                },
                cowork_dir: tmp.path().join("cowork"),
            };
            let (tx, notified) = mpsc::channel();
            let poller = Poller::spawn_with(deps, root.clone(), settings, move || {
                let _ = tx.send(());
            });
            Self {
                poller,
                notified,
                found,
                sources,
                root,
                tmp,
            }
        }

        fn home(&self) -> PathBuf {
            self.tmp.path().join("home")
        }

        /// Wait for the next notification and return the event it announced.
        fn next(&self) -> PollEvent {
            self.notified.recv_timeout(WAIT).expect("event within 1s");
            self.poller.try_recv().expect("event before notify")
        }

        /// The next profile event, skipping profile-list events.
        fn next_profile(&self) -> (ProfileId, PollOutcome) {
            loop {
                if let PollEvent::Profile { id, outcome } = self.next() {
                    return (id, outcome);
                }
            }
        }

        fn profile_ids(event: PollEvent) -> Vec<String> {
            match event {
                PollEvent::Profiles(list) => list.iter().map(|p| p.id.to_string()).collect(),
                other => panic!("expected Profiles, got {other:?}"),
            }
        }

        fn no_event_for(&self, d: Duration) -> bool {
            self.notified.recv_timeout(d).is_err()
        }
    }

    fn ok_dashboard(outcome: PollOutcome) -> Box<Dashboard> {
        match outcome {
            PollOutcome::Ok(d) => d,
            PollOutcome::Err(info) => panic!("unexpected error {info:?}"),
        }
    }

    fn ok_pct(outcome: PollOutcome) -> f64 {
        ok_dashboard(outcome).snapshot.five_hour_pct().unwrap()
    }

    fn single(source: impl UsageSource) -> Rig {
        Rig::start(vec![(DEFAULT_ID, Box::new(source))])
    }

    #[test]
    fn first_poll_is_immediate_and_refresh_polls_again() {
        let source = FakeSource::new(vec![Ok(10.0), Ok(20.0)]);
        let rig = single(source.clone());
        assert_eq!(Rig::profile_ids(rig.next()), ["default"]);
        let (id, first) = rig.next_profile();
        assert_eq!(id.as_str(), DEFAULT_ID);
        assert_eq!(ok_pct(first), 10.0);
        assert!(rig.poller.try_recv().is_none());

        rig.poller.send(PollMsg::RefreshNow);
        let d = ok_dashboard(rig.next_profile().1);
        assert_eq!(d.snapshot.five_hour_pct(), Some(20.0));
        assert_eq!(d.plan.as_deref(), Some("max"));
        // Both polls are in the in-memory history, oldest first.
        let pcts: Vec<_> = d.history.iter().map(|(_, p)| *p).collect();
        assert_eq!(pcts, [Some(10.0), Some(20.0)]);
        assert_eq!(source.calls(), 2);
    }

    #[test]
    fn update_settings_applies_and_polls_now() {
        let source = FakeSource::default();
        let rig = single(source.clone());
        rig.next_profile();
        let new = Settings {
            profiles: vec![ProfileSettings {
                id: DEFAULT_ID.into(),
                config_dir: rig.home().join(".claude"),
                label: Some("Work".into()),
                ..ProfileSettings::default()
            }],
            ..Settings::default()
        };
        rig.poller.send(PollMsg::UpdateSettings(new));
        // The relabelled profile is announced, then polled as such.
        match rig.next() {
            PollEvent::Profiles(list) => assert_eq!(list[0].label, "Work"),
            other => panic!("expected Profiles, got {other:?}"),
        }
        rig.next_profile();
        assert_eq!(source.labels(), ["default", "Work"]);
    }

    #[test]
    fn errors_are_reported_and_polling_continues() {
        let rig = single(FakeSource::new(vec![Err("boom"), Ok(30.0)]));
        match rig.next_profile().1 {
            PollOutcome::Err(info) => {
                assert_eq!(info.message, "HTTP 500: boom");
                assert_eq!(info.kind, ErrorKind::Status);
            }
            PollOutcome::Ok(_) => panic!("expected error"),
        }
        rig.poller.send(PollMsg::RefreshNow);
        assert_eq!(ok_pct(rig.next_profile().1), 30.0);
    }

    #[test]
    fn panic_in_source_is_reported_not_fatal() {
        struct Panicky(u32);
        impl UsageSource for Panicky {
            fn fetch(&mut self, _: &Profile) -> Result<FetchOk, FetchError> {
                self.0 += 1;
                if self.0 == 1 {
                    // Guarded: the panic hook logs this but shows no box.
                    // (If not, this panics without "kaboom" and the test fails.)
                    assert!(crate::crash::is_recoverable());
                    panic!("kaboom");
                }
                Ok(fetch_ok(5.0))
            }
        }
        let rig = single(Panicky(0));
        match rig.next_profile().1 {
            PollOutcome::Err(info) => {
                assert!(info.message.contains("kaboom"), "{info:?}");
                assert_eq!(info.kind, ErrorKind::Internal);
            }
            PollOutcome::Ok(_) => panic!("expected error"),
        }
        rig.poller.send(PollMsg::RefreshNow);
        assert_eq!(ok_pct(rig.next_profile().1), 5.0);
    }

    #[test]
    fn drop_shuts_down_promptly() {
        let source = FakeSource::default();
        let rig = single(source.clone());
        rig.next_profile();
        let started = Instant::now();
        drop(rig.poller);
        // The interval is 60 s; Drop must not wait for it.
        assert!(started.elapsed() < Duration::from_millis(500));
        // No further polls after shutdown.
        assert_eq!(source.calls(), 1);
    }

    #[test]
    fn two_profiles_poll_into_their_own_history() {
        let (a, b) = (
            FakeSource::new(vec![Ok(10.0)]),
            FakeSource::new(vec![Ok(70.0)]),
        );
        let rig = Rig::start(vec![
            (DEFAULT_ID, Box::new(a.clone())),
            ("claude-secondary", Box::new(b.clone())),
        ]);
        assert_eq!(
            Rig::profile_ids(rig.next()),
            ["default", "claude-secondary"]
        );
        let mut got = BTreeMap::new();
        for _ in 0..2 {
            let (id, outcome) = rig.next_profile();
            got.insert(id.to_string(), ok_pct(outcome));
        }
        assert_eq!(
            got,
            BTreeMap::from([("claude-secondary".into(), 70.0), ("default".into(), 10.0)])
        );
        assert_eq!(a.calls() + b.calls(), 2);
        for (id, pct) in [("default", "10.0"), ("claude-secondary", "70.0")] {
            let paths = rig.root.for_profile(&ProfileId::new(id));
            let text = fs::read_to_string(paths.history_file()).unwrap();
            assert_eq!(text.lines().count(), 1, "{id}: {text}");
            assert!(text.contains(pct), "{id}: {text}");
            assert!(paths.latest_file().exists());
        }
        // The index lists both, without secrets.
        let index = storage::read_profiles_index(&rig.root).unwrap();
        let ids: Vec<_> = index.profiles.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["default", "claude-secondary"]);
        assert!(index.profiles[0].is_default);
        let text = fs::read_to_string(rig.root.profiles_index_file()).unwrap();
        assert!(!text.contains("sk-ant"));
    }

    #[test]
    fn a_blocked_profile_does_not_hold_up_another() {
        let (started_tx, started) = mpsc::channel();
        let (gate, gate_rx) = mpsc::channel();
        let calls = Arc::new(Mutex::new(0));
        let gated = GatedSource {
            started: started_tx,
            gate: gate_rx,
            calls: Arc::clone(&calls),
        };
        let other = FakeSource::new(vec![Ok(40.0), Ok(41.0)]);
        let rig = Rig::start(vec![
            (DEFAULT_ID, Box::new(gated)),
            ("claude-secondary", Box::new(other.clone())),
        ]);
        rig.next(); // Profiles
        started.recv_timeout(WAIT).expect("gated fetch started");
        // The default profile is stuck (say, on a Keychain prompt); the
        // other one's result still arrives.
        let (id, outcome) = rig.next_profile();
        assert_eq!(id.as_str(), "claude-secondary");
        assert_eq!(ok_pct(outcome), 40.0);
        // A refresh polls the free profile again but not the stuck one.
        rig.poller.send(PollMsg::RefreshNow);
        let (id, outcome) = rig.next_profile();
        assert_eq!((id.as_str(), ok_pct(outcome)), ("claude-secondary", 41.0));
        assert_eq!(*calls.lock().unwrap(), 1, "no second fetch while in flight");
        // Once unblocked, its result arrives.
        gate.send(()).unwrap();
        let (id, _) = rig.next_profile();
        assert_eq!(id.as_str(), DEFAULT_ID);
        // The refresh asked for while it was stuck polls it once more.
        started
            .recv_timeout(WAIT)
            .expect("refetched after the refresh");
        gate.send(()).unwrap();
        assert_eq!(rig.next_profile().0.as_str(), DEFAULT_ID);
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn rate_limited_profile_waits_for_retry_after() {
        let limited = FakeSource::rate_limited_then(Duration::from_secs(3600), 5.0);
        let other = FakeSource::default();
        let rig = Rig::start(vec![
            (DEFAULT_ID, Box::new(limited.clone())),
            ("claude-secondary", Box::new(other.clone())),
        ]);
        rig.next(); // Profiles
        let mut errors = 0;
        for _ in 0..2 {
            if let (id, PollOutcome::Err(info)) = rig.next_profile() {
                assert_eq!(id.as_str(), DEFAULT_ID);
                assert_eq!(info.kind, ErrorKind::RateLimited);
                errors += 1;
            }
        }
        assert_eq!(errors, 1);
        // A refresh polls the other profile but not the rate-limited one.
        rig.poller.send(PollMsg::RefreshNow);
        assert_eq!(rig.next_profile().0.as_str(), "claude-secondary");
        assert!(rig.no_event_for(Duration::from_millis(200)));
        assert_eq!(limited.calls(), 1);
        assert_eq!(other.calls(), 2);
    }

    #[test]
    fn rate_limited_profile_polls_again_once_due() {
        let limited = FakeSource::rate_limited_then(Duration::from_millis(100), 5.0);
        let rig = single(limited.clone());
        assert!(matches!(rig.next_profile().1, PollOutcome::Err(_)));
        // No refresh needed: the backoff ends and it polls by itself.
        assert_eq!(ok_pct(rig.next_profile().1), 5.0);
        assert_eq!(limited.calls(), 2);
    }

    #[test]
    fn discovery_adds_a_profile_mid_run() {
        let rig = single(FakeSource::default());
        assert_eq!(Rig::profile_ids(rig.next()), ["default"]);
        rig.next_profile();
        let added = discovered(&rig.home(), "claude-secondary");
        rig.found.lock().unwrap().push(added);
        rig.sources.lock().unwrap().insert(
            "claude-secondary".into(),
            Box::new(FakeSource::new(vec![Ok(33.0)])),
        );
        rig.poller.send(PollMsg::RefreshNow);
        assert_eq!(
            Rig::profile_ids(rig.next()),
            ["default", "claude-secondary"]
        );
        let mut got = BTreeMap::new();
        for _ in 0..2 {
            let (id, outcome) = rig.next_profile();
            got.insert(id.to_string(), ok_pct(outcome));
        }
        assert_eq!(got.get("claude-secondary"), Some(&33.0));
        let index = storage::read_profiles_index(&rig.root).unwrap();
        assert_eq!(index.profiles.len(), 2);
    }

    #[test]
    fn disabled_profile_is_not_polled() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let settings = Settings {
            profiles: vec![ProfileSettings {
                id: "claude-secondary".into(),
                config_dir: home.join(".claude-secondary"),
                enabled: false,
                ..ProfileSettings::default()
            }],
            ..Settings::default()
        };
        let off = FakeSource::default();
        let rig = Rig::start_in(
            tmp,
            vec![
                (DEFAULT_ID, Box::new(FakeSource::default())),
                ("claude-secondary", Box::new(off.clone())),
            ],
            settings,
        );
        assert_eq!(Rig::profile_ids(rig.next()), ["default"]);
        rig.next_profile();
        assert!(rig.no_event_for(Duration::from_millis(100)));
        assert_eq!(off.calls(), 0);
    }

    #[test]
    fn account_change_archives_history() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Paths::with_data_dir(tmp.path().join("data"));
        let default = root.for_profile(&ProfileId::new(DEFAULT_ID));
        fs::create_dir_all(default.data_dir()).unwrap();
        let old_line = format!(
            r#"{{"ts": {}, "five_hour": 99.0}}"#,
            Utc::now().timestamp() - 60
        );
        fs::write(default.history_file(), format!("{old_line}\n")).unwrap();
        // Last time, ~/.claude belonged to another account.
        let mut entry = profile(&tmp.path().join("home"), DEFAULT_ID).index_entry();
        entry.account_uuid = Some("0ldacc0u-nt".into());
        storage::write_profiles_index(&root, &ProfileIndex::new(vec![entry])).unwrap();

        let rig = Rig::start_in(
            tmp,
            vec![(DEFAULT_ID, Box::new(FakeSource::new(vec![Ok(3.0)])))],
            Settings::default(),
        );
        let d = ok_dashboard(rig.next_profile().1);
        // The old account's samples neither seed the burn history nor stay
        // in history.jsonl.
        assert_eq!(d.history.len(), 1);
        let archived =
            fs::read_to_string(default.data_dir().join("history.0ldacc0u.jsonl")).unwrap();
        assert!(archived.contains("99.0"));
        let fresh = fs::read_to_string(default.history_file()).unwrap();
        assert_eq!(fresh.lines().count(), 1);
        assert!(!fresh.contains("99.0"));
        let index = storage::read_profiles_index(&rig.root).unwrap();
        assert_eq!(
            index.profiles[0].account_uuid.as_deref(),
            Some("uuid-default")
        );
    }

    #[test]
    fn one_shot_setup_can_leave_an_account_change_to_the_gui() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = Paths::with_data_dir(tmp.path().join("data"));
        let default = root.for_profile(&ProfileId::new(DEFAULT_ID));
        fs::create_dir_all(default.data_dir()).unwrap();
        fs::write(default.history_file(), "{\"ts\": 1, \"five_hour\": 99.0}\n").unwrap();
        let mut entry = profile(&home, DEFAULT_ID).index_entry();
        entry.account_uuid = Some("0ldacc0u-nt".into());
        let index = ProfileIndex::new(vec![entry]);
        storage::write_profiles_index(&root, &index).unwrap();
        let deps = |home: PathBuf, cowork: PathBuf| PollerDeps {
            source_for: Box::new(|_: &Profile| {
                Box::new(FakeSource::default()) as Box<dyn UsageSource>
            }),
            discover: Box::new(move || vec![discovered(&home, DEFAULT_ID)]),
            cowork_dir: cowork,
        };

        let cowork = tmp.path().join("cowork");
        let rts = prepare_runtimes(
            deps(home.clone(), cowork.clone()),
            root.clone(),
            Settings::default(),
            AccountChange::Leave,
        );
        assert_eq!(rts.len(), 1);
        assert!(!default.data_dir().join("history.0ldacc0u.jsonl").exists());
        assert!(fs::read_to_string(default.history_file())
            .unwrap()
            .contains("99.0"));
        assert_eq!(storage::read_profiles_index(&root).unwrap(), index);

        prepare_runtimes(
            deps(home, cowork),
            root.clone(),
            Settings::default(),
            AccountChange::Archive,
        );
        assert!(default.data_dir().join("history.0ldacc0u.jsonl").exists());
        assert_eq!(
            storage::read_profiles_index(&root).unwrap().profiles[0]
                .account_uuid
                .as_deref(),
            Some("uuid-default")
        );
    }

    #[test]
    fn legacy_history_migrates_once_into_the_default_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Paths::with_data_dir(tmp.path().join("data"));
        fs::create_dir_all(root.data_dir()).unwrap();
        let line = format!(
            r#"{{"ts": {}, "five_hour": 12.0}}"#,
            Utc::now().timestamp() - 60
        );
        fs::write(root.history_file(), format!("{line}\n")).unwrap();

        let rig = Rig::start_in(
            tmp,
            vec![(DEFAULT_ID, Box::new(FakeSource::default()))],
            Settings::default(),
        );
        // Seeded from the copied legacy history, then this poll.
        let d = ok_dashboard(rig.next_profile().1);
        assert_eq!(d.history.len(), 2);
        assert!(rig.root.legacy_marker_file().exists());
        assert!(rig.root.history_file().exists(), "the original stays");

        // Next start: the marker stops a second copy.
        let Rig { poller, tmp, .. } = rig;
        drop(poller);
        let default = root.for_profile(&ProfileId::new(DEFAULT_ID));
        fs::remove_file(default.history_file()).unwrap();
        let rig = Rig::start_in(
            tmp,
            vec![(DEFAULT_ID, Box::new(FakeSource::default()))],
            Settings::default(),
        );
        let d = ok_dashboard(rig.next_profile().1);
        assert_eq!(d.history.len(), 1);
    }

    #[test]
    fn is_alive_until_shutdown() {
        let rig = single(FakeSource::default());
        rig.next_profile();
        assert!(rig.poller.is_alive());
        rig.poller.send(PollMsg::Shutdown);
        let deadline = Instant::now() + WAIT;
        while rig.poller.is_alive() {
            assert!(Instant::now() < deadline, "poller still alive");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn panicking_notify_does_not_kill_poller() {
        let tmp = tempfile::tempdir().unwrap();
        let guarded = Arc::new(AtomicBool::new(false));
        let guarded_flag = Arc::clone(&guarded);
        let home = tmp.path().join("home");
        let source = FakeSource::new(vec![Ok(1.0), Ok(2.0)]);
        let deps = PollerDeps {
            source_for: Box::new(move |_: &Profile| {
                Box::new(source.clone()) as Box<dyn UsageSource>
            }),
            discover: Box::new(move || vec![discovered(&home, DEFAULT_ID)]),
            cowork_dir: tmp.path().join("cw"),
        };
        let poller = Poller::spawn_with(
            deps,
            Paths::with_data_dir(tmp.path().join("data")),
            Settings::default(),
            move || {
                guarded_flag.store(crate::crash::is_recoverable(), Ordering::SeqCst);
                panic!("notify failed")
            },
        );
        let wait_profile = || {
            let deadline = Instant::now() + WAIT;
            loop {
                if let Some(PollEvent::Profile { outcome, .. }) = poller.try_recv() {
                    return outcome;
                }
                assert!(Instant::now() < deadline, "no event");
                thread::sleep(Duration::from_millis(5));
            }
        };
        assert_eq!(ok_pct(wait_profile()), 1.0);
        poller.send(PollMsg::RefreshNow);
        assert_eq!(ok_pct(wait_profile()), 2.0);
        assert!(poller.is_alive());
        assert!(
            guarded.load(Ordering::SeqCst),
            "notify ran outside the guard"
        );
    }

    struct GatedRig {
        rig: Rig,
        started: Receiver<()>,
        gate: Sender<()>,
        calls: Arc<Mutex<u32>>,
    }

    impl GatedRig {
        fn start() -> Self {
            let (started_tx, started) = mpsc::channel();
            let (gate, gate_rx) = mpsc::channel();
            let calls = Arc::new(Mutex::new(0));
            let rig = single(GatedSource {
                started: started_tx,
                gate: gate_rx,
                calls: Arc::clone(&calls),
            });
            rig.next(); // Profiles
            Self {
                rig,
                started,
                gate,
                calls,
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }

        fn wait_started(&self) {
            self.started.recv_timeout(WAIT).expect("fetch started");
        }

        fn release(&self) {
            self.gate.send(()).unwrap();
        }

        fn wait_event(&self) {
            self.rig.next_profile();
        }

        fn wait_dead(&self) {
            let deadline = Instant::now() + WAIT;
            while self.rig.poller.is_alive() {
                assert!(Instant::now() < deadline, "poller still alive");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn queued_refreshes_coalesce_into_one_poll() {
        let g = GatedRig::start();
        g.wait_started();
        for _ in 0..5 {
            g.rig.poller.send(PollMsg::RefreshNow);
        }
        g.release();
        g.wait_event();
        g.wait_started();
        g.release();
        g.wait_event();
        // No third poll: the five refreshes folded into one.
        assert!(g.started.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(g.calls(), 2);
    }

    #[test]
    fn queued_refreshes_then_shutdown_exit_without_polling() {
        let g = GatedRig::start();
        g.wait_started();
        for _ in 0..5 {
            g.rig.poller.send(PollMsg::RefreshNow);
        }
        g.rig.poller.send(PollMsg::Shutdown);
        g.release();
        g.wait_dead();
        assert_eq!(g.calls(), 1);
        let started = Instant::now();
        let GatedRig { rig, .. } = g;
        drop(rig.poller);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn drop_during_blocked_fetch_returns_promptly() {
        let g = GatedRig::start();
        g.wait_started();
        let started = Instant::now();
        let GatedRig {
            rig,
            gate,
            calls,
            started: _s,
        } = g;
        let Rig {
            poller, notified, ..
        } = rig;
        drop(poller);
        assert!(started.elapsed() < Duration::from_millis(100));
        // Let the abandoned fetch finish; nothing polls again.
        gate.send(()).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(notified.try_recv().is_err(), "no event after shutdown");
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
            breakdown: None,
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

    /// A runtime for the default profile under `tmp`, fetching from `source`.
    fn runtime(tmp: &tempfile::TempDir, root: &Paths, source: impl UsageSource) -> ProfileRuntime {
        ProfileRuntime::new(
            profile(&tmp.path().join("home"), DEFAULT_ID),
            root,
            &tmp.path().join("cw"),
            Box::new(source),
        )
    }

    #[test]
    fn poll_profile_persists_and_builds_dashboard() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Paths::with_data_dir(tmp.path().join("data"));
        let mut rt = runtime(&tmp, &root, FakeSource::new(vec![Ok(42.0)]));
        let d = poll_profile(&mut rt, &CoworkFilter::All).unwrap();

        assert_eq!(d.snapshot.five_hour_pct(), Some(42.0));
        assert_eq!(d.plan.as_deref(), Some("max"));
        assert_eq!(d.history.len(), 1);
        assert_eq!(d.fable_history, [(d.snapshot.fetched_at, None)]);
        // The persisted copy of this poll is replaced by the exact one.
        assert_eq!(
            d.seven_day_recent,
            [(d.snapshot.fetched_at, d.snapshot.seven_day_pct())]
        );
        assert_eq!(d.fable_recent, [(d.snapshot.fetched_at, None)]);
        assert_eq!(
            d.week_samples,
            [WeekSample {
                t: d.snapshot.fetched_at,
                seven_day: d.snapshot.seven_day_pct(),
                fable: None,
            }]
        );
        assert_eq!(d.week_end - d.week_start, ChronoDuration::days(7));
        // Under the profile's own dir, not the root.
        let own = root.for_profile(&ProfileId::new(DEFAULT_ID));
        assert_eq!(rt.paths, own);
        assert!(own.history_file().exists());
        assert!(own.latest_file().exists());
        assert!(!root.history_file().exists());
        assert_eq!(
            d.window_start,
            d.snapshot.five_hour_resets_at().unwrap() - ChronoDuration::hours(5)
        );
        assert!(d.burners.is_empty());
        assert_eq!(d.persist_error, None);
        // The just-appended sample is in the window samples.
        assert!(d.window_samples.iter().any(|s| s.five_hour == Some(42.0)));
    }

    #[test]
    fn poll_profile_fills_window_samples_from_one_history_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Paths::with_data_dir(tmp.path().join("data"));
        let paths = root.for_profile(&ProfileId::new(DEFAULT_ID));
        // FakeSource resets 2h after the fetch, so the window began ~3h ago.
        let now = Utc::now();
        let earlier = now - ChronoDuration::hours(1);
        let ts = |t: DateTime<Utc>| t.timestamp() as f64;
        fs::create_dir_all(paths.data_dir()).unwrap();
        let lines = [
            format!(
                r#"{{"ts": {}, "five_hour": 1.0, "fable_7d": 5.0}}"#,
                ts(now - ChronoDuration::hours(4))
            ),
            format!(
                r#"{{"ts": {}, "five_hour": 30.0, "fable_7d": 6.0}}"#,
                ts(earlier)
            ),
        ];
        fs::write(paths.history_file(), lines.join("\n") + "\n").unwrap();

        let mut rt = runtime(&tmp, &root, FakeSource::new(vec![Ok(42.0)]));
        let before = storage::history_reads();
        let d = poll_profile(&mut rt, &CoworkFilter::All).unwrap();
        assert_eq!(storage::history_reads() - before, 1, "one history read");

        let fetched = d.snapshot.fetched_at;
        assert_eq!(d.window_end, d.snapshot.five_hour_resets_at().unwrap());
        assert_eq!(d.window_end, d.window_start + ChronoDuration::hours(5));
        assert_eq!(
            d.window_samples,
            [
                WindowSample {
                    t: DateTime::from_timestamp(earlier.timestamp(), 0).unwrap(),
                    five_hour: Some(30.0),
                    fable: Some(6.0),
                },
                // This poll, once, at its exact fetch time.
                WindowSample {
                    t: fetched,
                    five_hour: Some(42.0),
                    fable: None,
                },
            ]
        );
    }

    #[test]
    fn persist_failure_is_reported_with_fresh_data() {
        let tmp = tempfile::tempdir().unwrap();
        // A regular file where the data directory should be.
        let blocker = tmp.path().join("data");
        fs::write(&blocker, b"not a dir").unwrap();
        let mut rt = runtime(
            &tmp,
            &Paths::with_data_dir(blocker),
            FakeSource::new(vec![Ok(7.0)]),
        );
        let d = poll_profile(&mut rt, &CoworkFilter::All).unwrap();
        assert_eq!(d.snapshot.five_hour_pct(), Some(7.0));
        let msg = d.persist_error.expect("persist error");
        assert!(msg.starts_with("couldn't save history to "), "{msg}");
        assert!(msg.contains("history.jsonl"), "{msg}");
    }

    /// A history file with the previous window's climb (to 80% at 2:19p)
    /// and a 0% sample after its 2:20p reset; returns the fake paths.
    fn history_across_a_reset(
        tmp: &tempfile::TempDir,
        reset: DateTime<Utc>,
    ) -> (Paths, SessionDirs) {
        let paths = Paths::with_data_dir(tmp.path().join("data"));
        let dirs = SessionDirs {
            claude_code: tmp.path().join("cc"),
            cowork: tmp.path().join("cw"),
        };
        let ts = |m: i64| (reset + ChronoDuration::minutes(m)).timestamp() as f64;
        fs::create_dir_all(paths.data_dir()).unwrap();
        let lines: Vec<String> = [(-120, 30.0), (-60, 60.0), (-1, 80.0), (10, 0.0)]
            .iter()
            .map(|(m, v)| format!(r#"{{"ts": {}, "five_hour": {v}}}"#, ts(*m)))
            .collect();
        fs::write(paths.history_file(), lines.join("\n") + "\n").unwrap();
        (paths, dirs)
    }

    fn five_hour_only(at: DateTime<Utc>, pct: f64, resets_at: Option<DateTime<Utc>>) -> Snapshot {
        Snapshot {
            fetched_at: at,
            windows: vec![UsageWindow {
                label: "5-hour".into(),
                utilization: Some(pct),
                resets_at,
            }],
            extra: None,
            breakdown: None,
        }
    }

    fn build(
        snapshot: Snapshot,
        paths: &Paths,
        dirs: &SessionDirs,
        now: DateTime<Utc>,
    ) -> Dashboard {
        build_dashboard(
            snapshot,
            &BurnHistory::default(),
            paths,
            dirs,
            &CoworkFilter::All,
            now,
        )
    }

    #[test]
    fn idle_window_charts_nothing_from_the_old_window() {
        let tmp = tempfile::tempdir().unwrap();
        let reset = Utc::now() - ChronoDuration::minutes(33);
        let (paths, dirs) = history_across_a_reset(&tmp, reset);
        let now = Utc::now();
        let d = build(five_hour_only(now, 0.0, None), &paths, &dirs, now);
        assert_eq!(d.five_hour_now(now).state, analytics::FiveHourState::Idle);
        assert!(d.window_samples.is_empty(), "{:?}", d.window_samples);
    }

    #[test]
    fn trailing_window_keeps_only_samples_after_the_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let reset = Utc::now() - ChronoDuration::minutes(33);
        let (paths, dirs) = history_across_a_reset(&tmp, reset);
        let now = Utc::now();
        let d = build(
            five_hour_only(now - ChronoDuration::seconds(1), 4.0, None),
            &paths,
            &dirs,
            now,
        );
        assert_eq!(
            d.five_hour_now(now).state,
            analytics::FiveHourState::Trailing
        );
        let values: Vec<_> = d.window_samples.iter().map(|s| s.five_hour).collect();
        assert_eq!(values, [Some(0.0), Some(4.0)]);
    }

    #[test]
    fn new_window_starts_empty_without_the_previous_ones_samples() {
        let tmp = tempfile::tempdir().unwrap();
        let reset = Utc::now() - ChronoDuration::minutes(33);
        let (paths, dirs) = history_across_a_reset(&tmp, reset);
        let now = Utc::now();
        // A new window started 3 minutes ago, back to back with the old
        // one's end even with its reset jittered 2 minutes early.
        let new_reset = reset + ChronoDuration::hours(5) - ChronoDuration::minutes(2);
        let d = build(
            five_hour_only(now, 2.0, Some(new_reset)),
            &paths,
            &dirs,
            now,
        );
        assert_eq!(d.window_start, new_reset - ChronoDuration::hours(5));
        let values: Vec<_> = d.window_samples.iter().map(|s| s.five_hour).collect();
        assert_eq!(values, [Some(0.0), Some(2.0)], "no 80% from before 2:20p");
    }

    #[test]
    fn current_sample_is_added_when_persist_failed_and_kept_in_order() {
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        let s = snap(now, Some(50.0), Some(7.0)); // window 9:00..14:00
        let ws = window_start(&s, now);
        let old = WindowSample {
            t: now - ChronoDuration::minutes(5),
            five_hour: Some(45.0),
            fable: None,
        };
        let got = with_current_sample(vec![old], &s, ws, ws + ChronoDuration::hours(5));
        let current = WindowSample {
            t: now,
            five_hour: Some(50.0),
            fable: Some(7.0),
        };
        assert_eq!(got, [old, current]);
        // A window that already ended (stale reset): not added.
        let (start, end) = (
            now - ChronoDuration::hours(6),
            now - ChronoDuration::hours(1),
        );
        assert!(with_current_sample(vec![], &s, start, end).is_empty());
        // Half-open like storage: the start is in, the end is out.
        assert_eq!(
            with_current_sample(vec![], &s, now, end + ChronoDuration::hours(9)),
            [current]
        );
        assert!(with_current_sample(vec![], &s, start, now).is_empty());
    }

    #[test]
    fn fetch_error_info_names_the_profile_dir() {
        let home = Path::new("/h");
        let info = |e: FetchError| FetchErrorInfo::for_dir(&e, Path::new("/h/.claude"), home);
        for e in [FetchError::TokenExpired, FetchError::Unauthorized] {
            let kind = e.kind();
            assert_eq!(
                info(e),
                FetchErrorInfo {
                    kind,
                    message: "token expired — run Claude Code in ~/.claude to refresh".into(),
                }
            );
        }
        let got = info(FetchError::NoCredentials(String::new()));
        assert_eq!(got.kind, ErrorKind::NoCredentials);
        assert_eq!(
            got.message,
            "no Claude Code login found in ~/.claude — run `claude` and sign in"
        );
        assert_eq!(
            info(FetchError::NoCredentials(
                "malformed credential: no accessToken".into()
            ))
            .message,
            "no Claude Code login found in ~/.claude — run `claude` and sign in \
             (malformed credential: no accessToken)"
        );
        let got = info(FetchError::StoreUnavailable("security exited 51".into()));
        assert_eq!(got.kind, ErrorKind::StoreUnavailable);
        assert_eq!(
            got.message,
            format!(
                "couldn't read the Claude Code login for ~/.claude — {} (security exited 51)",
                credentials::UNAVAILABLE_HINT
            )
        );
        assert!(!got.message.contains("sign in"));
        let got = info(FetchError::Forbidden {
            body: "denied".into(),
        });
        assert_eq!(got.kind, ErrorKind::Forbidden);
        assert_eq!(got.message, "403 — forbidden: denied");
        let got = info(FetchError::RateLimited {
            retry_after: Some(Duration::from_secs(120)),
        });
        assert_eq!(got.kind, ErrorKind::RateLimited);
        assert_eq!(got.message, "429 — rate limited; retry after 120s");
        // Outside home, the path as is.
        let other = FetchErrorInfo::for_dir(&FetchError::TokenExpired, Path::new("/opt/cc"), home);
        assert_eq!(
            other.message,
            "token expired — run Claude Code in /opt/cc to refresh"
        );
    }

    #[test]
    fn cowork_goes_to_the_owning_account() {
        let home = Path::new("/h");
        let all = vec![
            profile(home, DEFAULT_ID),
            profile(home, "claude-b"),
            profile(home, "claude-c"),
        ];
        assert_eq!(
            cowork_filter(&all[0], &all),
            CoworkFilter::Unmatched(vec!["uuid-claude-b".into(), "uuid-claude-c".into()])
        );
        assert_eq!(
            cowork_filter(&all[1], &all),
            CoworkFilter::Account("uuid-claude-b".into())
        );
        let mut unknown = all[2].clone();
        unknown.account = None;
        assert_eq!(cowork_filter(&unknown, &all), CoworkFilter::None);
        // Alone, the default profile takes everything.
        assert_eq!(
            cowork_filter(&all[0], &all[..1]),
            CoworkFilter::Unmatched(vec![])
        );
    }

    #[test]
    fn drain_coalesces_and_prioritizes_shutdown() {
        let plan = |p: &str| Settings {
            plan: p.into(),
            ..Settings::default()
        };
        let ctl = |m| Inbox::Control(m);
        let (tx, rx) = mpsc::channel();
        let mut fetched = Vec::new();
        for _ in 0..3 {
            tx.send(ctl(PollMsg::RefreshNow)).unwrap();
        }
        assert_eq!(
            drain(ctl(PollMsg::RefreshNow), &rx, &mut fetched),
            Wake::Poll(None)
        );
        assert!(rx.try_recv().is_err(), "queue drained");

        tx.send(ctl(PollMsg::UpdateSettings(plan("A")))).unwrap();
        tx.send(ctl(PollMsg::RefreshNow)).unwrap();
        tx.send(ctl(PollMsg::UpdateSettings(plan("B")))).unwrap();
        assert_eq!(
            drain(ctl(PollMsg::RefreshNow), &rx, &mut fetched),
            Wake::Poll(Some(plan("B")))
        );

        // Fetch results alone are not a refresh; they are collected.
        let result = |n| {
            Inbox::Fetched(Fetched {
                id: ProfileId::new(DEFAULT_ID),
                generation: n,
                source: Box::new(FakeSource::default()),
                result: Ok(Ok(fetch_ok(1.0))),
            })
        };
        tx.send(result(2)).unwrap();
        assert_eq!(drain(result(1), &rx, &mut fetched), Wake::Results);
        assert_eq!(fetched.len(), 2);

        tx.send(ctl(PollMsg::Shutdown)).unwrap();
        tx.send(ctl(PollMsg::RefreshNow)).unwrap();
        assert_eq!(
            drain(ctl(PollMsg::UpdateSettings(plan("C"))), &rx, &mut fetched),
            Wake::Exit
        );

        drop(tx);
        assert_eq!(
            drain(ctl(PollMsg::RefreshNow), &rx, &mut fetched),
            Wake::Exit
        );
    }

    // --- OAuthSource ----------------------------------------------------

    const TOKEN: &str = "sk-ant-oat01-FIXTURE-SECRET";
    const TOKEN2: &str = "sk-ant-oat01-FIXTURE-ROTATED";

    /// A credential JSON for `token`, expiring `expires_in` from now.
    fn cred_json(token: &str, expires_in: ChronoDuration) -> String {
        serde_json::json!({"claudeAiOauth": {
            "accessToken": token,
            "expiresAt": (Utc::now() + expires_in).timestamp_millis(),
            "subscriptionType": "max",
        }})
        .to_string()
    }

    /// A queued credential: its JSON, or the store's error.
    type CredResult = Result<String, fn() -> CredError>;

    /// Serves queued credential JSONs in turn, repeating the last; counts
    /// loads and remembers the locators asked for.
    struct FakeStore {
        creds: Mutex<VecDeque<CredResult>>,
        loads: Mutex<Vec<CredentialLocator>>,
    }

    impl FakeStore {
        fn new(creds: Vec<CredResult>) -> Arc<Self> {
            Arc::new(Self {
                creds: Mutex::new(creds.into()),
                loads: Mutex::new(Vec::new()),
            })
        }

        fn loads(&self) -> usize {
            self.loads.lock().unwrap().len()
        }
    }

    impl CredentialStore for FakeStore {
        fn load(&self, loc: &CredentialLocator) -> Result<OAuthCredential, CredError> {
            self.loads.lock().unwrap().push(loc.clone());
            let mut q = self.creds.lock().unwrap();
            let next = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().expect("a credential queued")
            };
            match next {
                Ok(json) => credentials::parse_credentials_json(&json),
                Err(e) => Err(e()),
            }
        }
    }

    /// Answers scripted results and records each bearer it was given.
    #[derive(Default)]
    struct FakeApi {
        script: VecDeque<Result<f64, FetchError>>,
        bearers: Vec<String>,
    }

    impl UsageApi for FakeApi {
        fn get_usage(&mut self, bearer: &str) -> Result<Snapshot, FetchError> {
            self.bearers.push(bearer.to_owned());
            let next = self.script.pop_front().unwrap_or(Ok(1.0));
            next.map(|pct| snap(Utc::now(), Some(pct), None))
        }
    }

    fn oauth(
        creds: Vec<CredResult>,
        script: Vec<Result<f64, FetchError>>,
    ) -> (OAuthSource<FakeApi>, Arc<FakeStore>) {
        let store = FakeStore::new(creds);
        let api = FakeApi {
            script: script.into(),
            ..FakeApi::default()
        };
        (OAuthSource::with_api(api, store.clone()), store)
    }

    fn test_profile() -> Profile {
        profile(Path::new("/h"), "claude-b")
    }

    #[test]
    fn oauth_caches_the_credential_and_reports_the_plan() {
        let hour = ChronoDuration::hours(1);
        let (mut src, store) = oauth(vec![Ok(cred_json(TOKEN, hour))], vec![]);
        let p = test_profile();
        let ok = src.fetch(&p).unwrap();
        assert_eq!(ok.plan.as_deref(), Some("max"));
        assert!(ok.token_expires_at.unwrap() > Utc::now());
        src.fetch(&p).unwrap();
        assert_eq!(store.loads(), 1, "read once while current");
        assert_eq!(src.api.bearers, [TOKEN, TOKEN]);
        assert_eq!(store.loads.lock().unwrap()[0], p.locator());
        // A new Keychain override is a new locator: read again.
        let mut moved = p.clone();
        moved.keychain_override = Some("elsewhere".into());
        src.fetch(&moved).unwrap();
        assert_eq!(store.loads(), 2);
    }

    #[test]
    fn oauth_expired_token_fails_without_a_request() {
        let (mut src, store) = oauth(
            vec![Ok(cred_json(TOKEN, -ChronoDuration::minutes(1)))],
            vec![],
        );
        let e = src.fetch(&test_profile()).unwrap_err();
        assert!(matches!(e, FetchError::TokenExpired), "{e:?}");
        assert!(src.api.bearers.is_empty(), "no HTTP request");
        // Expired: re-read on the next fetch in case Claude Code refreshed.
        src.fetch(&test_profile()).unwrap_err();
        assert_eq!(store.loads(), 2);
        let info = FetchErrorInfo::for_profile(&e, &test_profile());
        assert!(!info.message.contains("FIXTURE") && !format!("{e:?}").contains("FIXTURE"));
    }

    #[test]
    fn oauth_401_rereads_and_retries_with_a_new_token() {
        let hour = ChronoDuration::hours(1);
        let (mut src, store) = oauth(
            vec![Ok(cred_json(TOKEN, hour)), Ok(cred_json(TOKEN2, hour))],
            vec![Err(FetchError::Unauthorized), Ok(9.0)],
        );
        let ok = src.fetch(&test_profile()).unwrap();
        assert_eq!(ok.snapshot.five_hour_pct(), Some(9.0));
        assert_eq!(src.api.bearers, [TOKEN, TOKEN2]);
        assert_eq!(store.loads(), 2);
        // The new token is cached.
        src.fetch(&test_profile()).unwrap();
        assert_eq!(store.loads(), 2);
        assert_eq!(src.api.bearers.last().unwrap(), TOKEN2);
    }

    #[test]
    fn oauth_401_with_the_same_token_is_unauthorized() {
        let (mut src, store) = oauth(
            vec![Ok(cred_json(TOKEN, ChronoDuration::hours(1)))],
            vec![Err(FetchError::Unauthorized)],
        );
        let e = src.fetch(&test_profile()).unwrap_err();
        assert!(matches!(e, FetchError::Unauthorized), "{e:?}");
        assert_eq!(src.api.bearers.len(), 1, "no retry with the same token");
        assert_eq!(store.loads(), 2, "re-read once");
        let info = FetchErrorInfo::for_profile(&e, &test_profile());
        assert!(info
            .message
            .starts_with("token expired — run Claude Code in "));
    }

    #[test]
    fn oauth_maps_credential_errors() {
        let (mut src, _) = oauth(vec![Err(|| CredError::NotFound("no item".into()))], vec![]);
        let e = src.fetch(&test_profile()).unwrap_err();
        assert!(
            matches!(&e, FetchError::NoCredentials(why) if why.is_empty()),
            "{e:?}"
        );
        assert!(src.api.bearers.is_empty());

        let (mut src, _) = oauth(
            vec![Err(|| CredError::Malformed("no accessToken".into()))],
            vec![],
        );
        assert_eq!(
            src.fetch(&test_profile()).unwrap_err().to_string(),
            "no Claude Code login found (malformed credential: no accessToken)"
        );

        // A locked or denied Keychain is not a missing login.
        let (mut src, _) = oauth(
            vec![Err(|| CredError::Unavailable("locked".into()))],
            vec![],
        );
        let e = src.fetch(&test_profile()).unwrap_err();
        assert!(
            matches!(&e, FetchError::StoreUnavailable(why) if why == "locked"),
            "{e:?}"
        );
        assert_eq!(e.kind(), ErrorKind::StoreUnavailable);
    }

    #[test]
    fn oauth_rereads_when_the_account_changes() {
        let hour = ChronoDuration::hours(1);
        let (mut src, store) = oauth(
            vec![
                Ok(cred_json(TOKEN, hour)),
                Ok(cred_json(TOKEN2, hour)),
                Err(|| CredError::NotFound("logged out".into())),
            ],
            vec![],
        );
        let p = test_profile();
        src.fetch(&p).unwrap();
        src.fetch(&p).unwrap();
        assert_eq!(store.loads(), 1, "same account: cached");
        // /login with another account in the same dir (same Keychain item).
        let mut other = p.clone();
        other.account.as_mut().unwrap().account_uuid = Some("uuid-other".into());
        src.fetch(&other).unwrap();
        assert_eq!(store.loads(), 2);
        assert_eq!(src.api.bearers, [TOKEN, TOKEN, TOKEN2]);
        // /logout: no account any more, so the cached token is not used.
        let mut out = other.clone();
        out.account = None;
        let e = src.fetch(&out).unwrap_err();
        assert!(matches!(e, FetchError::NoCredentials(_)), "{e:?}");
        assert_eq!(src.api.bearers.len(), 3, "no request after logout");
    }

    /// One config dir's Keychain item, which `/login` overwrites.
    struct SwitchStore(Mutex<&'static str>);

    impl CredentialStore for SwitchStore {
        fn load(&self, _: &CredentialLocator) -> Result<OAuthCredential, CredError> {
            let token = *self.0.lock().unwrap();
            credentials::parse_credentials_json(&cred_json(token, ChronoDuration::hours(1)))
        }
    }

    /// The usage endpoint: 11 % for [`TOKEN`]'s account, 22 % for any
    /// other; records the bearers.
    #[derive(Clone, Default)]
    struct SharedApi(Arc<Mutex<Vec<String>>>);

    impl UsageApi for SharedApi {
        fn get_usage(&mut self, bearer: &str) -> Result<Snapshot, FetchError> {
            self.0.lock().unwrap().push(bearer.to_owned());
            let pct = if bearer == TOKEN { 11.0 } else { 22.0 };
            Ok(snap(Utc::now(), Some(pct), None))
        }
    }

    /// An [`Engine`] driven by hand over real [`OAuthSource`]s for the
    /// default profile, signed in to `uuid-default` with [`TOKEN`].
    struct EngineRig {
        engine: Engine,
        results: Receiver<Inbox>,
        store: Arc<SwitchStore>,
        api: SharedApi,
        found: Arc<Mutex<Vec<Discovered>>>,
        root: Paths,
        home: PathBuf,
        _tmp: tempfile::TempDir,
    }

    impl EngineRig {
        fn start(ids: &[&str]) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let root = Paths::with_data_dir(tmp.path().join("data"));
            let store = Arc::new(SwitchStore(Mutex::new(TOKEN)));
            let api = SharedApi::default();
            let found: Vec<Discovered> = ids.iter().map(|id| discovered(&home, id)).collect();
            let found = Arc::new(Mutex::new(found));
            let deps = PollerDeps {
                source_for: {
                    let (store, api) = (Arc::clone(&store), api.clone());
                    Box::new(move |_: &Profile| {
                        let store: Arc<dyn CredentialStore> = store.clone();
                        Box::new(OAuthSource::with_api(api.clone(), store)) as Box<dyn UsageSource>
                    })
                },
                discover: {
                    let found = Arc::clone(&found);
                    Box::new(move || found.lock().unwrap().clone())
                },
                cowork_dir: tmp.path().join("cowork"),
            };
            let (tx, results) = mpsc::channel();
            let engine = Engine::new(deps, root.clone(), Settings::default(), tx);
            Self {
                engine,
                results,
                store,
                api,
                found,
                root,
                home,
                _tmp: tmp,
            }
        }

        /// `/login` with another account in `~/.claude`.
        fn switch_account(&self) {
            *self.store.0.lock().unwrap() = TOKEN2;
            let mut found = self.found.lock().unwrap();
            let account = found[0].account.as_mut().unwrap();
            account.account_uuid = Some("uuid-new".into());
            account.email = Some("new@example.com".into());
        }

        /// Fetch results as they arrive, until `n` or a quiet second.
        fn results(&self, n: usize) -> Vec<Fetched> {
            let mut out = Vec::new();
            while out.len() < n {
                match self.results.recv_timeout(WAIT) {
                    Ok(Inbox::Fetched(f)) => out.push(f),
                    Ok(Inbox::Control(_)) => {}
                    Err(_) => break,
                }
            }
            out
        }

        /// Fold in every result, returning the 5-hour % of each recorded.
        fn complete_all(&mut self, results: Vec<Fetched>) -> Vec<f64> {
            results
                .into_iter()
                .filter_map(|f| self.engine.complete(f, Instant::now()))
                .map(|event| match event {
                    PollEvent::Profile { outcome, .. } => ok_pct(outcome),
                    other => panic!("expected Profile, got {other:?}"),
                })
                .collect()
        }

        fn default_paths(&self) -> Paths {
            self.root.for_profile(&ProfileId::new(DEFAULT_ID))
        }

        fn ids(&self) -> Vec<String> {
            let rts = &self.engine.runtimes;
            rts.iter().map(|r| r.profile.id.to_string()).collect()
        }
    }

    #[test]
    fn account_switch_in_the_same_dir_fetches_with_the_new_token() {
        let mut rig = EngineRig::start(&[DEFAULT_ID]);
        rig.engine.reconcile(Instant::now());
        rig.engine.dispatch(Instant::now());
        let first = rig.results(1);
        assert_eq!(rig.complete_all(first), [11.0]);

        rig.switch_account();
        rig.engine.reconcile(Instant::now());
        rig.engine.refresh_all();
        rig.engine.dispatch(Instant::now());
        let second = rig.results(1);
        assert_eq!(rig.complete_all(second), [22.0]);
        assert_eq!(rig.api.0.lock().unwrap().last().unwrap(), TOKEN2);
        let paths = rig.default_paths();
        let fresh = fs::read_to_string(paths.history_file()).unwrap();
        assert!(fresh.contains("22.0") && !fresh.contains("11.0"), "{fresh}");
        let archived = fs::read_to_string(paths.data_dir().join("history.uuid-def.jsonl"));
        assert!(archived.unwrap().contains("11.0"));
    }

    #[test]
    fn old_accounts_in_flight_fetch_never_lands_in_the_new_history() {
        let mut rig = EngineRig::start(&[DEFAULT_ID]);
        rig.engine.reconcile(Instant::now());
        // A fetch for the old account is in flight (its result unread)...
        rig.engine.dispatch(Instant::now());
        // ...when the dir is signed in to another account.
        rig.switch_account();
        rig.engine.reconcile(Instant::now());
        // The new account is polled at once, on a fresh source.
        rig.engine.dispatch(Instant::now());
        let results = rig.results(2);
        assert_eq!(results.len(), 2, "a fetch for the new account started");
        // Whichever token the old fetch read, its result is discarded.
        assert_eq!(rig.complete_all(results), [22.0]);
        let fresh = fs::read_to_string(rig.default_paths().history_file()).unwrap();
        assert_eq!(fresh.lines().count(), 1, "{fresh}");
        assert!(!fresh.contains("11.0"), "{fresh}");
        // The runtime polls on afterwards (its source is not lost).
        rig.engine.refresh_all();
        rig.engine.dispatch(Instant::now());
        let next = rig.results(1);
        assert_eq!(rig.complete_all(next), [22.0]);
    }

    #[test]
    fn a_profile_missing_from_one_discovery_keeps_polling() {
        let mut rig = EngineRig::start(&[DEFAULT_ID, "claude-b"]);
        rig.engine.reconcile(Instant::now());
        assert_eq!(rig.ids(), ["default", "claude-b"]);
        // Its .claude.json is unreadable for a moment (mid-write).
        rig.found.lock().unwrap().truncate(1);
        rig.engine.reconcile(Instant::now());
        assert_eq!(rig.ids(), ["default", "claude-b"], "runtime kept");
        let b = &rig.engine.runtimes[1].profile;
        assert_eq!(b.config_dir, rig.home.join(".claude-b"));
        assert_eq!(b.locator(), profile(&rig.home, "claude-b").locator());
        assert!(rig
            .engine
            .settings
            .profiles
            .iter()
            .any(|p| p.id == "claude-b"));
    }

    #[test]
    fn session_dirs_for_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let p = profile(tmp.path(), DEFAULT_ID);
        let dirs = SessionDirs::for_profile(&p, Path::new("/cw"));
        assert_eq!(
            dirs.claude_code,
            tmp.path().join(".claude").join("projects")
        );
        assert_eq!(dirs.cowork, Path::new("/cw"));
    }
}
