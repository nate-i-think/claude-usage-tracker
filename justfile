# Build, then launch the GUI detached from the terminal; output goes to logs/
start:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --bin claude-usage
    mkdir -p logs
    log="logs/claude-usage-$(date +%Y%m%d-%H%M%S).log"
    nohup target/release/claude-usage > "$log" 2>&1 < /dev/null &
    disown
    echo "claude-usage started (pid $!), logging to $log"

# Run the GUI in the foreground, output to this terminal
start-fg:
    cargo run --release --bin claude-usage

# Alias for `just start`
run: start

# Run the CLI (once | debug | mcp)
cli *ARGS:
    cargo run --release --bin claude-usage-cli -- {{ARGS}}

# Run the MCP server standalone (stdio) — for testing; normally Claude launches it
mcp:
    cargo run --release --bin claude-usage-cli -- mcp

# Run the test suite
test:
    cargo test

# Lint with clippy, warnings are errors
lint:
    cargo clippy --all-targets -- -D warnings
