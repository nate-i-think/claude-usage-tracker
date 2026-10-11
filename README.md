# claude-usage-monitor

A small desktop widget that shows your `claude.ai` plan usage in real time. It polls the same `/api/organizations/{org}/usage` endpoint that powers the official settings page, so the numbers you see are the ones Anthropic uses to enforce your caps, across Claude Code, Cowork, web chat, and any other surface that draws from the same pool.

Written in Rust with [egui](https://github.com/emilk/egui). It ships two binaries: `claude-usage` (the GUI) and `claude-usage-cli` (a one-shot text report, a diagnostic dump, and an MCP server so Claude can check its own usage).

![Claude Usage Monitor](docs/screenshot.png)

## What it shows

- **Header**: plan label, plus buttons to refresh now, keep the window on top (📌), and open Settings.
- **Usage windows**: one row per window (5-hour, 7-day, and per-model caps such as Fable) with utilization, a bar color-coded by threshold (green < 50%, yellow ≥ 50%, orange ≥ 75%, red ≥ 90%), and time to reset. 5-hour, 7-day, and Fable show whenever the API reports them; other windows at 0% are hidden unless "Show all buckets" is on.
- **Burn rate**: a 15-minute rolling rate for all usage (ALL) and for Fable, with a "→ 100% in X" projection. The rate is available right after a restart because it is seeded from the saved history.
- **Extra usage**: credits used against your monthly limit, and why extra usage is disabled if it is.
- **Charts**: the current 5-hour window in 30-minute buckets and the last 7 days by weekday, with the Fable share drawn as an orange segment at the bottom of each bar. Side by side when the window is wide, stacked when it is narrow.
- **Top burners**: the projects using the most tokens in the current 5-hour window, attributed from local Claude Code (`~/.claude/projects/`) and Cowork (`%APPDATA%\Claude\local-agent-mode-sessions\`) session logs.
- **Projected use** and a "safe to spin up another agent" hint, plus how long ago the last poll was.
- **Errors**: a failed poll shows an error banner above the last good data, which stays visible. A `permission_error` from the API adds a hint that the client SHA may be stale, with a button to open Settings.

Window size and position, the theme, and your settings persist across restarts.

## Install

You need a Rust toolchain ([rustup](https://rustup.rs)). The project is built and tested with Rust 1.96 on Windows; other platforms should build but are untested.

```bash
git clone <this repo>
cd claude-usage-monitor
cargo build --release
```

The binaries land in `target/release/`: `claude-usage.exe` (GUI) and `claude-usage-cli.exe` (CLI). If you have [just](https://github.com/casey/just), the justfile wraps the common commands:

```bash
just start        # build (release) and run the GUI; `just run` is an alias
just cli once     # run the CLI with arguments: once | debug | mcp
just mcp          # run the MCP server on stdio, for testing
just test         # cargo test
just lint         # cargo clippy --all-targets -- -D warnings
```

`just start` runs from the repo root, so it picks up the repo's `client_sha.txt` (see [Refreshing the client SHA](#refreshing-the-client-sha)). If you copy `claude-usage.exe` somewhere else, or launch it from a shortcut whose "Start in" folder is not the repo root, copy `client_sha.txt` next to the exe or paste the SHA into Settings.

Before the first run, sign in (next section) and make sure a current client SHA is available (the section after).

## Signing in

Sign in to `claude.ai` in a Firefox-family browser: **Firefox, Zen, LibreWolf, Floorp, or Waterfox**. The app reads your session cookies straight from the browser's `cookies.sqlite` (through a temporary copy, so the browser can stay open) and picks the profile with the freshest `sessionKey`.

If your browser keeps its profile somewhere non-standard, point the app at the right `cookies.sqlite`: in the GUI, set **Cookie file** in Settings; on the CLI, pass `--cookie-file PATH`.

Chrome, Edge, Brave, and Safari are not supported (see [Changes from the Python version](#changes-from-the-python-version)).

## Refreshing the client SHA

Anthropic ships a new web-client build every couple of weeks, and the build hash (the `anthropic-client-sha` request header) rotates with it. When the hash goes stale, the API returns `permission_error` even with a valid session, and the GUI shows a "client SHA may be stale" hint.

To get the current value:

1. Open `claude.ai/settings/usage` in your browser.
2. Open DevTools (F12) → **Network** tab → filter by **Fetch/XHR**.
3. Reload the page.
4. Find the request to `/api/organizations/.../usage`.
5. In the **Headers** tab, copy the value of the `anthropic-client-sha` header.

Then put it in one of these places. The app uses the first one that has a value:

1. The `CLAUDE_USAGE_TUI_CLIENT_SHA` environment variable.
2. The **Client SHA** field in the GUI's Settings (GUI only; values shorter than 8 characters are ignored). Settings shows which source is in use.
3. `client_sha.txt` next to the executable.
4. `client_sha.txt` in the current working directory.

The repo ships a `client_sha.txt` at its root. A binary in `target/release/` finds it only through rule 4, that is, when you launch it from the repo root (as `just start` and `cargo run` do). If you run the binary from anywhere else, copy `client_sha.txt` next to the `.exe`, paste the SHA into Settings, or set the environment variable.

## CLI

```bash
claude-usage-cli once                      # fetch once, record it, print a text report
claude-usage-cli once --plan "Max 5x"      # plan label in the title (default "Max")
claude-usage-cli once --show-all           # include 0% windows
claude-usage-cli once --cookie-file PATH   # explicit cookies.sqlite
claude-usage-cli debug                     # diagnose cookies, client SHA, and the endpoint
claude-usage-cli debug --cookie-file PATH
claude-usage-cli mcp                       # MCP server on stdio (see below)
```

`once` goes through the same code path as the GUI's poller: it appends to `history.jsonl` and rewrites `latest.json`, so it also refreshes the data the MCP server reports. It exits non-zero on failure.

The CLI does **not** read the GUI's saved settings. It takes the cookie file from `--cookie-file` (or auto-detects it) and the client SHA from the environment variable or a `client_sha.txt` (rules 1, 3, and 4 above).

The poll interval is a GUI setting (Settings → Poll interval, default 60 s, minimum 5 s). It accepts plain seconds or a suffix: `30`, `30s`, `2m`, `1h`.

## MCP server (let Claude check its own usage)

`claude-usage-cli mcp` is an [MCP](https://modelcontextprotocol.io) server on stdio that lets a Claude instance (Claude Code, Claude Desktop, …) read your current usage and throttle itself when you're near a cap. It does **no** network or auth of its own. It reports what the stats were at the most recent poll, read from `latest.json`, so **the GUI must be running** (or something must run `claude-usage-cli once` regularly) to keep the data fresh.

Tools:

- **`get_usage()`**: non-blocking. Returns each window's utilization, reset time, and time to reset, plus the data's age, a `stale` flag (true if the monitor isn't updating), the extra-usage block, and a `recommendation`/`summary`.
- **`wait(seconds, confirm)`**: throttle by sleeping. It blocks for at most ~50 s per call, then returns how much time remains until the 5-hour reset and whether to call again, so a long wait is done as several short cycles. Call with `confirm=false` first to preview (nothing sleeps); `confirm=true` waits one cycle. Loop until `done` is true.
- **`wait_test(seconds)`**: diagnostic only. Blocks for the full requested duration (up to a 600 s ceiling) and reports how long it actually slept, so you can find your client's real tool-call timeout. If it returns, the limit is at least that long; if the client kills it first, you've found the ceiling. Then set `CLAUDE_USAGE_MCP_MAX_BLOCK` just under it.

Why the short cycles: most MCP clients inherit a ~60 s request timeout (Claude Desktop hard-codes it and silently drops later results), so a single multi-minute block would be killed. Bounded cycling is timeout-proof and never traps the agent.

### Registering it

**Claude Code (recommended).** Register the binary by absolute path so it works from any directory:

```bash
claude mcp add --scope user claude-usage -- "C:\absolute\path\to\claude-usage-monitor\target\release\claude-usage-cli.exe" mcp
```

**Claude Code, project scope.** The repo ships a `.mcp.json` that runs `target/release/claude-usage-cli mcp`. The path is relative, so it works only after `cargo build --release` and only when Claude Code runs from the repo root; started from a subdirectory, the server fails to launch. Approve the `claude-usage` server when asked and check it with `/mcp`.

**Claude Desktop.** Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "claude-usage": {
      "command": "C:\\absolute\\path\\to\\claude-usage-monitor\\target\\release\\claude-usage-cli.exe",
      "args": ["mcp"]
    }
  }
}
```

### Tuning

Set these in the `env` block of your MCP server config:

| Variable | Default | Meaning |
|---|---|---|
| `CLAUDE_USAGE_MCP_MAX_BLOCK` | 50 | Max seconds per `wait` cycle. Keep it under your client's tool timeout. |
| `CLAUDE_USAGE_MCP_STALE_AFTER` | 180 | Seconds before a snapshot is flagged stale. |
| `CLAUDE_USAGE_MCP_TEST_MAX_BLOCK` | 600 | Hard ceiling for `wait_test`. |

A value that isn't a whole number does not stop the server: it falls back to the default and prints a note to stderr. The two block limits are raised to at least 1.

## Recommended layout

Snap the window to a corner of your screen and turn on **Always on top** (📌). Pair it with [`claude-monitor`](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor) for a complete picture: `claude-monitor` shows local per-instance attribution from Claude Code logs; this widget shows the unified server-side meter that's actually about to gate you.

## When it breaks

Run `claude-usage-cli debug` first. It lists every browser profile it found with its `sessionKey` freshness, the cookies in the selected profile with their expiry status (values masked), where the client SHA came from, the response to one request with a sniff of the body, and a recommendation. You can probably have Claude Code fix things if you feed it this output.

Common failure modes:

- **`permission_error` / "Invalid authorization"**: most likely your `sessionKey` is stale (sign out of `claude.ai` and back in) or the wrong browser profile was picked (check the profiles table, or set the cookie file explicitly). Less commonly the client SHA has gone stale; refresh it.
- **403 with an HTML body**: Cloudflare is blocking the request. Visit `claude.ai/settings/usage` in your browser to refresh the `__cf_bm` cookie, then retry.
- **"No browser profile found"**: sign in to `claude.ai` in a supported Firefox-family browser, or set the cookie file explicitly.
- **`__cf_bm` missing or expired**: visit `claude.ai/settings/usage` in your browser within 30 minutes before polling.
- **"poller stopped — restart the app"** in the GUI: the background poller exited unexpectedly. Restart the app.

## Privacy & security

The app reads session cookies from your browser's local cookie store. They include your `sessionKey`, which is effectively a password for your `claude.ai` account. The app never writes the cookies anywhere except a temporary copy of the cookie database (deleted after reading), never logs them, and never sends them anywhere except `claude.ai`. `debug` masks cookie values.

A small amount of state is kept locally in the app's data directory:

- `history.jsonl`: usage snapshots (percentages and timestamps; no conversation content). Used for the charts and the burn rate.
- `latest.json`: the most recent full snapshot (every window's utilization and reset time, and the extra-usage block). Read by the MCP server.
- `anonymous_id`: a random UUID generated per install, sent as a telemetry header to `claude.ai`, as the web client does. Nothing ties one install to another.

The data directory is:

- Windows: `%APPDATA%\claude_usage_tui\`
- macOS: `~/Library/Application Support/claude_usage_tui/`
- Linux: `$XDG_DATA_HOME/claude_usage_tui/` (default `~/.local/share/claude_usage_tui/`)

This is the same directory the Python version used with python.org (non-Store) Python, so that history carries over as is. If you ran the Python version with Microsoft Store Python, see [Migrating history from the Python version](#migrating-history-from-the-python-version).

The GUI's settings (plan label, poll interval, show-all, always-on-top, cookie file path, client SHA) and its window geometry and theme are stored by eframe in `app.ron`:

- Windows: `%APPDATA%\claude-usage\data\app.ron`
- macOS: `~/Library/Application Support/claude-usage/app.ron`
- Linux: `$XDG_DATA_HOME/claude-usage/app.ron` (default `~/.local/share/claude-usage/app.ron`)

## Changes from the Python version

This project used to be a Python terminal dashboard (`claude_usage_tui.py`). The Rust port keeps the same endpoint, numbers, data files, and MCP tools, and drops:

- **Chromium and Safari cookie support.** Only Firefox-family browsers are read now.
- **The TLS-impersonation profile sweep in debug.** The Python version used `curl_cffi` to mimic browser TLS fingerprints, and `--debug` retried with several profiles. The Rust app uses plain rustls with Firefox-like headers, which Cloudflare currently accepts, and `debug` sends one request.
- **`--no-fullscreen`**, which only made sense for the terminal UI.

Other differences: `--once` and `--debug` became the `once` and `debug` subcommands of `claude-usage-cli`; the poll interval, plan label, cookie file, and client SHA are set in the GUI's Settings instead of on the command line; and the 5-hour chart labels the bucket that contains each hour rather than only buckets starting exactly on the hour.

### Migrating history from the Python version

With python.org Python, the Python version wrote `history.jsonl` to `%APPDATA%\claude_usage_tui\`, the directory the Rust app uses, so there is nothing to do.

Microsoft Store Python virtualizes `%APPDATA%`: the Python version's files actually landed in

```
%LOCALAPPDATA%\Packages\PythonSoftwareFoundation.Python.<version>_<id>\LocalCache\Roaming\claude_usage_tui\
```

and the Rust app does not see them. To keep your chart history, merge that `history.jsonl` into `%APPDATA%\claude_usage_tui\history.jsonl`. Both versions use the same one-JSON-object-per-line schema. Close the GUI and any Python monitor first, then run in PowerShell:

```powershell
# The Store Python file (if you have several Python versions installed, set $old by hand).
$old = Get-ChildItem "$env:LOCALAPPDATA\Packages\PythonSoftwareFoundation.Python.*\LocalCache\Roaming\claude_usage_tui\history.jsonl" |
    Select-Object -First 1 -ExpandProperty FullName
$new = "$env:APPDATA\claude_usage_tui\history.jsonl"

New-Item -ItemType Directory -Force (Split-Path $new) | Out-Null
if (Test-Path $new) { Copy-Item $new "$new.bak" }   # backup of the destination

# Old rows plus new rows, blank lines dropped, sorted by timestamp, written as UTF-8 without BOM.
$lines = (@(Get-Content $old) + @(if (Test-Path $new) { Get-Content $new })) | Where-Object { $_.Trim() }
$sorted = $lines | Sort-Object { [double]([regex]::Match($_, '"ts":\s*([-0-9.eE+]+)').Groups[1].Value) }
[IO.File]::WriteAllLines($new, [string[]]$sorted, (New-Object Text.UTF8Encoding $false))
```

It leaves the Store Python file untouched, and `history.jsonl.bak` holds the previous destination if you need to undo. Run it once: running it again duplicates the old rows.

## Caveats

This app polls an undocumented internal endpoint. Anthropic could change it at any time, and the app would stop working until updated. If Cloudflare starts rejecting non-browser TLS fingerprints, the plain rustls client may need replacing with one that impersonates a browser.

This is a personal tool, not an Anthropic-supported integration. Use accordingly.

## License

MIT. See [LICENSE](LICENSE).
