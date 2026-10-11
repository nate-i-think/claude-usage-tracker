//! Usage data model and parsing of the claude.ai `/usage` response.
//!
//! Port of the models and parsing in api.py. Pure: no I/O. Parsing is
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

/// One usage window (a top-level bucket or a per-model limit).
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

/// One fetched usage snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub fetched_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
    pub extra: Option<ExtraUsage>,
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
}

/// Parse a full `/usage` response body into a [`Snapshot`].
///
/// Top-level buckets come first in [`BUCKET_LABELS`] order (kept if in
/// [`ALWAYS_SHOW`] or utilization is non-null), then per-model limits from
/// the `limits` array, then `extra_usage`.
pub fn parse_usage(data: &Value, fetched_at: DateTime<Utc>) -> Snapshot {
    let mut windows: Vec<UsageWindow> = BUCKET_LABELS
        .iter()
        .filter_map(|&(key, label)| {
            let w = parse_window(label, data.get(key));
            (ALWAYS_SHOW.contains(&key) || w.utilization.is_some()).then_some(w)
        })
        .collect();
    windows.extend(parse_model_limits(data));

    Snapshot {
        fetched_at,
        windows,
        extra: parse_extra_usage(data.get("extra_usage")),
    }
}

/// Extract per-model windows from the `limits` array.
///
/// Entries with a non-empty `scope.model.display_name` become windows labeled
/// by that name, with `percent` as utilization. Others are skipped.
pub fn parse_model_limits(data: &Value) -> Vec<UsageWindow> {
    let Some(entries) = data.get("limits").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())?;
            Some(UsageWindow {
                label: name.to_owned(),
                utilization: entry.get("percent").and_then(as_f64),
                resets_at: parse_dt(entry.get("resets_at")),
            })
        })
        .collect()
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

    /// Trimmed copy of a real /usage packet (2026-07-01); from test_api.py.
    fn fable_packet() -> Value {
        json!({
            "five_hour": {"utilization": 12.0, "resets_at": "2026-07-01T21:10:01+00:00"},
            "seven_day": {"utilization": 13.0, "resets_at": "2026-07-07T06:00:01+00:00"},
            "seven_day_sonnet": null,
            "limits": [
                {"kind": "session", "percent": 12,
                 "resets_at": "2026-07-01T21:10:01+00:00", "scope": null},
                {"kind": "weekly_all", "percent": 13,
                 "resets_at": "2026-07-07T06:00:01+00:00", "scope": null},
                {"kind": "weekly_scoped", "percent": 0, "resets_at": null,
                 "scope": {"model": {"display_name": "Fable", "id": null},
                           "surface": null}}
            ]
        })
    }

    // --- Ports of test_api.py ---------------------------------------------

    #[test]
    fn extracts_fable_window_from_limits_array() {
        let labels: Vec<String> = parse_model_limits(&fable_packet())
            .into_iter()
            .map(|w| w.label)
            .collect();
        assert_eq!(labels, vec!["Fable"]);
    }

    #[test]
    fn fable_zero_percent_maps_to_zero_utilization_not_none() {
        let fable = &parse_model_limits(&fable_packet())[0];
        assert_eq!(fable.utilization, Some(0.0));
    }

    #[test]
    fn null_resets_at_parses_to_none() {
        let fable = &parse_model_limits(&fable_packet())[0];
        assert_eq!(fable.resets_at, None);
    }

    #[test]
    fn active_model_limit_parses_percent_and_resets() {
        let data = json!({"limits": [
            {"kind": "weekly_scoped", "percent": 42,
             "resets_at": "2026-07-07T06:00:01+00:00",
             "scope": {"model": {"display_name": "Fable"}}}
        ]});
        let fable = &parse_model_limits(&data)[0];
        assert_eq!(fable.utilization, Some(42.0));
        assert_eq!(fable.resets_at, Some(utc(2026, 7, 7, 6, 0, 1)));
    }

    #[test]
    fn ignores_session_and_weekly_all_entries() {
        let windows = parse_model_limits(&fable_packet());
        assert!(windows.iter().all(|w| w.label == "Fable"), "{windows:?}");
    }

    #[test]
    fn missing_limits_key_returns_empty_list() {
        assert!(parse_model_limits(&json!({"five_hour": {"utilization": 5.0}})).is_empty());
    }

    #[test]
    fn limits_none_returns_empty_list() {
        assert!(parse_model_limits(&json!({"limits": null})).is_empty());
    }

    // --- parse_usage --------------------------------------------------------

    #[test]
    fn parse_usage_orders_buckets_then_model_limits() {
        let mut data = fable_packet();
        data["seven_day_opus"] = json!({"utilization": 3, "resets_at": null});
        let fetched = utc(2026, 7, 1, 20, 0, 0);
        let snap = parse_usage(&data, fetched);
        let labels: Vec<&str> = snap.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["5-hour", "7-day all", "Opus 7d", "Fable"]);
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
        let snap = parse_usage(&fable_packet(), Utc::now());
        assert_eq!(snap.five_hour_pct(), Some(12.0));
        assert_eq!(snap.five_hour_resets_at(), Some(utc(2026, 7, 1, 21, 10, 1)));

        let empty = Snapshot {
            fetched_at: Utc::now(),
            windows: vec![],
            extra: None,
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
        assert_eq!(snap.window("Fable").unwrap().utilization, Some(18.0));
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
