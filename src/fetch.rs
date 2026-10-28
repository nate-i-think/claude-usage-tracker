//! HTTP fetch of the Claude Code OAuth usage endpoint
//! (`https://api.anthropic.com/api/oauth/usage`).
//!
//! One reusable blocking client (rustls over the `ring` provider, which
//! [`crate::install_crypto_provider`] must have installed first). The bearer
//! token rides only in a header marked sensitive; no error message or
//! `Display` ever includes it.

use std::time::Duration;

use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use thiserror::Error;

use crate::debug_report::redact_api_keys;
use crate::model::{self, Snapshot};

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The `anthropic-beta` value the OAuth endpoints require.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// `claude-usage-monitor/<version>`.
pub const USER_AGENT_VALUE: &str = concat!("claude-usage-monitor/", env!("CARGO_PKG_VERSION"));

const TIMEOUT: Duration = Duration::from_secs(15);

/// Characters of a non-2xx body kept in the error message.
const STATUS_BODY_CHARS: usize = 200;

/// A [`FetchError`]'s category, for display decisions that shouldn't parse
/// messages. `Internal` is a poll that panicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NoCredentials,
    /// The credential store couldn't be read (a locked or denied Keychain).
    StoreUnavailable,
    TokenExpired,
    Unauthorized,
    Forbidden,
    RateLimited,
    Status,
    Transport,
    Json,
    Internal,
}

/// Why a fetch failed. `Display` never includes the bearer token.
#[derive(Debug, Error)]
pub enum FetchError {
    /// No usable credential; the text says why (empty when simply absent).
    #[error("no Claude Code login found{}", detail(.0))]
    NoCredentials(String),
    /// The credential store couldn't be read: a locked Keychain, access
    /// denied to `/usr/bin/security`, an unreadable credentials file. The
    /// text says why. Not a missing login: signing in again won't help.
    #[error("credential store unavailable{}", detail(.0))]
    StoreUnavailable(String),
    /// The credential's `expiresAt` has passed; no request was sent.
    #[error("token expired")]
    TokenExpired,
    #[error("401 — token rejected")]
    Unauthorized,
    /// `body` is a short prefix of the response body.
    #[error("403 — forbidden: {body}")]
    Forbidden { body: String },
    #[error("429 — rate limited{}", retry_hint(*.retry_after))]
    RateLimited { retry_after: Option<Duration> },
    /// Any other non-2xx status; `body` is a short prefix of the body.
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    /// The request never produced a response (DNS, TLS, timeout, ...).
    #[error("request failed: {0}")]
    Transport(String),
    #[error("invalid JSON from usage endpoint: {0}")]
    Json(String),
}

impl FetchError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::NoCredentials(_) => ErrorKind::NoCredentials,
            Self::StoreUnavailable(_) => ErrorKind::StoreUnavailable,
            Self::TokenExpired => ErrorKind::TokenExpired,
            Self::Unauthorized => ErrorKind::Unauthorized,
            Self::Forbidden { .. } => ErrorKind::Forbidden,
            Self::RateLimited { .. } => ErrorKind::RateLimited,
            Self::Status { .. } => ErrorKind::Status,
            Self::Transport(_) => ErrorKind::Transport,
            Self::Json(_) => ErrorKind::Json,
        }
    }
}

fn detail(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        format!(" ({s})")
    }
}

fn retry_hint(retry_after: Option<Duration>) -> String {
    retry_after.map_or(String::new(), |d| format!("; retry after {}s", d.as_secs()))
}

