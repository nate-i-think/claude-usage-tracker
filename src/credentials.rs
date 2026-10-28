//! Claude Code OAuth credentials, read-only: the macOS Keychain (via
//! `/usr/bin/security`) or `<config_dir>/.credentials.json`.
//!
//! Never refreshes, rotates or writes a credential. The access token stays
//! private to [`OAuthCredential`] and never reaches a `Debug`, `Display` or
//! error message; the refresh token is not kept at all.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// The `security` CLI that reads the login Keychain.
const SECURITY: &str = "/usr/bin/security";

/// `security find-generic-password`'s exit code for "item not found".
const SECURITY_NOT_FOUND: i32 = 44;

/// Where one profile's credential lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialLocator {
    /// Keychain generic-password service; `None` skips the Keychain.
    pub keychain_service: Option<String>,
    /// `<config_dir>/.credentials.json`.
    pub file: PathBuf,
}

/// The `claudeAiOauth` object of a credential. `Debug` redacts the token.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthCredential {
    access_token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub subscription_type: Option<String>,
    pub scopes: Vec<String>,
}

impl OAuthCredential {
    /// The access token, for the `Authorization: Bearer` header only.
    pub fn bearer(&self) -> &str {
        &self.access_token
    }

    /// Expired at or before `now`; no `expiresAt` is never expired.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }

    /// A hash of the access token, to tell whether a re-read changed it.
    /// Not a secret-safe digest: never log or persist it.
    pub fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.access_token.hash(&mut h);
        h.finish()
    }
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("subscription_type", &self.subscription_type)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Why a credential couldn't be loaded. Messages never carry the token.
#[derive(Debug, thiserror::Error)]
pub enum CredError {
    #[error("no credential: {0}")]
    NotFound(String),
    #[error("malformed credential: {0}")]
    Malformed(String),
    #[error("credential store unavailable: {0}")]
    Unavailable(String),
}

/// What to do when the credential store can't be read
/// ([`CredError::Unavailable`]).
pub const UNAVAILABLE_HINT: &str = if cfg!(target_os = "macos") {
    "unlock the login keychain, or allow /usr/bin/security access when macOS asks"
} else {
    "check the credentials file's permissions"
};

/// A source of credentials.
pub trait CredentialStore: Send + Sync {
    fn load(&self, loc: &CredentialLocator) -> Result<OAuthCredential, CredError>;
}

/// Parse a credential JSON (`{"claudeAiOauth":{"accessToken",...}}`).
///
/// Errors name the JSON error's category and position, never its text:
/// serde quotes the input, and the input is the secret.
pub fn parse_credentials_json(s: &str) -> Result<OAuthCredential, CredError> {
    let v: Value = serde_json::from_str(s).map_err(|e| {
        CredError::Malformed(format!(
            "{:?} JSON error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    let oauth = v
        .get("claudeAiOauth")
        .filter(|o| o.is_object())
        .ok_or_else(|| CredError::Malformed("no claudeAiOauth object".into()))?;
    let access_token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| CredError::Malformed("no accessToken".into()))?
        .to_string();
    // Unix milliseconds; anything else reads as "no expiry".
    let expires_at = oauth
        .get("expiresAt")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .and_then(DateTime::from_timestamp_millis);
    let subscription_type = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(str::to_string);
    let scopes = oauth
        .get("scopes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(OAuthCredential {
        access_token,
        expires_at,
        subscription_type,
        scopes,
    })
}

/// Runs a program and returns its exit code and stdout; stderr is dropped.
pub trait CommandRunner: Send + Sync {
    fn run(&self, prog: &str, args: &[&str]) -> io::Result<(Option<i32>, Vec<u8>)>;
}

/// [`CommandRunner`] over [`std::process::Command`].
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, prog: &str, args: &[&str]) -> io::Result<(Option<i32>, Vec<u8>)> {
        let out = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        Ok((out.status.code(), out.stdout))
    }
}

/// The macOS Keychain via `security find-generic-password -s <svc> -w`.
pub struct KeychainCli<R: CommandRunner = SystemRunner>(pub R);

