//! Claude Code profiles: config directories (`~/.claude`,
//! `~/.claude-secondary`, ...), the account each is signed in to, and the
//! Keychain service its OAuth credential lives under.
//!
//! Discovery ([`discover`], [`read_account`]) reads the filesystem; merging
//! and resolving against the saved [`ProfileSettings`] is pure.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ProfileSettings;
use crate::credentials::CredentialLocator;

/// The id of the profile for `~/.claude` (`CLAUDE_CONFIG_DIR` unset).
pub const DEFAULT_ID: &str = "default";

/// Keychain service of the default profile; other profiles append a hash.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Longest label derived from an email's local part, in chars.
const MAX_LABEL_CHARS: usize = 16;

/// A profile's stable slug: `default`, or the config dir's name without its
/// leading `.` (`claude-secondary`). Keys storage, history, and alerts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT_ID
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `oauthAccount` object of a profile's `.claude.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
    pub display_name: Option<String>,
    pub billing_type: Option<String>,
}

/// A profile found on disk by [`discover`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub id: ProfileId,
    pub config_dir: PathBuf,
    /// The exact `CLAUDE_CONFIG_DIR` value for this dir; `None` = default.
    pub config_dir_env: Option<String>,
    pub account: Option<AccountInfo>,
}

/// An enabled profile, ready to poll: saved settings over discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub id: ProfileId,
    pub config_dir: PathBuf,
    /// The exact `CLAUDE_CONFIG_DIR` value; `None` = default profile.
    pub config_dir_env: Option<String>,
    pub label: String,
    /// Series color index.
    pub color: u8,
    /// Keychain service from settings, replacing the derived one.
    pub keychain_override: Option<String>,
    pub account: Option<AccountInfo>,
}

impl Profile {
    /// The `.claude.json` holding `oauthAccount`: `~/.claude.json` for the
    /// default profile, `<config_dir>/.claude.json` otherwise.
    pub fn identity_file(&self) -> PathBuf {
        match &self.config_dir_env {
            None => self
                .config_dir
                .parent()
                .unwrap_or(Path::new(""))
                .join(".claude.json"),
            Some(_) => self.config_dir.join(".claude.json"),
        }
    }

    /// Claude Code project session JSONLs: `<config_dir>/projects`.
    pub fn projects_dir(&self) -> PathBuf {
        self.config_dir.join("projects")
    }

    /// The macOS Keychain service holding this profile's OAuth credential.
    pub fn keychain_service(&self) -> String {
        self.keychain_override
            .clone()
            .unwrap_or_else(|| keychain_service_for(self.config_dir_env.as_deref()))
    }

    /// Where this profile's OAuth credential lives: its Keychain service
    /// and `<config_dir>/.credentials.json`.
    pub fn locator(&self) -> CredentialLocator {
        CredentialLocator {
            keychain_service: Some(self.keychain_service()),
            file: self.config_dir.join(".credentials.json"),
        }
    }

    /// The label cut to [`MAX_LABEL_CHARS`] chars, for tight spots.
    pub fn short_label(&self) -> &str {
        truncate_chars(&self.label, MAX_LABEL_CHARS)
    }
}

/// One profile in `profiles.json`, the index out-of-process readers (CLI,
/// MCP) use to map a `CLAUDE_CONFIG_DIR` to a profile. Holds no secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIndexEntry {
    pub id: ProfileId,
    pub label: String,
    pub color: u8,
    pub config_dir: PathBuf,
    pub config_dir_env: Option<String>,
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub is_default: bool,
}

/// Claude Code's Keychain service name for a `CLAUDE_CONFIG_DIR` value:
/// `Claude Code-credentials` when unset, else suffixed with the first 8 hex
/// digits of the SHA-256 of the exact string.
pub fn keychain_service_for(config_dir_env: Option<&str>) -> String {
    match config_dir_env {
        None => KEYCHAIN_SERVICE.to_owned(),
        Some(dir) => {
            let digest = ring::digest::digest(&ring::digest::SHA256, dir.as_bytes());
            let hex: String = digest.as_ref()[..4]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{KEYCHAIN_SERVICE}-{hex}")
        }
    }
}

