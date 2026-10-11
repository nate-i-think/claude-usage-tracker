//! HTTP fetch of the claude.ai `/usage` endpoint.
//!
//! Ported from the Python api.py (removed; see git history):
//! `STATIC_HEADERS`, `build_headers`, and `fetch_usage`. One
//! reusable blocking client (rustls over the `ring` provider, which
//! [`crate::install_crypto_provider`] must have installed first).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, COOKIE};
use thiserror::Error;

use crate::config::{self, Settings};
use crate::cookies::{self, CookieJar};
use crate::model::{self, Snapshot};
use crate::paths::Paths;

pub const USAGE_HOST: &str = "https://claude.ai";

/// A current Firefox release on Windows; Cloudflare accepts this shape.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:147.0) Gecko/20100101 Firefox/147.0";

const TIMEOUT: Duration = Duration::from_secs(15);

/// Characters of a 403 body kept in the error message (the Python api.py's `text[:200]`).
const FORBIDDEN_BODY_CHARS: usize = 200;

/// Characters of other non-2xx bodies kept in the error message.
const STATUS_BODY_CHARS: usize = 200;

/// Why a fetch failed. `Display` is the message shown to the user.
#[derive(Debug, Error)]
pub enum FetchError {
    /// Cookie loading, org id, or client SHA resolution failed.
    #[error("{0}")]
    Setup(String),
    /// The request never produced a response (DNS, TLS, timeout, ...).
    #[error("request failed: {0}")]
    Transport(String),
    #[error("401 — session expired. Sign in to claude.ai.")]
    Unauthorized,
    /// `body` is the first 200 characters of the response body.
    #[error("403 — auth or Cloudflare: {body}")]
    Forbidden { body: String },
    /// Any other non-2xx status; `body` is a short prefix of the body.
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("invalid JSON from usage endpoint: {0}")]
    Json(String),
}

impl FetchError {
    /// True when the failure looks like a stale `anthropic-client-sha`: a 403
    /// whose body carries an API `permission_error`.
    pub fn stale_sha_hint(&self) -> bool {
        matches!(self, Self::Forbidden { body } if body.contains("permission_error"))
    }
}

/// Map a response status and body to an error; `Ok` for 2xx.
pub fn check_status(status: u16, body: &str) -> Result<(), FetchError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(FetchError::Unauthorized),
        403 => Err(FetchError::Forbidden {
            body: prefix_chars(body, FORBIDDEN_BODY_CHARS),
        }),
        _ => Err(FetchError::Status {
            status,
            body: prefix_chars(body.trim(), STATUS_BODY_CHARS),
        }),
    }
}

/// An error and its `source()` chain joined with ": " (like anyhow's
/// `{:#}`), so timeouts, DNS, TLS, and refused connections are
/// distinguishable. Consecutive repeated messages are dropped.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts: Vec<String> = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(inner) = cur {
        let msg = inner.to_string();
        if parts.last() != Some(&msg) {
            parts.push(msg);
        }
        cur = inner.source();
    }
    parts.join(": ")
}

fn prefix_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `https://claude.ai/api/organizations/{org}/usage`.
pub fn usage_url(org_id: &str) -> String {
    format!("{USAGE_HOST}/api/organizations/{org_id}/usage")
}

/// Per-request headers, in order: the Python api.py's `STATIC_HEADERS` then the
/// `build_headers` additions. A fresh `x-activity-session-id` (uuid4) is
/// generated on every call. The Cookie header is not included.
pub fn build_headers(
    jar: &CookieJar,
    client_sha: &str,
    anonymous_id: &str,
) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("Referer", format!("{USAGE_HOST}/settings/usage")),
        ("anthropic-client-platform", "web_claude_ai".to_owned()),
        ("anthropic-client-version", "1.0.0".to_owned()),
        ("anthropic-client-sha", client_sha.to_owned()),
        ("content-type", "application/json".to_owned()),
        ("anthropic-anonymous-id", anonymous_id.to_owned()),
    ];
    if let Some(device) = jar.device_id() {
        h.push(("anthropic-device-id", device.to_owned()));
    }
    h.push(("x-activity-session-id", uuid::Uuid::new_v4().to_string()));
    h
}

/// The raw outcome of one `/usage` request.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
}

/// A reusable HTTP client for the usage endpoint.
pub struct Client {
    http: reqwest::blocking::Client,
}

