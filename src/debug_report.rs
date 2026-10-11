//! Pure formatting for `claude-usage-cli debug`: masking, profile and cookie
//! tables, the response body sniff, and the recommendation.
//!
//! Ported from the Python debug.py (removed; see git history), minus the
//! TLS-profile sweep (one HTTP stack now). No
//! function here prints a cookie value except through [`mask`].

use chrono::{DateTime, Local, TimeZone, Utc};

use crate::cookies::{Cookie, CookieJar, ProfileCandidate};
use crate::model;

/// Characters kept at each end by [`mask`].
const MASK_KEEP: usize = 6;
/// Characters of an unrecognized body shown in the sniff.
const SNIFF_CHARS: usize = 120;

/// Cookies listed first in the inventory, with what they are for.
const IMPORTANT_COOKIES: &[(&str, &str)] = &[
    ("cf_clearance", "Cloudflare challenge clearance"),
    ("__cf_bm", "Cloudflare bot management (~30m TTL)"),
    ("sessionKey", "claude.ai auth"),
    ("lastActiveOrg", "active org UUID"),
    ("anthropic-device-id", "device ID (sent as a header too)"),
];

/// As the Python debug.py `_mask` (removed): "<none>" for no value; all
/// `*` when 15 characters or fewer; else the first and last 6 characters
/// around "…" plus the length.
pub fn mask(value: Option<&str>) -> String {
    let Some(v) = value else {
        return "<none>".to_owned();
    };
    let chars: Vec<char> = v.chars().collect();
    let n = chars.len();
    if n <= MASK_KEEP * 2 + 3 {
        return "*".repeat(n);
    }
    let head: String = chars[..MASK_KEEP].iter().collect();
    let tail: String = chars[n - MASK_KEEP..].iter().collect();
    format!("{head}…{tail} ({n} chars)")
}

/// The meaning of a profile's score.
pub fn score_label(c: &ProfileCandidate) -> String {
    match c.score {
        3 => "live".to_owned(),
        2 => "expired".to_owned(),
        1 => "no sessionKey".to_owned(),
        0 => "no claude.ai cookies".to_owned(),
        _ => format!(
            "load error: {}",
            c.error.as_deref().unwrap_or("unknown error")
        ),
    }
}

/// sessionKey expiry relative to now: "valid 23.4h", "expired 3.2d ago",
/// or "—" when unknown.
pub fn session_relative(expiry: Option<f64>, now: f64) -> String {
    match expiry {
        None => "—".to_owned(),
        Some(e) if e < now => format!("expired {:.1}d ago", (now - e) / 86_400.0),
        Some(e) => format!("valid {:.1}h", (e - now) / 3_600.0),
    }
}

/// A Unix timestamp as "YYYY-MM-DD HH:MM" in `tz`; empty when out of range.
pub fn absolute_in<Tz: TimeZone>(ts: f64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    DateTime::<Utc>::from_timestamp(ts as i64, 0)
        .map(|t| t.with_timezone(tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn absolute(ts: f64) -> String {
    absolute_in(ts, &Local)
}

/// A cookie's expiry status (as the Python debug.py inventory): "session", "EXPIRED 5m
/// ago", minutes under 10 minutes, hours under a day, else days.
pub fn cookie_expiry(expiry: Option<f64>, now: f64) -> String {
    let Some(e) = expiry else {
        return "session".to_owned();
    };
    let d = e - now;
    if d < 0.0 {
        format!("EXPIRED {:.0}m ago", -d / 60.0)
    } else if d < 600.0 {
        format!("{:.0}m", d / 60.0)
    } else if d < 86_400.0 {
        format!("{:.1}h", d / 3_600.0)
    } else {
        format!("{:.1}d", d / 86_400.0)
    }
}

fn note(name: &str) -> &'static str {
    IMPORTANT_COOKIES
        .iter()
        .find(|(n, _)| *n == name)
        .map_or("", |(_, note)| note)
}

fn is_important(name: &str) -> bool {
    IMPORTANT_COOKIES.iter().any(|(n, _)| *n == name)
}

/// Left-aligned columns separated by two spaces; the last column is not
/// padded. Widths count characters.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| -> String {
        let last = cells.len().saturating_sub(1);
        let mut s = String::new();
        for (i, cell) in cells.into_iter().enumerate() {
            if i == last {
                s.push_str(cell);
            } else {
                let pad = widths[i] - cell.chars().count();
                s.push_str(cell);
                s.push_str(&" ".repeat(pad + 2));
            }
        }
        s.trim_end().to_owned()
    };
    let mut out = vec![line(headers.to_vec())];
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    out.push(line(rule.iter().map(String::as_str).collect()));
    out.extend(
        rows.iter()
            .map(|r| line(r.iter().map(String::as_str).collect())),
    );
    out.join("\n")
}

