use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use clap::{Parser, Subcommand};

use claude_usage::config::{self, Settings};
use claude_usage::credentials::{
    self, CredError, CredentialLocator, CredentialStore, OAuthCredential,
};
use claude_usage::debug_report::{self, CredentialState, Probe, ProfileDiag};
use claude_usage::mcp::McpConfig;
use claude_usage::paths::{self, Paths};
use claude_usage::poller::{
    self, AccountChange, FetchErrorInfo, OAuthSource, PollerDeps, UsageSource,
};
use claude_usage::profiles::{self, Profile};
use claude_usage::storage;
use claude_usage::text_report;

#[derive(Parser)]
#[command(name = "claude-usage-cli", about = "Claude usage monitor CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch usage once for every enabled profile, record it to history,
    /// and print it
    Once {
        /// Only this profile (id, label or config dir)
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
        /// Plan label shown in the title (e.g. "Max 5x"); defaults to the
        /// GUI's setting
        #[arg(long, value_name = "LABEL")]
        plan: Option<String>,
        /// Show every window, including 0% ones
        #[arg(long)]
        show_all: bool,
    },
    /// Diagnose Claude Code profile and credential problems
    Debug {
        /// Only this profile (id, label or config dir)
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },
    /// Run the MCP server over stdio
    Mcp,
}

/// Exit code for a bad command line (as clap uses).
const USAGE_ERROR: u8 = 2;

fn main() -> ExitCode {
    claude_usage::install_crypto_provider();
    match Cli::parse().command {
        Command::Once {
            profile,
            plan,
            show_all,
        } => once(profile.as_deref(), plan, show_all),
        Command::Debug { profile } => debug(profile.as_deref()),
        Command::Mcp => mcp(),
    }
}

/// The ids of `profiles` named by `query` (all of them without one); an
/// unknown name is reported with the valid ids.
fn select(profiles: &[Profile], query: Option<&str>) -> Result<Vec<usize>, ExitCode> {
    let Some(query) = query else {
        return Ok((0..profiles.len()).collect());
    };
    match profiles::find(profiles, query, &paths::home_dir()) {
        Some(p) => Ok(vec![profiles
            .iter()
            .position(|q| q.id == p.id)
            .expect("found among them")]),
        None => {
            let ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
            eprintln!(
                "error: no enabled profile {query:?}; valid: {}",
                ids.join(", ")
            );
            Err(ExitCode::from(USAGE_ERROR))
        }
    }
}

/// "── Work (claude-secondary, me@example.com) ──"; no email when unknown.
fn header(p: &Profile) -> String {
    match p.account.as_ref().and_then(|a| a.email.as_deref()) {
        Some(email) => format!("── {} ({}, {email}) ──", p.label, p.id),
        None => format!("── {} ({}) ──", p.label, p.id),
    }
}

