//! User settings, poll-interval parsing, and client SHA resolution.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Smallest poll interval the app accepts, in seconds.
pub const MIN_INTERVAL_SECS: u64 = 5;

/// Environment variable that overrides every other client SHA source.
pub const CLIENT_SHA_ENV: &str = "CLAUDE_USAGE_TUI_CLIENT_SHA";

/// File holding the anthropic-client-sha value.
pub const CLIENT_SHA_FILE: &str = "client_sha.txt";

/// Shortest string accepted as a client SHA from settings or file.
const MIN_CLIENT_SHA_LEN: usize = 8;

/// Persisted user settings. Missing fields take their [`Default`] values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Plan label shown in the UI.
    pub plan: String,
    pub interval_secs: u64,
    pub show_all: bool,
    pub always_on_top: bool,
    /// Explicit cookie database; `None` auto-detects the browser profile.
    pub cookie_file: Option<PathBuf>,
    /// anthropic-client-sha override entered in the app.
    pub client_sha: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            plan: "Max".to_owned(),
            interval_secs: 60,
            show_all: false,
            always_on_top: false,
            cookie_file: None,
            client_sha: None,
        }
    }
}

/// Parse `"30"`, `"30s"`, `"2m"`, `"1h"` into seconds.
pub fn parse_interval(s: &str) -> Result<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        bail!("empty interval");
    }
    let (digits, multiplier) = match s.as_bytes()[s.len() - 1] {
        b's' => (&s[..s.len() - 1], 1),
        b'm' => (&s[..s.len() - 1], 60),
        b'h' => (&s[..s.len() - 1], 3600),
        _ => (s.as_str(), 1),
    };
    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid interval {s:?}"))?;
    n.checked_mul(multiplier)
        .with_context(|| format!("interval {s:?} is too large"))
}

/// Resolve the anthropic-client-sha header value.
///
/// Order: env `CLAUDE_USAGE_TUI_CLIENT_SHA` (non-empty) > `settings_value`
/// (trimmed, at least 8 chars) > `client_sha.txt` next to the executable >
/// `client_sha.txt` in the current directory > error.
pub fn resolve_client_sha(settings_value: Option<&str>) -> Result<String> {
    let env = std::env::var(CLIENT_SHA_ENV).ok();
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(CLIENT_SHA_FILE));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(CLIENT_SHA_FILE);
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }
    resolve_client_sha_from(env.as_deref(), settings_value, &candidates)
}

/// [`resolve_client_sha`] with the environment value and file candidates
/// supplied by the caller.
fn resolve_client_sha_from(
    env_value: Option<&str>,
    settings_value: Option<&str>,
    candidates: &[PathBuf],
) -> Result<String> {
    if let Some(v) = env_value.map(str::trim).filter(|v| !v.is_empty()) {
        return Ok(v.to_owned());
    }
    let valid = |v: &str| {
        let v = v.trim();
        (v.len() >= MIN_CLIENT_SHA_LEN).then(|| v.to_owned())
    };
    if let Some(v) = settings_value.and_then(valid) {
        return Ok(v);
    }
    for path in candidates {
        if let Some(v) = fs::read_to_string(path).ok().as_deref().and_then(valid) {
            return Ok(v);
        }
    }
    let tried = if candidates.is_empty() {
        "  (no candidate paths)".to_owned()
    } else {
        candidates
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    bail!(
        "{CLIENT_SHA_FILE} is missing, empty, or unreadable. Tried:\n{tried}\n\
         Restore the file with a current anthropic-client-sha value, set the \
         {CLIENT_SHA_ENV} environment variable, or enter the client SHA in the \
         app's Settings. See README.md."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_values() {
        let s = Settings::default();
        assert_eq!(s.plan, "Max");
        assert_eq!(s.interval_secs, 60);
        assert!(!s.show_all);
        assert!(!s.always_on_top);
        assert_eq!(s.cookie_file, None);
        assert_eq!(s.client_sha, None);
    }

    #[test]
    fn settings_deserialize_empty_object_is_default() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn settings_deserialize_partial_keeps_other_defaults() {
        let s: Settings =
            serde_json::from_str(r#"{"interval_secs": 30, "cookie_file": "c.sqlite", "extra": 1}"#)
                .unwrap();
        assert_eq!(
            s,
            Settings {
                interval_secs: 30,
                cookie_file: Some(PathBuf::from("c.sqlite")),
                ..Settings::default()
            }
        );
    }

    #[test]
    fn settings_round_trip() {
        let s = Settings {
            plan: "Max 5x".into(),
            client_sha: Some("abcdef123".into()),
            ..Settings::default()
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn parse_interval_units() {
        assert_eq!(parse_interval("30").unwrap(), 30);
        assert_eq!(parse_interval("30s").unwrap(), 30);
        assert_eq!(parse_interval("2m").unwrap(), 120);
        assert_eq!(parse_interval("1h").unwrap(), 3600);
        assert_eq!(parse_interval("  2M \n").unwrap(), 120);
        assert_eq!(parse_interval("0").unwrap(), 0);
    }

    #[test]
    fn parse_interval_rejects_bad_input() {
        for bad in [
            "",
            "   ",
            "s",
            "m",
            "abc",
            "1x",
            "1.5m",
            "-5",
            "10d",
            "99999999999999999999h",
        ] {
            assert!(parse_interval(bad).is_err(), "{bad:?} should fail");
        }
    }

    fn sha_file(dir: &Path, contents: &str) -> PathBuf {
        let p = dir.join(CLIENT_SHA_FILE);
        fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn client_sha_env_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let file = sha_file(tmp.path(), "filesha1234");
        let got = resolve_client_sha_from(Some("envsha"), Some("settingssha"), &[file]).unwrap();
        assert_eq!(got, "envsha");
    }

    #[test]
    fn client_sha_empty_env_falls_through_to_settings() {
        let got = resolve_client_sha_from(Some(""), Some("  settingssha  "), &[]).unwrap();
        assert_eq!(got, "settingssha");
    }

    #[test]
    fn client_sha_short_settings_falls_through_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = sha_file(tmp.path(), "  filesha1234\n");
        let got = resolve_client_sha_from(None, Some("short"), &[file]).unwrap();
        assert_eq!(got, "filesha1234");
    }

    #[test]
    fn client_sha_files_tried_in_order() {
        let exe_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let missing = exe_dir.path().join("nope").join(CLIENT_SHA_FILE);
        let short = sha_file(exe_dir.path(), "tiny");
        let good = sha_file(cwd.path(), "cwdsha12345");
        let got =
            resolve_client_sha_from(None, None, &[missing, short.clone(), good.clone()]).unwrap();
        assert_eq!(got, "cwdsha12345");

        fs::write(&short, "exesha12345").unwrap();
        let got = resolve_client_sha_from(None, None, &[short, good]).unwrap();
        assert_eq!(got, "exesha12345");
    }

    #[test]
    fn client_sha_error_lists_paths_and_remedies() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a").join(CLIENT_SHA_FILE);
        let b = sha_file(tmp.path(), "   ");
        let msg = resolve_client_sha_from(None, None, &[a.clone(), b.clone()])
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("client_sha.txt is missing, empty, or unreadable"),
            "{msg}"
        );
        assert!(msg.contains(&a.display().to_string()), "{msg}");
        assert!(msg.contains(&b.display().to_string()), "{msg}");
        assert!(msg.contains(CLIENT_SHA_ENV), "{msg}");
        assert!(msg.contains("Settings"), "{msg}");
    }
}
