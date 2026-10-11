//! Stdio MCP server exposing the latest usage snapshot to a Claude agent.
//!
//! Ported from the Python mcp_server.py (removed; see git history). The
//! monitor writes latest.json every poll; this server reads it on demand
//! and offers `get_usage`, `wait` and `wait_test`.
//! It does no network or auth of its own. The JSON logic lives in plain
//! functions taking `&Paths` and `now`, so it is testable without the MCP
//! runtime; the tokio runtime exists only inside [`run_stdio`].

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::paths::Paths;
use crate::storage;

pub const STALE_AFTER_ENV: &str = "CLAUDE_USAGE_MCP_STALE_AFTER";
pub const MAX_BLOCK_ENV: &str = "CLAUDE_USAGE_MCP_MAX_BLOCK";
pub const TEST_MAX_BLOCK_ENV: &str = "CLAUDE_USAGE_MCP_TEST_MAX_BLOCK";

/// Default `wait_test` duration: just over `wait`'s 50s cap.
pub const DEFAULT_TEST_SECONDS: i64 = 90;

/// Tunables read once from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpConfig {
    /// Snapshot older than this (seconds) is flagged stale.
    pub stale_after: i64,
    /// Max seconds a single `wait` call blocks (at least 1).
    pub max_block: i64,
    /// Hard ceiling for `wait_test` (at least 1).
    pub test_max_block: i64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            stale_after: 180,
            max_block: 50,
            test_max_block: 600,
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

/// The structured "most recent ping" view built from latest.json.
pub fn current_usage(paths: &Paths, cfg: &McpConfig, now: DateTime<Utc>) -> Value {
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

/// `seconds` of `None` means the tool's default, `cfg.max_block`.
pub fn wait_begin(
    paths: &Paths,
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

    let usage = current_usage(paths, cfg, now);
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
pub fn wait_finish(
    paths: &Paths,
    cfg: &McpConfig,
    slept_seconds: f64,
    now: DateTime<Utc>,
) -> Value {
    let after = current_usage(paths, cfg, now);
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
Report Claude's current claude.ai usage from the most recent monitor ping.

Read-only and non-blocking. Use this to decide whether to slow down or keep
going. Returns, for each usage window (5-hour, 7-day, Fable, ...):
utilization percent, when it resets, and a human-readable time-to-reset —
plus the data's age, a `stale` flag (true if the monitor isn't updating),
the extra-usage block, and a `recommendation`/`summary` you can act on.

Data is whatever the running claude-usage monitor app last fetched; if
the monitor isn't running the result is marked stale.";

    const WAIT_DESC: &str = "\
Throttle yourself by sleeping in short, bounded cycles near a usage cap.

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

    fn cfg() -> McpConfig {
        McpConfig::default()
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
                test_max_block: 1
            }
        );
        let bad = McpConfig::from_lookup(|_| Some("abc".into()));
        assert_eq!(bad, McpConfig::default());
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
                "available": false,
                "stale": true,
                "note": "No usage snapshot found. Start the monitor to produce one: \
                         run the claude-usage app (or `just start`).",
                "windows": [],
                "five_hour": null,
                "extra": null,
                "recommendation": "No data yet — the monitor has not written a snapshot.",
                "summary": "No usage data available.",
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

        let (_tmp, paths) = temp_paths();
        write_latest(&paths, &snapshot(0, 12.0, 3600));
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let server = UsageServer::new(paths, cfg());
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
        assert_eq!(structured["five_hour"]["utilization_pct"], 12.0);
        let text = usage.content[0].as_text().unwrap().text.clone();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), structured);

        // No arguments at all: a preview with the default block.
        let preview = client
            .call_tool(CallToolRequestParams::new("wait"))
            .await
            .unwrap();
        let p = preview.structured_content.unwrap();
        assert_eq!(p["preview"], true);
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
