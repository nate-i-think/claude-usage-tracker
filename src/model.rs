//! Usage data model and parsing of the claude.ai `/usage` response.
//!
//! Ported from the models and parsing in the Python api.py (removed; see
//! git history). Pure: no I/O. Parsing is
//! tolerant in the way Python dict access is: missing keys, nulls, and
//! values of the wrong type degrade to `None`/defaults instead of failing.

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

/// Top-level usage buckets, as `(response key, display label)`, in display order.
pub const BUCKET_LABELS: &[(&str, &str)] = &[
    ("five_hour", "5-hour"),
    ("seven_day", "7-day all"),
    ("seven_day_sonnet", "Sonnet 7d"),
    ("seven_day_opus", "Opus 7d"),
    ("seven_day_cowork", "Cowork 7d"),
    ("seven_day_oauth_apps", "OAuth 7d"),
    ("seven_day_omelette", "Omelette 7d"),
    ("iguana_necktie", "Iguana"),
    ("omelette_promotional", "Omelette promo"),
];

/// Bucket keys included in a snapshot even when their utilization is null.
pub const ALWAYS_SHOW: &[&str] = &["five_hour", "seven_day"];

/// Label of the five-hour window.
pub const FIVE_HOUR_LABEL: &str = "5-hour";
/// Label of the seven-day (all models) window.
pub const SEVEN_DAY_LABEL: &str = "7-day all";

/// One usage window (a top-level bucket).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    pub label: String,
    /// Percent used, or `None` when the window is inactive.
    pub utilization: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

/// Paid extra-usage state. Money amounts are in currency units (not cents).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraUsage {
    pub enabled: bool,
    pub monthly_limit: f64,
    pub used_credits: f64,
    pub utilization: f64,
    pub currency: String,
    pub disabled_reason: Option<String>,
}

/// One surface's share of the week's usage (e.g. Claude Code, Chats).
#[derive(Debug, Clone, PartialEq)]
pub struct BreakdownRow {
    pub key: String,
    pub display_name: String,
    /// Percent of the week's usage on this surface.
    pub percent: f64,
}

/// The `seven_day_breakdown`: share of the week's usage by surface.
#[derive(Debug, Clone, PartialEq)]
pub struct WeekBreakdown {
    pub as_of: Option<DateTime<Utc>>,
    pub window_started_at: Option<DateTime<Utc>>,
    pub rows: Vec<BreakdownRow>,
}

/// One fetched usage snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub fetched_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
    pub extra: Option<ExtraUsage>,
    pub breakdown: Option<WeekBreakdown>,
}

impl Snapshot {
    /// The first window with this label, if any.
    pub fn window(&self, label: &str) -> Option<&UsageWindow> {
        self.windows.iter().find(|w| w.label == label)
    }

    pub fn five_hour_pct(&self) -> Option<f64> {
        self.window(FIVE_HOUR_LABEL).and_then(|w| w.utilization)
    }

    pub fn five_hour_resets_at(&self) -> Option<DateTime<Utc>> {
        self.window(FIVE_HOUR_LABEL).and_then(|w| w.resets_at)
    }

    pub fn seven_day_pct(&self) -> Option<f64> {
        self.window(SEVEN_DAY_LABEL).and_then(|w| w.utilization)
    }

    pub fn seven_day_resets_at(&self) -> Option<DateTime<Utc>> {
        self.window(SEVEN_DAY_LABEL).and_then(|w| w.resets_at)
    }
}

/// Parse a full `/usage` response body into a [`Snapshot`].
///
/// The windows are the top-level buckets in [`BUCKET_LABELS`] order (kept
/// if in [`ALWAYS_SHOW`] or utilization is non-null); `extra_usage` and
/// `seven_day_breakdown` fill the rest. Other keys, the `limits` array
/// among them, are ignored.
pub fn parse_usage(data: &Value, fetched_at: DateTime<Utc>) -> Snapshot {
    let windows: Vec<UsageWindow> = BUCKET_LABELS
        .iter()
        .filter_map(|&(key, label)| {
            let w = parse_window(label, data.get(key));
            (ALWAYS_SHOW.contains(&key) || w.utilization.is_some()).then_some(w)
        })
        .collect();

    Snapshot {
        fetched_at,
        windows,
        extra: parse_extra_usage(data.get("extra_usage")),
        breakdown: parse_breakdown(data.get("seven_day_breakdown")),
    }
}

