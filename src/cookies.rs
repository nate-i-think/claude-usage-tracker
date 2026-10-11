//! Firefox-family browser cookies: profile discovery, sqlite read, scoring.
//!
//! Ported from the cookie half of the Python api.py (removed; see git
//! history): `BROWSERS`, `find_cookie_files`, `normalize_expires`,
//! `score_profile`, `load_cookie_jar`, `detect_org_id`; restricted to
//! browsers that store cookies in `cookies.sqlite`.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::paths::{home_dir, non_empty_env};

const CLAUDE_HOST: &str = "claude.ai";
const SESSION_KEY: &str = "sessionKey";
const COOKIE_QUERY: &str =
    "SELECT name, value, host, path, expiry FROM moz_cookies WHERE host LIKE '%claude.ai'";

/// A Firefox-family browser and its profile glob patterns per OS.
struct BrowserDef {
    name: &'static str,
    win: &'static [&'static str],
    mac: &'static [&'static str],
    linux: &'static [&'static str],
}

impl BrowserDef {
    fn patterns(&self) -> &'static [&'static str] {
        if cfg!(windows) {
            self.win
        } else if cfg!(target_os = "macos") {
            self.mac
        } else {
            self.linux
        }
    }
}

const BROWSERS: &[BrowserDef] = &[
    BrowserDef {
        name: "Firefox",
        win: &["$APPDATA/Mozilla/Firefox/Profiles/*/cookies.sqlite"],
        mac: &["~/Library/Application Support/Firefox/Profiles/*/cookies.sqlite"],
        linux: &[
            "~/.mozilla/firefox/*/cookies.sqlite",
            "~/snap/firefox/common/.mozilla/firefox/*/cookies.sqlite",
        ],
    },
    BrowserDef {
        name: "Zen",
        win: &[
            "$APPDATA/zen/Profiles/*/cookies.sqlite",
            "$APPDATA/Zen/Profiles/*/cookies.sqlite",
        ],
        mac: &["~/Library/Application Support/zen/Profiles/*/cookies.sqlite"],
        linux: &["~/.zen/*/cookies.sqlite"],
    },
    BrowserDef {
        name: "LibreWolf",
        win: &[
            "$APPDATA/LibreWolf/Profiles/*/cookies.sqlite",
            "$APPDATA/librewolf/Profiles/*/cookies.sqlite",
        ],
        mac: &["~/Library/Application Support/LibreWolf/Profiles/*/cookies.sqlite"],
        linux: &["~/.librewolf/*/cookies.sqlite"],
    },
    BrowserDef {
        name: "Floorp",
        win: &["$APPDATA/Floorp/Profiles/*/cookies.sqlite"],
        mac: &["~/Library/Application Support/Floorp/Profiles/*/cookies.sqlite"],
        linux: &["~/.floorp/*/cookies.sqlite"],
    },
    BrowserDef {
        name: "Waterfox",
        win: &["$APPDATA/Waterfox/Profiles/*/cookies.sqlite"],
        mac: &["~/Library/Application Support/Waterfox/Profiles/*/cookies.sqlite"],
        linux: &["~/.waterfox/*/cookies.sqlite"],
    },
];

/// One cookie row. `expiry` is seconds since the epoch (normalized), `None`
/// for a session cookie. `Debug` shows the value only as its length.
#[derive(Clone, PartialEq)]
pub struct Cookie {
    pub host: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub expiry: Option<f64>,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("host", &self.host)
            .field("name", &self.name)
            .field("value_len", &self.value.len())
            .field("path", &self.path)
            .field("expiry", &self.expiry)
            .finish()
    }
}

impl Cookie {
    /// Exactly `claude.ai` or a subdomain of it (not `evilclaude.ai`).
    fn is_claude(&self) -> bool {
        is_claude_host(&self.host)
    }

    fn is_expired(&self, now: f64) -> bool {
        matches!(self.expiry, Some(e) if e <= now)
    }
}

