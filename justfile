# Run the Claude usage TUI monitor
start:
    python claude_usage_tui.py --interval 30 --plan "Max 20x"

# Alias for `just start`
run: start

# Run the MCP server standalone (stdio) — for testing; normally Claude launches it
mcp:
    python mcp_server.py