/// The `oauthAccount` of a `.claude.json`; `None` if the file is missing,
/// unparsable, or not signed in.
pub fn read_account(identity_file: &Path) -> Option<AccountInfo> {
    let text = fs::read_to_string(identity_file).ok()?;
    let root: Value = serde_json::from_str(&text).ok()?;
    let account = root.get("oauthAccount")?.as_object()?;
    let field = |key: &str| account.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(AccountInfo {
        email: field("emailAddress"),
        account_uuid: field("accountUuid"),
        organization_uuid: field("organizationUuid"),
        organization_name: field("organizationName"),
        display_name: field("displayName"),
        billing_type: field("billingType"),
    })
}

/// The Claude Code profiles under `home`: `~/.claude` (if present) first,
/// then each `~/.claude-*` dir whose `.claude.json` has an `oauthAccount`,
/// by name. Accounts seen twice keep the first dir only.
pub fn discover(home: &Path) -> Vec<Discovered> {
    let mut found = Vec::new();
    let default_dir = home.join(".claude");
    if default_dir.is_dir() {
        found.push(Discovered {
            id: ProfileId::new(DEFAULT_ID),
            config_dir: default_dir,
            config_dir_env: None,
            account: read_account(&home.join(".claude.json")),
        });
    }

    let mut names: Vec<String> = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(".claude-"))
        .collect();
    names.sort();
    for name in names {
        let dir = home.join(&name);
        let Some(account) = read_account(&dir.join(".claude.json")) else {
            continue;
        };
        let taken = |id: &str| found.iter().any(|d: &Discovered| d.id.as_str() == id);
        let id = unique_id(&slug(&name), taken);
        found.push(Discovered {
            id: ProfileId::new(id),
            config_dir: dir,
            config_dir_env: Some(format!("{}/{}", home.display(), name)),
            account: Some(account),
        });
    }

    let mut seen = Vec::new();
    found.retain(
        |d| match d.account.as_ref().and_then(|a| a.account_uuid.clone()) {
            Some(uuid) if seen.contains(&uuid) => false,
            Some(uuid) => {
                seen.push(uuid);
                true
            }
            None => true,
        },
    );
    found
}

/// The default profile (`<home>/.claude`), filled in from discovery when
/// that finds it. Used until every enabled profile is polled.
pub fn default_profile(home: &Path) -> Profile {
    let saved = [ProfileSettings {
        id: DEFAULT_ID.to_owned(),
        config_dir: home.join(".claude"),
        ..ProfileSettings::default()
    }];
    resolve(&saved, &discover(home))
        .pop()
        .expect("an enabled saved profile resolves")
}

/// Append each discovered profile not yet saved (matched by config dir) to
/// `saved`, keeping its order and leaving disabled entries disabled. Returns
/// whether anything was added.
pub fn merge_discovered(saved: &mut Vec<ProfileSettings>, found: &[Discovered]) -> bool {
    let mut changed = false;
    for d in found {
        if saved.iter().any(|s| s.config_dir == d.config_dir) {
            continue;
        }
        let id = unique_id(d.id.as_str(), |id| saved.iter().any(|s| s.id == id));
        saved.push(ProfileSettings {
            id,
            config_dir: d.config_dir.clone(),
            ..ProfileSettings::default()
        });
        changed = true;
    }
    changed
}

/// The enabled saved profiles, in saved order, filled in from discovery.
/// An entry nothing discovered (added by hand) still resolves from its
/// config dir, without an account.
pub fn resolve(saved: &[ProfileSettings], found: &[Discovered]) -> Vec<Profile> {
    saved
        .iter()
        .enumerate()
        .filter(|(_, s)| s.enabled)
        .map(|(i, s)| {
            let d = found.iter().find(|d| d.config_dir == s.config_dir);
            let id = ProfileId::new(s.id.clone());
            let config_dir_env = match d {
                Some(d) => d.config_dir_env.clone(),
                None if id.is_default() => None,
                None => Some(s.config_dir.display().to_string()),
            };
            let account = d.and_then(|d| d.account.clone());
            let label = s
                .label
                .clone()
                .or_else(|| account.as_ref().and_then(email_label))
                .unwrap_or_else(|| dir_label(&s.config_dir));
            Profile {
                id,
                config_dir: s.config_dir.clone(),
                config_dir_env,
                label,
                color: s.color.unwrap_or(i as u8),
                keychain_override: s.keychain_service.clone(),
                account,
            }
        })
        .collect()
}

