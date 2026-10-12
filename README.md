# claude-usage-monitor

A small desktop widget that shows your `claude.ai` plan usage in real time. It polls the same `/api/organizations/{org}/usage` endpoint that powers the official settings page, so the numbers you see are the ones Anthropic uses to enforce your caps, across Claude Code, Cowork, web chat, and any other surface that draws from the same pool.

Written in Rust with [egui](https://github.com/emilk/egui). It ships two binaries: `claude-usage` (the GUI) and `claude-usage-cli` (a one-shot text report, a diagnostic dump, and an MCP server so Claude can check its own usage).

![Claude Usage Monitor](docs/screenshot.png)

## What it shows

The window opens at 420x760 (minimum 360x320) in a warm dark theme. From top to bottom:

- **Stat chips**: one chip each for the 5-hour window, the 7-day window (all models), and Fable when the API reports it. Each shows the utilization (rounded down, so 99.6% never reads as the cap) in its state color: green below 50%, yellow from 50%, orange from 75%, red from 90%. Next to it is the **pace**: utilization minus the share of the window that has elapsed, so `▲+6` means you are 6 points ahead of an even burn that would land exactly at 100% at the reset, and `▼-4` means 4 points behind. The delta is muted unless it is 10 points or more either way (ahead in amber, behind in green). Hover a chip for the reset time and the pace in words. To the right are three buttons: refresh now (⟳, rests 5 s after a click), pill mode (◱), and Settings (⚙).
- **5-hour window** (the hero timeline, titled with the window's span, "6:10p – 11:10p"): the current 5-hour window, from its start to the reset, on a 0–100% scale.
  - The 5-hour utilization as a filled area drawn from the saved samples, in the current state color.
  - The Fable share as a clay layer at the bottom of the area.
  - A "now" marker, and a dashed projection from now at the 15-minute burn rate. If the projection reaches 100% before the reset, a red ✕ marks the cap with its time ("cap 4:10p"); otherwise the line ends at the reset with a green "safe". A dashed line marks the 100% cap.
  - Stretches with no samples (the app was closed, the laptop slept) are bridged with a faint dotted line instead of leaving a hole. A reset still breaks the line.
  - Hover for a crosshair with the time, the utilization and the Fable share; past "now" it shows the projected value.
  - **5h burn** under the chart: the 15-minute rate for all usage ("+0.21%/min") and either "cap in 1h 48m" or "safe until reset". Hover for the full burn line. The rate is available right after a restart, because it is seeded from the saved history.
  - **After a reset** the chart starts over. Once a 5-hour window ends, the API reports 0% and no reset time until your next message starts a new window. Meanwhile the chart is empty (no times, now marker or projection) with "No active window · starts with your next message", the burn row reads "idle", and the 5-hour chip shows 0% with no pace. The same happens as soon as the reset time passes, without waiting for the next poll. The new window's chart begins at its start with only its own samples. In the rare case the API reports usage without a reset time, the chart shows the last 5 hours up to "now", from the latest reset on.
- **7-day window** (a second timeline, titled with the weekly window's span, "Tue 2a – Tue 2a"): the current weekly window from its start to the weekly reset, drawn like the 5-hour one, because the weekly limits often run out first.
  - The 7-day utilization (all models) as a filled area in its state color, and Fable's own 7-day limit as a clay line (hidden until Fable shows usage or a reset time).
  - A tick at each local midnight, labelled with the weekday, and "reset Tue 2a" at the end. With no known reset it shows the last 7 days up to now.
  - A "now" marker with a dot per limit, and a dashed projection per limit from its weekly rate (below): to 100% with a ✕ and "cap Mon 10:45a" (7-day all in its state color, Fable in clay) when it caps before the reset, else to the projected % at the reset with "safe". When the cap is too close to now for its label, only the ✕ shows; a limit already at 100% gets a ✕ on the 100% line at now. The rows under the chart give the words. The labels keep clear of each other, the ✕ marks and the dots.
  - Gaps of more than an hour (the app was closed) stay solid when the value is the same at both ends (nothing was used meanwhile), and are bridged with a faint dotted line when it changed.
  - Both timelines draw their lines through the moments each whole percent was first seen (the API reports whole percents), straight in between (but never more than a point away from the reading, so a jump after a quiet stretch isn't spread across it) and flat after the latest one, instead of as steps. Hover shows the real readings. Nothing extra is stored for this.
  - Hover for "Sat 3:12p · 7d all 41% · Fable 52%"; past "now", the projected values.
  - **Weekly projections** under the chart, one row per weekly limit: **7d all**, and **Fable** (Fable's own 7-day limit, once it shows usage or a reset time; it can cap before 7-day all). Each gives the rate per day over the last 24 hours, measured from the latest sample at or before then (when that sample is older because the app was closed, the rise is spread over the whole gap, "over last 3d (app was closed)"; if the samples since then span 6 hours or more and rise faster, that faster rate is shown instead), only since the weekly reset if that is later, and projected to that limit's reset. If that crosses 100% first it warns "cap in 16h 00m (Mon 3p)" in orange (red under a day); otherwise it reads "≈78% at reset · safe until Thu 9a" in green; at the limit it reads "at cap · resets Mon 11p" in red. With too little history it says "need more history"; with no known reset, the rate alone. A short stretch of samples is never spread over less than 6 hours. Hover a row for the full text and what the rate was measured over ("over last 24h", "over last 3h (app was closed)"). A narrow row drops the cap time or the % at reset (the tooltip keeps them). "Cap in" counts down from the last good poll to now; the rows are muted when that poll is older than two poll intervals.
- When the window is wide (a content column of 640 pt or more; the column grows to 1000 pt), the two timelines sit side by side, half each, and both cards end level. Narrower, they stack.
- **Extra**: credits used against your monthly limit as a small gauge ("$23 / $35"), when extra usage is set up. Hover for the details, including why extra usage is disabled if it is.
- **Details** (click to expand; it stays open or closed across restarts):
  - The other usage windows (per-model caps beyond Fable). Windows at 0% are hidden unless "Show all buckets" is on.
  - The ALL and FABLE burn lines in full, and the projected-use advice ("safe to spin up another agent").
  - **Top burners**: the projects using the most tokens in the current 5-hour window, attributed from local Claude Code (`~/.claude/projects/`) and Cowork (`%APPDATA%\Claude\local-agent-mode-sessions\`) session logs. A project is named after the last folder of the working directory recorded in its session log.
  - The plan label and how long ago the last poll was.
- **Errors**: a failed poll shows a banner above the last good data, which stays visible. A `permission_error` from the API adds a hint that the client SHA may be stale, with a button to open Settings.

Numbers and bars glide to their new values on each poll, and the projection draws itself in. When the 5-hour window reaches 90%, its chip breathes three times and then holds a red outline and glow; it re-arms once usage drops below 88% or a new window starts.

The window's size, position, maximized state and mode (full or pill), the theme, the Details state, and your settings persist across restarts.

The theme is dark by default, and a light "cream" theme is built in:

![Light theme](docs/screenshot-light.png)

Switch between them (or follow the system) with **Theme** in [Settings](#settings).

## Pill mode

![Pill mode](docs/screenshot-pill.png)

A compact 300x44 always-on-top bar for parking in a screen corner: a ring and the 5-hour percentage, the time to reset ("5h · 3h55m", or "5h · 10m" under an hour; "5h · idle" between windows), the pace, and a state dot. The dot is hollow while polls fail or before the first data, and pulses at 90% like the chip.

- **Enter** it with the ◱ button in the header, or the tray menu's **Pill mode**. Settings → **Start in pill mode** opens the app as the pill every time.
- **Leave** it with a click on the pill; the full window comes back at its old place and size (maximized, if it was).
- **Drag** anywhere on the pill to move it. The full window and the pill remember their positions separately, and the pill reopens where you left it.
- **Right-click** swaps the pill's content for a row of actions: Expand, Refresh, Settings, Quit. Esc, a click on the background, or clicking another window closes the row.
- **Hover** for a two-line summary: the 5-hour window with its pace and reset, then the 7-day and Fable figures (or "Last poll failed").

## Tray icon

With **Show tray icon** on (the default), the notification area shows a rounded square with the 5-hour percentage in two digits ("08", "46"), colored by state; "!!" means 100% or more and "–" means no data yet. Its tooltip reads "Claude usage · 5h 46% · resets 3:42 PM" (no reset between windows), plus "last poll failed" while polls fail. The icon keeps updating while the window is hidden.

- **Left-click** hides the window or brings it back.
- **Right-click** opens the menu: Show / Hide, Pill mode (checked while in the pill), Refresh now, and Quit.
- **Close to tray**: while the tray icon is on and **Close button hides to tray** is ticked (the default), the window's ✕ button and Alt+F4 hide the window instead of quitting. Polling, the icon and alerts keep running.
- **To quit**, use **Quit** in the tray menu or in the pill's right-click row, or untick **Close button hides to tray** (or **Show tray icon**) and close the window.

## Alerts

The app sends a Windows notification when the 5-hour window:

- reaches 75%,
- reaches 90%,
- is projected to hit the cap before the reset, at least 10 minutes ahead.

Each alert fires at most once per 5-hour window, including across restarts. Each has its own toggle in Settings → Alerts, and **Send test notification** there shows a sample right away.

Notifications appear under the name **Claude Usage Monitor** with a coral "%" icon, in the notification banner and in the notification center. With Windows' **Do not disturb** on, they go to the notification center without a banner. To make the toasts carry that name, the app registers itself under an explicit AppUserModelID (see [Privacy & security](#privacy--security)). That ID also groups the taskbar button, so if you pinned `claude-usage.exe` to the taskbar before this version, the old pin may not group with the running window: unpin it and pin the running window again.

Clicking a notification is meant to bring the window forward, but this has not been confirmed yet (see [Known limitations](#known-limitations)).

## Settings

Open Settings with ⚙. Changes take effect on **Apply**, except **Always on top** and **Theme**, which apply at once.

- **Plan label**: shown in the Details footer line (default "Max").
- **Poll interval**: default 60 s, minimum 5 s. It accepts plain seconds or a suffix: `30`, `30s`, `2m`, `1h`.
- **Show all buckets**: list usage windows at 0% in Details too.
- **Always on top**: keep the full window above other windows. (The pill is always on top.)
- **Theme**: **System**, **Dark** (the default) or **Light**.
- **Cookie file**: an explicit `cookies.sqlite`; blank means auto-detect (see [Signing in](#signing-in)).
- **Client SHA**: see [Refreshing the client SHA](#refreshing-the-client-sha). Settings shows which source is in use.
- **Window**: **Show tray icon**; **Close button hides to tray** (needs the tray icon); **Start in pill mode**.
- **Alerts**: **5-hour window reaches 75%**; **5-hour window reaches 90%**; **Projected to hit the cap before reset**; **Send test notification**.
- **Motion**: **Reduce motion** (values snap instead of gliding, and the near-cap chip holds steady instead of pulsing); **Keep pulsing near cap** (the chip and pill dot keep breathing for as long as the 5-hour window is at 90% or more, instead of three breaths. It repaints about 10 times a second, which costs a few percent of a CPU core while it lasts: about 9% of one core in testing at an earlier 15 times a second, against about 2.5% when idle).

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

Snap the window to a corner of your screen and turn on **Always on top** in Settings, or switch to [pill mode](#pill-mode), which always stays on top. With close to tray on, the ✕ button tucks it away and the tray icon keeps showing the 5-hour percentage. Pair it with [`claude-monitor`](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor) for a complete picture: `claude-monitor` shows local per-instance attribution from Claude Code logs; this widget shows the unified server-side meter that's actually about to gate you.

## When it breaks

Run `claude-usage-cli debug` first. It lists every browser profile it found with its `sessionKey` freshness, the cookies in the selected profile with their expiry status (values masked), where the client SHA came from, the response to one request with a sniff of the body, and a recommendation. You can probably have Claude Code fix things if you feed it this output.

Common failure modes:

- **`permission_error` / "Invalid authorization"**: most likely your `sessionKey` is stale (sign out of `claude.ai` and back in) or the wrong browser profile was picked (check the profiles table, or set the cookie file explicitly). Less commonly the client SHA has gone stale; refresh it.
- **403 with an HTML body**: Cloudflare is blocking the request. Visit `claude.ai/settings/usage` in your browser to refresh the `__cf_bm` cookie, then retry.
- **"No browser profile found"**: sign in to `claude.ai` in a supported Firefox-family browser, or set the cookie file explicitly.
- **`__cf_bm` missing or expired**: visit `claude.ai/settings/usage` in your browser within 30 minutes before polling.
- **"poller stopped — restart the app"** in the GUI: the background poller exited unexpectedly. Restart the app.
- **The GUI closed or showed a "crashed" message box**: the details are in `crash.log` in the [data directory](#privacy--security), with a backtrace for a panic. Panics the poller recovers from are logged there too, without a message box. If the tray icon or the notification registration fails, the reason is logged there as well, as a plain entry without a backtrace.
- **The window seems gone**: with close to tray on, ✕ only hides it. Left-click the tray icon (it may be in the taskbar's "^" overflow) to bring it back.

## Privacy & security

The app reads session cookies from your browser's local cookie store. They include your `sessionKey`, which is effectively a password for your `claude.ai` account. The app never writes the cookies anywhere except a temporary copy of the cookie database (deleted after reading), never logs them, and never sends them anywhere except `claude.ai`. `debug` masks cookie values.

A small amount of state is kept locally in the app's data directory:

- `history.jsonl`: usage snapshots (percentages and timestamps; no conversation content). Used for the charts and the burn rate.
- `latest.json`: the most recent full snapshot (every window's utilization and reset time, and the extra-usage block). Read by the MCP server.
- `anonymous_id`: a random UUID generated per install, sent as a telemetry header to `claude.ai`, as the web client does. Nothing ties one install to another.
- `crash.log` (GUI): panics (with a backtrace), startup failures, and tray icon or notification registration failures, each with a timestamp.
- `app-icon.png` (GUI, Windows): the coral "%" icon that notifications show, rewritten only when it changes.

The data directory is:

- Windows: `%APPDATA%\claude_usage_tui\`
- macOS: `~/Library/Application Support/claude_usage_tui/`
- Linux: `$XDG_DATA_HOME/claude_usage_tui/` (default `~/.local/share/claude_usage_tui/`)

This is the same directory the Python version used with python.org (non-Store) Python, so that history carries over as is. If you ran the Python version with Microsoft Store Python, see [Migrating history from the Python version](#migrating-history-from-the-python-version).

The GUI's settings (everything in the [Settings](#settings) panel, including the cookie file path and client SHA), its window state (full-window and pill positions, size, maximized, current mode, Details open or closed), the per-window alert state, and the theme are stored in eframe's `app.ron`:

- Windows: `%APPDATA%\claude-usage\data\app.ron`
- macOS: `~/Library/Application Support/claude-usage/app.ron`
- Linux: `$XDG_DATA_HOME/claude-usage/app.ron` (default `~/.local/share/claude-usage/app.ron`)

The app keeps its own window geometry inside the `"settings"` entry. On the first start of this version it removes the old `"window"` entry that earlier versions let eframe write, so a stale size from before cannot override it.

On Windows the GUI also creates one per-user registry key, `HKCU\Software\Classes\AppUserModelId\ClaudeUsageMonitor`, with the values `DisplayName` ("Claude Usage Monitor") and `IconUri` (the path of `app-icon.png`). Windows uses it to label the app's notifications. The app rewrites it on every start; it needs no admin rights, and you can delete it after removing the app. Windows' own notification settings for the app live under `HKCU\Software\Microsoft\Windows\CurrentVersion\Notifications\Settings\ClaudeUsageMonitor`.

## Changes from the Python version

This project used to be a Python terminal dashboard (`claude_usage_tui.py`). The Rust port keeps the same endpoint, numbers, data files, and MCP tools, and drops:

- **Chromium and Safari cookie support.** Only Firefox-family browsers are read now.
- **The TLS-impersonation profile sweep in debug.** The Python version used `curl_cffi` to mimic browser TLS fingerprints, and `--debug` retried with several profiles. The Rust app uses plain rustls with Firefox-like headers, which Cloudflare currently accepts, and `debug` sends one request.
- **`--no-fullscreen`**, which only made sense for the terminal UI.

Other differences: `--once` and `--debug` became the `once` and `debug` subcommands of `claude-usage-cli`; the poll interval, plan label, cookie file, and client SHA are set in the GUI's Settings instead of on the command line; and the GUI replaces the terminal's bar charts with the two timelines and the chips described in [What it shows](#what-it-shows).

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

## Known limitations

- **Clicking a notification** is wired to bring the window forward, but this has not been confirmed on a real banner yet. It may not work from the notification center after the banner is gone, and Windows may show the window without focusing it.
- **Windows only**: the tray icon, alerts and the rounded pill corners. Other platforms build without them (and are untested in general).

## Caveats

This app polls an undocumented internal endpoint. Anthropic could change it at any time, and the app would stop working until updated. If Cloudflare starts rejecting non-browser TLS fingerprints, the plain rustls client may need replacing with one that impersonates a browser.

This is a personal tool, not an Anthropic-supported integration. Use accordingly.

## License

MIT. See [LICENSE](LICENSE).