/// Map a response status, body and `Retry-After` header to an error; `Ok`
/// for 2xx. `Retry-After` is honored in its delay-seconds form only.
pub fn check_status(status: u16, body: &str, retry_after: Option<&str>) -> Result<(), FetchError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(FetchError::Unauthorized),
        403 => Err(FetchError::Forbidden {
            body: prefix_chars(body.trim(), STATUS_BODY_CHARS),
        }),
        429 => Err(FetchError::RateLimited {
            retry_after: retry_after
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
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

/// The first `n` chars of `s` with any `sk-ant-` key redacted first (an
/// error body must never carry a token into a message).
fn prefix_chars(s: &str, n: usize) -> String {
    redact_api_keys(s).chars().take(n).collect()
}

/// The request headers: `Authorization: Bearer` (marked sensitive),
/// `anthropic-beta`, `Accept` and `User-Agent`.
pub fn build_headers(bearer: &str) -> Result<HeaderMap, FetchError> {
    let mut auth = HeaderValue::from_str(&format!("Bearer {bearer}")).map_err(|_| {
        FetchError::NoCredentials("access token is not a valid HTTP header value".to_owned())
    })?;
    auth.set_sensitive(true);
    let mut h = HeaderMap::new();
    h.insert(AUTHORIZATION, auth);
    h.insert("anthropic-beta", HeaderValue::from_static(OAUTH_BETA));
    h.insert(ACCEPT, HeaderValue::from_static("application/json"));
    h.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    Ok(h)
}

/// The raw outcome of one usage request.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    /// The `Retry-After` header, if any.
    pub retry_after: Option<String>,
    pub body: String,
}

/// A reusable HTTP client for the usage endpoint. Cloning shares the
/// connection pool.
#[derive(Clone)]
pub struct Client {
    http: reqwest::blocking::Client,
}

impl Client {
    /// Build the client. Call [`crate::install_crypto_provider`] first.
    pub fn new() -> Result<Self, FetchError> {
        let http = reqwest::blocking::Client::builder()
            .tls_backend_rustls()
            .gzip(true)
            .brotli(true)
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| {
                FetchError::Transport(format!("building HTTP client: {}", error_chain(&e)))
            })?;
        Ok(Self { http })
    }

    /// GET the usage endpoint with `bearer`. Any status is returned as-is.
    pub fn request_usage(&self, bearer: &str) -> Result<RawResponse, FetchError> {
        let resp = self
            .http
            .get(USAGE_URL)
            .headers(build_headers(bearer)?)
            .send()
            .map_err(|e| FetchError::Transport(error_chain(&e.without_url())))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp
            .text()
            .map_err(|e| FetchError::Transport(error_chain(&e.without_url())))?;
        Ok(RawResponse {
            status,
            retry_after,
            body,
        })
    }

    /// Fetch one usage snapshot: send the request, map errors, parse.
    pub fn fetch_usage(&self, bearer: &str) -> Result<Snapshot, FetchError> {
        let resp = self.request_usage(bearer)?;
        check_status(resp.status, &resp.body, resp.retry_after.as_deref())?;
        parse_response(resp.status, &resp.body)
    }
}