/// "Profiles considered" table. `selected` is the index of the profile in
/// use, marked with `*`.
pub fn profiles_table(
    candidates: &[ProfileCandidate],
    selected: Option<usize>,
    now: f64,
) -> String {
    let rows: Vec<Vec<String>> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let sk = match c.session_expiry {
                Some(e) => format!("{} ({})", session_relative(Some(e), now), absolute(e)),
                None => "—".to_owned(),
            };
            vec![
                if Some(i) == selected { "*" } else { "" }.to_owned(),
                c.browser.to_owned(),
                c.path.display().to_string(),
                score_label(c),
                sk,
            ]
        })
        .collect();
    table(
        &["", "browser", "path", "score", "sessionKey expiry"],
        &rows,
    )
}

/// Index of the profile `load_best` picked: the first with a score of at
/// least 1 and no load error (candidates are already sorted).
pub fn selected_index(candidates: &[ProfileCandidate]) -> Option<usize> {
    candidates
        .iter()
        .position(|c| c.score >= 1 && c.error.is_none())
}

/// The selected profile's cookies: important ones first, then by name.
/// Values are masked.
pub fn cookie_inventory(jar: &CookieJar, now: f64) -> String {
    let mut cookies: Vec<&Cookie> = jar.cookies.iter().collect();
    cookies.sort_by(|a, b| {
        (!is_important(&a.name), &a.name, &a.host).cmp(&(!is_important(&b.name), &b.name, &b.host))
    });
    let rows: Vec<Vec<String>> = cookies
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                c.host.clone(),
                mask(Some(&c.value)),
                cookie_expiry(c.expiry, now),
                note(&c.name).to_owned(),
            ]
        })
        .collect();
    format!(
        "Selected profile has {} claude.ai cookies\n{}",
        cookies.len(),
        table(
            &["name", "host", "value (masked)", "expires in", "note"],
            &rows
        )
    )
}

/// What the response body looks like.
#[derive(Debug, Clone, PartialEq)]
pub enum Sniff {
    /// A 200 with usage JSON: window labels and percentages.
    Usage(Vec<(String, Option<f64>)>),
    /// A JSON API error such as `permission_error`.
    ApiError {
        kind: String,
        message: Option<String>,
    },
    /// An HTML page: most likely a Cloudflare challenge.
    Html,
    Empty,
    /// Anything else: a short, redacted prefix of the body.
    Other(String),
}

/// Classify a response body.
pub fn sniff(status: u16, content_type: Option<&str>, body: &str) -> Sniff {
    let json = serde_json::from_str::<serde_json::Value>(body).ok();
    if let Some(v) = &json {
        if let Some(kind) = v.pointer("/error/type").and_then(|t| t.as_str()) {
            let message = v
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(|m| short(m, SNIFF_CHARS));
            return Sniff::ApiError {
                kind: kind.to_owned(),
                message,
            };
        }
        if status == 200 && v.is_object() {
            let snap = model::parse_usage(v, Utc::now());
            return Sniff::Usage(
                snap.windows
                    .into_iter()
                    .map(|w| (w.label, w.utilization))
                    .collect(),
            );
        }
    }
    let trimmed = body.trim_start();
    if content_type.is_some_and(|c| c.contains("text/html")) || trimmed.starts_with('<') {
        return Sniff::Html;
    }
    if trimmed.is_empty() {
        return Sniff::Empty;
    }
    Sniff::Other(redact_api_keys(&short(body, SNIFF_CHARS)))
}

