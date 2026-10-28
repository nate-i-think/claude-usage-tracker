use std::io::{self, Write};
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};

use claude_usage::config::Settings;
use claude_usage::mcp::McpConfig;
use claude_usage::paths::Paths;
use claude_usage::poller::{self, FetchErrorInfo, PollerDeps};
use claude_usage::profiles::Profile;
use claude_usage::text_report;

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
        /// Plan label shown in the title (e.g. "Max 5x")
        #[arg(long, value_name = "LABEL", default_value = "Max")]
        plan: String,
        /// Show every window, including 0% ones
        #[arg(long)]
        show_all: bool,
    },
    /// Diagnose Claude Code profile and credential problems
    Debug,
    /// Run the MCP server over stdio
    Mcp,
}

fn main() -> ExitCode {
    claude_usage::install_crypto_provider();
    match Cli::parse().command {
        Command::Once { plan, show_all } => once(plan, show_all),
        Command::Debug => debug(),
        Command::Mcp => mcp(),
    }
}

/// One poll of every enabled Claude Code profile through the shared poller
/// path (each appends to its own history.jsonl and refreshes its
/// latest.json), printed as plain text, one section per profile. The GUI's
/// saved settings are not read: every discovered profile is polled.
fn once(plan: String, show_all: bool) -> ExitCode {
    let settings = Settings {
        plan,
        show_all,
        ..Settings::default()
    };
    let mut runtimes = poller::prepare_runtimes(
        PollerDeps::platform(),
        Paths::platform_default(),
        settings.clone(),
    );
    if runtimes.is_empty() {
        eprintln!("error: no Claude Code profile found — run `claude` and sign in");
        return ExitCode::FAILURE;
    }
    let all: Vec<Profile> = runtimes.iter().map(|rt| rt.profile.clone()).collect();
    let mut failed = false;
    for (i, rt) in runtimes.iter_mut().enumerate() {
        if i > 0 {
            println!();
        }
        println!("── {} ({}) ──", rt.profile.label, rt.profile.id);
        let cowork = poller::cowork_filter(&rt.profile, &all);
        match poller::poll_profile(rt, &cowork) {
            Ok(d) => {
                print!(
                    "{}",
                    text_report::render(&d, &settings.plan, settings.show_all, Utc::now())
                );
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

/// Placeholder until per-profile diagnostics land.
fn debug() -> ExitCode {
    println!("profile diagnostics: coming in a later task");
    ExitCode::SUCCESS
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
