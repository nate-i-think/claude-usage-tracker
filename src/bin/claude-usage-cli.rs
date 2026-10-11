use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use clap::{Parser, Subcommand};

use claude_usage::config::{self, ClientShaSource, Settings};
use claude_usage::debug_report::{self as report, Probe};
use claude_usage::fetch::{self, FetchError};
use claude_usage::mcp::McpConfig;
use claude_usage::paths::Paths;
use claude_usage::poller::{self, BurnHistory, HttpSource, SessionDirs};
use claude_usage::{cookies, text_report};

#[derive(Parser)]
#[command(name = "claude-usage-cli", about = "Claude usage monitor CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch usage once, record it to history, and print it
    Once {
        /// Explicit path to a Firefox-family cookies.sqlite
        #[arg(long, value_name = "PATH")]
        cookie_file: Option<PathBuf>,
        /// Plan label shown in the title (e.g. "Max 5x")
        #[arg(long, value_name = "LABEL", default_value = "Max")]
        plan: String,
        /// Show every window, including 0% ones
        #[arg(long)]
        show_all: bool,
    },
    /// Diagnose cookie, client SHA, and endpoint problems
    Debug {
        /// Explicit path to a Firefox-family cookies.sqlite
        #[arg(long, value_name = "PATH")]
        cookie_file: Option<PathBuf>,
    },
    /// Run the MCP server over stdio
    Mcp,
}

fn main() -> ExitCode {
    claude_usage::install_crypto_provider();
    match Cli::parse().command {
        Command::Once {
            cookie_file,
            plan,
            show_all,
        } => once(cookie_file, plan, show_all),
        Command::Debug { cookie_file } => debug(cookie_file.as_deref()),
        Command::Mcp => mcp(),
    }
}

/// One poll through the shared poller path (appends to history.jsonl and
/// refreshes latest.json), printed as plain text. The client SHA comes from
/// the environment or client_sha.txt; the GUI's saved settings are not read.
fn once(cookie_file: Option<PathBuf>, plan: String, show_all: bool) -> ExitCode {
    let paths = Paths::platform_default();
    let settings = Settings {
        plan,
        show_all,
        cookie_file,
        ..Settings::default()
    };
    let mut history = BurnHistory::seeded(&paths, Utc::now());
    let result = poller::poll_once(
        &mut HttpSource::default(),
        &paths,
        &SessionDirs::default(),
        &settings,
        &mut history,
    );
    match result {
        Ok(d) => {
            print!(
                "{}",
                text_report::render(&d, &settings.plan, settings.show_all, Utc::now())
            );
            match d.persist_error {
                Some(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
                None => ExitCode::SUCCESS,
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            if e.stale_sha_hint() {
                eprintln!(
                    "hint: the anthropic-client-sha may be stale — refresh client_sha.txt or set {}",
                    config::CLIENT_SHA_ENV
                );
            }
            ExitCode::FAILURE
        }
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

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn section(title: &str) {
    println!("\n== {title} ==");
}

/// Port of debug.py: profiles, cookie inventory (masked), request setup, one
/// request, and a recommendation. Exit 0 only when the request returned
/// usage data.
fn debug(cookie_file: Option<&Path>) -> ExitCode {
    let now = unix_now();
    println!("claude-usage-cli — debug");

    let (jar, candidates) = match cookies::load_best(cookie_file, now) {
        Ok(found) => found,
        Err(e) => {
            println!("\ncookie load failed: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    section("Profiles considered (highest score wins, * = selected)");
    println!(
        "{}",
        report::profiles_table(&candidates, report::selected_index(&candidates), now)
    );

    section("Selected profile cookies");
    println!("{}", report::cookie_inventory(&jar, now));

    section("Request");
    let org_id = match jar.org_id() {
        Ok(id) => id,
        Err(e) => {
            println!("org detection failed: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    println!("Org ID:     {}", report::mask(Some(&org_id)));
    println!("URL:        {}", fetch::usage_url("<org id>"));
    let (sha, source) = match config::resolve_client_sha_with_source(None) {
        Ok(found) => found,
        Err(e) => {
            println!("client SHA: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let source = match source {
        ClientShaSource::Env => format!("env {}", config::CLIENT_SHA_ENV),
        ClientShaSource::Settings => "settings".to_owned(),
        ClientShaSource::File(p) => format!("file {}", p.display()),
    };
    println!("Client SHA: {} (from {source})", report::mask(Some(&sha)));

    let paths = Paths::platform_default();
    let headers = fetch::build_headers(&jar, &sha, &paths.get_or_create_anonymous_id());
    let names: Vec<&str> = headers.iter().map(|(n, _)| *n).collect();
    println!("Headers:    {}, Cookie", names.join(", "));
    println!("            (plus User-Agent, Accept, Accept-Language, sec-fetch-*)");

    section("Response");
    let probe = match send(&org_id, &headers, &jar.cookie_header(now)) {
        Ok(r) => {
            println!("Status:       {}", r.status);
            println!("Content-Type: {}", r.content_type.as_deref().unwrap_or("—"));
            let sniff = report::sniff(r.status, r.content_type.as_deref(), &r.body);
            println!("Body sniff:   {}", report::sniff_text(&sniff));
            Probe::Response {
                status: r.status,
                sniff,
            }
        }
        Err(msg) => {
            println!("request failed: {msg}");
            Probe::Transport(msg)
        }
    };

    section("Recommendation");
    for line in report::recommendation(&jar, &probe, now) {
        println!("{line}");
    }
    if probe.succeeded() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The one request; `Err` carries the transport (or setup) failure.
fn send(
    org_id: &str,
    headers: &[(&'static str, String)],
    cookie_header: &str,
) -> Result<fetch::RawResponse, String> {
    fetch::Client::new()
        .and_then(|c| c.request_usage(org_id, headers, cookie_header))
        .map_err(|e| match e {
            FetchError::Transport(msg) => msg,
            other => other.to_string(),
        })
}