/// One-line description of a [`Sniff`].
pub fn sniff_text(s: &Sniff) -> String {
    match s {
        Sniff::Usage(ws) if ws.is_empty() => "usage JSON with no windows".to_owned(),
        Sniff::Usage(ws) => ws
            .iter()
            .map(|(label, pct)| match pct {
                Some(p) => format!("{label} {p:.1}%"),
                None => format!("{label} inactive"),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Sniff::ApiError { kind, message } => match message {
            Some(m) => format!("API error {kind}: {m}"),
            None => format!("API error {kind}"),
        },
        Sniff::Html => "HTML page — Cloudflare challenge likely".to_owned(),
        Sniff::Empty => "empty body".to_owned(),
        Sniff::Other(s) => s.clone(),
    }
}

/// First `n` characters on one line.
fn short(s: &str, n: usize) -> String {
    s.chars()
        .take(n)
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// `sk-ant-[\w-]{6,}` → `sk-ant-***`. The Python debug.py used `\w{6,}`, which misses
/// real keys such as `sk-ant-api03-…` (the hyphen ends the match early).
fn redact_api_keys(s: &str) -> String {
    const PREFIX: &str = "sk-ant-";
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(PREFIX) {
        out.push_str(&rest[..i]);
        let after = &rest[i + PREFIX.len()..];
        let word = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        if after[..word].chars().count() >= 6 {
            out.push_str("sk-ant-***");
        } else {
            out.push_str(&rest[i..i + PREFIX.len() + word]);
        }
        rest = &after[word..];
    }
    out.push_str(rest);
    out
}

/// What the one request produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// No response (DNS, TLS, timeout, ...).
    Transport(String),
    Response {
        status: u16,
        sniff: Sniff,
    },
}

impl Probe {
    pub fn succeeded(&self) -> bool {
        matches!(
            self,
            Probe::Response {
                status: 200,
                sniff: Sniff::Usage(_)
            }
        )
    }
}

fn latest<'a>(jar: &'a CookieJar, name: &str) -> Option<&'a Cookie> {
    jar.cookies
        .iter()
        .filter(|c| c.name == name)
        .max_by(|a, b| {
            a.expiry
                .unwrap_or(f64::INFINITY)
                .total_cmp(&b.expiry.unwrap_or(f64::INFINITY))
        })
}

fn expired(c: &Cookie, now: f64) -> bool {
    c.expiry.is_some_and(|e| e < now)
}

