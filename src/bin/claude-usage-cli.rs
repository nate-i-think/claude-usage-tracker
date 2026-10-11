use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "claude-usage-cli", about = "Claude usage monitor CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch usage once and print it
    Once,
    /// Print debug information
    Debug,
    /// Run the MCP server over stdio
    Mcp,
}

fn main() {
    claude_usage::install_crypto_provider();
    let _cli = Cli::parse();
    eprintln!("not implemented yet");
    std::process::exit(1);
}