/// The index entry for a `CLAUDE_CONFIG_DIR` value: unset (or empty) means
/// the default profile; otherwise compared, after trimming trailing slashes
/// and expanding `~`, to each entry's config dir.
pub fn match_env<'a>(
    profiles: &'a [ProfileIndexEntry],
    env: Option<&str>,
) -> Option<&'a ProfileIndexEntry> {
    match_env_in(profiles, env, &crate::paths::home_dir())
}

/// [`match_env`] with `~` expanding to `home`.
fn match_env_in<'a>(
    profiles: &'a [ProfileIndexEntry],
    env: Option<&str>,
    home: &Path,
) -> Option<&'a ProfileIndexEntry> {
    let Some(env) = env.filter(|e| !e.is_empty()) else {
        return profiles.iter().find(|p| p.is_default);
    };
    let want = normalize(env, home);
    profiles.iter().find(|p| {
        normalize(&p.config_dir.to_string_lossy(), home) == want
            || p.config_dir_env
                .as_deref()
                .is_some_and(|e| normalize(e, home) == want)
    })
}

/// Trim trailing separators, expand a leading `~`, then canonicalize when
/// the path exists.
fn normalize(dir: &str, home: &Path) -> PathBuf {
    let trimmed = dir.trim_end_matches(['/', '\\']);
    let trimmed = if trimmed.is_empty() { dir } else { trimmed };
    let path = match trimmed.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with(['/', '\\']) => home.join(&rest[1..]),
        _ => PathBuf::from(trimmed),
    };
    fs::canonicalize(&path).unwrap_or(path)
}

/// A dir name as an id: no leading `.`, lowercase, anything outside
/// `[a-z0-9-]` as `-`.
fn slug(dir_name: &str) -> String {
    dir_name
        .trim_start_matches('.')
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `base`, or `base-2`, `base-3`, ... until `taken` says no.
fn unique_id(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(id))
        .expect("an unused suffix")
}

/// The email's local part, cut to [`MAX_LABEL_CHARS`] chars.
fn email_label(account: &AccountInfo) -> Option<String> {
    let email = account.email.as_deref()?;
    let local = email.split('@').next().unwrap_or(email);
    (!local.is_empty()).then(|| truncate_chars(local, MAX_LABEL_CHARS).to_owned())
}

/// The config dir's own name (`.claude-secondary`).
fn dir_label(config_dir: &Path) -> String {
    config_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| config_dir.display().to_string())
}

