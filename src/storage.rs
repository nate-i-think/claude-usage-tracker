//! Local persistence: history JSONL, latest snapshot, chart rollups, and
//! per-instance token attribution from local session JSONLs.
//!
//! Ported from the Python storage.py (removed; see git history). Every
//! function takes its locations explicitly
//! (`&Paths` or directories) and, where time matters, `now`, so tests are
//! hermetic and deterministic.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::analytics::{project_display_name, WindowSample, WEEKLY_RATE_HOURS};
use crate::model::{ExtraUsage, Snapshot};
use crate::paths::Paths;

/// One row of history.jsonl. Field names are the on-disk schema shared with
/// the Python app; do not rename.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryEntry {
    /// Unix seconds (fractional).
    pub ts: f64,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub fable_7d: Option<f64>,
    pub extra_pct: Option<f64>,
}

/// Tokens used by one local Claude Code project or Cowork session.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceUsage {
    /// The project directory name, or `cowork:<id>`.
    pub name: String,
    /// What the burner rows show: the last component of the project's most
    /// recent session `cwd`, else the prettified directory name; Cowork
    /// sessions keep `name`.
    pub display_name: String,
    /// `"claude-code"` or `"cowork"`.
    pub source: &'static str,
    pub tokens: u64,
    pub last_active: DateTime<Utc>,
}

// --- History append/read ---------------------------------------------------

/// Append one history row for `snap`, then best-effort refresh latest.json.
pub fn append_snapshot(paths: &Paths, snap: &Snapshot) -> io::Result<()> {
    fs::create_dir_all(paths.data_dir())?;
    let entry = HistoryEntry {
        ts: unix_seconds(snap.fetched_at),
        five_hour: snap.five_hour_pct(),
        seven_day: snap.window("7-day all").and_then(|w| w.utilization),
        fable_7d: snap.window("Fable").and_then(|w| w.utilization),
        extra_pct: snap.extra.as_ref().map(|e| e.utilization),
    };
    let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
    line.push('\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.history_file())?
        .write_all(line.as_bytes())?;

    // Out-of-process readers (the MCP server) want the full snapshot. A
    // failure here must never break the monitor.
    let _ = write_latest_snapshot(paths, snap);
    Ok(())
}

/// History rows with `ts >= since`. Unreadable lines are skipped; unknown
/// keys are ignored and missing or non-numeric values become `None`.
pub fn read_recent(paths: &Paths, since: DateTime<Utc>) -> Vec<HistoryEntry> {
    #[cfg(test)]
    HISTORY_READS.with(|n| n.set(n.get() + 1));
    let Ok(file) = File::open(paths.history_file()) else {
        return Vec::new();
    };
    let cutoff = unix_seconds(since);
    lossy_lines(file)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|d| history_entry_from_json(&d))
        .filter(|e| e.ts >= cutoff)
        .collect()
}