impl Client {
    /// Build the client. Call [`crate::install_crypto_provider`] first.
    pub fn new() -> Result<Self, FetchError> {
        let mut defaults = HeaderMap::new();
        defaults.insert(ACCEPT, HeaderValue::from_static("*/*"));
        defaults.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.5"));
        // The spike's successful request shape also sent these.
        for (name, value) in [
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-origin"),
        ] {
            defaults.insert(name, HeaderValue::from_static(value));
        }
        let http = reqwest::blocking::Client::builder()
            .tls_backend_rustls()
            .user_agent(USER_AGENT)
            .default_headers(defaults)
            .gzip(true)
            .brotli(true)
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| {
                FetchError::Transport(format!("building HTTP client: {}", error_chain(&e)))
            })?;
        Ok(Self { http })
    }

    /// GET the usage endpoint for `org_id` with exactly these headers plus
    /// `Cookie: cookie_header`. Any status is returned as-is.
    pub fn request_usage(
        &self,
        org_id: &str,
        headers: &[(&'static str, String)],
        cookie_header: &str,
    ) -> Result<RawResponse, FetchError> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let value = HeaderValue::from_str(value).map_err(|_| {
                FetchError::Setup(format!("header {name} has a value that is not valid HTTP"))
            })?;
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| FetchError::Setup(format!("invalid header name {name}")))?;
            map.insert(name, value);
        }
        let mut cookie = HeaderValue::from_str(cookie_header).map_err(|_| {
            FetchError::Setup("cookie values contain characters not valid in HTTP".to_owned())
        })?;
        cookie.set_sensitive(true);
        map.insert(COOKIE, cookie);

        let resp = self
            .http
            .get(usage_url(org_id))
            .headers(map)
            .send()
            .map_err(|e| FetchError::Transport(error_chain(&e.without_url())))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp
            .text()
            .map_err(|e| FetchError::Transport(error_chain(&e.without_url())))?;
        Ok(RawResponse {
            status,
            content_type,
            body,
        })
    }

    /// Fetch one usage snapshot: load cookies, resolve org id and client SHA,
    /// send the request, map errors, parse.
    pub fn fetch_usage(&self, paths: &Paths, settings: &Settings) -> Result<Snapshot, FetchError> {
        let now = unix_now();
        let (jar, _candidates) = cookies::load_best(settings.cookie_file.as_deref(), now)
            .map_err(|e| FetchError::Setup(format!("{e:#}")))?;
        let org_id = jar
            .org_id()
            .map_err(|e| FetchError::Setup(format!("{e:#}")))?;
        let sha = config::resolve_client_sha(settings.client_sha.as_deref())
            .map_err(|e| FetchError::Setup(format!("{e:#}")))?;
        let headers = build_headers(&jar, &sha, &paths.get_or_create_anonymous_id());
        let resp = self.request_usage(&org_id, &headers, &jar.cookie_header(now))?;
        parse_response(resp.status, &resp.body)
    }
}

