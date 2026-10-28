//! Pure formatting for `claude-usage-cli debug`: the per-profile table and
//! recommendations ([`ProfileDiag`], [`profiles_table`],
//! [`recommendations`]), plus masking secrets, redacting API keys, aligned
//! tables and local timestamps. No function here prints a secret except
//! through [`mask`]; a [`ProfileDiag`] never holds one.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::fetch::{ErrorKind, FetchError};
use crate::poller::{self, FetchErrorInfo};

/// Characters kept at each end by [`mask`].
const MASK_KEEP: usize = 6;

/// As the Python debug.py `_mask` (removed): `"<none>"` for no value; all
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

/// A Unix timestamp as "YYYY-MM-DD HH:MM" in `tz`; empty when out of range.
pub fn absolute_in<Tz: TimeZone>(ts: f64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    DateTime::<Utc>::from_timestamp(ts as i64, 0)
        .map(|t| t.with_timezone(tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
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

/// `sk-ant-[\w-]{6,}` → `sk-ant-***`. The Python debug.py used `\w{6,}`, which misses
/// real keys such as `sk-ant-api03-…` (the hyphen ends the match early).
pub fn redact_api_keys(s: &str) -> String {
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

/// What `debug` found of a profile's credential. Never the token.
#[derive(Debug, Clone, PartialEq)]
pub enum CredentialState {
    /// Read; `None` = no `expiresAt`.
    Present {
        expires_at: Option<DateTime<Utc>>,
    },
    Missing,
    /// The store failed; only the failure's kind ("malformed",
    /// "unavailable"), never its text.
    Error(&'static str),
}

/// One probe fetch of the usage endpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    Ok,
    /// `status` when the endpoint answered.
    Failed {
        status: Option<u16>,
        error: FetchErrorInfo,
    },
}

impl Probe {
    /// A failed fetch for the profile whose config dir is `dir`.
    pub fn failed(e: &FetchError, dir: &Path, home: &Path) -> Self {
        let status = match e {
            FetchError::Unauthorized => Some(401),
            FetchError::Forbidden { .. } => Some(403),
            FetchError::RateLimited { .. } => Some(429),
            FetchError::Status { status, .. } => Some(*status),
            _ => None,
        };
        let mut error = FetchErrorInfo::for_dir(e, dir, home);
        // Bodies and transport errors are already redacted by `fetch`; this
        // is the last line of defence before the terminal.
        error.message = redact_api_keys(&error.message);
        Self::Failed { status, error }
    }
}

/// Everything `debug` shows for one profile; gathered by the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileDiag {
    pub id: String,
    pub config_dir: PathBuf,
    /// The exact `CLAUDE_CONFIG_DIR` value; `None` = default profile.
    pub config_dir_env: Option<String>,
    pub keychain_service: String,
    pub credential: CredentialState,
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    /// Last change to a session JSONL under `<config_dir>/projects`.
    pub newest_session: Option<DateTime<Utc>>,
    pub probe: Probe,
}

/// "45s", "12m", "3h 12m", "2d 4h"; negative spans count as their size.
pub fn span(d: Duration) -> String {
    let secs = d.num_seconds().abs();
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{mins}m")
    } else if days == 0 {
        format!("{hours}h {mins:02}m")
    } else {
        format!("{days}d {hours}h")
    }
}

/// "in 3h 12m" or "2h 05m ago".
pub fn relative(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if t > now {
        format!("in {}", span(t - now))
    } else {
        format!("{} ago", span(now - t))
    }
}

/// The credential's expiry: "in 3h 12m", "expired 2h 05m ago", "no expiry";
/// "—" without a credential.
fn expires(c: &CredentialState, now: DateTime<Utc>) -> String {
    match c {
        CredentialState::Present {
            expires_at: Some(t),
        } if *t <= now => format!("expired {}", relative(*t, now)),
        CredentialState::Present {
            expires_at: Some(t),
        } => relative(*t, now),
        CredentialState::Present { expires_at: None } => "no expiry".to_owned(),
        _ => "—".to_owned(),
    }
}

fn credential(c: &CredentialState) -> String {
    match c {
        CredentialState::Present { .. } => "present".to_owned(),
        CredentialState::Missing => "missing".to_owned(),
        CredentialState::Error(kind) => format!("error: {kind}"),
    }
}

/// "HTTP 200", "HTTP 401", or the error kind when nothing answered.
fn probe(p: &Probe) -> String {
    match p {
        Probe::Ok => "HTTP 200".to_owned(),
        Probe::Failed {
            status: Some(s), ..
        } => format!("HTTP {s}"),
        Probe::Failed { error, .. } => format!("{:?}", error.kind),
    }
}

/// One row per profile: id, config dir (`~` for `home`), Keychain service,
/// credential, expiry, email, account (uuid's first 8), newest session,
/// probe.
pub fn profiles_table(diags: &[ProfileDiag], home: &Path, now: DateTime<Utc>) -> String {
    let dash = || "—".to_owned();
    let rows: Vec<Vec<String>> = diags
        .iter()
        .map(|d| {
            vec![
                d.id.clone(),
                poller::tilde(&d.config_dir, home),
                d.keychain_service.clone(),
                credential(&d.credential),
                expires(&d.credential, now),
                d.email.clone().unwrap_or_else(dash),
                d.account_uuid
                    .as_deref()
                    .map(|u| u.chars().take(8).collect())
                    .unwrap_or_else(dash),
                d.newest_session
                    .map(|t| relative(t, now))
                    .unwrap_or_else(dash),
                probe(&d.probe),
            ]
        })
        .collect();
    table(
        &[
            "id",
            "config dir",
            "keychain service",
            "credential",
            "expires",
            "email",
            "account",
            "last session",
            "probe",
        ],
        &rows,
    )
}

/// `s` quoted for a POSIX shell when it needs it.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-~+:@%=".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// How to start Claude Code in `d`'s config dir.
fn claude_command(d: &ProfileDiag) -> String {
    match &d.config_dir_env {
        None => "claude".to_owned(),
        Some(env) => format!("CLAUDE_CONFIG_DIR={} claude", shell_quote(env)),
    }
}

/// What to do for one profile, from its probe.
fn advice(d: &ProfileDiag) -> String {
    let Probe::Failed { error, .. } = &d.probe else {
        return "OK".to_owned();
    };
    let cmd = claude_command(d);
    let message = redact_api_keys(&error.message);
    match error.kind {
        ErrorKind::NoCredentials => {
            format!("no credential — run `{cmd}` and sign in with /login")
        }
        ErrorKind::StoreUnavailable => format!(
            "couldn't read the credential store (not a missing login) — {}",
            crate::credentials::UNAVAILABLE_HINT
        ),
        ErrorKind::TokenExpired | ErrorKind::Unauthorized => format!(
            "token expired or rejected — run `{cmd}` once so Claude Code refreshes it \
             (this app never refreshes tokens)"
        ),
        ErrorKind::RateLimited => {
            format!("rate limited — wait a few minutes and try again ({message})")
        }
        ErrorKind::Transport => {
            format!("network problem — check the connection, proxy or firewall ({message})")
        }
        ErrorKind::Forbidden | ErrorKind::Status => {
            format!("the usage endpoint refused the request: {message}")
        }
        ErrorKind::Json => format!(
            "the usage endpoint's answer didn't parse; the API may have changed ({message})"
        ),
        ErrorKind::Internal => format!("internal error ({message})"),
    }
}

/// One line per profile: "<id>: <what to do>", or "OK".
pub fn recommendations(diags: &[ProfileDiag]) -> String {
    diags
        .iter()
        .map(|d| format!("{}: {}", d.id, advice(d)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const NOW: f64 = 1_800_000_000.0;

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
    fn absolute_time_in_zone() {
        let utc = FixedOffset::east_opt(0).unwrap();
        assert_eq!(absolute_in(NOW, &utc), "2027-01-15 08:00");
        assert_eq!(absolute_in(f64::MAX, &utc), "");
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
    fn redacts_api_keys() {
        let s = "oops sk-ant-api03-SECRETSECRET and sk-ant-ab end";
        let r = redact_api_keys(s);
        assert_eq!(r, "oops sk-ant-*** and sk-ant-ab end");
        assert!(!r.contains("SECRET"));
        assert_eq!(
            redact_api_keys("Bearer sk-ant-oat01-FIXTURE-SECRET"),
            "Bearer sk-ant-***"
        );
    }

    const FIXTURE: &str = "sk-ant-oat01-FIXTURE-SECRET";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 28, 12, 0, 0).unwrap()
    }

    fn home() -> PathBuf {
        PathBuf::from("/Users/me")
    }

    fn diag(id: &str, probe: Probe) -> ProfileDiag {
        let (dir, env) = if id == "default" {
            (home().join(".claude"), None)
        } else {
            let dir = home().join(format!(".{id}"));
            let env = Some(dir.display().to_string());
            (dir, env)
        };
        ProfileDiag {
            id: id.into(),
            config_dir: dir,
            config_dir_env: env,
            keychain_service: "Claude Code-credentials".into(),
            credential: CredentialState::Present {
                expires_at: Some(now() + Duration::minutes(3 * 60 + 12)),
            },
            email: Some("me@example.com".into()),
            account_uuid: Some("0123456789abcdef".into()),
            newest_session: Some(now() - Duration::minutes(5)),
            probe,
        }
    }

    fn failed(e: FetchError, id: &str) -> Probe {
        Probe::failed(&e, &diag(id, Probe::Ok).config_dir, &home())
    }

    #[test]
    fn spans_and_relative_times() {
        assert_eq!(span(Duration::seconds(45)), "45s");
        assert_eq!(span(Duration::minutes(12)), "12m");
        assert_eq!(span(Duration::minutes(3 * 60 + 2)), "3h 02m");
        assert_eq!(span(Duration::hours(52)), "2d 4h");
        assert_eq!(relative(now() + Duration::minutes(90), now()), "in 1h 30m");
        assert_eq!(relative(now() - Duration::minutes(7), now()), "7m ago");
    }

    fn cells(line: &str) -> Vec<&str> {
        line.split("  ")
            .filter(|c| !c.is_empty())
            .map(str::trim)
            .collect()
    }

    #[test]
    fn table_has_a_row_per_profile() {
        let mut second = diag("claude-secondary", failed(FetchError::TokenExpired, "x"));
        second.credential = CredentialState::Present {
            expires_at: Some(now() - Duration::hours(2)),
        };
        second.email = None;
        second.account_uuid = None;
        second.newest_session = None;
        let mut third = diag(
            "work",
            failed(FetchError::NoCredentials(String::new()), "x"),
        );
        third.credential = CredentialState::Missing;
        let mut fourth = diag("odd", failed(FetchError::Transport("dns".into()), "x"));
        fourth.credential = CredentialState::Error("malformed");
        fourth.keychain_service = "svc".into();
        let t = profiles_table(
            &[diag("default", Probe::Ok), second, third, fourth],
            &home(),
            now(),
        );
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 6, "{t}");
        assert!(lines[0].starts_with("id  "), "{t}");
        assert_eq!(
            cells(lines[2]),
            [
                "default",
                "~/.claude",
                "Claude Code-credentials",
                "present",
                "in 3h 12m",
                "me@example.com",
                "01234567",
                "5m ago",
                "HTTP 200"
            ]
        );
        assert_eq!(
            cells(lines[3]),
            [
                "claude-secondary",
                "~/.claude-secondary",
                "Claude Code-credentials",
                "present",
                "expired 2h 00m ago",
                "—",
                "—",
                "—",
                "TokenExpired"
            ]
        );
        assert!(
            lines[4].contains("missing") && lines[4].ends_with("NoCredentials"),
            "{t}"
        );
        assert!(
            lines[5].contains("error: malformed") && lines[5].ends_with("Transport"),
            "{t}"
        );
    }

    #[test]
    fn probe_shows_the_http_status_when_one_came_back() {
        let p = failed(FetchError::Unauthorized, "default");
        let t = profiles_table(&[diag("default", p)], &home(), now());
        assert!(t.ends_with("HTTP 401"), "{t}");
        let p = failed(FetchError::RateLimited { retry_after: None }, "default");
        assert!(profiles_table(&[diag("default", p)], &home(), now()).ends_with("HTTP 429"));
    }

    #[test]
    fn a_recommendation_per_error_kind() {
        let advice = |e: FetchError, id: &str| recommendations(&[diag(id, failed(e, id))]);
        assert_eq!(
            recommendations(&[diag("default", Probe::Ok)]),
            "default: OK"
        );
        assert_eq!(
            advice(FetchError::NoCredentials(String::new()), "default"),
            "default: no credential — run `claude` and sign in with /login"
        );
        assert_eq!(
            advice(FetchError::NoCredentials(String::new()), "claude-secondary"),
            "claude-secondary: no credential — run \
             `CLAUDE_CONFIG_DIR=/Users/me/.claude-secondary claude` and sign in with /login"
        );
        let r = advice(
            FetchError::StoreUnavailable("security exited 51".into()),
            "default",
        );
        assert_eq!(
            r,
            format!(
                "default: couldn't read the credential store (not a missing login) — {}",
                crate::credentials::UNAVAILABLE_HINT
            )
        );
        for e in [FetchError::TokenExpired, FetchError::Unauthorized] {
            let r = advice(e, "claude-secondary");
            assert!(
                r.contains("run `CLAUDE_CONFIG_DIR=/Users/me/.claude-secondary claude` once")
                    && r.contains("refresh"),
                "{r}"
            );
        }
        let r = advice(
            FetchError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(30)),
            },
            "default",
        );
        assert!(r.contains("wait") && r.contains("retry after 30s"), "{r}");
        let r = advice(FetchError::Transport("dns error".into()), "default");
        assert!(r.contains("network") && r.contains("dns error"), "{r}");
        let r = advice(
            FetchError::Forbidden {
                body: "nope".into(),
            },
            "default",
        );
        assert!(
            r.contains("refused") && r.contains("403 — forbidden: nope"),
            "{r}"
        );
        let r = advice(
            FetchError::Status {
                status: 500,
                body: "oops".into(),
            },
            "default",
        );
        assert!(r.contains("HTTP 500: oops"), "{r}");
        let r = advice(FetchError::Json("bad".into()), "default");
        assert!(r.contains("didn't parse"), "{r}");
    }

    #[test]
    fn config_dirs_with_spaces_are_quoted() {
        let mut d = diag("x", failed(FetchError::TokenExpired, "x"));
        d.config_dir_env = Some("/Users/me/my claude's".into());
        let r = recommendations(&[d]);
        assert!(
            r.contains(r"CLAUDE_CONFIG_DIR='/Users/me/my claude'\''s' claude"),
            "{r}"
        );
    }

    #[test]
    fn bodies_are_redacted() {
        // Even a body that slipped past fetch's own redaction.
        let probe = Probe::Failed {
            status: Some(403),
            error: FetchErrorInfo {
                kind: ErrorKind::Forbidden,
                message: format!("403 — forbidden: token {FIXTURE} invalid"),
            },
        };
        let r = recommendations(&[diag("default", probe)]);
        assert!(r.contains("token sk-ant-*** invalid"), "{r}");
        assert!(!r.contains("FIXTURE-SECRET"), "{r}");
    }

    #[test]
    fn output_never_contains_the_fixture_token() {
        let kinds = [
            FetchError::NoCredentials(format!("malformed credential near {FIXTURE}")),
            FetchError::TokenExpired,
            FetchError::Unauthorized,
            FetchError::Forbidden {
                body: format!("Bearer {FIXTURE}"),
            },
            FetchError::RateLimited { retry_after: None },
            FetchError::Status {
                status: 502,
                body: FIXTURE.into(),
            },
            FetchError::Transport(FIXTURE.into()),
            FetchError::Json(FIXTURE.into()),
        ];
        let diags: Vec<ProfileDiag> = kinds
            .into_iter()
            .map(|e| diag("claude-secondary", failed(e, "claude-secondary")))
            .collect();
        let out = format!(
            "{}\n{}\n{diags:?}",
            profiles_table(&diags, &home(), now()),
            recommendations(&diags)
        );
        assert!(!out.contains("FIXTURE-SECRET"), "{out}");
        assert!(!out.contains("oat01"), "{out}");
    }
}