#[cfg(test)]
thread_local! {
    /// Calls to [`read_recent`] on this thread, so tests can assert how many
    /// times a code path read history.jsonl.
    static HISTORY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [`read_recent`] calls so far on this thread (tests only).
#[cfg(test)]
pub(crate) fn history_reads() -> usize {
    HISTORY_READS.with(std::cell::Cell::get)
}

fn history_entry_from_json(d: &Value) -> Option<HistoryEntry> {
    let obj = d.as_object()?;
    let num = |k: &str| obj.get(k).and_then(Value::as_f64);
    let ts = match obj.get("ts") {
        // Python: `d.get("ts", 0)`; a non-numeric ts fails the comparison.
        None => 0.0,
        Some(v) => v.as_f64()?,
    };
    Some(HistoryEntry {
        ts,
        five_hour: num("five_hour"),
        seven_day: num("seven_day"),
        fable_7d: num("fable_7d"),
        extra_pct: num("extra_pct"),
    })
}

// --- Latest full snapshot (for the MCP server) -----------------------------

/// Persist the complete snapshot to latest.json atomically (temp + rename),
/// so a concurrent reader never sees a half-written file.
pub fn write_latest_snapshot(paths: &Paths, snap: &Snapshot) -> io::Result<()> {
    fs::create_dir_all(paths.data_dir())?;
    let payload = json!({
        "fetched_at": iso(snap.fetched_at),
        "windows": snap.windows.iter().map(|w| json!({
            "label": w.label,
            "utilization": w.utilization,
            "resets_at": w.resets_at.map(iso),
        })).collect::<Vec<_>>(),
        "extra": snap.extra.as_ref().map(extra_json),
    });
    // A uniquely named temp file, so the GUI and a concurrent `once` never
    // write the same temp path; persist() replaces the target (also on
    // Windows).
    let mut tmp = tempfile::NamedTempFile::new_in(paths.data_dir())?;
    tmp.write_all(payload.to_string().as_bytes())?;
    // Windows briefly denies a replace while another writer's replace of
    // the same target is in flight; retry a few times before giving up.
    let mut attempts = 0;
    loop {
        match tmp.persist(paths.latest_file()) {
            Ok(_) => return Ok(()),
            Err(e) if e.error.kind() == io::ErrorKind::PermissionDenied && attempts < 20 => {
                attempts += 1;
                tmp = e.file;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e.error),
        }
    }
}

/// The JSON written by [`write_latest_snapshot`], or `None` when the file
/// is missing, unreadable, or corrupt.
pub fn read_latest_snapshot(paths: &Paths) -> Option<Value> {
    let text = fs::read_to_string(paths.latest_file()).ok()?;
    serde_json::from_str(&text).ok()
}

fn extra_json(e: &ExtraUsage) -> Value {
    json!({
        "enabled": e.enabled,
        "monthly_limit": e.monthly_limit,
        "used_credits": e.used_credits,
        "utilization": e.utilization,
        "currency": e.currency,
        "disabled_reason": e.disabled_reason,
    })
}

/// Python `datetime.isoformat()` for a UTC datetime: microseconds only when
/// non-zero, and a `+00:00` offset.
fn iso(dt: DateTime<Utc>) -> String {
    if dt.timestamp_subsec_micros() == 0 {
        dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        dt.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

// --- Rollups for charts ----------------------------------------------------

/// The daily rollups plus the raw samples of the five-hour window and the
/// last [`WEEKLY_RATE_HOURS`] of 7-day-all samples, from a single read of
/// history.jsonl. The rollups are identical to calling
/// [`daily_history`] and [`fable_daily_history`] separately, but the
/// history is read and parsed once (from the earliest cutoff any of them
/// needs).
#[derive(Debug, Clone, PartialEq)]
pub struct Rollups {
    pub daily: Vec<(NaiveDate, Option<f64>)>,
    pub fable_daily: Vec<(NaiveDate, Option<f64>)>,
    /// Every row in `[window_start, window_end)`, oldest first.
    pub window_samples: Vec<WindowSample>,
    /// 7-day-all utilization of every row in the last [`WEEKLY_RATE_HOURS`]
    /// up to `now`, oldest first, for the weekly projection.
    pub seven_day_recent: Vec<(DateTime<Utc>, Option<f64>)>,
}

/// See [`Rollups`].
pub fn rollups(
    paths: &Paths,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    days: usize,
    now: DateTime<Utc>,
) -> Rollups {
    let recent_start = now - Duration::hours(WEEKLY_RATE_HOURS);
    let since = match daily_since(days, now) {
        Some(d) => d.min(window_start),
        None => window_start,
    }
    .min(recent_start);
    let rows = read_recent(paths, since);
    Rollups {
        daily: daily_max(&rows, days, now, |e| e.seven_day),
        fable_daily: daily_max(&rows, days, now, |e| e.fable_7d),
        window_samples: window_samples(&rows, window_start, window_end),
        seven_day_recent: seven_day_recent(&rows, recent_start, now),
    }
}

/// The rows' 7-day-all values in `[start, now]`, oldest first.
fn seven_day_recent(
    rows: &[HistoryEntry],
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, Option<f64>)> {
    let (start_ts, end_ts) = (unix_seconds(start), unix_seconds(now));
    let mut out: Vec<_> = rows
        .iter()
        .filter(|e| e.ts >= start_ts && e.ts <= end_ts)
        .filter_map(|e| Some((from_unix_seconds(e.ts)?, e.seven_day)))
        .collect();
    out.sort_by_key(|(t, _)| *t);
    out
}

/// The rows in the half-open window `[window_start, window_end)`, as
/// samples, oldest first. `rows` may hold entries outside the window.
fn window_samples(
    rows: &[HistoryEntry],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Vec<WindowSample> {
    let start_ts = unix_seconds(window_start);
    let end_ts = unix_seconds(window_end);
    let mut samples: Vec<WindowSample> = rows
        .iter()
        .filter(|e| e.ts >= start_ts && e.ts < end_ts)
        .filter_map(|e| {
            Some(WindowSample {
                t: from_unix_seconds(e.ts)?,
                five_hour: e.five_hour,
                fable: e.fable_7d,
            })
        })
        .collect();
    // The file is append-only, but concurrent writers can interleave.
    samples.sort_by_key(|s| s.t);
    samples
}

/// Last `days` local calendar days of 7-day utilization, max per day,
/// oldest first. Days without samples are `None`.
pub fn daily_history(
    paths: &Paths,
    days: usize,
    now: DateTime<Utc>,
) -> Vec<(NaiveDate, Option<f64>)> {
    let rows = daily_since(days, now).map_or_else(Vec::new, |since| read_recent(paths, since));
    daily_max(&rows, days, now, |e| e.seven_day)
}

/// Like [`daily_history`] but for Fable 7-day utilization, with the same day
/// indices. Legacy rows predating Fable contribute nothing.
pub fn fable_daily_history(
    paths: &Paths,
    days: usize,
    now: DateTime<Utc>,
) -> Vec<(NaiveDate, Option<f64>)> {
    let rows = daily_since(days, now).map_or_else(Vec::new, |since| read_recent(paths, since));
    daily_max(&rows, days, now, |e| e.fable_7d)
}

/// The last `days` local calendar days ending today, oldest first.
fn day_list(days: usize, now: DateTime<Utc>) -> Vec<NaiveDate> {
    let today = now.with_timezone(&Local).date_naive();
    (0..days as i64)
        .rev()
        .map(|i| today - Duration::days(i))
        .collect()
}

/// Earliest instant the daily rollup needs (local midnight of the oldest
/// day), or `None` when `days == 0`.
fn daily_since(days: usize, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    day_list(days, now).first().map(|d| local_midnight(*d))
}

/// Max of one field per local day. `rows` may hold entries older than the
/// range; they are ignored.
fn daily_max(
    rows: &[HistoryEntry],
    days: usize,
    now: DateTime<Utc>,
    field: impl Fn(&HistoryEntry) -> Option<f64>,
) -> Vec<(NaiveDate, Option<f64>)> {
    if days == 0 {
        return Vec::new();
    }
    let day_list = day_list(days, now);
    let since = unix_seconds(local_midnight(day_list[0]));
    let mut by_day: Vec<Option<f64>> = vec![None; days];

    for e in rows.iter().filter(|e| e.ts >= since) {
        let Some(val) = field(e) else { continue };
        let Some(day) = from_unix_seconds(e.ts).map(|t| t.with_timezone(&Local).date_naive())
        else {
            continue;
        };
        if let Some(idx) = day_list.iter().position(|d| *d == day) {
            // Python seeds the per-day max with 0.
            by_day[idx] = Some(by_day[idx].unwrap_or(0.0).max(val));
        }
    }

    day_list.into_iter().zip(by_day).collect()
}

/// The instant of local midnight starting `date` (the earliest one if the
/// clock repeats; one hour later if midnight is skipped by a DST jump).
fn local_midnight(date: NaiveDate) -> DateTime<Utc> {
    let midnight = date.and_hms_opt(0, 0, 0).expect("valid time");
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .or_else(|| {
            Local
                .from_local_datetime(&(midnight + Duration::hours(1)))
                .earliest()
        })
        .map_or_else(|| midnight.and_utc(), |t| t.with_timezone(&Utc))
}

// --- Per-instance attribution from local JSONLs ----------------------------

/// Token usage since `window_start` per Claude Code project (each subdir of
/// `claude_code_dir`) and per Cowork session (`*/*/local_*` under
/// `cowork_dir`), sorted by tokens, highest first. Instances with no tokens
/// are omitted.
///
/// Files last modified before `window_start` are skipped unread: they cannot
/// hold entries newer than their mtime.
pub fn attribute_recent(
    claude_code_dir: &Path,
    cowork_dir: &Path,
    window_start: DateTime<Utc>,
) -> Vec<InstanceUsage> {
    let mut out = Vec::new();

    for project_dir in subdirs(claude_code_dir) {
        let files: Vec<PathBuf> = read_dir_paths(&project_dir)
            .into_iter()
            .filter(|p| p.is_file() && has_jsonl_ext(p))
            .collect();
        if let Some(usage) = scan_instance(&files, window_start) {
            let name = file_name(&project_dir);
            out.push(InstanceUsage {
                display_name: project_display_name(&name, usage.cwd.as_deref()),
                name,
                source: "claude-code",
                tokens: usage.tokens,
                last_active: usage.last_active,
            });
        }
    }

    // Cowork groups by user-uuid/org-uuid/local_session-uuid.
    for session_dir in subdirs(cowork_dir)
        .iter()
        .flat_map(|d| subdirs(d))
        .flat_map(|d| subdirs(&d))
        .filter(|d| file_name(d).starts_with("local_"))
    {
        let mut files = Vec::new();
        collect_jsonls(&session_dir, &mut files);
        if let Some(usage) = scan_instance(&files, window_start) {
            let short: String = file_name(&session_dir).chars().skip(6).take(8).collect();
            let name = format!("cowork:{short}");
            out.push(InstanceUsage {
                display_name: name.clone(),
                name,
                source: "cowork",
                tokens: usage.tokens,
                last_active: usage.last_active,
            });
        }
    }

    out.sort_by_key(|i| std::cmp::Reverse(i.tokens));
    out
}

/// What one instance's session files say about the window.
struct InstanceScan {
    tokens: u64,
    last_active: DateTime<Utc>,
    /// The non-empty `cwd` of the latest timestamped entry that has one.
    cwd: Option<String>,
}

/// The scan of `files`, or `None` when no tokens were used.
fn scan_instance(files: &[PathBuf], window_start: DateTime<Utc>) -> Option<InstanceScan> {
    let (tokens, last_ts, cwd) = scan_jsonls(files, window_start);
    (tokens > 0).then(|| InstanceScan {
        tokens: tokens as u64,
        last_active: from_unix_seconds(last_ts).unwrap_or(window_start),
        cwd,
    })
}

/// Sum tokens of entries at or after `window_start`; also the latest entry
/// timestamp (starting from the cutoff itself), and the `cwd` of the latest
/// timestamped entry carrying a non-empty one (older entries of a scanned
/// file count too: the directory rarely changes).
fn scan_jsonls(files: &[PathBuf], window_start: DateTime<Utc>) -> (i64, f64, Option<String>) {
    let cutoff = unix_seconds(window_start);
    let mut tokens: i64 = 0;
    let mut last_ts = cutoff;
    let mut cwd: Option<(f64, String)> = None;
    for path in files {
        if modified_before(path, window_start) {
            continue;
        }
        let Ok(file) = File::open(path) else { continue };
        for line in lossy_lines(file) {
            let Ok(d) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(ts) = extract_ts(&d) else { continue };
            if let Some(dir) = d.get("cwd").and_then(Value::as_str) {
                if !dir.is_empty() && cwd.as_ref().is_none_or(|(at, _)| ts >= *at) {
                    cwd = Some((ts, dir.to_owned()));
                }
            }
            if ts < cutoff {
                continue;
            }
            tokens = tokens.saturating_add(extract_tokens(&d));
            if ts > last_ts {
                last_ts = ts;
            }
        }
    }
    (tokens, last_ts, cwd.map(|(_, dir)| dir))
}

fn modified_before(path: &Path, t: DateTime<Utc>) -> bool {
    let mtime = fs::metadata(path).and_then(|m| m.modified());
    mtime.is_ok_and(|m| m < SystemTime::from(t))
}

/// Entry timestamp from the first usable of `timestamp`, `created_at`, `ts`:
/// a number (unix seconds) or an ISO-8601 string.
fn extract_ts(d: &Value) -> Option<f64> {
    ["timestamp", "created_at", "ts"]
        .iter()
        .find_map(|k| match d.get(k)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => parse_iso_ts(s),
            _ => None,
        })
}

/// Python `datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()`:
/// offset-less values are local time.
fn parse_iso_ts(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(unix_seconds(dt.with_timezone(&Utc)));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Some(unix_seconds(dt.with_timezone(&Utc)));
        }
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(s, fmt).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })?;
    let local = Local.from_local_datetime(&naive).earliest()?;
    Some(unix_seconds(local.with_timezone(&Utc)))
}