fn is_claude_host(host: &str) -> bool {
    host == CLAUDE_HOST
        || host
            .strip_suffix(CLAUDE_HOST)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Ordering key for expiry: a session cookie (no expiry) counts as latest.
fn expiry_rank(expiry: Option<f64>) -> f64 {
    expiry.unwrap_or(f64::INFINITY)
}

/// The claude.ai cookies read from one profile. `Debug` never shows values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CookieJar {
    pub cookies: Vec<Cookie>,
}

impl CookieJar {
    /// The `lastActiveOrg` cookie value (latest-expiring row if duplicated).
    pub fn org_id(&self) -> Result<String> {
        self.latest_named("lastActiveOrg")
            .map(|c| c.value.clone())
            .ok_or_else(|| anyhow!("No lastActiveOrg cookie. Visit claude.ai/settings/usage."))
    }

    /// The claude.ai `anthropic-device-id` cookie value, if present.
    pub fn device_id(&self) -> Option<&str> {
        self.latest_named("anthropic-device-id")
            .map(|c| c.value.as_str())
    }

    /// `name=value; name2=value2` for claude.ai cookies, one per name.
    ///
    /// Expired rows are dropped; among duplicates the longest value wins,
    /// then the latest expiry. Names keep their first-seen order.
    pub fn cookie_header(&self, now: f64) -> String {
        let mut chosen: Vec<&Cookie> = Vec::new();
        for c in self
            .cookies
            .iter()
            .filter(|c| c.is_claude() && !c.is_expired(now))
        {
            match chosen.iter_mut().find(|k| k.name == c.name) {
                Some(kept) => {
                    let better = (c.value.len(), expiry_rank(c.expiry))
                        > (kept.value.len(), expiry_rank(kept.expiry));
                    if better {
                        *kept = c;
                    }
                }
                None => chosen.push(c),
            }
        }
        chosen
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn latest_named(&self, name: &str) -> Option<&Cookie> {
        self.cookies
            .iter()
            .filter(|c| c.name == name && c.is_claude())
            .max_by(|a, b| expiry_rank(a.expiry).total_cmp(&expiry_rank(b.expiry)))
    }
}

/// Detect µs vs ms vs s and return seconds since the epoch; 0 means none.
pub fn normalize_expires(raw: f64) -> Option<f64> {
    if raw == 0.0 {
        None
    } else if raw > 1e14 {
        Some(raw / 1_000_000.0)
    } else if raw > 1e11 {
        Some(raw / 1_000.0)
    } else {
        Some(raw)
    }
}

/// 3 = live sessionKey, 2 = expired, 1 = claude.ai cookies but no sessionKey
/// expiry, 0 = no claude.ai cookies. The expiry is the max across all
/// sessionKey rows (profiles hold expired stubs next to the real key).
pub fn score(jar: &CookieJar, now: f64) -> (u8, Option<f64>) {
    let mut has_claude = false;
    let mut session_exp: Option<f64> = None;
    for c in jar.cookies.iter().filter(|c| c.is_claude()) {
        has_claude = true;
        if c.name == SESSION_KEY {
            if let Some(e) = c.expiry {
                session_exp = Some(session_exp.map_or(e, |best| best.max(e)));
            }
        }
    }
    match (has_claude, session_exp) {
        (false, _) => (0, None),
        (true, None) => (1, None),
        (true, Some(e)) => (if e > now { 3 } else { 2 }, Some(e)),
    }
}

/// One profile considered during selection (for the debug command).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileCandidate {
    pub browser: &'static str,
    pub path: PathBuf,
    /// 0..=3 as in [`score`], or -1 when the file could not be read.
    pub score: i32,
    pub session_expiry: Option<f64>,
    pub error: Option<String>,
}

