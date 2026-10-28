//! Stdio MCP server exposing the latest usage snapshots to a Claude agent.
//!
//! Ported from the Python mcp_server.py (removed; see git history). The
//! monitor writes each profile's latest.json every poll; this server reads
//! them on demand and offers `get_usage`, `wait` and `wait_test`. The
//! calling Claude Code profile is the one whose config dir matches the
//! server's `CLAUDE_CONFIG_DIR` (unset: the default profile); `get_usage`
//! leads with it and lists the others, and `wait` follows its 5-hour window.
//! It does no network or auth of its own. The JSON logic lives in plain
//! functions taking `&Paths` and `now`, so it is testable without the MCP
//! runtime; the tokio runtime exists only inside [`run_stdio`].

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::paths::{self, Paths};
use crate::profiles::{self, Discovered, Profile, ProfileId, ProfileIndexEntry};
use crate::storage;

pub const STALE_AFTER_ENV: &str = "CLAUDE_USAGE_MCP_STALE_AFTER";
pub const MAX_BLOCK_ENV: &str = "CLAUDE_USAGE_MCP_MAX_BLOCK";
pub const TEST_MAX_BLOCK_ENV: &str = "CLAUDE_USAGE_MCP_TEST_MAX_BLOCK";
/// Set by Claude Code for every profile but the default (`~/.claude`).
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Default `wait_test` duration: just over `wait`'s 50s cap.
pub const DEFAULT_TEST_SECONDS: i64 = 90;

/// Tunables and the calling profile, read once from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    /// Snapshot older than this (seconds) is flagged stale.
    pub stale_after: i64,
    /// Max seconds a single `wait` call blocks (at least 1).
    pub max_block: i64,
    /// Hard ceiling for `wait_test` (at least 1).
    pub test_max_block: i64,
    /// The calling Claude Code's `CLAUDE_CONFIG_DIR`; `None` when unset or
    /// empty (the default profile).
    pub config_dir: Option<String>,
    /// Where profiles are discovered when `profiles.json` is unreadable.
    pub home: PathBuf,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            stale_after: 180,
            max_block: 50,
            test_max_block: 600,
            config_dir: None,
            home: paths::home_dir(),
        }
    }
}

impl McpConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build from a variable lookup. Unparseable values fall back to the
    /// default (with a note on stderr); the block limits are clamped to >= 1.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        let int = |key: &str, default: i64| match lookup(key) {
            None => default,
            Some(raw) => raw.trim().parse().unwrap_or_else(|_| {
                eprintln!("claude-usage mcp: ignoring invalid {key}={raw:?}, using {default}");
                default
            }),
        };
        Self {
            stale_after: int(STALE_AFTER_ENV, d.stale_after),
            max_block: int(MAX_BLOCK_ENV, d.max_block).max(1),
            test_max_block: int(TEST_MAX_BLOCK_ENV, d.test_max_block).max(1),
            config_dir: lookup(CONFIG_DIR_ENV).filter(|v| !v.is_empty()),
            home: d.home,
        }
    }
}