impl<R: CommandRunner> CredentialStore for KeychainCli<R> {
    fn load(&self, loc: &CredentialLocator) -> Result<OAuthCredential, CredError> {
        let Some(service) = loc.keychain_service.as_deref() else {
            return Err(CredError::NotFound("no Keychain service".into()));
        };
        let args = ["find-generic-password", "-s", service, "-w"];
        // stdout is the secret: it goes to the parser and nowhere else.
        match self.0.run(SECURITY, &args) {
            Ok((Some(0), stdout)) => {
                let text = String::from_utf8(stdout).map_err(|_| {
                    CredError::Malformed(format!("Keychain item {service:?} is not UTF-8"))
                })?;
                parse_credentials_json(text.trim())
            }
            Ok((Some(SECURITY_NOT_FOUND), _)) => {
                Err(CredError::NotFound(format!("no Keychain item {service:?}")))
            }
            Ok((Some(code), _)) => Err(CredError::Unavailable(format!(
                "security exited {code} reading {service:?}"
            ))),
            Ok((None, _)) => Err(CredError::Unavailable(format!(
                "security killed reading {service:?}"
            ))),
            Err(e) => Err(CredError::Unavailable(format!("can't run {SECURITY}: {e}"))),
        }
    }
}

/// `loc.file`, the `.credentials.json` Claude Code writes off macOS.
pub struct FileStore;

impl CredentialStore for FileStore {
    fn load(&self, loc: &CredentialLocator) -> Result<OAuthCredential, CredError> {
        let path = loc.file.display();
        match fs::read_to_string(&loc.file) {
            Ok(text) => parse_credentials_json(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(CredError::NotFound(format!("no {path}")))
            }
            Err(e) => Err(CredError::Unavailable(format!("can't read {path}: {e}"))),
        }
    }
}

/// Stores tried in order: the first result that isn't `NotFound` wins.
pub struct Chain(pub Vec<Box<dyn CredentialStore>>);

impl CredentialStore for Chain {
    fn load(&self, loc: &CredentialLocator) -> Result<OAuthCredential, CredError> {
        let mut missing = Vec::new();
        for store in &self.0 {
            match store.load(loc) {
                Err(CredError::NotFound(why)) => missing.push(why),
                other => return other,
            }
        }
        if missing.is_empty() {
            missing.push("no credential stores".into());
        }
        Err(CredError::NotFound(missing.join("; ")))
    }
}