/// One poll of every enabled Claude Code profile (or just `--profile`)
/// through the shared poller path (each appends to its own history.jsonl
/// and refreshes its latest.json), printed as plain text, one section per
/// profile. The GUI's saved settings apply (disabled profiles, labels,
/// Keychain overrides); an account change is left for the GUI to archive.
/// Succeeds only if every polled profile did.
fn once(query: Option<&str>, plan: Option<String>, show_all: bool) -> ExitCode {
    let saved = config::load_saved_settings();
    let plan = plan.unwrap_or_else(|| saved.plan.clone());
    let show_all = show_all || saved.show_all;
    let mut runtimes = poller::prepare_runtimes(
        PollerDeps::platform(),
        Paths::platform_default(),
        saved,
        AccountChange::Leave,
    );
    if runtimes.is_empty() {
        eprintln!("error: no Claude Code profile found — run `claude` and sign in");
        return ExitCode::FAILURE;
    }
    let all: Vec<Profile> = runtimes.iter().map(|rt| rt.profile.clone()).collect();
    let chosen = match select(&all, query) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut failed = false;
    for (n, &i) in chosen.iter().enumerate() {
        let rt = &mut runtimes[i];
        if n > 0 {
            println!();
        }
        println!("{}", header(&rt.profile));
        let cowork = poller::cowork_filter(&rt.profile, &all);
        match poller::poll_profile(rt, &cowork) {
            Ok(d) => {
                print!("{}", text_report::render(&d, &plan, show_all, Utc::now()));
                if let Some(e) = d.persist_error {
                    eprintln!("error: {e}");
                    failed = true;
                }
            }
            Err(e) => {
                let info = FetchErrorInfo::for_profile(&e, &rt.profile);
                // After this section's header, even when stdout is a pipe.
                let _ = io::stdout().flush();
                eprintln!("error: {}", info.message);
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// A store that answers its first load with a credential already read (so
/// `debug` reads each Keychain item once), then defers to `inner` (the
/// re-read after a 401).
struct Preloaded {
    first: Mutex<Option<Result<OAuthCredential, CredError>>>,
    inner: Arc<dyn CredentialStore>,
}

impl CredentialStore for Preloaded {
    fn load(&self, loc: &CredentialLocator) -> Result<OAuthCredential, CredError> {
        let first = self.first.lock().unwrap_or_else(|e| e.into_inner()).take();
        first.unwrap_or_else(|| self.inner.load(loc))
    }
}

/// `profile`'s diagnostics: its credential (read once, never shown), its
/// newest session and one probe fetch.
fn diagnose(profile: &Profile, store: &Arc<dyn CredentialStore>) -> ProfileDiag {
    let loaded = store.load(&profile.locator());
    let credential = match &loaded {
        Ok(c) => CredentialState::Present {
            expires_at: c.expires_at,
        },
        Err(CredError::NotFound(_)) => CredentialState::Missing,
        Err(CredError::Malformed(_)) => CredentialState::Error("malformed"),
        Err(CredError::Unavailable(_)) => CredentialState::Error("unavailable"),
    };
    let preloaded = Preloaded {
        first: Mutex::new(Some(loaded)),
        inner: Arc::clone(store),
    };
    let mut source = OAuthSource::new(Arc::new(preloaded));
    let probe = match source.fetch(profile) {
        Ok(_) => Probe::Ok,
        Err(e) => Probe::failed(&e, &profile.config_dir, &paths::home_dir()),
    };
    let account = profile.account.as_ref();
    ProfileDiag {
        id: profile.id.to_string(),
        config_dir: profile.config_dir.clone(),
        config_dir_env: profile.config_dir_env.clone(),
        keychain_service: profile.keychain_service(),
        credential,
        email: account.and_then(|a| a.email.clone()),
        account_uuid: account.and_then(|a| a.account_uuid.clone()),
        newest_session: storage::newest_jsonl(&profile.projects_dir()),
        probe,
    }
}

/// A table of every enabled profile (or just `--profile`): config dir,
/// Keychain service, credential, expiry, account, newest session and one
/// probe fetch; then what to do about each. Read-only: nothing is written
/// and no token is ever printed. Succeeds only if every probe did.
fn debug(query: Option<&str>) -> ExitCode {
    let settings: Settings = config::load_saved_settings();
    let home = paths::home_dir();
    let all = profiles::polled(&settings.profiles, &profiles::discover(&home));
    if all.is_empty() {
        eprintln!("error: no Claude Code profile found — run `claude` and sign in");
        return ExitCode::FAILURE;
    }
    let chosen = match select(&all, query) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let store = credentials::platform_store();
    let diags: Vec<ProfileDiag> = chosen.iter().map(|&i| diagnose(&all[i], &store)).collect();
    println!(
        "{}",
        debug_report::profiles_table(&diags, &home, Utc::now())
    );
    println!();
    println!("Recommendations:");
    println!("{}", debug_report::recommendations(&diags));
    if diags.iter().all(|d| d.probe == Probe::Ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Serve the MCP tools over stdio from latest.json (written by the GUI or
/// `once`). Only protocol messages go to stdout; errors go to stderr.
fn mcp() -> ExitCode {
    let paths = Paths::platform_default();
    match claude_usage::mcp::run_stdio(paths, McpConfig::from_env()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