fn parse_dt(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = v?.as_str()?;
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00"))
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Python `int((a - b).total_seconds())`: truncates toward zero.
fn secs_between(later: DateTime<Utc>, earlier: DateTime<Utc>) -> i64 {
    let d = later - earlier;
    d.num_microseconds()
        .map_or_else(|| d.num_seconds(), |us| us / 1_000_000)
}

/// Python `str(float)` for the values we print: always shows a decimal.
fn py_float(f: f64) -> String {
    if f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Compact time-until string, e.g. '2d 4h', '1h 09m', '8m 30s', '14s'.
pub fn humanize_until(dt: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    let secs = secs_between(dt?, now);
    if secs <= 0 {
        return Some("now".into());
    }
    let (h, rem) = (secs / 3600, secs % 3600);
    let (m, s) = (rem / 60, rem % 60);
    Some(if h >= 24 {
        format!("{}d {}h", h / 24, h % 24)
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    })
}

/// `(recommendation, summary)` keyed off whichever window is closest to its
/// cap (the first one, on ties).
pub fn advise(windows: &[Value], stale: bool) -> (String, String) {
    let mut top: Option<(&Value, f64)> = None;
    for w in windows {
        if let Some(pct) = w.get("utilization_pct").and_then(Value::as_f64) {
            if top.is_none_or(|(_, best)| pct > best) {
                top = Some((w, pct));
            }
        }
    }
    let Some((top, pct)) = top else {
        return (
            "No utilization data in the latest snapshot.".into(),
            "No usage data available.".into(),
        );
    };
    let label = top.get("label").and_then(Value::as_str).unwrap_or("None");
    let rin = top
        .get("resets_in_human")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");

    let mut rec = if pct >= 95.0 {
        format!(
            "CRITICAL: {label} at {pct:.0}%. You are at/near the cap. \
             Throttle now — call wait() in a loop until it resets (in {rin}), \
             or pause heavy work."
        )
    } else if pct >= 80.0 {
        format!(
            "WARNING: {label} at {pct:.0}%. Approaching the cap; consider \
             slowing down or batching work. Resets in {rin}."
        )
    } else if pct >= 50.0 {
        format!("{label} at {pct:.0}%. Headroom is fine but worth watching. Resets in {rin}.")
    } else {
        format!("{label} at {pct:.0}%. Usage is low; no need to throttle.")
    };
    if stale {
        rec = format!("(data may be stale — monitor possibly not running) {rec}");
    }
    let summary = format!("{label} {pct:.0}% · resets in {rin}");
    (rec, summary)
}

// --- Profiles ---------------------------------------------------------------

/// The calling profile and the rest, in index order.
struct Calling {
    own: ProfileIndexEntry,
    others: Vec<ProfileIndexEntry>,
    /// Why `own` is the default profile despite `CLAUDE_CONFIG_DIR`.
    note: Option<String>,
}

/// The known profiles: `profiles.json`, or (missing, corrupt or empty)
/// discovery under `home`, whose ids match what the monitor writes under.
fn profile_index(root: &Paths, home: &Path) -> Vec<ProfileIndexEntry> {
    match storage::read_profiles_index(root) {
        Some(index) if !index.profiles.is_empty() => index.profiles,
        _ => profiles::polled(&[], &profiles::discover(home))
            .iter()
            .map(Profile::index_entry)
            .collect(),
    }
}

/// A stand-in default profile for when none is known (fresh install, or
/// an older build that wrote only the root latest.json).
fn synthetic_default(home: &Path) -> ProfileIndexEntry {
    let found = Discovered {
        id: ProfileId::new(profiles::DEFAULT_ID),
        config_dir: home.join(".claude"),
        config_dir_env: None,
        account: None,
    };
    profiles::polled(&[], &[found])[0].index_entry()
}

/// Split the known profiles into the caller's (by `cfg.config_dir`, else
/// the default) and the others.
fn calling_profile(root: &Paths, cfg: &McpConfig) -> Calling {
    let mut index = profile_index(root, &cfg.home);
    let env = cfg.config_dir.as_deref();
    let matched =
        profiles::match_env(&index, env).and_then(|m| index.iter().position(|e| e.id == m.id));
    let (pos, note) = match matched {
        Some(i) => (Some(i), None),
        None => (
            index.iter().position(|e| e.is_default),
            env.map(|dir| {
                format!(
                    "{CONFIG_DIR_ENV}={dir:?} matches no known profile, so this \
                     reports the default profile. Open the claude-usage app so it \
                     discovers the profile."
                )
            }),
        ),
    };
    let own = match pos {
        Some(i) => index.remove(i),
        None => synthetic_default(&cfg.home),
    };
    Calling {
        own,
        others: index,
        note,
    }
}

/// Where a profile's latest.json lives. The default profile falls back to
/// the root one an older build wrote, until the monitor writes its own.
fn snapshot_paths(root: &Paths, entry: &ProfileIndexEntry) -> Paths {
    let own = root.for_profile(&entry.id);
    if (entry.is_default || entry.id.is_default()) && !own.latest_file().exists() {
        root.clone()
    } else {
        own
    }
}

fn profile_json(entry: &ProfileIndexEntry) -> Value {
    json!({
        "id": entry.id.as_str(),
        "label": entry.label,
        "email": entry.email,
        "config_dir": entry.config_dir.display().to_string(),
    })
}

/// `{"profile": ...}` followed by that profile's usage fields.
fn with_profile(entry: &ProfileIndexEntry, usage: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("profile".into(), profile_json(entry));
    if let Value::Object(fields) = usage {
        m.extend(fields);
    }
    m
}

// --- Usage -------------------------------------------------------------------

/// The structured "most recent ping" view of the calling profile (the
/// top-level fields), plus `profile`, an optional `profile_note`, and the
/// same view of every other profile under `other_profiles`. `root` is the
/// app's data dir, holding profiles.json.
pub fn current_usage(root: &Paths, cfg: &McpConfig, now: DateTime<Utc>) -> Value {
    let calling = calling_profile(root, cfg);
    let usage = |e: &ProfileIndexEntry| profile_usage(&snapshot_paths(root, e), cfg, now);
    let mut result = with_profile(&calling.own, usage(&calling.own));
    if let Some(note) = calling.note {
        result.insert("profile_note".into(), note.into());
    }
    let others: Vec<Value> = calling
        .others
        .iter()
        .map(|e| Value::Object(with_profile(e, usage(e))))
        .collect();
    result.insert("other_profiles".into(), others.into());
    Value::Object(result)
}

/// One profile's view, built from the latest.json under `paths`.
fn profile_usage(paths: &Paths, cfg: &McpConfig, now: DateTime<Utc>) -> Value {
    let data = storage::read_latest_snapshot(paths).and_then(|v| match v {
        Value::Object(m) if !m.is_empty() => Some(m),
        _ => None,
    });
    let Some(data) = data else {
        return json!({
            "available": false,
            "stale": true,
            "note": "No usage snapshot found. Start the monitor to produce one: \
                     run the claude-usage app (or `just start`).",
            "windows": [],
            "five_hour": null,
            "extra": null,
            "seven_day_breakdown": null,
            "recommendation": "No data yet — the monitor has not written a snapshot.",
            "summary": "No usage data available.",
        });
    };

    let fetched_at = parse_dt(data.get("fetched_at"));
    let age = fetched_at.map(|f| secs_between(now, f));
    let stale = age.is_none_or(|a| a > cfg.stale_after);

    let windows: Vec<Value> = data
        .get("windows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|w| {
            let field = |k: &str| w.get(k).cloned().unwrap_or(Value::Null);
            let resets_at = parse_dt(w.get("resets_at"));
            json!({
                "label": field("label"),
                "utilization_pct": field("utilization"),
                "resets_at": field("resets_at"),
                "resets_in_seconds": resets_at.map(|r| secs_between(r, now).max(0)),
                "resets_in_human": humanize_until(resets_at, now),
            })
        })
        .collect();

    let five_hour = windows
        .iter()
        .find(|w| w.get("label").and_then(Value::as_str) == Some("5-hour"))
        .cloned()
        .unwrap_or(Value::Null);
    let (recommendation, summary) = advise(&windows, stale);

    let mut result = Map::new();
    result.insert("available".into(), true.into());
    result.insert(
        "as_of".into(),
        data.get("fetched_at").cloned().unwrap_or(Value::Null),
    );
    result.insert("age_seconds".into(), age.into());
    result.insert("stale".into(), stale.into());
    result.insert("windows".into(), windows.into());
    result.insert("five_hour".into(), five_hour);
    result.insert(
        "extra".into(),
        data.get("extra").cloned().unwrap_or(Value::Null),
    );
    result.insert(
        "seven_day_breakdown".into(),
        data.get("seven_day_breakdown")
            .cloned()
            .unwrap_or(Value::Null),
    );
    result.insert("recommendation".into(), recommendation.into());
    result.insert("summary".into(), summary.into());
    if stale {
        let age = age.map_or_else(|| "None".to_owned(), |a| a.to_string());
        result.insert(
            "note".into(),
            format!(
                "Snapshot is {age}s old (> {}s threshold). The monitor \
                 may not be running; values reflect the last successful ping.",
                cfg.stale_after
            )
            .into(),
        );
    }
    Value::Object(result)
}

fn five_hour_field(usage: &Value, key: &str) -> Value {
    usage
        .get("five_hour")
        .and_then(|f| f.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

/// First half of the `wait` tool: either a preview (nothing sleeps) or how
/// long to sleep before calling [`wait_finish`].
#[derive(Debug, Clone, PartialEq)]
pub enum WaitPlan {
    Preview(Value),
    Sleep { block_seconds: i64 },
}

/// `seconds` of `None` means the tool's default, `cfg.max_block`. Follows
/// the calling profile's 5-hour window (see [`current_usage`]).
pub fn wait_begin(
    root: &Paths,
    cfg: &McpConfig,
    seconds: Option<i64>,
    confirm: bool,
    now: DateTime<Utc>,
) -> WaitPlan {
    let seconds = seconds.unwrap_or(cfg.max_block).max(0);
    let block = if seconds != 0 {
        seconds.min(cfg.max_block)
    } else {
        cfg.max_block
    };
    if confirm {
        return WaitPlan::Sleep {
            block_seconds: block,
        };
    }

    let usage = current_usage(root, cfg, now);
    let reset_in = five_hour_field(&usage, "resets_in_seconds").as_i64();
    let reset_at = five_hour_field(&usage, "resets_at");
    let cycles = reset_in
        .filter(|&r| r != 0 && block != 0)
        .map(|r| (r + block - 1) / block);

    let mut msg = format!("This will BLOCK for up to {block}s per call. ");
    if let Some(r) = reset_in {
        msg += &format!("The 5-hour window resets in ~{r}s");
        if let Some(c) = cycles.filter(|&c| c != 0) {
            msg += &format!(" (~{c} call(s) of {block}s)");
        }
        msg += ". ";
    }
    msg += "Re-call wait(seconds=..., confirm=true) to actually wait. Each \
            call blocks only a bounded amount and then returns, so you are \
            never trapped — loop until 'done' is true.";

    WaitPlan::Preview(json!({
        "blocked": false,
        "preview": true,
        "profile": usage["profile"]["id"],
        "would_block_seconds": block,
        "requested_seconds": seconds,
        "max_block_seconds": cfg.max_block,
        "five_hour_reset_at": reset_at,
        "five_hour_resets_in_seconds": reset_in,
        "estimated_cycles_to_reset": cycles,
        "message": msg,
    }))
}

/// Second half of a confirmed `wait`: report after sleeping `slept_seconds`.
pub fn wait_finish(root: &Paths, cfg: &McpConfig, slept_seconds: f64, now: DateTime<Utc>) -> Value {
    let after = current_usage(root, cfg, now);
    let remaining = five_hour_field(&after, "resets_in_seconds").as_i64();
    let done = remaining.is_some_and(|r| r <= 0);

    let next_step = match remaining {
        _ if done => "The 5-hour window has reset — you can resume normal work.".to_owned(),
        Some(r) => format!(
            "~{r}s remain until the 5-hour reset. Call \
             wait(seconds={r}, confirm=true) again to keep waiting."
        ),
        None => "Reset time unknown (monitor may not be running). Call \
                 get_usage() to check, then decide whether to wait more."
            .to_owned(),
    };

    json!({
        "blocked": true,
        "profile": after["profile"]["id"],
        "slept_seconds": round1(slept_seconds),
        "five_hour_reset_at": five_hour_field(&after, "resets_at"),
        "five_hour_resets_in_seconds": remaining,
        "five_hour_utilization_pct": five_hour_field(&after, "utilization_pct"),
        "done": done,
        "next_step": next_step,
        "usage": after,
    })
}

fn round1(f: f64) -> f64 {
    (f * 10.0).round() / 10.0
}

/// `wait_test` sizing: `(requested, block)` with the request clamped to
/// `[0, cfg.test_max_block]`.
pub fn wait_test_block(cfg: &McpConfig, seconds: i64) -> (i64, i64) {
    let requested = seconds.max(0);
    (requested, requested.min(cfg.test_max_block))
}

/// `wait_test` report after sleeping `slept_seconds` of `block`.
pub fn wait_test_result(cfg: &McpConfig, requested: i64, block: i64, slept_seconds: f64) -> Value {
    let slept = round1(slept_seconds);
    json!({
        "requested_seconds": requested,
        "blocked_seconds": block,
        "capped_to_ceiling": block < requested,
        "test_max_block_seconds": cfg.test_max_block,
        "slept_seconds": slept,
        "result": format!(
            "Returned after blocking ~{}s. If you can read this, your MCP \
             client's single tool-call timeout is at least {block}s. If instead \
             the call errored or timed out, the client's limit is below {block}s — \
             keep wait()'s CLAUDE_USAGE_MCP_MAX_BLOCK under that.",
            py_float(slept)
        ),
    })
}

// --- MCP server ------------------------------------------------------------

mod server {
    use std::time::{Duration, Instant};

    use chrono::Utc;
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::CallToolResult;
    use rmcp::{tool, tool_handler, tool_router, ServerHandler};
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;

    const GET_USAGE_DESC: &str = "\
Report the calling Claude Code profile's current claude.ai usage, plus
other profiles, from the most recent monitor ping.

Read-only and non-blocking. Use this to decide whether to slow down or keep
going. The top-level fields describe YOUR profile (the account this Claude
Code session runs under, identified by `profile`): for each usage window
(5-hour, 7-day, Fable, ...) utilization percent, when it resets, and a
human-readable time-to-reset — plus the data's age, a `stale` flag (true if
the monitor isn't updating), the extra-usage block, the 7-day per-model
breakdown, and a `recommendation`/`summary` you can act on.
`other_profiles` lists the same view for every other signed-in account.

Data is whatever the running claude-usage monitor app last fetched; if
the monitor isn't running the result is marked stale.";

    const WAIT_DESC: &str = "\
Throttle yourself by sleeping in short, bounded cycles near a usage cap.
Follows the calling Claude Code profile's 5-hour window (other profiles'
usage is in get_usage()).

BLOCKS the call while it sleeps, but only up to ~50s per call (MAX_BLOCK_SEC)
— deliberately short so no MCP client times out and you can never get
trapped in a long sleep. To wait longer (e.g. until the 5-hour window
resets), call this repeatedly until the result's `done` is true.

Two-step, so you never block by accident:
  - confirm=false (default): PREVIEW only. Nothing sleeps. Returns how long
    a real call would block and roughly how many cycles reaching the reset
    would take.
  - confirm=true: actually sleep one bounded cycle (min(seconds,
    MAX_BLOCK_SEC)), then return how much time remains until the 5-hour
    reset and whether to call again (`next_step`).

Typical loop: get_usage() → if near the cap, wait(confirm=false) to preview,
then wait(seconds=<remaining>, confirm=true) repeatedly until done.";

    const WAIT_TEST_DESC: &str = "\
DIAGNOSTIC: block for `seconds` to discover your MCP client's tool-call timeout.

Unlike wait(), this is NOT capped at ~50s — it blocks for the full requested
duration (up to a 600s hard ceiling) and immediately returns how long it
actually slept. Use it to probe how long your client (e.g. Claude Desktop)
lets a single tool call run: if this returns normally, the client's timeout
is at least that long; if the client kills the call first, you've found the
ceiling, and you can set wait()'s CLAUDE_USAGE_MCP_MAX_BLOCK just under it.

For real throttling use wait() instead, which stays under the common 60s
limit and cycles. Default 90s — just over wait()'s 50s cap, a good first probe.";

    #[derive(Debug, Default, Deserialize, JsonSchema)]
    pub struct WaitArgs {
        /// Seconds to block on a confirmed call, capped at MAX_BLOCK_SEC
        /// (CLAUDE_USAGE_MCP_MAX_BLOCK, default 50). Defaults to MAX_BLOCK_SEC.
        #[serde(default)]
        pub seconds: Option<i64>,
        /// false (default) previews only; true actually sleeps one cycle.
        #[serde(default)]
        pub confirm: bool,
    }

    fn default_test_seconds() -> i64 {
        DEFAULT_TEST_SECONDS
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct WaitTestArgs {
        /// Seconds to block (default 90, hard ceiling
        /// CLAUDE_USAGE_MCP_TEST_MAX_BLOCK, default 600).
        #[serde(default = "default_test_seconds")]
        #[schemars(default = "default_test_seconds")]
        pub seconds: i64,
    }

    async fn sleep_secs(secs: i64) -> f64 {
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(secs.max(0).unsigned_abs())).await;
        start.elapsed().as_secs_f64()
    }

    #[derive(Debug, Clone)]
    pub struct UsageServer {
        paths: Paths,
        cfg: McpConfig,
    }

    impl UsageServer {
        pub fn new(paths: Paths, cfg: McpConfig) -> Self {
            Self { paths, cfg }
        }
    }

    #[tool_router]
    impl UsageServer {
        #[tool(description = GET_USAGE_DESC)]
        fn get_usage(&self) -> CallToolResult {
            CallToolResult::structured(current_usage(&self.paths, &self.cfg, Utc::now()))
        }

        #[tool(description = WAIT_DESC)]
        async fn wait(&self, Parameters(args): Parameters<WaitArgs>) -> CallToolResult {
            let plan = wait_begin(
                &self.paths,
                &self.cfg,
                args.seconds,
                args.confirm,
                Utc::now(),
            );
            let value = match plan {
                WaitPlan::Preview(v) => v,
                WaitPlan::Sleep { block_seconds } => {
                    let slept = sleep_secs(block_seconds).await;
                    wait_finish(&self.paths, &self.cfg, slept, Utc::now())
                }
            };
            CallToolResult::structured(value)
        }

        #[tool(description = WAIT_TEST_DESC)]
        async fn wait_test(&self, Parameters(args): Parameters<WaitTestArgs>) -> CallToolResult {
            let (requested, block) = wait_test_block(&self.cfg, args.seconds);
            let slept = sleep_secs(block).await;
            CallToolResult::structured(wait_test_result(&self.cfg, requested, block, slept))
        }
    }

    #[tool_handler(name = "claude-usage")]
    impl ServerHandler for UsageServer {}
}

pub use server::UsageServer;

/// Serve MCP over stdin/stdout until the client disconnects. Builds its own
/// single-threaded tokio runtime; nothing but protocol messages reaches
/// stdout.
pub fn run_stdio(paths: Paths, cfg: McpConfig) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let service = UsageServer::new(paths, cfg)
            .serve(rmcp::transport::stdio())
            .await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use std::fs;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    fn iso(dt: DateTime<Utc>) -> String {
        dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    }

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(tmp.path().join("data"));
        (tmp, paths)
    }

    fn write_latest(paths: &Paths, v: &Value) {
        fs::create_dir_all(paths.data_dir()).unwrap();
        fs::write(paths.latest_file(), v.to_string()).unwrap();
    }

    /// A snapshot fetched `age` seconds before `now()` with a 5-hour window
    /// at `five_pct` resetting `reset_in` seconds after `now()`.
    fn snapshot(age: i64, five_pct: f64, reset_in: i64) -> Value {
        json!({
            "fetched_at": iso(now() - Duration::seconds(age)),
            "windows": [
                {"label": "5-hour", "utilization": five_pct,
                 "resets_at": iso(now() + Duration::seconds(reset_in))},
                {"label": "7-day", "utilization": 10.0, "resets_at": null},
            ],
            "extra": {"enabled": false},
        })
    }

    /// Defaults, with a home that holds no profiles so discovery never
    /// sees the real ones.
    fn cfg() -> McpConfig {
        McpConfig {
            home: PathBuf::from("/nonexistent/claude-usage-mcp-test-home"),
            ..McpConfig::default()
        }
    }

    // --- config ---------------------------------------------------------

    #[test]
    fn config_defaults_and_clamping() {
        assert_eq!(McpConfig::from_lookup(|_| None), McpConfig::default());
        let c = McpConfig::from_lookup(|k| {
            Some(match k {
                STALE_AFTER_ENV => "30".into(),
                MAX_BLOCK_ENV => "0".into(),
                _ => "-5".into(),
            })
        });
        assert_eq!(
            c,
            McpConfig {
                stale_after: 30,
                max_block: 1,
                test_max_block: 1,
                config_dir: Some("-5".into()),
                ..McpConfig::default()
            }
        );
        let bad = McpConfig::from_lookup(|k| (k != CONFIG_DIR_ENV).then(|| "abc".into()));
        assert_eq!(bad, McpConfig::default());
    }

    #[test]
    fn config_reads_claude_config_dir() {
        let dir = "/Users/me/.claude-secondary";
        let c = McpConfig::from_lookup(|k| (k == CONFIG_DIR_ENV).then(|| dir.into()));
        assert_eq!(c.config_dir.as_deref(), Some(dir));
        let empty = McpConfig::from_lookup(|k| (k == CONFIG_DIR_ENV).then(String::new));
        assert_eq!(empty.config_dir, None);
    }

    // --- humanize -------------------------------------------------------

    #[test]
    fn humanize_until_forms() {
        let h = |s: i64| humanize_until(Some(now() + Duration::seconds(s)), now()).unwrap();
        assert_eq!(humanize_until(None, now()), None);
        assert_eq!(h(0), "now");
        assert_eq!(h(-10), "now");
        assert_eq!(h(14), "14s");
        assert_eq!(h(8 * 60 + 5), "8m 05s");
        assert_eq!(h(3600 + 9 * 60 + 59), "1h 09m");
        assert_eq!(h(2 * 86400 + 4 * 3600 + 120), "2d 4h");
    }

    // --- advise ---------------------------------------------------------

    fn win(label: &str, pct: Option<f64>, rin: Option<&str>) -> Value {
        json!({"label": label, "utilization_pct": pct, "resets_in_human": rin})
    }

    #[test]
    fn advise_thresholds() {
        let rec = |pct: f64| advise(&[win("5-hour", Some(pct), Some("1h 00m"))], false).0;
        assert_eq!(
            rec(95.0),
            "CRITICAL: 5-hour at 95%. You are at/near the cap. Throttle now — call \
             wait() in a loop until it resets (in 1h 00m), or pause heavy work."
        );
        assert_eq!(
            rec(80.0),
            "WARNING: 5-hour at 80%. Approaching the cap; consider slowing down or \
             batching work. Resets in 1h 00m."
        );
        assert_eq!(
            rec(50.0),
            "5-hour at 50%. Headroom is fine but worth watching. Resets in 1h 00m."
        );
        assert_eq!(
            rec(49.9),
            "5-hour at 50%. Usage is low; no need to throttle."
        );
        assert!(rec(94.9).starts_with("WARNING"));
        assert!(rec(79.9).starts_with("5-hour at 80%. Headroom"));
    }

    #[test]
    fn advise_picks_highest_window_and_stale_prefix() {
        let ws = [
            win("5-hour", Some(20.0), Some("2h 00m")),
            win("7-day", Some(85.0), None),
            win("Fable", None, None),
        ];
        let (rec, summary) = advise(&ws, true);
        assert!(rec.starts_with(
            "(data may be stale — monitor possibly not running) WARNING: 7-day at 85%."
        ));
        assert!(rec.ends_with("Resets in unknown."));
        assert_eq!(summary, "7-day 85% · resets in unknown");
    }

    #[test]
    fn advise_without_utilization() {
        assert_eq!(
            advise(&[win("5-hour", None, None)], false),
            (
                "No utilization data in the latest snapshot.".into(),
                "No usage data available.".into()
            )
        );
    }

    // --- current_usage --------------------------------------------------

    #[test]
    fn no_snapshot() {
        let (_tmp, paths) = temp_paths();
        let u = current_usage(&paths, &cfg(), now());
        assert_eq!(
            u,
            json!({
                "profile": {
                    "id": "default",
                    "label": ".claude",
                    "email": null,
                    "config_dir": "/nonexistent/claude-usage-mcp-test-home/.claude",
                },
                "available": false,
                "stale": true,
                "note": "No usage snapshot found. Start the monitor to produce one: \
                         run the claude-usage app (or `just start`).",
                "windows": [],
                "five_hour": null,
                "extra": null,
                "seven_day_breakdown": null,
                "recommendation": "No data yet — the monitor has not written a snapshot.",
                "summary": "No usage data available.",
                "other_profiles": [],
            })
        );
        write_latest(&paths, &json!({}));
        assert_eq!(current_usage(&paths, &cfg(), now())["available"], false);
    }

    #[test]
    fn fresh_snapshot() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(30, 42.0, 3600));
        let u = current_usage(&paths, &cfg(), now());
        assert_eq!(u["available"], true);
        assert_eq!(u["as_of"], iso(now() - Duration::seconds(30)));
        assert_eq!(u["age_seconds"], 30);
        assert_eq!(u["stale"], false);
        assert!(u.get("note").is_none());
        assert_eq!(u["extra"], json!({"enabled": false}));
        assert_eq!(
            u["five_hour"],
            json!({
                "label": "5-hour",
                "utilization_pct": 42.0,
                "resets_at": iso(now() + Duration::seconds(3600)),
                "resets_in_seconds": 3600,
                "resets_in_human": "1h 00m",
            })
        );
        assert_eq!(u["windows"].as_array().unwrap().len(), 2);
        assert_eq!(u["windows"][1]["resets_in_seconds"], Value::Null);
        assert_eq!(u["windows"][1]["resets_in_human"], Value::Null);
        assert_eq!(u["summary"], "5-hour 42% · resets in 1h 00m");
    }

    /// Contract between the writer (GUI/CLI) and the MCP reader: a Snapshot
    /// written by storage must come back intact through current_usage.
    #[test]
    fn storage_written_snapshot_round_trips_through_current_usage() {
        use crate::model::{ExtraUsage, Snapshot, UsageWindow};
        let (_tmp, paths) = temp_paths();
        let fetched = now() - Duration::seconds(20);
        let snap = Snapshot {
            fetched_at: fetched,
            windows: vec![
                UsageWindow {
                    label: "5-hour".into(),
                    utilization: Some(37.5),
                    resets_at: Some(now() + Duration::seconds(5400)),
                },
                UsageWindow {
                    label: "7-day all".into(),
                    utilization: Some(61.0),
                    resets_at: Some(now() + Duration::days(2)),
                },
                UsageWindow {
                    label: "Fable".into(),
                    utilization: None,
                    resets_at: None,
                },
            ],
            extra: Some(ExtraUsage {
                enabled: true,
                monthly_limit: 75.0,
                used_credits: 12.34,
                utilization: 16.45,
                currency: "EUR".into(),
                disabled_reason: None,
            }),
            breakdown: None,
        };
        storage::write_latest_snapshot(&paths, &snap).unwrap();

        let u = current_usage(&paths, &cfg(), now());
        assert_eq!(u["available"], true);
        assert_eq!(u["age_seconds"], 20);
        assert_eq!(u["stale"], false);
        let labels: Vec<&str> = u["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["5-hour", "7-day all", "Fable"]);
        assert_eq!(u["windows"][0]["utilization_pct"], 37.5);
        assert_eq!(u["windows"][0]["resets_in_seconds"], 5400);
        assert_eq!(u["windows"][1]["utilization_pct"], 61.0);
        assert_eq!(u["windows"][1]["resets_in_seconds"], 2 * 86_400);
        assert_eq!(u["windows"][2]["utilization_pct"], Value::Null);
        assert_eq!(u["windows"][2]["resets_in_seconds"], Value::Null);
        assert_eq!(u["five_hour"]["utilization_pct"], 37.5);
        assert_eq!(
            u["extra"],
            json!({
                "enabled": true,
                "monthly_limit": 75.0,
                "used_credits": 12.34,
                "utilization": 16.45,
                "currency": "EUR",
                "disabled_reason": null,
            })
        );
    }

    #[test]
    fn current_usage_keys_keep_insertion_order() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(30, 42.0, 3600));
        let u = current_usage(&paths, &cfg(), now());
        let keys: Vec<&str> = u.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "profile",
                "available",
                "as_of",
                "age_seconds",
                "stale",
                "windows",
                "five_hour",
                "extra",
                "seven_day_breakdown",
                "recommendation",
                "summary",
                "other_profiles"
            ]
        );
    }

    #[test]
    fn stale_snapshot() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(181, 42.0, 3600));
        let u = current_usage(&paths, &cfg(), now());
        assert_eq!(u["stale"], true);
        assert_eq!(u["age_seconds"], 181);
        assert_eq!(
            u["note"],
            "Snapshot is 181s old (> 180s threshold). The monitor may not be running; \
             values reflect the last successful ping."
        );
        assert!(u["recommendation"]
            .as_str()
            .unwrap()
            .starts_with("(data may be stale"));

        // Exactly at the threshold is still fresh.
        write_latest(&paths, &snapshot(180, 42.0, 3600));
        assert_eq!(current_usage(&paths, &cfg(), now())["stale"], false);
    }

    #[test]
    fn missing_fetched_at_is_stale_with_none_age() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &json!({"windows": []}));
        let u = current_usage(&paths, &cfg(), now());
        assert_eq!(u["stale"], true);
        assert_eq!(u["age_seconds"], Value::Null);
        assert!(u["note"]
            .as_str()
            .unwrap()
            .starts_with("Snapshot is Nones old"));
    }

    #[test]
    fn resets_in_seconds_clamped_at_zero() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(10, 42.0, -300));
        let u = current_usage(&paths, &cfg(), now());
        assert_eq!(u["five_hour"]["resets_in_seconds"], 0);
        assert_eq!(u["five_hour"]["resets_in_human"], "now");
    }

    #[test]
    fn five_hour_absent_is_null() {
        let (_tmp, paths) = temp_paths();
        write_latest(
            &paths,
            &json!({"fetched_at": iso(now()), "windows": [{"label": "7-day", "utilization": 5}]}),
        );
        let u = current_usage(&paths, &cfg(), now());
        assert_eq!(u["five_hour"], Value::Null);
        assert_eq!(u["windows"][0]["utilization_pct"], 5);
    }

    // --- profiles -------------------------------------------------------

    /// A data root and home with two profiles in profiles.json: `default`
    /// (5-hour at 20%) and `claude-secondary` (5-hour at 70%), each with
    /// its own latest.json. Returns the secondary's `CLAUDE_CONFIG_DIR`.
    fn two_profiles() -> (tempfile::TempDir, Paths, McpConfig, String) {
        use crate::storage::ProfileIndex;
        let (tmp, root) = temp_paths();
        let home = tmp.path().join("home");
        let secondary = home.join(".claude-secondary");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(&secondary).unwrap();
        let env = secondary.display().to_string();
        let entry = |id: &str, dir: PathBuf, env: Option<String>, label: &str| ProfileIndexEntry {
            id: ProfileId::new(id),
            label: label.into(),
            color: 0,
            config_dir: dir,
            config_dir_env: env,
            email: Some(format!("{label}@example.com")),
            account_uuid: None,
            is_default: id == profiles::DEFAULT_ID,
        };
        let index = ProfileIndex::new(vec![
            entry("default", home.join(".claude"), None, "main"),
            entry("claude-secondary", secondary, Some(env.clone()), "second"),
        ]);
        storage::write_profiles_index(&root, &index).unwrap();
        for (id, pct) in [("default", 20.0), ("claude-secondary", 70.0)] {
            write_latest(
                &root.for_profile(&ProfileId::new(id)),
                &snapshot(10, pct, 600),
            );
        }
        let cfg = McpConfig { home, ..cfg() };
        (tmp, root, cfg, env)
    }

    #[test]
    fn calling_secondary_leads_and_lists_default() {
        let (_tmp, root, cfg, env) = two_profiles();
        let cfg = McpConfig {
            config_dir: Some(env.clone()),
            ..cfg
        };
        let u = current_usage(&root, &cfg, now());
        assert_eq!(
            u["profile"],
            json!({
                "id": "claude-secondary",
                "label": "second",
                "email": "second@example.com",
                "config_dir": env,
            })
        );
        assert_eq!(u["five_hour"]["utilization_pct"], 70.0);
        assert_eq!(u["summary"], "5-hour 70% · resets in 10m 00s");
        assert!(u.get("profile_note").is_none());
        let others = u["other_profiles"].as_array().unwrap();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0]["profile"]["id"], "default");
        assert_eq!(others[0]["five_hour"]["utilization_pct"], 20.0);
        assert_eq!(others[0]["stale"], false);
        assert!(others[0]["recommendation"]
            .as_str()
            .unwrap()
            .contains("20%"));
        let keys: Vec<&str> = others[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys[0], "profile");
        assert!(!keys.contains(&"other_profiles"));

        // A trailing slash still matches.
        let slash = McpConfig {
            config_dir: Some(format!("{env}/")),
            ..cfg.clone()
        };
        assert_eq!(
            current_usage(&root, &slash, now())["profile"]["id"],
            "claude-secondary"
        );

        // wait follows the caller's 5-hour window.
        let WaitPlan::Preview(p) = wait_begin(&root, &cfg, Some(50), false, now()) else {
            panic!("expected a preview");
        };
        assert_eq!(p["profile"], "claude-secondary");
        assert_eq!(p["five_hour_resets_in_seconds"], 600);
        let r = wait_finish(&root, &cfg, 50.0, now());
        assert_eq!(r["profile"], "claude-secondary");
        assert_eq!(r["five_hour_utilization_pct"], 70.0);
    }

    #[test]
    fn env_unset_is_the_default_profile() {
        let (_tmp, root, cfg, _env) = two_profiles();
        let u = current_usage(&root, &cfg, now());
        assert_eq!(u["profile"]["id"], "default");
        assert_eq!(u["profile"]["email"], "main@example.com");
        assert_eq!(u["five_hour"]["utilization_pct"], 20.0);
        assert!(u.get("profile_note").is_none());
        assert_eq!(u["other_profiles"][0]["profile"]["id"], "claude-secondary");
        assert_eq!(u["other_profiles"][0]["five_hour"]["utilization_pct"], 70.0);
    }

    #[test]
    fn unknown_env_falls_back_to_default_with_note() {
        let (_tmp, root, cfg, _env) = two_profiles();
        let cfg = McpConfig {
            config_dir: Some("/somewhere/.claude-other".into()),
            ..cfg
        };
        let u = current_usage(&root, &cfg, now());
        assert_eq!(u["profile"]["id"], "default");
        assert_eq!(
            u["profile_note"],
            "CLAUDE_CONFIG_DIR=\"/somewhere/.claude-other\" matches no known profile, so \
             this reports the default profile. Open the claude-usage app so it discovers \
             the profile."
        );
        assert_eq!(u["other_profiles"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn default_without_own_snapshot_reads_the_root_one() {
        let (_tmp, root, cfg, _env) = two_profiles();
        fs::remove_file(root.for_profile(&ProfileId::new("default")).latest_file()).unwrap();
        write_latest(&root, &snapshot(10, 33.0, 600));
        let u = current_usage(&root, &cfg, now());
        assert_eq!(u["profile"]["id"], "default");
        assert_eq!(u["five_hour"]["utilization_pct"], 33.0);
        // A non-default profile never borrows the root snapshot.
        fs::remove_file(
            root.for_profile(&ProfileId::new("claude-secondary"))
                .latest_file(),
        )
        .unwrap();
        assert_eq!(u["other_profiles"][0]["available"], true);
        let u = current_usage(&root, &cfg, now());
        assert_eq!(u["other_profiles"][0]["available"], false);
    }

    #[test]
    fn missing_index_falls_back_to_discovery() {
        let (_tmp, root, cfg, env) = two_profiles();
        fs::write(root.profiles_index_file(), "{ not json").unwrap();
        // Discovery needs a signed-in .claude.json in each extra dir.
        fs::write(
            cfg.home.join(".claude-secondary").join(".claude.json"),
            json!({"oauthAccount": {"emailAddress": "second@example.com",
                                    "accountUuid": "u-2"}})
            .to_string(),
        )
        .unwrap();
        let cfg = McpConfig {
            config_dir: Some(env),
            ..cfg
        };
        let u = current_usage(&root, &cfg, now());
        assert_eq!(u["profile"]["id"], "claude-secondary");
        assert_eq!(u["profile"]["email"], "second@example.com");
        assert_eq!(u["five_hour"]["utilization_pct"], 70.0);
        assert_eq!(u["other_profiles"][0]["profile"]["id"], "default");
        assert_eq!(u["other_profiles"][0]["five_hour"]["utilization_pct"], 20.0);
    }

    #[test]
    fn seven_day_breakdown_passes_through() {
        let (_tmp, root, cfg, _env) = two_profiles();
        let mut snap = snapshot(10, 20.0, 600);
        let breakdown = json!({"as_of": null, "window_started_at": null,
                               "rows": [{"key": "opus", "display_name": "Opus", "percent": 12.5}]});
        snap["seven_day_breakdown"] = breakdown.clone();
        write_latest(&root.for_profile(&ProfileId::new("default")), &snap);
        let u = current_usage(&root, &cfg, now());
        assert_eq!(u["seven_day_breakdown"], breakdown);
        assert_eq!(u["other_profiles"][0]["seven_day_breakdown"], Value::Null);
    }

    // --- wait -----------------------------------------------------------

    fn preview(paths: &Paths, seconds: Option<i64>) -> Value {
        match wait_begin(paths, &cfg(), seconds, false, now()) {
            WaitPlan::Preview(v) => v,
            other => panic!("expected preview, got {other:?}"),
        }
    }

    #[test]
    fn wait_preview_fields_and_cycles() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(10, 97.0, 101));
        let p = preview(&paths, None);
        assert_eq!(
            p,
            json!({
                "blocked": false,
                "preview": true,
                "profile": "default",
                "would_block_seconds": 50,
                "requested_seconds": 50,
                "max_block_seconds": 50,
                "five_hour_reset_at": iso(now() + Duration::seconds(101)),
                "five_hour_resets_in_seconds": 101,
                "estimated_cycles_to_reset": 3,
                "message": "This will BLOCK for up to 50s per call. The 5-hour window resets \
                            in ~101s (~3 call(s) of 50s). Re-call wait(seconds=..., confirm=true) \
                            to actually wait. Each call blocks only a bounded amount and then \
                            returns, so you are never trapped — loop until 'done' is true.",
            })
        );
        let p = preview(&paths, Some(20));
        assert_eq!(p["would_block_seconds"], 20);
        assert_eq!(p["estimated_cycles_to_reset"], 6);
        // 0 and negatives mean "the max block".
        let p = preview(&paths, Some(-4));
        assert_eq!(p["requested_seconds"], 0);
        assert_eq!(p["would_block_seconds"], 50);
        assert_eq!(preview(&paths, Some(500))["would_block_seconds"], 50);
    }

    #[test]
    fn wait_preview_without_reset() {
        let (_tmp, paths) = temp_paths();
        let p = preview(&paths, Some(10));
        assert_eq!(p["five_hour_resets_in_seconds"], Value::Null);
        assert_eq!(p["estimated_cycles_to_reset"], Value::Null);
        assert_eq!(
            p["message"],
            "This will BLOCK for up to 10s per call. Re-call wait(seconds=..., confirm=true) \
             to actually wait. Each call blocks only a bounded amount and then returns, so \
             you are never trapped — loop until 'done' is true."
        );
        // A reset that is already due: no cycle estimate.
        write_latest(&paths, &snapshot(10, 97.0, -5));
        let p = preview(&paths, Some(10));
        assert_eq!(p["estimated_cycles_to_reset"], Value::Null);
        assert!(p["message"]
            .as_str()
            .unwrap()
            .contains("resets in ~0s. Re-call"));
    }

    #[test]
    fn wait_confirm_plans_bounded_sleep() {
        let (_tmp, paths) = temp_paths();
        let plan = |s| wait_begin(&paths, &cfg(), s, true, now());
        assert_eq!(plan(None), WaitPlan::Sleep { block_seconds: 50 });
        assert_eq!(plan(Some(7)), WaitPlan::Sleep { block_seconds: 7 });
        assert_eq!(plan(Some(0)), WaitPlan::Sleep { block_seconds: 50 });
        assert_eq!(plan(Some(999)), WaitPlan::Sleep { block_seconds: 50 });
    }

    #[test]
    fn wait_finish_done() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(10, 3.0, -1));
        let r = wait_finish(&paths, &cfg(), 50.04, now());
        assert_eq!(r["blocked"], true);
        assert_eq!(r["slept_seconds"], 50.0);
        assert_eq!(r["done"], true);
        assert_eq!(r["five_hour_resets_in_seconds"], 0);
        assert_eq!(r["five_hour_utilization_pct"], 3.0);
        assert_eq!(
            r["next_step"],
            "The 5-hour window has reset — you can resume normal work."
        );
        assert_eq!(r["usage"]["available"], true);
    }

    #[test]
    fn wait_finish_remaining() {
        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(10, 97.0, 240));
        let r = wait_finish(&paths, &cfg(), 49.96, now());
        assert_eq!(r["done"], false);
        assert_eq!(r["slept_seconds"], 50.0);
        assert_eq!(r["five_hour_resets_in_seconds"], 240);
        assert_eq!(r["five_hour_reset_at"], iso(now() + Duration::seconds(240)));
        assert_eq!(
            r["next_step"],
            "~240s remain until the 5-hour reset. Call wait(seconds=240, confirm=true) \
             again to keep waiting."
        );
    }

    #[test]
    fn wait_finish_unknown() {
        let (_tmp, paths) = temp_paths();
        let r = wait_finish(&paths, &cfg(), 1.0, now());
        assert_eq!(r["done"], false);
        assert_eq!(r["five_hour_resets_in_seconds"], Value::Null);
        assert_eq!(r["five_hour_reset_at"], Value::Null);
        assert_eq!(r["five_hour_utilization_pct"], Value::Null);
        assert_eq!(
            r["next_step"],
            "Reset time unknown (monitor may not be running). Call get_usage() to check, \
             then decide whether to wait more."
        );
    }

    // --- wait_test ------------------------------------------------------

    #[test]
    fn wait_test_capping() {
        let c = cfg();
        assert_eq!(wait_test_block(&c, 90), (90, 90));
        assert_eq!(wait_test_block(&c, 900), (900, 600));
        assert_eq!(wait_test_block(&c, -3), (0, 0));

        let r = wait_test_result(&c, 900, 600, 600.02);
        assert_eq!(r["requested_seconds"], 900);
        assert_eq!(r["blocked_seconds"], 600);
        assert_eq!(r["capped_to_ceiling"], true);
        assert_eq!(r["test_max_block_seconds"], 600);
        assert_eq!(r["slept_seconds"], 600.0);
        assert_eq!(
            r["result"],
            "Returned after blocking ~600.0s. If you can read this, your MCP client's single \
             tool-call timeout is at least 600s. If instead the call errored or timed out, the \
             client's limit is below 600s — keep wait()'s CLAUDE_USAGE_MCP_MAX_BLOCK under that."
        );
        let r = wait_test_result(&c, 90, 90, 90.14);
        assert_eq!(r["capped_to_ceiling"], false);
        assert!(r["result"]
            .as_str()
            .unwrap()
            .starts_with("Returned after blocking ~90.1s."));
    }

    // --- server over an in-memory duplex ---------------------------------

    #[derive(Debug, Clone, Default)]
    struct Client;
    impl rmcp::ClientHandler for Client {}

    #[tokio::test]
    async fn server_lists_three_tools_and_answers_calls() {
        use rmcp::model::CallToolRequestParams;
        use rmcp::ServiceExt;

        let (_tmp, paths, cfg, env) = two_profiles();
        write_latest(
            &paths.for_profile(&ProfileId::new("claude-secondary")),
            &snapshot(0, 12.0, 3600),
        );
        let cfg = McpConfig {
            config_dir: Some(env),
            ..cfg
        };
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let server = UsageServer::new(paths, cfg);
        let handle = tokio::spawn(async move {
            server.serve(server_io).await?.waiting().await?;
            anyhow::Ok(())
        });
        let client = Client.serve(client_io).await.unwrap();

        let info = client.peer_info().unwrap();
        assert_eq!(info.server_info.as_ref().unwrap().name, "claude-usage");

        let tools = client.list_all_tools().await.unwrap();
        let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(names, ["get_usage", "wait", "wait_test"]);
        let wait_test = tools.iter().find(|t| t.name == "wait_test").unwrap();
        assert_eq!(
            wait_test.input_schema["properties"]["seconds"]["default"],
            DEFAULT_TEST_SECONDS
        );

        let usage = client
            .call_tool(CallToolRequestParams::new("get_usage"))
            .await
            .unwrap();
        let structured = usage.structured_content.clone().unwrap();
        assert_eq!(structured["available"], true);
        assert_eq!(structured["profile"]["id"], "claude-secondary");
        assert_eq!(structured["five_hour"]["utilization_pct"], 12.0);
        assert_eq!(structured["other_profiles"][0]["profile"]["id"], "default");
        let text = usage.content[0].as_text().unwrap().text.clone();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), structured);

        // No arguments at all: a preview with the default block.
        let preview = client
            .call_tool(CallToolRequestParams::new("wait"))
            .await
            .unwrap();
        let p = preview.structured_content.unwrap();
        assert_eq!(p["preview"], true);
        assert_eq!(p["profile"], "claude-secondary");
        assert_eq!(p["would_block_seconds"], 50);

        // A zero-second probe returns immediately.
        let probe = client
            .call_tool(
                CallToolRequestParams::new("wait_test")
                    .with_arguments(json!({"seconds": 0}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(probe.structured_content.unwrap()["blocked_seconds"], 0);

        client.cancel().await.unwrap();
        handle.await.unwrap().unwrap();
    }
}