/// Every Firefox-family `cookies.sqlite` on this OS, deduped by canonical path.
pub fn find_cookie_files() -> Vec<(&'static str, PathBuf)> {
    let home = home_dir();
    let mut found = Vec::new();
    for browser in BROWSERS {
        for pattern in browser.patterns() {
            let Some(expanded) = expand_pattern(pattern, &home, non_empty_env) else {
                continue;
            };
            let Ok(paths) = glob::glob(&expanded) else {
                continue;
            };
            found.extend(paths.flatten().map(|p| (browser.name, p)));
        }
    }
    dedupe_paths(found)
}

/// Expand a leading `$VAR/` or `~/`. Returns `None` when the variable is
/// unset or the home dir is unknown, so the pattern is skipped. The expanded
/// base is glob-escaped.
fn expand_pattern(
    pattern: &str,
    home: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let (base, rest) = if let Some(rest) = pattern.strip_prefix("~/") {
        if home.as_os_str().is_empty() {
            return None;
        }
        (home.to_string_lossy().into_owned(), rest)
    } else if let Some(rest) = pattern.strip_prefix('$') {
        let (var, rest) = rest.split_once('/')?;
        (env(var)?, rest)
    } else {
        return Some(pattern.to_owned());
    };
    Some(format!("{}/{}", glob::Pattern::escape(&base), rest))
}