/// Sum of input, output, cache-read and cache-creation tokens from `usage`
/// (or `message.usage` when `usage` is absent or empty).
fn extract_tokens(d: &Value) -> i64 {
    let usage = d
        .get("usage")
        .filter(|u| truthy(u))
        .or_else(|| d.get("message").and_then(|m| m.get("usage")));
    let Some(usage) = usage.and_then(Value::as_object) else {
        return 0;
    };
    [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .map(|k| usage.get(*k).map_or(0, as_int))
    .fold(0i64, i64::saturating_add)
}

/// Python `int(v or 0)`, with unconvertible values counted as 0.
fn as_int(v: &Value) -> i64 {
    match v {
        Value::Bool(b) => i64::from(*b),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Python truthiness, for the `d.get("usage") or ...` fallback.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// --- Filesystem helpers ----------------------------------------------------

fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

/// Immediate subdirectories (following symlinks, like `Path.is_dir`).
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    read_dir_paths(dir)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

/// Every `*.jsonl` file below `dir`, recursively. Symlinked directories are
/// not descended into (pathlib `rglob` default).
fn collect_jsonls(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_jsonls(&path, out);
        } else if has_jsonl_ext(&path) && path.is_file() {
            out.push(path);
        }
    }
}

fn has_jsonl_ext(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "jsonl")
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Lines of `file`, decoded lossily (invalid UTF-8 replaced), stopping at
/// the first read error.
fn lossy_lines(file: File) -> impl Iterator<Item = String> {
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    std::iter::from_fn(move || {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(String::from_utf8_lossy(&buf).into_owned()),
        }
    })
}

