# Run the Claude usage monitor GUI
start:
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