/// This platform's store: the Keychain, then the file, on macOS; else the file.
pub fn platform_store() -> Arc<dyn CredentialStore> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(Chain(vec![
            Box::new(KeychainCli(SystemRunner)),
            Box::new(FileStore),
        ]))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(FileStore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const TOKEN: &str = "sk-ant-oat01-FIXTURE-SECRET";
    const REFRESH: &str = "sk-ant-ort01-FIXTURE-REFRESH";

    fn fixture() -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": TOKEN,
                "refreshToken": REFRESH,
                "expiresAt": 1_791_000_000_250_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_20x",
            }
        })
        .to_string()
    }

    fn cred(expires_at: Option<DateTime<Utc>>) -> OAuthCredential {
        OAuthCredential {
            access_token: TOKEN.into(),
            expires_at,
            subscription_type: None,
            scopes: Vec::new(),
        }
    }

    fn loc(service: Option<&str>, file: PathBuf) -> CredentialLocator {
        CredentialLocator {
            keychain_service: service.map(str::to_string),
            file,
        }
    }

    fn assert_no_secret(text: &str) {
        assert!(!text.contains(TOKEN), "token leaked: {text}");
        assert!(!text.contains("FIXTURE"), "secret leaked: {text}");
    }

    #[test]
    fn parses_the_fixture() {
        let c = parse_credentials_json(&fixture()).unwrap();
        assert_eq!(c.bearer(), TOKEN);
        assert_eq!(
            c.expires_at,
            DateTime::from_timestamp_millis(1_791_000_000_250)
        );
        assert_eq!(c.subscription_type.as_deref(), Some("max"));
        assert_eq!(c.scopes, ["user:inference", "user:profile"]);
    }

    #[test]
    fn optional_fields_may_be_missing() {
        let c = parse_credentials_json(r#"{"claudeAiOauth":{"accessToken":"t"}}"#).unwrap();
        assert_eq!(c.bearer(), "t");
        assert_eq!(c.expires_at, None);
        assert_eq!(c.subscription_type, None);
        assert!(c.scopes.is_empty());
        assert!(!c.is_expired(Utc::now()));
    }

    #[test]
    fn garbage_and_missing_fields_are_malformed() {
        for s in [
            "",
            "not json",
            TOKEN,
            "[]",
            r#"{"other":{}}"#,
            r#"{"claudeAiOauth":{}}"#,
            r#"{"claudeAiOauth":{"accessToken":""}}"#,
            r#"{"claudeAiOauth":{"accessToken":42}}"#,
            r#"{"claudeAiOauth":{"refreshToken":"sk-ant-ort01-FIXTURE-REFRESH"}}"#,
        ] {
            match parse_credentials_json(s) {
                Err(e @ CredError::Malformed(_)) => assert_no_secret(&e.to_string()),
                other => panic!("{s:?} → {other:?}"),
            }
        }
    }

    #[test]
    fn malformed_messages_never_echo_the_input() {
        // A syntax error just after the token: serde would quote the input.
        let s = format!(r#"{{"claudeAiOauth":{{"accessToken":"{TOKEN}" oops}}}}"#);
        let e = parse_credentials_json(&s).unwrap_err();
        assert_no_secret(&e.to_string());
        assert_no_secret(&format!("{e:?}"));
        // The token where a number belongs.
        let s = format!(r#"{{"claudeAiOauth":{{"accessToken":"x","expiresAt":"{TOKEN}"}}}}"#);
        if let Err(e) = parse_credentials_json(&s) {
            assert_no_secret(&format!("{e} {e:?}"));
        }
    }

    #[test]
    fn debug_redacts_the_token() {
        let c = parse_credentials_json(&fixture()).unwrap();
        assert_no_secret(&format!("{c:?}"));
        assert_no_secret(&format!("{c:#?}"));
    }

    #[test]
    fn errors_never_contain_the_token() {
        let errors = [
            CredError::NotFound("Claude Code-credentials".into()),
            CredError::Malformed("bad".into()),
            CredError::Unavailable("security exited 1".into()),
        ];
        for e in &errors {
            assert_no_secret(&format!("{e} {e:?}"));
        }
        // And the ones the stores build, fed the token on stdout.
        for code in [Some(1), Some(SECURITY_NOT_FOUND), None] {
            let runner = FakeRunner::new(Ok((code, TOKEN.as_bytes().to_vec())));
            let e = KeychainCli(runner)
                .load(&loc(Some("svc"), PathBuf::new()))
                .unwrap_err();
            assert_no_secret(&format!("{e} {e:?}"));
        }
        let runner = FakeRunner::new(Ok((Some(0), TOKEN.as_bytes().to_vec())));
        let e = KeychainCli(runner)
            .load(&loc(Some("svc"), PathBuf::new()))
            .unwrap_err();
        assert!(matches!(e, CredError::Malformed(_)));
        assert_no_secret(&format!("{e} {e:?}"));
    }

    #[test]
    fn is_expired_at_the_boundary() {
        let t = DateTime::from_timestamp(1_791_000_000, 0).unwrap();
        let s = chrono::Duration::seconds(1);
        assert!(cred(Some(t)).is_expired(t));
        assert!(cred(Some(t)).is_expired(t + s));
        assert!(!cred(Some(t)).is_expired(t - s));
        assert!(!cred(None).is_expired(t));
    }

    #[test]
    fn fingerprint_follows_the_token() {
        let a = cred(None);
        let mut b = cred(Some(Utc::now()));
        assert_eq!(a.fingerprint(), b.fingerprint());
        b.access_token = "other".into();
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    type RunResult = io::Result<(Option<i32>, Vec<u8>)>;

    /// Records each call and answers with a canned result.
    struct FakeRunner {
        result: Mutex<Option<RunResult>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn new(result: io::Result<(Option<i32>, Vec<u8>)>) -> Self {
            Self {
                result: Mutex::new(Some(result)),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, prog: &str, args: &[&str]) -> io::Result<(Option<i32>, Vec<u8>)> {
            let args = args.iter().map(|a| a.to_string()).collect();
            self.calls.lock().unwrap().push((prog.to_string(), args));
            self.result.lock().unwrap().take().expect("run called once")
        }
    }

    #[test]
    fn keychain_reads_trimmed_stdout() {
        let out = format!("  {}\n", fixture());
        let kc = KeychainCli(FakeRunner::new(Ok((Some(0), out.into_bytes()))));
        let c = kc
            .load(&loc(
                Some("Claude Code-credentials-c32b2325"),
                PathBuf::new(),
            ))
            .unwrap();
        assert_eq!(c.bearer(), TOKEN);
        let args = [
            "find-generic-password",
            "-s",
            "Claude Code-credentials-c32b2325",
            "-w",
        ];
        assert_eq!(
            kc.0.calls(),
            [(
                "/usr/bin/security".to_string(),
                args.map(String::from).to_vec()
            )]
        );
    }

    #[test]
    fn keychain_exit_codes() {
        let l = loc(Some("svc"), PathBuf::new());
        let kc = KeychainCli(FakeRunner::new(Ok((Some(44), Vec::new()))));
        assert!(matches!(kc.load(&l), Err(CredError::NotFound(_))));
        let kc = KeychainCli(FakeRunner::new(Ok((Some(1), Vec::new()))));
        assert!(matches!(kc.load(&l), Err(CredError::Unavailable(_))));
        // Killed by a signal: no exit code.
        let kc = KeychainCli(FakeRunner::new(Ok((None, Vec::new()))));
        assert!(matches!(kc.load(&l), Err(CredError::Unavailable(_))));
        let err = io::Error::new(io::ErrorKind::NotFound, "no security");
        let kc = KeychainCli(FakeRunner::new(Err(err)));
        assert!(matches!(kc.load(&l), Err(CredError::Unavailable(_))));
    }

    #[test]
    fn keychain_without_a_service_is_not_found() {
        let kc = KeychainCli(FakeRunner::new(Ok((Some(0), fixture().into_bytes()))));
        assert!(matches!(
            kc.load(&loc(None, PathBuf::new())),
            Err(CredError::NotFound(_))
        ));
        assert!(kc.0.calls().is_empty());
    }

    #[test]
    fn file_store_present_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".credentials.json");
        let l = loc(Some("ignored"), file.clone());
        assert!(matches!(FileStore.load(&l), Err(CredError::NotFound(_))));
        fs::write(&file, fixture()).unwrap();
        assert_eq!(FileStore.load(&l).unwrap().bearer(), TOKEN);
        fs::write(&file, "{").unwrap();
        assert!(matches!(FileStore.load(&l), Err(CredError::Malformed(_))));
    }

    /// A store that always answers the same way.
    struct Fixed(fn() -> Result<OAuthCredential, CredError>);

    impl CredentialStore for Fixed {
        fn load(&self, _loc: &CredentialLocator) -> Result<OAuthCredential, CredError> {
            (self.0)()
        }
    }

    fn not_found() -> Result<OAuthCredential, CredError> {
        Err(CredError::NotFound("a".into()))
    }
    fn malformed() -> Result<OAuthCredential, CredError> {
        Err(CredError::Malformed("b".into()))
    }
    fn found() -> Result<OAuthCredential, CredError> {
        Ok(cred(None))
    }

    #[test]
    fn chain_falls_through_not_found_only() {
        let l = loc(None, PathBuf::new());
        let chain = |stores: Vec<fn() -> Result<OAuthCredential, CredError>>| {
            Chain(
                stores
                    .into_iter()
                    .map(|f| Box::new(Fixed(f)) as Box<_>)
                    .collect(),
            )
        };
        assert!(chain(vec![not_found, found]).load(&l).is_ok());
        assert!(matches!(
            chain(vec![malformed, found]).load(&l),
            Err(CredError::Malformed(_))
        ));
        assert!(chain(vec![found, malformed]).load(&l).is_ok());
        assert!(matches!(
            chain(vec![not_found, not_found]).load(&l),
            Err(CredError::NotFound(_))
        ));
        assert!(matches!(
            chain(vec![]).load(&l),
            Err(CredError::NotFound(_))
        ));
    }
}
