use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};

use claude_usage::config::Settings;
use claude_usage::mcp::McpConfig;
use claude_usage::paths::Paths;
use claude_usage::poller::{self, BurnHistory, FetchErrorInfo, HttpSource, SessionDirs};
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

/// One poll of the default Claude Code profile through the shared poller
/// path (appends to history.jsonl and refreshes latest.json), printed as
/// plain text. The GUI's saved settings are not read.
fn once(plan: String, show_all: bool) -> ExitCode {
    let paths = Paths::platform_default();
    let settings = Settings {
        plan,
        show_all,
        ..Settings::default()
    };
    let profile = poller::default_profile();
    let dirs = SessionDirs::for_profile(&profile);
    let mut history = BurnHistory::seeded(&paths, Utc::now());
    let result = poller::poll_once(
        &mut HttpSource::for_profile(&profile),
        &paths,
        &dirs,
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
            let info = FetchErrorInfo::for_dir(&e, dirs.config_dir());
            eprintln!("error: {}", info.message);
            ExitCode::FAILURE
        }
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