fn truncate_chars(s: &str, max: usize) -> &str {
    s.char_indices().nth(max).map_or(s, |(i, _)| &s[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(email: &str, uuid: &str) -> String {
        serde_json::json!({
            "numStartups": 3,
            "oauthAccount": {
                "emailAddress": email,
                "accountUuid": uuid,
                "organizationUuid": "org-1",
                "organizationName": "Org",
                "displayName": "Nate",
                "billingType": "stripe_subscription",
            }
        })
        .to_string()
    }

    /// A config dir `<home>/<name>`, with `.claude.json` contents if given.
    fn profile_dir(home: &Path, name: &str, claude_json: Option<&str>) -> PathBuf {
        let dir = home.join(name);
        fs::create_dir_all(&dir).unwrap();
        if let Some(text) = claude_json {
            fs::write(dir.join(".claude.json"), text).unwrap();
        }
        dir
    }

    fn saved(id: &str, dir: &Path) -> ProfileSettings {
        ProfileSettings {
            id: id.into(),
            config_dir: dir.to_path_buf(),
            ..ProfileSettings::default()
        }
    }

    fn found(id: &str, dir: &str, env: Option<&str>, email: Option<&str>) -> Discovered {
        Discovered {
            id: ProfileId::new(id),
            config_dir: PathBuf::from(dir),
            config_dir_env: env.map(str::to_owned),
            account: email.map(|e| AccountInfo {
                email: Some(e.into()),
                account_uuid: Some(format!("uuid-{e}")),
                ..AccountInfo::default()
            }),
        }
    }

    fn ids(found: &[Discovered]) -> Vec<&str> {
        found.iter().map(|d| d.id.as_str()).collect()
    }

    #[test]
    fn keychain_service_matches_claude_code() {
        assert_eq!(keychain_service_for(None), "Claude Code-credentials");
        assert_eq!(
            keychain_service_for(Some("/Users/nmyers/.claude-secondary")),
            "Claude Code-credentials-c32b2325"
        );
    }

    #[test]
    fn read_account_needs_oauth_account() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(".claude.json");
        assert_eq!(read_account(&file), None, "missing file");
        fs::write(&file, "not json").unwrap();
        assert_eq!(read_account(&file), None, "corrupt file");
        fs::write(&file, r#"{"numStartups":1}"#).unwrap();
        assert_eq!(read_account(&file), None, "signed out");

        fs::write(&file, identity("nate@example.com", "u-1")).unwrap();
        assert_eq!(
            read_account(&file),
            Some(AccountInfo {
                email: Some("nate@example.com".into()),
                account_uuid: Some("u-1".into()),
                organization_uuid: Some("org-1".into()),
                organization_name: Some("Org".into()),
                display_name: Some("Nate".into()),
                billing_type: Some("stripe_subscription".into()),
            })
        );
    }

    #[test]
    fn discover_finds_signed_in_dirs_default_first() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude.json"), identity("a@x.com", "u-a")).unwrap();
        profile_dir(home, ".claude-secondary", Some(&identity("b@x.com", "u-b")));
        profile_dir(home, ".claude-sync", None);
        profile_dir(home, ".claude-empty", Some(r#"{"numStartups":1}"#));
        fs::write(home.join(".claude-notadir"), identity("c@x.com", "u-c")).unwrap();

        let found = discover(home);
        assert_eq!(ids(&found), ["default", "claude-secondary"]);

        let default = &found[0];
        assert_eq!(default.config_dir, home.join(".claude"));
        assert_eq!(default.config_dir_env, None);
        assert_eq!(
            default.account.as_ref().unwrap().email.as_deref(),
            Some("a@x.com")
        );

        let second = &found[1];
        assert_eq!(second.config_dir, home.join(".claude-secondary"));
        assert_eq!(
            second.config_dir_env.as_deref(),
            Some(format!("{}/.claude-secondary", home.display()).as_str())
        );
        assert_eq!(
            second.account.as_ref().unwrap().account_uuid.as_deref(),
            Some("u-b")
        );
    }

    #[test]
    fn discover_default_without_account_and_missing_default() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        assert!(discover(home).is_empty());

        fs::create_dir_all(home.join(".claude")).unwrap();
        let found = discover(home);
        assert_eq!(ids(&found), ["default"]);
        assert_eq!(found[0].account, None, "signed out default still listed");
    }

    #[test]
    fn discover_dedupes_accounts_first_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude.json"), identity("a@x.com", "u-a")).unwrap();
        profile_dir(home, ".claude-b", Some(&identity("b@x.com", "u-b")));
        profile_dir(home, ".claude-c", Some(&identity("b@x.com", "u-b")));
        profile_dir(home, ".claude-d", Some(&identity("a@x.com", "u-a")));
        assert_eq!(ids(&discover(home)), ["default", "claude-b"]);
    }

    #[test]
    fn discover_suffixes_clashing_slugs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        profile_dir(home, ".claude-x.y", Some(&identity("a@x.com", "u-a")));
        profile_dir(home, ".claude-x_y", Some(&identity("b@x.com", "u-b")));
        profile_dir(home, ".claude-x-y", Some(&identity("c@x.com", "u-c")));
        // Sorted by name: "-" < "." < "_".
        assert_eq!(
            ids(&discover(home)),
            ["claude-x-y", "claude-x-y-2", "claude-x-y-3"]
        );
    }

    #[test]
    fn merge_appends_new_and_keeps_order() {
        let mut list = vec![
            saved("claude-b", Path::new("/h/.claude-b")),
            saved("default", Path::new("/h/.claude")),
        ];
        let discovered = [
            found("default", "/h/.claude", None, Some("a@x.com")),
            found(
                "claude-b",
                "/h/.claude-b",
                Some("/h/.claude-b"),
                Some("b@x.com"),
            ),
            found(
                "claude-c",
                "/h/.claude-c",
                Some("/h/.claude-c"),
                Some("c@x.com"),
            ),
        ];
        assert!(merge_discovered(&mut list, &discovered));
        let got: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(got, ["claude-b", "default", "claude-c"]);
        assert!(list[2].enabled && list[2].label.is_none());

        assert!(!merge_discovered(&mut list, &discovered), "nothing new");
    }

    #[test]
    fn merge_keeps_disabled_disabled() {
        let mut list = vec![ProfileSettings {
            enabled: false,
            ..saved("claude-b", Path::new("/h/.claude-b"))
        }];
        let discovered = [found(
            "claude-b",
            "/h/.claude-b",
            Some("/h/.claude-b"),
            Some("b@x.com"),
        )];
        assert!(!merge_discovered(&mut list, &discovered));
        assert!(!list[0].enabled);
    }

    #[test]
    fn merge_suffixes_an_id_a_manual_entry_holds() {
        let mut list = vec![saved("claude-b", Path::new("/elsewhere/b"))];
        let discovered = [found(
            "claude-b",
            "/h/.claude-b",
            Some("/h/.claude-b"),
            Some("b@x.com"),
        )];
        assert!(merge_discovered(&mut list, &discovered));
        assert_eq!(list[1].id, "claude-b-2");
    }

    #[test]
    fn resolve_enabled_in_saved_order() {
        let list = vec![
            saved("claude-c", Path::new("/h/.claude-c")),
            ProfileSettings {
                enabled: false,
                ..saved("claude-b", Path::new("/h/.claude-b"))
            },
            saved("default", Path::new("/h/.claude")),
            saved("manual", Path::new("/opt/claude-work")),
        ];
        let discovered = [
            found(
                "default",
                "/h/.claude",
                None,
                Some("averyveryverylongname@x.com"),
            ),
            found(
                "claude-b",
                "/h/.claude-b",
                Some("/h/.claude-b"),
                Some("b@x.com"),
            ),
            found(
                "claude-c",
                "/h/.claude-c",
                Some("/h/.claude-c"),
                Some("c@x.com"),
            ),
        ];
        let got = resolve(&list, &discovered);
        let ids: Vec<&str> = got.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["claude-c", "default", "manual"]);

        let c = &got[0];
        assert_eq!(c.label, "c");
        assert_eq!(c.color, 0);
        assert_eq!(c.config_dir_env.as_deref(), Some("/h/.claude-c"));
        assert_eq!(
            c.account.as_ref().unwrap().email.as_deref(),
            Some("c@x.com")
        );

        let d = &got[1];
        assert_eq!(d.label, "averyveryverylon", "16 chars of the local part");
        assert_eq!(d.color, 2, "color follows the saved position");
        assert_eq!(d.config_dir_env, None);

        let m = &got[2];
        assert_eq!(m.config_dir, Path::new("/opt/claude-work"));
        assert_eq!(m.config_dir_env.as_deref(), Some("/opt/claude-work"));
        assert_eq!(m.label, "claude-work", "no account: the dir name");
        assert_eq!(m.account, None);
    }

    #[test]
    fn resolve_prefers_saved_label_color_and_keychain() {
        let list = vec![ProfileSettings {
            label: Some("work".into()),
            color: Some(4),
            keychain_service: Some("custom-svc".into()),
            ..saved("claude-b", Path::new("/h/.claude-b"))
        }];
        let discovered = [found(
            "claude-b",
            "/h/.claude-b",
            Some("/h/.claude-b"),
            Some("b@x.com"),
        )];
        let p = &resolve(&list, &discovered)[0];
        assert_eq!((p.label.as_str(), p.color), ("work", 4));
        assert_eq!(p.keychain_service(), "custom-svc");
    }

    #[test]
    fn profile_paths_and_keychain_service() {
        let list = vec![
            saved("default", Path::new("/h/.claude")),
            saved("claude-b", Path::new("/h/.claude-b")),
        ];
        let discovered = [
            found("default", "/h/.claude", None, None),
            found(
                "claude-b",
                "/h/.claude-b",
                Some("/h/.claude-b"),
                Some("b@x.com"),
            ),
        ];
        let got = resolve(&list, &discovered);
        let (d, b) = (&got[0], &got[1]);
        assert_eq!(d.label, ".claude");
        assert_eq!(d.identity_file(), Path::new("/h/.claude.json"));
        assert_eq!(b.identity_file(), Path::new("/h/.claude-b/.claude.json"));
        assert_eq!(d.projects_dir(), Path::new("/h/.claude/projects"));
        assert_eq!(d.keychain_service(), "Claude Code-credentials");
        assert_eq!(
            b.keychain_service(),
            keychain_service_for(Some("/h/.claude-b"))
        );
    }

    #[test]
    fn locator_uses_keychain_service_and_credentials_file() {
        let list = vec![
            saved("default", Path::new("/h/.claude")),
            saved("claude-b", Path::new("/h/.claude-b")),
        ];
        let discovered = [found(
            "claude-b",
            "/h/.claude-b",
            Some("/h/.claude-b"),
            Some("b@x.com"),
        )];
        let got = resolve(&list, &discovered);
        assert_eq!(
            got[0].locator(),
            CredentialLocator {
                keychain_service: Some("Claude Code-credentials".into()),
                file: PathBuf::from("/h/.claude/.credentials.json"),
            }
        );
        assert_eq!(
            got[1].locator(),
            CredentialLocator {
                keychain_service: Some(keychain_service_for(Some("/h/.claude-b"))),
                file: PathBuf::from("/h/.claude-b/.credentials.json"),
            }
        );
    }

    #[test]
    fn default_profile_with_and_without_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let p = default_profile(home);
        assert!(p.id.is_default());
        assert_eq!(p.config_dir, home.join(".claude"));
        assert_eq!(p.config_dir_env, None);
        assert_eq!(p.account, None);

        fs::create_dir(home.join(".claude")).unwrap();
        fs::write(home.join(".claude.json"), identity("nate@x.com", "u-1")).unwrap();
        let p = default_profile(home);
        assert_eq!(p.label, "nate");
        assert_eq!(p.keychain_service(), "Claude Code-credentials");
    }

    #[test]
    fn short_label_cuts_on_char_boundaries() {
        let mut p = resolve(&[saved("default", Path::new("/h/.claude"))], &[])[0].clone();
        p.label = "ééééééééééééééééééé".into();
        assert_eq!(p.short_label().chars().count(), 16);
        p.label = "ok".into();
        assert_eq!(p.short_label(), "ok");
    }

    fn entry(id: &str, dir: &Path, env: Option<String>) -> ProfileIndexEntry {
        ProfileIndexEntry {
            id: ProfileId::new(id),
            label: id.into(),
            color: 0,
            config_dir: dir.to_path_buf(),
            config_dir_env: env,
            email: None,
            account_uuid: None,
            is_default: id == DEFAULT_ID,
        }
    }

    #[test]
    fn match_env_finds_the_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let default_dir = profile_dir(home, ".claude", None);
        let second = profile_dir(home, ".claude-secondary", None);
        let env = format!("{}/.claude-secondary", home.display());
        let index = [
            entry("default", &default_dir, None),
            entry("claude-secondary", &second, Some(env.clone())),
        ];
        let id = |e: Option<&str>| match_env_in(&index, e, home).map(|p| p.id.as_str());

        assert_eq!(id(None), Some("default"));
        assert_eq!(id(Some("")), Some("default"));
        assert_eq!(id(Some(&env)), Some("claude-secondary"));
        assert_eq!(id(Some(&format!("{env}/"))), Some("claude-secondary"));
        assert_eq!(id(Some("~/.claude-secondary")), Some("claude-secondary"));
        assert_eq!(id(Some("~/.claude-secondary/")), Some("claude-secondary"));
        assert_eq!(id(Some("~/.claude")), Some("default"));
        assert_eq!(id(Some("~/.claude-unknown")), None);
        assert_eq!(id(Some("/nowhere/at/all")), None);
    }

    #[test]
    fn index_entry_round_trips_json() {
        let e = entry(
            "claude-b",
            Path::new("/h/.claude-b"),
            Some("/h/.claude-b".into()),
        );
        let text = serde_json::to_string(&e).unwrap();
        assert!(text.contains(r#""id":"claude-b""#), "{text}");
        assert_eq!(serde_json::from_str::<ProfileIndexEntry>(&text).unwrap(), e);
    }
}