/// Drop entries whose canonical path was already seen; keep the original
/// (display-friendly) path of the first occurrence.
fn dedupe_paths(found: Vec<(&'static str, PathBuf)>) -> Vec<(&'static str, PathBuf)> {
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|(_, p)| seen.insert(fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .collect()
}

/// Read claude.ai cookies from a `cookies.sqlite`, via a temp copy.
///
/// The browser holds the live DB open, so the file and its `-wal` (if any)
/// are copied and the copy is opened read-write so SQLite replays the WAL.
pub fn read_cookie_file(path: &Path) -> Result<CookieJar> {
    let tmp = tempfile::TempDir::new().context("creating temp dir")?;
    let db = tmp.path().join("cookies.sqlite");
    fs::copy(path, &db).with_context(|| format!("copying {}", path.display()))?;
    let wal = sibling_with_suffix(path, "-wal");
    if wal.exists() {
        fs::copy(&wal, tmp.path().join("cookies.sqlite-wal"))
            .with_context(|| format!("copying {}", wal.display()))?;
    }

    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let cookies = {
        let mut stmt = conn.prepare(COOKIE_QUERY)?;
        let rows = stmt.query_map([], |row| {
            Ok(Cookie {
                name: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                value: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                host: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                path: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                expiry: row.get::<_, Option<f64>>(4)?.and_then(normalize_expires),
            })
        })?;
        let mut cookies = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        // The SQL LIKE also matches look-alikes such as evilclaude.ai.
        cookies.retain(Cookie::is_claude);
        cookies
    };
    // Close before the temp dir is removed (Windows will not delete open files).
    drop(conn);
    Ok(CookieJar { cookies })
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Load the best cookie jar.
///
/// With `explicit`, read only that file. Otherwise scan every profile, sort by
/// score then sessionKey expiry (both descending) and take the first with a
/// score of at least 1. The candidate list is returned for diagnostics.
pub fn load_best(explicit: Option<&Path>, now: f64) -> Result<(CookieJar, Vec<ProfileCandidate>)> {
    if let Some(path) = explicit {
        return load_explicit(path, now);
    }
    let files = find_cookie_files();
    if files.is_empty() {
        let tried: Vec<&str> = BROWSERS.iter().map(|b| b.name).collect();
        bail!(
            "Couldn't find any browser profile on this system.\n\
             Tried: {}.\n\
             Set a cookie file (--cookie-file PATH, or the cookie file setting) \
             if your profile lives elsewhere.",
            tried.join(", ")
        );
    }
    load_from_files(files, now)
}

fn load_explicit(path: &Path, now: f64) -> Result<(CookieJar, Vec<ProfileCandidate>)> {
    if !path.exists() {
        bail!("cookie file does not exist: {}", path.display());
    }
    let is_sqlite = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sqlite"));
    if !is_sqlite {
        bail!(
            "Unsupported cookie file {}: only Firefox-family cookies.sqlite files are supported.",
            path.display()
        );
    }
    let jar =
        read_cookie_file(path).map_err(|e| anyhow!("Couldn't read {}: {e:#}", path.display()))?;
    let (s, exp) = score(&jar, now);
    let candidate = ProfileCandidate {
        browser: "Firefox",
        path: path.to_path_buf(),
        score: i32::from(s),
        session_expiry: exp,
        error: None,
    };
    Ok((jar, vec![candidate]))
}

fn load_from_files(
    files: Vec<(&'static str, PathBuf)>,
    now: f64,
) -> Result<(CookieJar, Vec<ProfileCandidate>)> {
    let mut scored: Vec<(ProfileCandidate, Option<CookieJar>)> = files
        .into_iter()
        .map(|(browser, path)| match read_cookie_file(&path) {
            Ok(jar) => {
                let (s, exp) = score(&jar, now);
                let c = ProfileCandidate {
                    browser,
                    path,
                    score: i32::from(s),
                    session_expiry: exp,
                    error: None,
                };
                (c, Some(jar))
            }
            Err(e) => {
                let c = ProfileCandidate {
                    browser,
                    path,
                    score: -1,
                    session_expiry: None,
                    error: Some(format!("{e:#}")),
                };
                (c, None)
            }
        })
        .collect();

    scored.sort_by(|(a, _), (b, _)| {
        b.score.cmp(&a.score).then_with(|| {
            b.session_expiry
                .unwrap_or(0.0)
                .total_cmp(&a.session_expiry.unwrap_or(0.0))
        })
    });

    let best = scored
        .iter()
        .position(|(c, jar)| c.score >= 1 && jar.is_some());
    let mut jars: Vec<Option<CookieJar>> = Vec::with_capacity(scored.len());
    let mut candidates = Vec::with_capacity(scored.len());
    for (c, jar) in scored {
        candidates.push(c);
        jars.push(jar);
    }
    match best.and_then(|i| jars.swap_remove(i)) {
        Some(jar) => Ok((jar, candidates)),
        None => bail!(
            "No browser profile has usable claude.ai cookies. Sign in to claude.ai, then retry."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_800_000_000.0;
    const DAY: f64 = 86_400.0;

    /// Firefox stores expiry in milliseconds on the reference machine.
    fn ms(secs: f64) -> i64 {
        (secs * 1000.0) as i64
    }

    fn cookie(host: &str, name: &str, value: &str, expiry: Option<f64>) -> Cookie {
        Cookie {
            host: host.into(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            expiry,
        }
    }

    fn jar(cookies: Vec<Cookie>) -> CookieJar {
        CookieJar { cookies }
    }

    const SCHEMA: &str = "CREATE TABLE moz_cookies (
        id INTEGER PRIMARY KEY, name TEXT, value TEXT, host TEXT,
        path TEXT, expiry INTEGER, isSecure INTEGER)";

    fn insert(conn: &Connection, rows: &[(&str, &str, &str, i64)]) {
        for (name, value, host, expiry) in rows {
            conn.execute(
                "INSERT INTO moz_cookies (name, value, host, path, expiry, isSecure)
                 VALUES (?1, ?2, ?3, '/', ?4, 1)",
                rusqlite::params![name, value, host, expiry],
            )
            .unwrap();
        }
    }

    /// Create `<dir>/<profile>/cookies.sqlite` with the given rows.
    fn make_db(dir: &Path, profile: &str, rows: &[(&str, &str, &str, i64)]) -> PathBuf {
        let profile_dir = dir.join(profile);
        fs::create_dir_all(&profile_dir).unwrap();
        let path = profile_dir.join("cookies.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        insert(&conn, rows);
        path
    }

    #[test]
    fn normalize_expires_detects_units() {
        assert_eq!(normalize_expires(0.0), None);
        assert_eq!(normalize_expires(1_794_078_519.0), Some(1_794_078_519.0));
        assert_eq!(
            normalize_expires(1_794_078_519_874.0),
            Some(1_794_078_519.874)
        );
        assert_eq!(
            normalize_expires(1_794_078_519_874_000.0),
            Some(1_794_078_519.874)
        );
        // Boundaries are strict: exactly 1e11 is still seconds.
        assert_eq!(normalize_expires(1e11), Some(1e11));
        assert_eq!(normalize_expires(1e14), Some(1e11));
    }

    #[test]
    fn score_levels() {
        assert_eq!(score(&jar(vec![]), NOW), (0, None));
        assert_eq!(
            score(
                &jar(vec![cookie(
                    "example.com",
                    SESSION_KEY,
                    "v",
                    Some(NOW + DAY)
                )]),
                NOW
            ),
            (0, None)
        );
        assert_eq!(
            score(
                &jar(vec![cookie(".claude.ai", "lastActiveOrg", "o", None)]),
                NOW
            ),
            (1, None)
        );
        assert_eq!(
            score(
                &jar(vec![cookie(".claude.ai", SESSION_KEY, "v", None)]),
                NOW
            ),
            (1, None)
        );
        assert_eq!(
            score(
                &jar(vec![cookie(
                    ".claude.ai",
                    SESSION_KEY,
                    "v",
                    Some(NOW - DAY)
                )]),
                NOW
            ),
            (2, Some(NOW - DAY))
        );
        assert_eq!(
            score(
                &jar(vec![cookie(
                    ".claude.ai",
                    SESSION_KEY,
                    "v",
                    Some(NOW + DAY)
                )]),
                NOW
            ),
            (3, Some(NOW + DAY))
        );
    }

    #[test]
    fn score_uses_max_expiry_across_session_keys() {
        // Live key first, expired stub last: Python's "last wins" would say 2.
        let j = jar(vec![
            cookie(
                ".claude.ai",
                SESSION_KEY,
                "long-real-key",
                Some(NOW + 20.0 * DAY),
            ),
            cookie(
                ".claude.ai",
                SESSION_KEY,
                "other-real-key",
                Some(NOW + 5.0 * DAY),
            ),
            cookie("claude.ai", SESSION_KEY, "xx", Some(NOW - 100.0 * DAY)),
            cookie(".claude.ai", "sessionKeyLC", "z", Some(NOW + 99.0 * DAY)),
        ]);
        assert_eq!(score(&j, NOW), (3, Some(NOW + 20.0 * DAY)));
    }

    #[test]
    fn cookie_header_drops_expired_stub() {
        let real = "k".repeat(131);
        let j = jar(vec![
            cookie("claude.ai", SESSION_KEY, "xx", Some(NOW - DAY)),
            cookie(".claude.ai", SESSION_KEY, &real, Some(NOW + DAY)),
            cookie(".claude.ai", "lastActiveOrg", "org", None),
        ]);
        assert_eq!(
            j.cookie_header(NOW),
            format!("{SESSION_KEY}={real}; lastActiveOrg=org")
        );
    }

    #[test]
    fn cookie_header_keeps_longest_then_latest() {
        let j = jar(vec![
            cookie(".claude.ai", SESSION_KEY, "short", Some(NOW + 9.0 * DAY)),
            cookie(".claude.ai", SESSION_KEY, "longer-value", Some(NOW + DAY)),
            cookie(".claude.ai", "a", "same1", Some(NOW + DAY)),
            cookie(".claude.ai", "a", "same2", Some(NOW + 2.0 * DAY)),
            cookie(".claude.ai", "a", "same3", Some(NOW + 0.5 * DAY)),
            cookie("example.com", "other", "nope", None),
            // An expiry exactly at `now` counts as expired.
            cookie(".claude.ai", "gone", "v", Some(NOW)),
        ]);
        assert_eq!(j.cookie_header(NOW), "sessionKey=longer-value; a=same2");
    }

    #[test]
    fn debug_shows_value_lengths_not_values() {
        let secret = "sk-very-secret-session-value";
        let j = jar(vec![cookie(".claude.ai", SESSION_KEY, secret, Some(NOW))]);
        let out = format!("{j:?}");
        assert!(!out.contains(secret), "{out}");
        assert!(out.contains(SESSION_KEY), "{out}");
        assert!(out.contains(&secret.len().to_string()), "{out}");
        let pretty = format!("{:#?}", j.cookies[0]);
        assert!(!pretty.contains(secret), "{pretty}");
    }

    #[test]
    fn lookalike_hosts_are_not_claude() {
        let j = jar(vec![
            cookie("evilclaude.ai", "evil", "x", None),
            cookie(".evilclaude.ai", "evil2", "x", None),
            cookie(
                "evilclaude.ai",
                "lastActiveOrg",
                "evil-org",
                Some(NOW + DAY),
            ),
            cookie("evilclaude.ai", "anthropic-device-id", "evil-dev", None),
            cookie("claude.ai", "a", "1", None),
            cookie(".claude.ai", "b", "2", None),
            cookie("www.claude.ai", "c", "3", None),
        ]);
        assert_eq!(j.cookie_header(NOW), "a=1; b=2; c=3");
        assert!(j.org_id().is_err());
        assert_eq!(j.device_id(), None);

        let only_evil = jar(vec![cookie(
            "evilclaude.ai",
            SESSION_KEY,
            "v",
            Some(NOW + DAY),
        )]);
        assert_eq!(score(&only_evil, NOW), (0, None));
    }

    #[test]
    fn read_drops_lookalike_hosts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(
            tmp.path(),
            "p",
            &[
                ("a", "1", ".claude.ai", 0),
                ("evil", "x", "evilclaude.ai", 0),
            ],
        );
        let j = read_cookie_file(&path).unwrap();
        let names: Vec<_> = j.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn org_id_and_device_id() {
        let j = jar(vec![
            cookie("example.com", "anthropic-device-id", "not-claude", None),
            cookie(
                ".claude.ai",
                "anthropic-device-id",
                "dev-1",
                Some(NOW + DAY),
            ),
            cookie(".claude.ai", "lastActiveOrg", "org-1", Some(NOW + DAY)),
        ]);
        assert_eq!(j.org_id().unwrap(), "org-1");
        assert_eq!(j.device_id(), Some("dev-1"));

        let empty = jar(vec![]);
        assert_eq!(
            empty.org_id().unwrap_err().to_string(),
            "No lastActiveOrg cookie. Visit claude.ai/settings/usage."
        );
        assert_eq!(empty.device_id(), None);
    }

    #[test]
    fn read_filters_hosts_and_normalizes_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(
            tmp.path(),
            "p",
            &[
                (SESSION_KEY, "real", ".claude.ai", ms(NOW + DAY)),
                ("lastActiveOrg", "org", "claude.ai", 0),
                ("x", "y", "example.com", ms(NOW + DAY)),
                ("z", "w", "claude.ai.evil.com", ms(NOW + DAY)),
            ],
        );
        let j = read_cookie_file(&path).unwrap();
        let mut names: Vec<_> = j.cookies.iter().map(|c| c.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["lastActiveOrg", SESSION_KEY]);
        let sk = j.cookies.iter().find(|c| c.name == SESSION_KEY).unwrap();
        assert_eq!(sk.expiry, Some(NOW + DAY));
        assert_eq!(sk.path, "/");
        let org = j
            .cookies
            .iter()
            .find(|c| c.name == "lastActiveOrg")
            .unwrap();
        assert_eq!(org.expiry, None);
    }

    #[test]
    fn read_replays_wal_from_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cookies.sqlite");
        let conn = Connection::open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        conn.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        insert(
            &conn,
            &[(SESSION_KEY, "in-wal", ".claude.ai", ms(NOW + DAY))],
        );

        // Connection still open: rows live only in the -wal file.
        let wal = sibling_with_suffix(&path, "-wal");
        assert!(fs::metadata(&wal).unwrap().len() > 0);

        let j = read_cookie_file(&path).unwrap();
        assert_eq!(j.cookies.len(), 1);
        assert_eq!(j.cookies[0].value, "in-wal");

        // Without the WAL, the main file alone has no rows (not even the table).
        let main_only = tmp.path().join("main-only");
        fs::create_dir(&main_only).unwrap();
        let copy = main_only.join("cookies.sqlite");
        fs::copy(&path, &copy).unwrap();
        assert!(read_cookie_file(&copy).map_or(true, |j| j.cookies.is_empty()));
        drop(conn);
    }

    #[test]
    fn read_missing_table_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cookies.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE other (x INTEGER)")
            .unwrap();
        assert!(read_cookie_file(&path).is_err());
    }

    #[test]
    fn load_from_files_orders_and_picks_best() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let expired = make_db(
            d,
            "expired",
            &[(SESSION_KEY, "a", ".claude.ai", ms(NOW - DAY))],
        );
        let soon = make_db(
            d,
            "soon",
            &[(SESSION_KEY, "b", ".claude.ai", ms(NOW + DAY))],
        );
        let later = make_db(
            d,
            "later",
            &[
                ("lastActiveOrg", "org-later", ".claude.ai", 0),
                (SESSION_KEY, "c", ".claude.ai", ms(NOW + 10.0 * DAY)),
            ],
        );
        let none = make_db(d, "none", &[("x", "y", "example.com", 0)]);
        let nosession = make_db(d, "nosession", &[("lastActiveOrg", "o", ".claude.ai", 0)]);
        let broken = d.join("broken").join("cookies.sqlite");
        fs::create_dir_all(broken.parent().unwrap()).unwrap();
        fs::write(&broken, b"not a database").unwrap();

        let files = vec![
            ("Firefox", broken.clone()),
            ("Firefox", none.clone()),
            ("Zen", expired.clone()),
            ("Zen", soon.clone()),
            ("Floorp", nosession.clone()),
            ("Zen", later.clone()),
        ];
        let (j, candidates) = load_from_files(files, NOW).unwrap();
        assert_eq!(j.org_id().unwrap(), "org-later");

        let order: Vec<(&Path, i32)> = candidates
            .iter()
            .map(|c| (c.path.as_path(), c.score))
            .collect();
        assert_eq!(
            order,
            [
                (later.as_path(), 3),
                (soon.as_path(), 3),
                (expired.as_path(), 2),
                (nosession.as_path(), 1),
                (none.as_path(), 0),
                (broken.as_path(), -1),
            ]
        );
        assert_eq!(candidates[0].browser, "Zen");
        assert_eq!(candidates[0].session_expiry, Some(NOW + 10.0 * DAY));
        assert!(candidates[5].error.is_some());
        assert!(candidates[..5].iter().all(|c| c.error.is_none()));
    }

    #[test]
    fn load_from_files_without_usable_profile_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let none = make_db(tmp.path(), "none", &[("x", "y", "example.com", 0)]);
        let err = load_from_files(vec![("Firefox", none)], NOW).unwrap_err();
        assert_eq!(
            err.to_string(),
            "No browser profile has usable claude.ai cookies. Sign in to claude.ai, then retry."
        );
    }

    #[test]
    fn load_best_explicit_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(
            tmp.path(),
            "p",
            &[(SESSION_KEY, "v", ".claude.ai", ms(NOW + DAY))],
        );
        let (j, candidates) = load_best(Some(&path), NOW).unwrap();
        assert_eq!(j.cookies.len(), 1);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, path);
        assert_eq!(candidates[0].score, 3);
        assert_eq!(candidates[0].session_expiry, Some(NOW + DAY));
    }

    #[test]
    fn load_best_explicit_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.sqlite");
        let err = load_best(Some(&missing), NOW).unwrap_err().to_string();
        assert_eq!(
            err,
            format!("cookie file does not exist: {}", missing.display())
        );

        let chrome = tmp.path().join("Cookies");
        fs::write(&chrome, b"x").unwrap();
        let err = load_best(Some(&chrome), NOW).unwrap_err().to_string();
        assert!(err.contains("only Firefox-family cookies.sqlite"), "{err}");

        let broken = tmp.path().join("cookies.sqlite");
        fs::write(&broken, b"not a database").unwrap();
        let err = load_best(Some(&broken), NOW).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("Couldn't read {}: ", broken.display())),
            "{err}"
        );
    }

    #[test]
    fn expand_pattern_handles_env_home_and_unset() {
        let home = Path::new("/home/u[1]");
        let env = |k: &str| (k == "APPDATA").then(|| "C:/Users/A B/AppData".to_owned());
        assert_eq!(
            expand_pattern("$APPDATA/zen/Profiles/*/cookies.sqlite", home, env).as_deref(),
            Some("C:/Users/A B/AppData/zen/Profiles/*/cookies.sqlite")
        );
        assert_eq!(expand_pattern("$LOCALAPPDATA/x/*", home, env), None);
        assert_eq!(
            expand_pattern("~/.zen/*/cookies.sqlite", home, env).as_deref(),
            Some("/home/u[[]1[]]/.zen/*/cookies.sqlite")
        );
        assert_eq!(expand_pattern("~/.zen/*", Path::new(""), env), None);
    }

    #[test]
    fn expanded_pattern_globs_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let a = make_db(tmp.path(), "zen/Profiles/a.default", &[]);
        let b = make_db(tmp.path(), "zen/Profiles/b.beta", &[]);
        let root = tmp.path().to_string_lossy().into_owned();
        let env = |k: &str| (k == "APPDATA").then(|| root.clone());
        let pattern =
            expand_pattern("$APPDATA/zen/Profiles/*/cookies.sqlite", Path::new(""), env).unwrap();
        let mut found: Vec<PathBuf> = glob::glob(&pattern).unwrap().flatten().collect();
        found.sort();
        let canon = |p: &Path| fs::canonicalize(p).unwrap();
        let found: Vec<PathBuf> = found.iter().map(|p| canon(p)).collect();
        assert_eq!(found, [canon(&a), canon(&b)]);
    }

    #[test]
    fn dedupe_paths_by_canonical_path() {
        let tmp = tempfile::tempdir().unwrap();
        let a = make_db(tmp.path(), "zen/p", &[]);
        let alias = tmp
            .path()
            .join("zen")
            .join("..")
            .join("zen")
            .join("p")
            .join("cookies.sqlite");
        let other = make_db(tmp.path(), "ff/p", &[]);
        let out = dedupe_paths(vec![
            ("Zen", a.clone()),
            ("Zen", alias),
            ("Firefox", other.clone()),
        ]);
        assert_eq!(out, [("Zen", a), ("Firefox", other)]);
    }

    #[test]
    fn browsers_cover_firefox_family_on_every_os() {
        let names: Vec<&str> = BROWSERS.iter().map(|b| b.name).collect();
        assert_eq!(names, ["Firefox", "Zen", "LibreWolf", "Floorp", "Waterfox"]);
        for b in BROWSERS {
            for p in b.win.iter().chain(b.mac).chain(b.linux) {
                assert!(p.ends_with("/cookies.sqlite"), "{p}");
            }
        }
    }
}