/// Status check then JSON parse into a [`Snapshot`] stamped now.
pub fn parse_response(status: u16, body: &str) -> Result<Snapshot, FetchError> {
    check_status(status, body, None)?;
    let data: serde_json::Value =
        serde_json::from_str(body).map_err(|e| FetchError::Json(e.to_string()))?;
    Ok(model::parse_usage(&data, Utc::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "sk-ant-oat01-FIXTURE-SECRET";

    #[test]
    fn headers_carry_bearer_beta_and_user_agent_only() {
        let h = build_headers(TOKEN).unwrap();
        let get = |n: &str| h.get(n).unwrap().to_str().unwrap();
        assert_eq!(get("authorization"), format!("Bearer {TOKEN}"));
        assert!(h.get(AUTHORIZATION).unwrap().is_sensitive());
        assert_eq!(get("anthropic-beta"), "oauth-2025-04-20");
        assert_eq!(get("accept"), "application/json");
        assert_eq!(
            get("user-agent"),
            format!("claude-usage-monitor/{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(h.len(), 4);
        for name in h.keys() {
            let name = name.as_str();
            assert!(
                !name.contains("cookie")
                    && !name.starts_with("anthropic-client")
                    && !name.starts_with("sec-fetch")
                    && name != "referer",
                "browser header {name}"
            );
        }
    }

    #[test]
    fn invalid_bearer_is_an_error_without_the_token() {
        let bad = "sk-ant-FIXTURE\nbad";
        let e = build_headers(bad).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NoCredentials);
        assert!(!e.to_string().contains("FIXTURE"), "{e}");
        assert!(!format!("{e:?}").contains("FIXTURE"), "{e:?}");
    }

    #[test]
    fn status_mapping() {
        assert!(check_status(200, "{}", None).is_ok());
        assert!(check_status(204, "", None).is_ok());

        let e = check_status(401, "whatever", None).unwrap_err();
        assert!(matches!(e, FetchError::Unauthorized));
        assert_eq!(e.kind(), ErrorKind::Unauthorized);
        assert_eq!(e.to_string(), "401 — token rejected");

        let long = "x".repeat(500);
        let e = check_status(403, &long, None).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Forbidden);
        assert_eq!(
            e.to_string(),
            format!("403 — forbidden: {}", "x".repeat(200))
        );

        let e = check_status(500, "  Internal Server Error\n", None).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Status);
        assert_eq!(e.to_string(), "HTTP 500: Internal Server Error");
        let e = check_status(503, "", None).unwrap_err();
        assert!(matches!(e, FetchError::Status { status: 503, .. }));
    }

    #[test]
    fn rate_limited_parses_retry_after_seconds() {
        let e = check_status(429, "", Some("120")).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::RateLimited);
        assert!(matches!(
            e,
            FetchError::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(120)
        ));
        assert_eq!(e.to_string(), "429 — rate limited; retry after 120s");
        assert!(matches!(
            check_status(429, "", Some(" 7 ")).unwrap_err(),
            FetchError::RateLimited { retry_after: Some(d) } if d.as_secs() == 7
        ));
        // HTTP-date form, garbage and absence: no delay.
        for header in [Some("Wed, 21 Oct 2026 07:28:00 GMT"), Some("soon"), None] {
            let e = check_status(429, "", header).unwrap_err();
            assert!(
                matches!(e, FetchError::RateLimited { retry_after: None }),
                "{header:?}"
            );
            assert_eq!(e.to_string(), "429 — rate limited");
        }
    }

    #[test]
    fn forbidden_body_truncates_on_char_boundary() {
        let body = "é".repeat(300);
        let FetchError::Forbidden { body: kept } = check_status(403, &body, None).unwrap_err()
        else {
            panic!("expected Forbidden");
        };
        assert_eq!(kept.chars().count(), 200);
    }

    #[test]
    fn error_bodies_redact_keys() {
        let body = format!("bad token {TOKEN} rejected");
        for e in [
            check_status(403, &body, None).unwrap_err(),
            check_status(500, &body, None).unwrap_err(),
        ] {
            let text = format!("{e} {e:?}");
            assert!(!text.contains("FIXTURE"), "{text}");
            assert!(text.contains("bad token sk-ant-*** rejected"), "{text}");
        }
    }

    #[test]
    fn parse_response_ok_and_bad_json() {
        let snap = parse_response(
            200,
            r#"{"five_hour":{"utilization":12.5,"resets_at":null},"seven_day":null}"#,
        )
        .unwrap();
        assert_eq!(snap.five_hour_pct(), Some(12.5));

        let e = parse_response(200, "not json").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Json);
        assert!(e
            .to_string()
            .starts_with("invalid JSON from usage endpoint: "));
        // No HTML sniffing: a page is just bad JSON.
        assert!(matches!(
            parse_response(200, "<!DOCTYPE html>").unwrap_err(),
            FetchError::Json(_)
        ));

        assert!(matches!(
            parse_response(401, "{}").unwrap_err(),
            FetchError::Unauthorized
        ));
    }

    #[test]
    fn display_never_includes_the_bearer() {
        let errors = [
            FetchError::NoCredentials(String::new()),
            FetchError::NoCredentials("malformed credential: no accessToken".into()),
            FetchError::StoreUnavailable("security exited 51".into()),
            FetchError::TokenExpired,
            FetchError::Unauthorized,
            check_status(403, "denied", None).unwrap_err(),
            check_status(429, "", Some("3")).unwrap_err(),
            check_status(500, "oops", None).unwrap_err(),
            FetchError::Transport("timed out".into()),
            FetchError::Json("eof".into()),
            build_headers("FIXTURE\u{7f}").unwrap_err(),
        ];
        for e in errors {
            for text in [e.to_string(), format!("{e:?}")] {
                assert!(!text.contains(TOKEN) && !text.contains("FIXTURE"), "{text}");
            }
        }
    }

    #[test]
    fn no_credentials_display() {
        assert_eq!(
            FetchError::NoCredentials(String::new()).to_string(),
            "no Claude Code login found"
        );
        assert_eq!(
            FetchError::NoCredentials("malformed credential: x".into()).to_string(),
            "no Claude Code login found (malformed credential: x)"
        );
        assert_eq!(FetchError::TokenExpired.to_string(), "token expired");
        let locked = FetchError::StoreUnavailable("security exited 51".into());
        assert_eq!(
            locked.to_string(),
            "credential store unavailable (security exited 51)"
        );
        assert_eq!(locked.kind(), ErrorKind::StoreUnavailable);
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
    fn client_builds_and_clones() {
        crate::install_crypto_provider();
        let c = Client::new().unwrap();
        let _shared = c.clone();
    }
}