// --- Time helpers ----------------------------------------------------------

/// Python `datetime.timestamp()`.
fn unix_seconds(dt: DateTime<Utc>) -> f64 {
    dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_nanos()) / 1e9
}

fn from_unix_seconds(ts: f64) -> Option<DateTime<Utc>> {
    if !ts.is_finite() {
        return None;
    }
    DateTime::from_timestamp_micros((ts * 1e6).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageWindow;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(tmp.path().join("data"));
        (tmp, paths)
    }

    fn window(
        label: &str,
        utilization: Option<f64>,
        resets_at: Option<DateTime<Utc>>,
    ) -> UsageWindow {
        UsageWindow {
            label: label.into(),
            utilization,
            resets_at,
        }
    }

    fn snapshot(fetched_at: DateTime<Utc>) -> Snapshot {
        Snapshot {
            fetched_at,
            windows: vec![
                window("5-hour", Some(12.0), Some(utc(2026, 10, 11, 17, 10, 0))),
                window("7-day all", Some(30.5), None),
                window("Fable", Some(4.0), None),
            ],
            extra: Some(ExtraUsage {
                enabled: true,
                monthly_limit: 75.0,
                used_credits: 76.83,
                utilization: 100.0,
                currency: "EUR".into(),
                disabled_reason: Some("spend_limit".into()),
            }),
        }
    }

    /// Write raw history lines (each gets a trailing newline).
    fn write_history(paths: &Paths, lines: &[String]) {
        fs::create_dir_all(paths.data_dir()).unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(paths.history_file(), body).unwrap();
    }

    fn row(ts: DateTime<Utc>, field: &str, v: f64) -> String {
        json!({"ts": unix_seconds(ts), field: v}).to_string()
    }

    // --- history ------------------------------------------------------------

    #[test]
    fn append_then_read_round_trip() {
        let (_tmp, paths) = temp_paths();
        let t = DateTime::from_timestamp_micros(1_791_000_000_250_000).unwrap();
        append_snapshot(&paths, &snapshot(t)).unwrap();
        let mut bare = snapshot(t + Duration::minutes(5));
        bare.windows.truncate(1);
        bare.extra = None;
        append_snapshot(&paths, &bare).unwrap();

        let rows = read_recent(&paths, t);
        assert_eq!(
            rows,
            vec![
                HistoryEntry {
                    ts: 1_791_000_000.25,
                    five_hour: Some(12.0),
                    seven_day: Some(30.5),
                    fable_7d: Some(4.0),
                    extra_pct: Some(100.0),
                },
                HistoryEntry {
                    ts: 1_791_000_300.25,
                    five_hour: Some(12.0),
                    seven_day: None,
                    fable_7d: None,
                    extra_pct: None,
                },
            ]
        );

        // On-disk keys are exactly the Python schema.
        let text = fs::read_to_string(paths.history_file()).unwrap();
        let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        let mut keys: Vec<&str> = first
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["extra_pct", "fable_7d", "five_hour", "seven_day", "ts"]
        );
        // append also refreshed latest.json.
        assert!(read_latest_snapshot(&paths).is_some());
    }

    #[test]
    fn read_recent_tolerates_legacy_and_missing_keys() {
        let (_tmp, paths) = temp_paths();
        write_history(
            &paths,
            &[
                r#"{"ts": 100.0, "five_hour": 5.0, "seven_day": 9.0, "sonnet_7d": 3.0, "extra_pct": null}"#.into(),
                r#"{"ts": 200, "seven_day": 10}"#.into(),
            ],
        );
        let rows = read_recent(&paths, utc(1970, 1, 1, 0, 0, 0));
        assert_eq!(
            rows,
            vec![
                HistoryEntry {
                    ts: 100.0,
                    five_hour: Some(5.0),
                    seven_day: Some(9.0),
                    fable_7d: None,
                    extra_pct: None,
                },
                HistoryEntry {
                    ts: 200.0,
                    five_hour: None,
                    seven_day: Some(10.0),
                    fable_7d: None,
                    extra_pct: None,
                },
            ]
        );
    }

    #[test]
    fn read_recent_skips_corrupt_lines_and_filters_by_cutoff() {
        let (_tmp, paths) = temp_paths();
        fs::create_dir_all(paths.data_dir()).unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(b"{\"ts\": 50, \"five_hour\": 1}\n");
        body.extend_from_slice(b"not json\n");
        body.extend_from_slice(b"{\"ts\": 150, \"five_hour\": 2, \"x\": \"\xff\xfe\"}\n");
        body.extend_from_slice(b"[1, 2]\n");
        body.extend_from_slice(b"{\"ts\": \"soon\", \"five_hour\": 3}\n");
        body.extend_from_slice(b"{\"five_hour\": 4}\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"{\"ts\": 100, \"five_hour\": 5}");
        fs::write(paths.history_file(), body).unwrap();

        let cutoff = DateTime::from_timestamp(100, 0).unwrap();
        let got: Vec<(f64, Option<f64>)> = read_recent(&paths, cutoff)
            .into_iter()
            .map(|e| (e.ts, e.five_hour))
            .collect();
        assert_eq!(got, vec![(150.0, Some(2.0)), (100.0, Some(5.0))]);
    }

    #[test]
    fn read_recent_missing_file_is_empty() {
        let (_tmp, paths) = temp_paths();
        assert!(read_recent(&paths, utc(2026, 1, 1, 0, 0, 0)).is_empty());
    }

    // --- latest.json ----------------------------------------------------------

    #[test]
    fn latest_json_shape() {
        let (_tmp, paths) = temp_paths();
        let t = DateTime::from_timestamp_micros(1_791_000_000_989_019).unwrap();
        let mut snap = snapshot(t);
        snap.windows.push(window("Opus 7d", None, None));
        write_latest_snapshot(&paths, &snap).unwrap();

        assert_eq!(
            read_latest_snapshot(&paths).unwrap(),
            json!({
                "fetched_at": "2026-10-03T04:00:00.989019+00:00",
                "windows": [
                    {"label": "5-hour", "utilization": 12.0,
                     "resets_at": "2026-10-11T17:10:00+00:00"},
                    {"label": "7-day all", "utilization": 30.5, "resets_at": null},
                    {"label": "Fable", "utilization": 4.0, "resets_at": null},
                    {"label": "Opus 7d", "utilization": null, "resets_at": null},
                ],
                "extra": {"enabled": true, "monthly_limit": 75.0, "used_credits": 76.83,
                          "utilization": 100.0, "currency": "EUR",
                          "disabled_reason": "spend_limit"},
            })
        );
        assert_only_latest_json(&paths);
    }

    /// No temp files are left next to latest.json.
    fn assert_only_latest_json(paths: &Paths) {
        let names: Vec<String> = fs::read_dir(paths.data_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["latest.json"]);
    }

    #[test]
    fn latest_json_keys_keep_insertion_order() {
        let (_tmp, paths) = temp_paths();
        write_latest_snapshot(&paths, &snapshot(utc(2026, 10, 11, 1, 0, 0))).unwrap();
        let text = fs::read_to_string(paths.latest_file()).unwrap();
        let at = |k: &str| text.find(&format!("\"{k}\"")).unwrap();
        assert!(
            at("fetched_at") < at("windows") && at("windows") < at("extra"),
            "{text}"
        );
        assert!(at("label") < at("utilization") && at("utilization") < at("resets_at"));
    }

    #[test]
    fn latest_json_concurrent_writers_do_not_collide() {
        let (_tmp, paths) = temp_paths();
        std::thread::scope(|s| {
            for i in 0..4 {
                let paths = &paths;
                s.spawn(move || {
                    for j in 0..10 {
                        let t = utc(2026, 10, 11, 1, 0, 0) + Duration::seconds(i * 100 + j);
                        write_latest_snapshot(paths, &snapshot(t)).unwrap();
                    }
                });
            }
        });
        assert!(read_latest_snapshot(&paths).is_some());
        assert_only_latest_json(&paths);
    }

    #[test]
    fn latest_json_overwrites_existing_and_null_extra() {
        let (_tmp, paths) = temp_paths();
        write_latest_snapshot(&paths, &snapshot(utc(2026, 10, 11, 1, 0, 0))).unwrap();
        let mut second = snapshot(utc(2026, 10, 11, 2, 0, 0));
        second.extra = None;
        write_latest_snapshot(&paths, &second).unwrap();

        let got = read_latest_snapshot(&paths).unwrap();
        assert_eq!(got["fetched_at"], "2026-10-11T02:00:00+00:00");
        assert_eq!(got["extra"], Value::Null);
        assert_only_latest_json(&paths);
    }

    #[test]
    fn read_latest_missing_or_corrupt_is_none() {
        let (_tmp, paths) = temp_paths();
        assert_eq!(read_latest_snapshot(&paths), None);
        fs::create_dir_all(paths.data_dir()).unwrap();
        fs::write(paths.latest_file(), "{\"fetched_at\": ").unwrap();
        assert_eq!(read_latest_snapshot(&paths), None);
    }

    // --- daily history ------------------------------------------------------------

    #[test]
    fn daily_max_per_local_day_oldest_first() {
        let (_tmp, paths) = temp_paths();
        write_history(
            &paths,
            &[
                row(local(2026, 10, 4, 12, 0), "seven_day", 90.0), // before range
                row(local(2026, 10, 5, 0, 30), "seven_day", 7.0),
                row(local(2026, 10, 9, 9, 0), "seven_day", 10.0),
                row(local(2026, 10, 9, 20, 0), "seven_day", 15.0),
                row(local(2026, 10, 9, 22, 0), "seven_day", 11.0),
                row(local(2026, 10, 11, 1, 0), "seven_day", 20.0),
                row(local(2026, 10, 11, 2, 0), "fable_7d", 2.0),
            ],
        );
        let now = local(2026, 10, 11, 12, 0);
        let got = daily_history(&paths, 7, now);

        let days: Vec<NaiveDate> = got.iter().map(|(d, _)| *d).collect();
        let expected_days: Vec<NaiveDate> = (5..=11)
            .map(|d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap())
            .collect();
        assert_eq!(days, expected_days);
        let vals: Vec<Option<f64>> = got.iter().map(|(_, v)| *v).collect();
        assert_eq!(
            vals,
            vec![Some(7.0), None, None, None, Some(15.0), None, Some(20.0)]
        );

        let fable: Vec<Option<f64>> = fable_daily_history(&paths, 7, now)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        assert_eq!(fable, vec![None, None, None, None, None, None, Some(2.0)]);
    }

    #[test]
    fn rollups_match_the_separate_functions() {
        let (_tmp, paths) = temp_paths();
        let now = local(2026, 10, 11, 12, 0);
        let ws = now - Duration::minutes(100);
        let mut lines = Vec::new();
        // Every 20 minutes for 9 days, all fields, so each rollup's cutoff
        // and every window/day edge has neighbours on both sides.
        let mut t = now - Duration::days(9);
        let mut i = 0.0;
        while t <= now {
            lines.push(
                json!({"ts": unix_seconds(t), "five_hour": i % 97.0,
                       "seven_day": i % 89.0, "fable_7d": i % 13.0})
                .to_string(),
            );
            t += Duration::minutes(20);
            i += 1.0;
        }
        lines.push("not json".into());
        write_history(&paths, &lines);

        for days in [0, 1, 7] {
            let got = rollups(&paths, ws, ws + Duration::hours(5), days, now);
            assert_eq!(got.daily, daily_history(&paths, days, now));
            assert_eq!(got.fable_daily, fable_daily_history(&paths, days, now));
        }
        // Window older than the daily range: the window cutoff wins.
        let old_ws = now - Duration::days(8);
        let got = rollups(&paths, old_ws, old_ws + Duration::hours(5), 7, now);
        assert!(got.window_samples.iter().any(|w| w.five_hour.is_some()));
        assert_eq!(got.daily, daily_history(&paths, 7, now));
        // The weekly samples: the last 24 h every 20 minutes, both ends in.
        let recent = &got.seven_day_recent;
        assert_eq!(recent.len(), 24 * 3 + 1);
        assert!(recent.windows(2).all(|w| w[0].0 < w[1].0));
        let first = recent.first().unwrap().0;
        assert!((first - (now - Duration::hours(24))).num_seconds().abs() <= 1);
        assert!((recent.last().unwrap().0 - now).num_seconds().abs() <= 1);
        // Even with no daily range, the 24 h are read.
        let got = rollups(&paths, ws, ws + Duration::hours(5), 0, now);
        assert_eq!(got.seven_day_recent.len(), 24 * 3 + 1);
    }

    #[test]
    fn rollups_window_samples_are_the_rows_in_the_window_from_one_read() {
        let (_tmp, paths) = temp_paths();
        let ws = utc(2026, 10, 11, 10, 0, 0);
        let now = ws + Duration::minutes(200);
        let line = |t: DateTime<Utc>, five: Value, fable: Value| {
            json!({"ts": unix_seconds(t), "five_hour": five, "fable_7d": fable}).to_string()
        };
        write_history(
            &paths,
            &[
                line(ws - Duration::seconds(1), json!(99.0), json!(9.0)), // before
                line(ws + Duration::minutes(30), json!(20.0), json!(3.0)),
                line(ws, json!(10.0), Value::Null), // at the start, out of order
                "garbage".into(),
                line(ws + Duration::minutes(60), Value::Null, json!(4.0)),
                line(ws + Duration::minutes(300), json!(1.0), json!(1.0)), // at the end
            ],
        );
        let before = history_reads();
        let got = rollups(&paths, ws, ws + Duration::hours(5), 7, now);
        assert_eq!(history_reads() - before, 1, "rollups reads history once");
        let sample = |m: i64, five, fable| WindowSample {
            t: ws + Duration::minutes(m),
            five_hour: five,
            fable,
        };
        assert_eq!(
            got.window_samples,
            [
                sample(0, Some(10.0), None),
                sample(30, Some(20.0), Some(3.0)),
                sample(60, None, Some(4.0)),
            ]
        );
    }

    #[test]
    fn daily_zero_days_is_empty() {
        let (_tmp, paths) = temp_paths();
        assert!(daily_history(&paths, 0, Utc::now()).is_empty());
    }

    // --- attribution -----------------------------------------------------------------

    fn write_lines(path: &Path, lines: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn attribution_both_sources_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("projects");
        let cw = tmp.path().join("sessions");
        let cutoff = utc(2026, 10, 11, 10, 0, 0);
        let cutoff_s = unix_seconds(cutoff);

        // Claude Code project A: message.usage nesting, ISO "Z" timestamps.
        write_lines(
            &cc.join("proj-a").join("s1.jsonl"),
            &[
                json!({"timestamp": "2026-10-11T09:59:59Z",
                       "message": {"usage": {"input_tokens": 1000}}}),
                json!({"timestamp": "2026-10-11T10:30:00.123Z",
                       "message": {"usage": {"input_tokens": 10, "output_tokens": 20,
                                             "cache_read_input_tokens": 30,
                                             "cache_creation_input_tokens": 40}}}),
                json!({"timestamp": "2026-10-11T11:00:00+00:00",
                       "usage": {}, "message": {"usage": {"output_tokens": 5}}}),
            ],
        );
        // Not a jsonl; and a nested jsonl that Claude Code scanning ignores.
        fs::write(cc.join("proj-a").join("notes.txt"), "{\"ts\": 1e12}").unwrap();
        write_lines(
            &cc.join("proj-a").join("sub").join("deep.jsonl"),
            &[json!({"ts": cutoff_s + 1.0, "usage": {"input_tokens": 99999}})],
        );
        // Project B: top-level usage, numeric ts, garbage lines.
        let b = cc.join("proj-b").join("x.jsonl");
        fs::create_dir_all(b.parent().unwrap()).unwrap();
        fs::write(
            &b,
            format!(
                "garbage\n{}\n[1]\n{}\n",
                json!({"ts": cutoff_s + 60.0, "usage": {"input_tokens": 500, "output_tokens": "7"}}),
                json!({"created_at": cutoff_s - 1.0, "usage": {"input_tokens": 1}}),
            ),
        )
        .unwrap();
        // Project C: only old entries -> omitted.
        write_lines(
            &cc.join("proj-c").join("old.jsonl"),
            &[json!({"ts": cutoff_s - 10.0, "usage": {"input_tokens": 5}})],
        );

        // Cowork: user/org/local_<id>, jsonl found recursively.
        let session = cw.join("user").join("org").join("local_abcdef12-3456");
        write_lines(
            &session.join("a").join("b").join("log.jsonl"),
            &[json!({"created_at": "2026-10-11T12:00:00Z",
                     "usage": {"cache_read_input_tokens": 200}})],
        );
        // Not under a local_* dir -> ignored.
        write_lines(
            &cw.join("user").join("org").join("other").join("log.jsonl"),
            &[json!({"ts": cutoff_s + 1.0, "usage": {"input_tokens": 7777}})],
        );

        let got = attribute_recent(&cc, &cw, cutoff);
        assert_eq!(
            got,
            vec![
                InstanceUsage {
                    name: "proj-b".into(),
                    display_name: "proj-b".into(),
                    source: "claude-code",
                    tokens: 507,
                    last_active: cutoff + Duration::minutes(1),
                },
                InstanceUsage {
                    name: "cowork:abcdef12".into(),
                    display_name: "cowork:abcdef12".into(),
                    source: "cowork",
                    tokens: 200,
                    last_active: utc(2026, 10, 11, 12, 0, 0),
                },
                InstanceUsage {
                    name: "proj-a".into(),
                    display_name: "proj-a".into(),
                    source: "claude-code",
                    tokens: 105,
                    last_active: utc(2026, 10, 11, 11, 0, 0),
                },
            ]
        );
    }

    #[test]
    fn attribution_display_names_from_latest_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("projects");
        let cutoff = utc(2026, 10, 11, 10, 0, 0);
        let s = unix_seconds(cutoff);
        let used = |ts: f64, tokens: i64, cwd: Option<&str>| {
            let mut v = json!({"ts": ts, "usage": {"input_tokens": tokens}});
            if let Some(dir) = cwd {
                v["cwd"] = json!(dir);
            }
            v
        };

        // Windows paths across two files; the latest non-empty cwd wins.
        let win = cc.join("C--Users-x-Documents---tomos-claude-usage-monitor");
        write_lines(
            &win.join("a.jsonl"),
            &[
                used(s + 10.0, 100, Some(r"C:\Users\x\old-name")),
                used(
                    s + 30.0,
                    1,
                    Some(r"C:\Users\x\Documents\__tomos\claude-usage-monitor"),
                ),
            ],
        );
        write_lines(
            &win.join("b.jsonl"),
            &[
                used(s + 20.0, 1, Some(r"C:\Users\x\middle")),
                used(s + 40.0, 1, Some("")), // empty: ignored
                json!({"cwd": r"C:\no-timestamp", "usage": {}}), // no ts: ignored
            ],
        );
        // POSIX path, only on an entry from before the window.
        write_lines(
            &cc.join("-home-bob-widget").join("s.jsonl"),
            &[
                used(s - 600.0, 5, Some("/home/bob/widget")),
                used(s + 5.0, 50, None),
            ],
        );
        // No cwd at all: the prettified directory name.
        write_lines(
            &cc.join("F--MIMIC-III").join("s.jsonl"),
            &[used(s + 5.0, 10, None)],
        );

        let got = attribute_recent(&cc, &tmp.path().join("cw"), cutoff);
        let names: Vec<(&str, &str)> = got
            .iter()
            .map(|i| (i.name.as_str(), i.display_name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (
                    "C--Users-x-Documents---tomos-claude-usage-monitor",
                    "claude-usage-monitor"
                ),
                ("-home-bob-widget", "widget"),
                ("F--MIMIC-III", "MIMIC-III"),
            ]
        );
    }

    #[test]
    fn attribution_missing_dirs_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let got = attribute_recent(
            &tmp.path().join("nope"),
            &tmp.path().join("nada"),
            Utc::now(),
        );
        assert!(got.is_empty());
    }

    #[test]
    fn attribution_skips_files_modified_before_window() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("projects");
        let f = cc.join("proj").join("s.jsonl");
        // Entry claims a recent timestamp, but the file is older than the window.
        write_lines(&f, &[json!({"ts": 4.0e9, "usage": {"input_tokens": 9}})]);
        let window_start = Utc::now() + Duration::hours(1);
        assert!(attribute_recent(&cc, &tmp.path().join("cw"), window_start).is_empty());
    }

    #[test]
    fn extract_ts_key_order_and_formats() {
        assert_eq!(extract_ts(&json!({"timestamp": 5, "ts": 9})), Some(5.0));
        assert_eq!(extract_ts(&json!({"timestamp": "bad", "ts": 9})), Some(9.0));
        assert_eq!(
            extract_ts(&json!({"timestamp": null, "created_at": 2.5})),
            Some(2.5)
        );
        assert_eq!(
            extract_ts(&json!({"ts": "2026-10-11T10:00:00+02:00"})),
            Some(unix_seconds(utc(2026, 10, 11, 8, 0, 0)))
        );
        assert_eq!(
            extract_ts(&json!({"ts": "2026-10-11T10:00:00"})),
            Some(unix_seconds(local(2026, 10, 11, 10, 0)))
        );
        assert_eq!(extract_ts(&json!({"other": 1})), None);
    }

    #[test]
    fn extract_tokens_shapes() {
        assert_eq!(
            extract_tokens(&json!({"usage": {"input_tokens": 1.9, "output_tokens": null}})),
            1
        );
        assert_eq!(extract_tokens(&json!({"usage": "x"})), 0);
        assert_eq!(extract_tokens(&json!({"message": "x"})), 0);
        assert_eq!(extract_tokens(&json!({})), 0);
        assert_eq!(
            extract_tokens(&json!({"usage": {"input_tokens": "abc"}})),
            0
        );
    }
}