/// Status check then JSON parse into a [`Snapshot`] stamped now.
pub fn parse_response(status: u16, body: &str) -> Result<Snapshot, FetchError> {
    check_status(status, body)?;
    let data: serde_json::Value =
        serde_json::from_str(body).map_err(|e| FetchError::Json(e.to_string()))?;
    Ok(model::parse_usage(&data, Utc::now()))
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::Cookie;

    fn cookie(host: &str, name: &str, value: &str) -> Cookie {
        Cookie {
            host: host.into(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            expiry: None,
        }
    }

    #[test]
    fn headers_match_api_py() {
        let jar = CookieJar {
            cookies: vec![
                cookie(".claude.ai", "anthropic-device-id", "dev-123"),
                cookie(".claude.ai", "sessionKey", "secret"),
            ],
        };
        let h = build_headers(&jar, "sha-abc12345", "claudeai.v1.anon");
        let names: Vec<&str> = h.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "Referer",
                "anthropic-client-platform",
                "anthropic-client-version",
                "anthropic-client-sha",
                "content-type",
                "anthropic-anonymous-id",
                "anthropic-device-id",
                "x-activity-session-id",
            ]
        );
        let get = |n: &str| h.iter().find(|(k, _)| *k == n).unwrap().1.as_str();
        assert_eq!(get("Referer"), "https://claude.ai/settings/usage");
        assert_eq!(get("anthropic-client-platform"), "web_claude_ai");
        assert_eq!(get("anthropic-client-version"), "1.0.0");
        assert_eq!(get("anthropic-client-sha"), "sha-abc12345");
        assert_eq!(get("content-type"), "application/json");
        assert_eq!(get("anthropic-anonymous-id"), "claudeai.v1.anon");
        assert_eq!(get("anthropic-device-id"), "dev-123");
        let sid = uuid::Uuid::parse_str(get("x-activity-session-id")).unwrap();
        assert_eq!(sid.get_version_num(), 4);
        // Cookies never ride in these headers.
        assert!(h.iter().all(|(_, v)| !v.contains("secret")));
    }

    #[test]
    fn headers_without_device_id_and_fresh_session_id() {
        let jar = CookieJar {
            cookies: vec![cookie("evilclaude.ai", "anthropic-device-id", "evil")],
        };
        let a = build_headers(&jar, "s", "a");
        let b = build_headers(&jar, "s", "a");
        assert!(a.iter().all(|(n, _)| *n != "anthropic-device-id"));
        assert_eq!(a.len(), 7);
        assert_ne!(a.last().unwrap().1, b.last().unwrap().1);
    }

    #[test]
    fn every_header_name_is_valid_for_http() {
        let jar = CookieJar {
            cookies: vec![cookie("claude.ai", "anthropic-device-id", "d")],
        };
        for (name, _) in build_headers(&jar, "s", "a") {
            assert!(HeaderName::from_bytes(name.as_bytes()).is_ok(), "{name}");
        }
    }

    #[test]
    fn usage_url_format() {
        assert_eq!(
            usage_url("org-1"),
            "https://claude.ai/api/organizations/org-1/usage"
        );
    }

    #[test]
    fn status_mapping() {
        assert!(check_status(200, "{}").is_ok());
        assert!(check_status(204, "").is_ok());

        let e = check_status(401, "whatever").unwrap_err();
        assert_eq!(
            e.to_string(),
            "401 — session expired. Sign in to claude.ai."
        );
        assert!(!e.stale_sha_hint());

        let long = "x".repeat(500);
        let e = check_status(403, &long).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("403 — auth or Cloudflare: {}", "x".repeat(200))
        );
        assert!(!e.stale_sha_hint());

        let body = r#"{"type":"error","error":{"type":"permission_error","message":"nope"}}"#;
        let e = check_status(403, body).unwrap_err();
        assert_eq!(e.to_string(), format!("403 — auth or Cloudflare: {body}"));
        assert!(e.stale_sha_hint());

        let e = check_status(500, "  Internal Server Error\n").unwrap_err();
        assert_eq!(e.to_string(), "HTTP 500: Internal Server Error");
        assert!(!e.stale_sha_hint());
    }

    #[test]
    fn forbidden_body_truncates_on_char_boundary() {
        let body = "é".repeat(300);
        let FetchError::Forbidden { body: kept } = check_status(403, &body).unwrap_err() else {
            panic!("expected Forbidden");
        };
        assert_eq!(kept.chars().count(), 200);
    }

    #[test]
    fn parse_response_ok_and_bad_json() {
        let snap = parse_response(
            200,
            r#"{"five_hour":{"utilization":12.5,"resets_at":null},"seven_day":null}"#,
        )
        .unwrap();
        assert_eq!(snap.five_hour_pct(), Some(12.5));

        let e = parse_response(200, "<html>").unwrap_err();
        assert!(matches!(e, FetchError::Json(_)));
        assert!(e
            .to_string()
            .starts_with("invalid JSON from usage endpoint: "));

        assert!(matches!(
            parse_response(401, "{}").unwrap_err(),
            FetchError::Unauthorized
        ));
    }

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|l| l as _)
        }
    }

    #[test]
    fn error_chain_joins_sources() {
        let e = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "client error (Connect)",
                Some(Box::new(Layer(
                    "client error (Connect)",
                    Some(Box::new(Layer("operation timed out", None))),
                ))),
            ))),
        );
        assert_eq!(
            error_chain(&e),
            "error sending request: client error (Connect): operation timed out"
        );
        assert_eq!(error_chain(&Layer("alone", None)), "alone");
    }

    #[test]
    fn client_builds() {
        crate::install_crypto_provider();
        assert!(Client::new().is_ok());
    }

    /// Live check against the real endpoint with the user's browser session.
    /// Prints only window labels and percentages. Run manually:
    /// `cargo test --lib live_fetch_usage -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the real claude.ai endpoint"]
    fn live_fetch_usage() {
        crate::install_crypto_provider();
        let client = Client::new().unwrap();
        match client.fetch_usage(&Paths::platform_default(), &Settings::default()) {
            Ok(snap) => {
                for w in &snap.windows {
                    let pct = w.utilization.map_or("-".to_owned(), |p| format!("{p:.1}%"));
                    println!("{:<16} {pct}", w.label);
                }
            }
            Err(e) => println!("error: {e}"),
        }
    }
}