/// The recommendation (the Python debug.py `_print_recommendation`, adapted: no TLS
/// impersonation advice; a `permission_error` points at the client SHA).
/// The first line is the headline.
pub fn recommendation(jar: &CookieJar, probe: &Probe, now: f64) -> Vec<String> {
    if probe.succeeded() {
        return vec![
            "Success: the request returned usage data.".to_owned(),
            "`claude-usage-cli once` and the GUI will work with these cookies and this client SHA."
                .to_owned(),
        ];
    }
    let mut lines = vec!["The request failed.".to_owned()];
    let sess = latest(jar, "sessionKey");
    let (status, sniff) = match probe {
        Probe::Transport(msg) => {
            lines.push(format!(
                "• The request never got a response ({msg}) — check your network or proxy, then re-run."
            ));
            (None, None)
        }
        Probe::Response { status, sniff } => (Some(*status), Some(sniff)),
    };
    match sess {
        None => lines.push("• No sessionKey cookie. Sign in to claude.ai.".to_owned()),
        Some(s) if expired(s, now) => {
            let days = (now - s.expiry.unwrap_or(now)) / 86_400.0;
            lines.push(format!(
                "• sessionKey is EXPIRED ({days:.0}d ago) — this is the cause of 'Invalid authorization'.\n  \
                 Sign out of claude.ai, sign back in, visit claude.ai/settings/usage, then re-run."
            ));
        }
        Some(_) if status == Some(401) => lines.push(
            "• 401 — claude.ai rejected the session. Sign out of claude.ai, sign back in, then re-run."
                .to_owned(),
        ),
        Some(_) => {}
    }
    if latest(jar, "__cf_bm").is_none_or(|c| expired(c, now)) {
        lines.push(
            "• __cf_bm missing or expired — visit claude.ai/settings/usage to refresh.".to_owned(),
        );
    }
    if latest(jar, "cf_clearance").is_none() {
        lines.push("• cf_clearance missing — visit claude.ai to clear Cloudflare.".to_owned());
    }
    let permission_error =
        matches!(sniff, Some(Sniff::ApiError { kind, .. }) if kind == "permission_error");
    if permission_error {
        lines.push(
            "• permission_error — the anthropic-client-sha is probably stale. Refresh \
             client_sha.txt (or set CLAUDE_USAGE_TUI_CLIENT_SHA); if that doesn't help, sign out \
             of claude.ai and back in."
                .to_owned(),
        );
    }
    if matches!(sniff, Some(Sniff::Html)) {
        lines.push(
            "• Got an HTML page instead of JSON — likely a Cloudflare challenge. Open \
             claude.ai/settings/usage in your browser (complete any check), then re-run."
                .to_owned(),
        );
    }
    if !permission_error {
        lines.push(
            "\nIf a later run says 'permission_error' while the cookies look fresh, the \
             anthropic-client-sha may be stale — refresh client_sha.txt."
                .to_owned(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use std::path::PathBuf;

    const NOW: f64 = 1_800_000_000.0;

    fn cookie(name: &str, value: &str, expiry: Option<f64>) -> Cookie {
        Cookie {
            host: ".claude.ai".into(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            expiry,
        }
    }

    fn healthy_jar() -> CookieJar {
        CookieJar {
            cookies: vec![
                cookie(
                    "sessionKey",
                    "sk-ant-sid01-abcdefghijklmnop",
                    Some(NOW + 86_400.0),
                ),
                cookie("__cf_bm", "cfbmvalue", Some(NOW + 600.0)),
                cookie("cf_clearance", "clear", Some(NOW + 86_400.0)),
            ],
        }
    }

    fn candidate(score: i32, error: Option<&str>) -> ProfileCandidate {
        ProfileCandidate {
            browser: "Firefox",
            path: PathBuf::from("/p/cookies.sqlite"),
            score,
            session_expiry: None,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn mask_matches_debug_py() {
        assert_eq!(mask(None), "<none>");
        assert_eq!(mask(Some("")), "");
        assert_eq!(mask(Some("abcdefghijklmno")), "***************");
        assert_eq!(mask(Some("abcdefghijklmnop")), "abcdef…klmnop (16 chars)");
        // Counts characters, not bytes, and never splits one.
        assert_eq!(mask(Some(&"é".repeat(20))), "éééééé…éééééé (20 chars)");
    }

    #[test]
    fn mask_never_reveals_the_middle() {
        let secret = "sk-ant-sid01-THEMIDDLEISSECRET-xyz123";
        let m = mask(Some(secret));
        assert!(!m.contains("THEMIDDLEISSECRET"), "{m}");
    }

    #[test]
    fn score_labels() {
        assert_eq!(score_label(&candidate(3, None)), "live");
        assert_eq!(score_label(&candidate(2, None)), "expired");
        assert_eq!(score_label(&candidate(1, None)), "no sessionKey");
        assert_eq!(score_label(&candidate(0, None)), "no claude.ai cookies");
        assert_eq!(
            score_label(&candidate(-1, Some("database is locked"))),
            "load error: database is locked"
        );
    }

    #[test]
    fn selected_is_first_usable() {
        let cs = [
            candidate(-1, Some("x")),
            candidate(0, None),
            candidate(2, None),
        ];
        assert_eq!(selected_index(&cs), Some(2));
        assert_eq!(selected_index(&cs[..2]), None);
    }

    #[test]
    fn relative_and_absolute_times() {
        assert_eq!(session_relative(None, NOW), "—");
        assert_eq!(
            session_relative(Some(NOW + 5.0 * 3600.0), NOW),
            "valid 5.0h"
        );
        assert_eq!(
            session_relative(Some(NOW - 2.5 * 86_400.0), NOW),
            "expired 2.5d ago"
        );
        let utc = FixedOffset::east_opt(0).unwrap();
        assert_eq!(absolute_in(NOW, &utc), "2027-01-15 08:00");
    }

    #[test]
    fn cookie_expiry_buckets() {
        assert_eq!(cookie_expiry(None, NOW), "session");
        assert_eq!(cookie_expiry(Some(NOW - 300.0), NOW), "EXPIRED 5m ago");
        assert_eq!(cookie_expiry(Some(NOW + 300.0), NOW), "5m");
        assert_eq!(cookie_expiry(Some(NOW + 7200.0), NOW), "2.0h");
        assert_eq!(cookie_expiry(Some(NOW + 3.0 * 86_400.0), NOW), "3.0d");
    }

    #[test]
    fn table_aligns_columns() {
        let t = table(
            &["a", "bb", "c"],
            &[
                vec!["xxx".into(), "y".into(), "z".into()],
                vec!["".into(), "é".into(), "".into()],
            ],
        );
        assert_eq!(t, "a    bb  c\n---  --  -\nxxx  y   z\n     é");
    }

    #[test]
    fn inventory_masks_and_orders() {
        let mut jar = healthy_jar();
        jar.cookies
            .insert(0, cookie("aaa_other", "plainvalue12345678", None));
        let inv = cookie_inventory(&jar, NOW);
        assert!(inv.starts_with("Selected profile has 4 claude.ai cookies\n"));
        assert!(!inv.contains("abcdefghijklmnop"), "{inv}");
        assert!(!inv.contains("plainvalue12345678"), "{inv}");
        assert!(inv.contains("sk-ant…klmnop (29 chars)"), "{inv}");
        let names: Vec<&str> = inv
            .lines()
            .skip(3)
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(
            names,
            ["__cf_bm", "cf_clearance", "sessionKey", "aaa_other"]
        );
    }

    #[test]
    fn sniff_classifies_bodies() {
        let body = r#"{"type":"error","error":{"type":"permission_error","message":"Invalid authorization"}}"#;
        let s = sniff(403, Some("application/json"), body);
        assert_eq!(
            s,
            Sniff::ApiError {
                kind: "permission_error".into(),
                message: Some("Invalid authorization".into())
            }
        );
        assert_eq!(
            sniff_text(&s),
            "API error permission_error: Invalid authorization"
        );

        let s = sniff(
            403,
            Some("text/html; charset=UTF-8"),
            "<!DOCTYPE html><title>Just a moment...</title>",
        );
        assert_eq!(s, Sniff::Html);
        assert_eq!(sniff(503, None, "  <html>"), Sniff::Html);
        assert_eq!(sniff(500, None, ""), Sniff::Empty);

        let s = sniff(
            200,
            Some("application/json"),
            r#"{"five_hour":{"utilization":46.0,"resets_at":null},"seven_day":{"utilization":null,"resets_at":null}}"#,
        );
        let Sniff::Usage(ws) = &s else {
            panic!("{s:?}")
        };
        assert!(ws.contains(&("5-hour".to_owned(), Some(46.0))));
        assert!(sniff_text(&s).contains("5-hour 46.0%"));
    }

    #[test]
    fn sniff_other_is_short_and_redacted() {
        let body = format!(
            "oops sk-ant-api03-SECRETSECRET and sk-ant-ab\n{}",
            "x".repeat(300)
        );
        let Sniff::Other(s) = sniff(500, Some("text/plain"), &body) else {
            panic!()
        };
        assert!(s.starts_with("oops sk-ant-*** and sk-ant-ab x"), "{s}");
        assert!(!s.contains("SECRET"));
        assert!(s.chars().count() <= SNIFF_CHARS);
    }

    fn rec(jar: &CookieJar, probe: Probe) -> String {
        recommendation(jar, &probe, NOW).join("\n")
    }

    #[test]
    fn recommendation_success() {
        let r = rec(
            &healthy_jar(),
            Probe::Response {
                status: 200,
                sniff: Sniff::Usage(vec![]),
            },
        );
        assert!(r.starts_with("Success"), "{r}");
    }

    #[test]
    fn recommendation_permission_error_points_at_client_sha() {
        let r = rec(
            &healthy_jar(),
            Probe::Response {
                status: 403,
                sniff: Sniff::ApiError {
                    kind: "permission_error".into(),
                    message: None,
                },
            },
        );
        assert!(r.starts_with("The request failed."));
        assert!(r.contains("client_sha.txt"), "{r}");
        assert!(r.contains("sign out"), "{r}");
        assert!(!r.contains("__cf_bm"), "{r}");
        assert!(!r.contains("impersonat"), "{r}");
    }

    #[test]
    fn recommendation_cookie_problems() {
        let jar = CookieJar {
            cookies: vec![
                cookie("sessionKey", "old", Some(NOW - 3.0 * 86_400.0)),
                cookie("__cf_bm", "x", Some(NOW - 10.0)),
            ],
        };
        let r = rec(
            &jar,
            Probe::Response {
                status: 401,
                sniff: Sniff::Empty,
            },
        );
        assert!(r.contains("sessionKey is EXPIRED (3d ago)"), "{r}");
        assert!(r.contains("__cf_bm missing or expired"), "{r}");
        assert!(r.contains("cf_clearance missing"), "{r}");
        assert!(r.contains("permission_error"), "trailer: {r}");

        let r = rec(&CookieJar::default(), Probe::Transport("timed out".into()));
        assert!(r.contains("never got a response (timed out)"), "{r}");
        assert!(r.contains("No sessionKey cookie"), "{r}");
    }

    #[test]
    fn recommendation_401_with_live_session_and_html() {
        let r = rec(
            &healthy_jar(),
            Probe::Response {
                status: 401,
                sniff: Sniff::Empty,
            },
        );
        assert!(r.contains("401 — claude.ai rejected the session"), "{r}");

        let r = rec(
            &healthy_jar(),
            Probe::Response {
                status: 403,
                sniff: Sniff::Html,
            },
        );
        assert!(r.contains("Cloudflare challenge"), "{r}");
    }
}