/// Parse a `seven_day_breakdown` object (from the API or latest.json).
///
/// Null, `{}` and non-objects are `None`. Rows missing a string `key` or
/// `display_name`, or a numeric `percent`, are skipped; an object with no
/// valid rows is still `Some` with empty `rows`.
pub fn parse_breakdown(raw: Option<&Value>) -> Option<WeekBreakdown> {
    let b = raw.and_then(Value::as_object).filter(|o| !o.is_empty())?;
    let rows = b
        .get("rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_owned);
            Some(BreakdownRow {
                key: s("key")?,
                display_name: s("display_name")?,
                percent: r.get("percent").and_then(as_f64)?,
            })
        })
        .collect();
    Some(WeekBreakdown {
        as_of: parse_dt(b.get("as_of")),
        window_started_at: parse_dt(b.get("window_started_at")),
        rows,
    })
}

fn parse_window(label: &str, raw: Option<&Value>) -> UsageWindow {
    UsageWindow {
        label: label.to_owned(),
        utilization: raw.and_then(|r| r.get("utilization")).and_then(as_f64),
        resets_at: parse_dt(raw.and_then(|r| r.get("resets_at"))),
    }
}

fn parse_extra_usage(raw: Option<&Value>) -> Option<ExtraUsage> {
    // Python: `if (er := data.get("extra_usage"))` -- null and {} are skipped.
    let er = raw.and_then(Value::as_object).filter(|o| !o.is_empty())?;
    let num = |k: &str| er.get(k).and_then(as_f64).unwrap_or(0.0);
    Some(ExtraUsage {
        enabled: er.get("is_enabled").is_some_and(truthy),
        // API returns money amounts in cents.
        monthly_limit: num("monthly_limit") / 100.0,
        used_credits: num("used_credits") / 100.0,
        utilization: num("utilization"),
        currency: er
            .get("currency")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .unwrap_or("USD")
            .to_owned(),
        disabled_reason: er
            .get("disabled_reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// A JSON number (or numeric string) as f64.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python-style truthiness of a JSON value.
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

/// Parse an ISO-8601 timestamp. Accepts `Z` or numeric offsets and fractional
/// seconds; an offset-less timestamp is taken as UTC. Unparseable or
/// non-string values yield `None`.
fn parse_dt(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = v.and_then(Value::as_str)?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|n| n.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    /// Trimmed copy of a real /usage packet (2026-07-01); from the Python test_api.py (removed).
    fn packet() -> Value {
        json!({
            "five_hour": {"utilization": 12.0, "resets_at": "2026-07-01T21:10:01+00:00"},
            "seven_day": {"utilization": 13.0, "resets_at": "2026-07-07T06:00:01+00:00"},
            "seven_day_sonnet": null
        })
    }

    // --- parse_usage --------------------------------------------------------

    #[test]
    fn parse_usage_orders_buckets() {
        let mut data = packet();
        data["seven_day_opus"] = json!({"utilization": 3, "resets_at": null});
        let fetched = utc(2026, 7, 1, 20, 0, 0);
        let snap = parse_usage(&data, fetched);
        let labels: Vec<&str> = snap.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["5-hour", "7-day all", "Opus 7d"]);
        assert_eq!(snap.fetched_at, fetched);
        assert_eq!(snap.window("Opus 7d").unwrap().utilization, Some(3.0));
        assert_eq!(snap.extra, None);
    }

    #[test]
    fn always_show_buckets_present_even_when_missing_or_null() {
        let snap = parse_usage(&json!({"seven_day": null}), Utc::now());
        let labels: Vec<&str> = snap.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["5-hour", "7-day all"]);
        assert!(snap
            .windows
            .iter()
            .all(|w| w.utilization.is_none() && w.resets_at.is_none()));
    }

    #[test]
    fn non_always_bucket_with_null_utilization_is_skipped() {
        let data =
            json!({"seven_day_sonnet": {"utilization": null, "resets_at": "2026-07-07T06:00:01Z"}});
        let snap = parse_usage(&data, Utc::now());
        assert!(snap.window("Sonnet 7d").is_none());
    }

    #[test]
    fn five_hour_accessors() {
        let snap = parse_usage(&packet(), Utc::now());
        assert_eq!(snap.five_hour_pct(), Some(12.0));
        assert_eq!(snap.five_hour_resets_at(), Some(utc(2026, 7, 1, 21, 10, 1)));

        let empty = Snapshot {
            fetched_at: Utc::now(),
            windows: vec![],
            extra: None,
            breakdown: None,
        };
        assert_eq!(empty.five_hour_pct(), None);
        assert_eq!(empty.five_hour_resets_at(), None);
    }

    #[test]
    fn extra_usage_converts_cents_and_reads_fields() {
        let data = json!({"extra_usage": {
            "is_enabled": true, "monthly_limit": 7500, "used_credits": 7683.0,
            "utilization": 100.0, "currency": "EUR", "disabled_reason": "spend_limit",
            "decimal_places": 2
        }});
        let extra = parse_usage(&data, Utc::now()).extra.unwrap();
        assert_eq!(
            extra,
            ExtraUsage {
                enabled: true,
                monthly_limit: 75.0,
                used_credits: 76.83,
                utilization: 100.0,
                currency: "EUR".into(),
                disabled_reason: Some("spend_limit".into()),
            }
        );
    }

    #[test]
    fn extra_usage_defaults() {
        let data = json!({"extra_usage": {"monthly_limit": null, "currency": null}});
        let extra = parse_usage(&data, Utc::now()).extra.unwrap();
        assert!(!extra.enabled);
        assert_eq!(extra.monthly_limit, 0.0);
        assert_eq!(extra.used_credits, 0.0);
        assert_eq!(extra.utilization, 0.0);
        assert_eq!(extra.currency, "USD");
        assert_eq!(extra.disabled_reason, None);
    }

    #[test]
    fn extra_usage_null_or_empty_is_none() {
        assert_eq!(
            parse_usage(&json!({"extra_usage": null}), Utc::now()).extra,
            None
        );
        assert_eq!(
            parse_usage(&json!({"extra_usage": {}}), Utc::now()).extra,
            None
        );
    }

    #[test]
    fn datetimes_accept_z_offset_and_fractions() {
        let parse = |s: &str| parse_dt(Some(&json!(s)));
        let base = utc(2026, 10, 11, 17, 10, 0);
        assert_eq!(parse("2026-10-11T17:10:00Z"), Some(base));
        assert_eq!(parse("2026-10-11T17:10:00+00:00"), Some(base));
        assert_eq!(parse("2026-10-11T19:10:00+02:00"), Some(base));
        let frac = parse("2026-10-11T17:10:00.989019+00:00").unwrap();
        assert_eq!(frac.timestamp_subsec_micros(), 989019);
        assert_eq!(parse("2026-10-11T17:10:00"), Some(base));
        assert_eq!(parse(""), None);
        assert_eq!(parse("not a date"), None);
        assert_eq!(parse_dt(Some(&json!(5))), None);
        assert_eq!(parse_dt(None), None);
    }

    #[test]
    fn ignores_unknown_fields_like_live_response() {
        let data = json!({
            "five_hour": {"utilization": 14.0, "resets_at": "2026-10-11T17:10:00.989019+00:00",
                          "limit_dollars": null, "used_dollars": null,
                          "remaining_dollars": null, "locked_reason": null},
            "amber_cistern": null,
            "limits": [{"group": "weekly", "kind": "weekly_scoped", "is_active": false,
                        "percent": 18, "resets_at": "2026-10-13T06:00:00.989214+00:00",
                        "severity": "normal",
                        "scope": {"model": {"display_name": "Fable", "id": null}, "surface": null}}],
            "spend": {"percent": 1},
            "member_dashboard_available": true
        });
        let snap = parse_usage(&data, Utc::now());
        assert_eq!(snap.five_hour_pct(), Some(14.0));
        // A per-model limit (Fable's) in `limits` makes no window.
        let labels: Vec<&str> = snap.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["5-hour", "7-day all"]);
    }

    // --- seven_day_breakdown -------------------------------------------------

    /// Live OAuth `/api/oauth/usage` response (2026-10-28); contains no secrets.
    const OAUTH_FIXTURE: &str = r#"{"five_hour":{"utilization":0.0,"resets_at":"2026-10-28T22:20:00.634580+00:00"},"seven_day":{"utilization":7.0,"resets_at":"2026-11-03T06:00:00.634598+00:00"},"seven_day_oauth_apps":null,"seven_day_opus":null,"seven_day_sonnet":null,"seven_day_cowork":null,"seven_day_omelette":null,"tangelo":null,"iguana_necktie":{"utilization":0.0,"resets_at":"2026-11-05T07:59:00+00:00","limit_dollars":250,"used_dollars":0.0,"remaining_dollars":250.0,"locked_reason":null},"extra_usage":{"is_enabled":false,"monthly_limit":7500,"used_credits":7683.0,"utilization":100.0,"currency":"USD","decimal_places":2,"disabled_reason":"org_level_disabled_until","user_disabled":false,"spend_limit_reached":true},"limits":[{"kind":"session","group":"session","percent":0,"severity":"normal","resets_at":"2026-10-28T22:20:00.634580+00:00","scope":null,"is_active":false},{"kind":"weekly_all","group":"weekly","percent":7,"severity":"normal","resets_at":"2026-11-03T06:00:00.634598+00:00","scope":null,"is_active":true},{"kind":"weekly_scoped","group":"weekly","percent":0,"severity":"normal","resets_at":"2026-11-03T05:59:59.634730+00:00","scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},"is_active":false}],"seven_day_breakdown":{"as_of":"2026-10-28T17:35:00.691317+00:00","window_started_at":"2026-10-27T06:00:00.634598+00:00","rows":[{"key":"claude_code","display_name":"Claude Code","percent":72},{"key":"chat","display_name":"Chats","percent":11},{"key":"cowork","display_name":"Cowork","percent":17},{"key":"other","display_name":"Other","percent":0}]}}"#;

    fn row(key: &str, display_name: &str, percent: f64) -> BreakdownRow {
        BreakdownRow {
            key: key.into(),
            display_name: display_name.into(),
            percent,
        }
    }

    #[test]
    fn oauth_fixture_parses_windows_extra_and_breakdown() {
        let data: Value = serde_json::from_str(OAUTH_FIXTURE).unwrap();
        let snap = parse_usage(&data, Utc::now());
        let labels: Vec<&str> = snap.windows.iter().map(|w| w.label.as_str()).collect();
        // Its `limits` array (a Fable entry among them) is ignored.
        assert_eq!(labels, vec!["5-hour", "7-day all", "Iguana"]);
        assert_eq!(snap.five_hour_pct(), Some(0.0));
        assert_eq!(snap.seven_day_pct(), Some(7.0));
        assert_eq!(
            snap.seven_day_resets_at()
                .unwrap()
                .timestamp_subsec_micros(),
            634598
        );
        let extra = snap.extra.as_ref().unwrap();
        assert!(!extra.enabled);
        assert_eq!(extra.monthly_limit, 75.0);
        assert_eq!(
            extra.disabled_reason.as_deref(),
            Some("org_level_disabled_until")
        );

        let b = snap.breakdown.unwrap();
        assert_eq!(
            b.as_of
                .map(|t| (t.timestamp(), t.timestamp_subsec_micros())),
            Some((utc(2026, 10, 28, 17, 35, 0).timestamp(), 691317))
        );
        assert_eq!(
            b.window_started_at.map(|t| t.timestamp()),
            Some(utc(2026, 10, 27, 6, 0, 0).timestamp())
        );
        assert_eq!(
            b.rows,
            vec![
                row("claude_code", "Claude Code", 72.0),
                row("chat", "Chats", 11.0),
                row("cowork", "Cowork", 17.0),
                row("other", "Other", 0.0),
            ]
        );
    }

    #[test]
    fn breakdown_null_missing_or_not_an_object_is_none() {
        for data in [
            json!({}),
            json!({"seven_day_breakdown": null}),
            json!({"seven_day_breakdown": {}}),
            json!({"seven_day_breakdown": [1, 2]}),
            json!({"seven_day_breakdown": "x"}),
        ] {
            assert_eq!(parse_usage(&data, Utc::now()).breakdown, None, "{data}");
        }
    }

    #[test]
    fn breakdown_skips_malformed_rows_and_tolerates_bad_fields() {
        let data = json!({"seven_day_breakdown": {
            "as_of": "not a date",
            "window_started_at": null,
            "rows": [
                {"key": "claude_code", "display_name": "Claude Code", "percent": 72.5},
                {"display_name": "No key", "percent": 1},
                {"key": "nameless", "percent": 1},
                {"key": "nopct", "display_name": "No percent"},
                {"key": "nullpct", "display_name": "Null percent", "percent": null},
                {"key": 7, "display_name": "Bad key", "percent": 1},
                null,
                "x",
                {"key": "chat", "display_name": "Chats", "percent": 11}
            ]
        }});
        let b = parse_usage(&data, Utc::now()).breakdown.unwrap();
        assert_eq!(b.as_of, None);
        assert_eq!(b.window_started_at, None);
        assert_eq!(
            b.rows,
            vec![
                row("claude_code", "Claude Code", 72.5),
                row("chat", "Chats", 11.0)
            ]
        );
    }

    #[test]
    fn breakdown_with_no_valid_rows_is_some_and_empty() {
        for rows in [json!(null), json!("x"), json!([]), json!([{"key": "a"}])] {
            let data = json!({"seven_day_breakdown": {
                "as_of": "2026-10-28T17:35:00Z", "rows": rows
            }});
            let b = parse_usage(&data, Utc::now()).breakdown.unwrap();
            assert_eq!(b.as_of, Some(utc(2026, 10, 28, 17, 35, 0)));
            assert!(b.rows.is_empty(), "{data}");
        }
    }

    #[test]
    fn malformed_shapes_do_not_panic() {
        for data in [
            json!(null),
            json!([]),
            json!("x"),
            json!({"five_hour": "oops", "limits": {"not": "array"}, "extra_usage": 5}),
            json!({"five_hour": [1, 2], "limits": [null, 3, {"scope": "x"},
                   {"scope": {"model": "x"}}, {"scope": {"model": {"display_name": 7}}},
                   {"scope": {"model": {"display_name": ""}}}]}),
            json!({"extra_usage": {"monthly_limit": "abc", "currency": 3, "is_enabled": "yes"}}),
        ] {
            let snap = parse_usage(&data, Utc::now());
            assert_eq!(snap.windows.len(), 2, "only ALWAYS_SHOW for {data}");
        }
    }
}
