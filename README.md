# claude-usage-monitor

A real-time terminal dashboard for your `claude.ai` Max-plan usage. Polls the same `/api/organizations/{org}/usage` endpoint that powers the official settings page, so the numbers you see are the same numbers Anthropic actually uses to enforce your caps, across Claude Code, Cowork, web chat, and any other surface that draws from the same pool.

```
╭───────────────────────────────── claude.ai usage  ·  Max 5x ──────────────────────────────────╮
│ 5-hour           46.0% ━━━━━━━━━━━━━━━━━━━━━━━━━━━╺━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━     1h 59m │
│ 7-day all        13.0% ━━━━━━━╸━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━     5d 12h │
│ Sonnet 7d         0.0% ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━          — │
│                                                                                               │
│ 15m burn:   +0.00%/min   (over last 7m, 11 samples)                                           │
│ → 100% in   steady      (no projection)                                                       │
│                                                                                               │
│ Extra usage: $23.70 / $35 USD  (67.7%)                                                        │
│                                                                                               │
│ ╭────────────── 5-hour window ───────────────╮ ╭─────────────── Last 7 days ────────────────╮ │
│ │ 100%┤                                      │ │     100%┤                                  │ │
│ │     ┤                                      │ │         ┤                                  │ │
│ │  75%┤                                      │ │      75%┤                                  │ │
│ │     ┤                                      │ │         ┤                                  │ │
│ │  50%┤                    ██  ██            │ │      50%┤                                  │ │
│ │     ┤                ██  ██  ██            │ │         ┤                                  │ │
│ │  25%┤                ██  ██  ██            │ │      25%┤                        ██        │ │
│ │     ┤                ██  ██  ██            │ │         ┤                        ██        │ │
│ │   0%└────────────────────────────────────… │ │       0%└────────────────────────────      │ │
│ │                                            │ │          Thu Fri Sat Sun Mon Tue Wed       │ │
│ ╰────────────────────────────────────────────╯ ╰────────────────────────────────────────────╯ │
│                                                                                               │
│ Top burners (5h):  no local activity yet                                                      │
│ Headroom: 54%  ·  idle, safe to spin up agents                            last polled 24s ago │
╰───────────────────────────────────────────────────────────────────────────────────────────────
```

## What it shows

- **5-hour, 7-day, Sonnet 7-day windows**: utilization, color-coded thresholds (green < 50%, yellow ≥ 50%, orange ≥ 75%, red ≥ 90%), and time-to-reset.
- **15-minute rolling burn rate**: how fast your 5h window is filling, computed from the last 15 minutes of samples.
- **"100% in X" projection**: when you'll hit the cap at the current rate
- **Side-by-side bar charts**: the current 5h window curve and the last 7 days.
- **Extra-usage gauge**
- **Top burners**: top-N agents by token usage in the current 5h window, attributed from local Claude Code (`~/.claude/projects/`) and Cowork (`%APPDATA%/Claude/local-agent-mode-sessions/`) JSONLs.
- **Last Polled X Seconds Ago**

## Install

Requires Python 3.9+ and three packages:

```bash
pip install --user curl_cffi rich browser-cookie3
```

Clone or download this repo, then run:

```bash
python claude_usage_tui.py --plan "Max 5x"
```

Sign in to `claude.ai` in any supported browser first. The script reads your session cookies directly from the browser's cookie store.

You must then follow the instructions just below to collect your build hash.

## Refreshing `client_sha.txt`

Anthropic ships a new web-client build every couple of weeks, and the build hash (`anthropic-client-sha` header) rotates with it. When the build hash goes stale, the script will start getting `permission_error` responses from the API even with a valid session.

To refresh:

1. Open `claude.ai/settings/usage` in your browser
2. Open DevTools (F12) → **Network** tab → filter by **Fetch/XHR**
3. Reload the page
4. Find the request to `/api/organizations/.../usage`
5. In the **Headers** tab, copy the value of the `anthropic-client-sha` header
6. Paste it into `client_sha.txt`, replacing the existing value

You can also override via env var: `CLAUDE_USAGE_TUI_CLIENT_SHA=<hash> python claude_usage_tui.py`.

## Supported browsers

Auto-detected: Firefox, Zen, LibreWolf, Floorp, Waterfox, Chrome, Edge, Brave. Safari supported on macOS (with caveat, see below). The script picks the profile with the freshest `sessionKey` and matches its TLS fingerprint via `curl_cffi` so Cloudflare's bot-management cookie stays valid.

If you're using a browser stored in a non-standard location, pass `--cookie-file PATH` pointing at the right file (`cookies.sqlite` for Firefox-family, `Cookies` for Chromium-family, `Cookies.binarycookies` for Safari).

### macOS Safari note

Reading Safari's cookies requires Full Disk Access for the terminal running Python. Grant it in System Settings → Privacy & Security → Full Disk Access, then add Terminal.app (or iTerm2, etc). Without this permission you'll get a clean error and the script falls through to other browsers.

## Usage

```bash
python claude_usage_tui.py                    # 60s polls, fullscreen
python claude_usage_tui.py --interval 30s     # faster polls
python claude_usage_tui.py --interval 5m      # slower polls
python claude_usage_tui.py --plan "Max 20x"   # custom plan label
python claude_usage_tui.py --once             # one snapshot, exit
python claude_usage_tui.py --debug            # diagnostic dump
python claude_usage_tui.py --show-all         # show every bucket including 0%
python claude_usage_tui.py --no-fullscreen    # render inline, keep scrollback
python claude_usage_tui.py --cookie-file ...  # explicit cookie file path
```

`--interval` accepts plain seconds or suffix syntax (`30`, `30s`, `2m`, `1h`).


## Recommended layout

Park the script in a Windows Terminal pane (or tmux split, iTerm tab) snapped to a quarter of your screen, alongside your music/status. Use it together with [`claude-monitor`](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor) for a complete picture: `claude-monitor` shows local per-instance attribution from Claude Code JSONLs; this dashboard shows the unified server-side meter that's actually about to gate you.

![recomended layout](./TUI%20Ref.png)

## When it breaks

Run `python claude_usage_tui.py --debug` first. It dumps every browser profile it finds with their `sessionKey` freshness, every cookie in the selected profile with expiry status, and tries the request against multiple TLS impersonation profiles to identify whether you're hitting Cloudflare or Anthropic's auth layer. You can probably have Claude Code/Claude Cowork repair the script itself if you feed it the debug information.

Common failure modes:

- **"Invalid authorization"**: the body sniff in `--debug` shows `permission_error`. Most likely your `sessionKey` is stale (sign out and back in to `claude.ai`), or the script is reading from the wrong browser profile (check the "Profiles considered" table). Less commonly, `client_sha.txt` has gone stale, so refresh it.
- **All profiles return 403 with HTML body**: Cloudflare's TLS fingerprint detection got us. Try editing `IMPERSONATE_PROFILE` or `DEBUG_PROFILES` in `api.py` to use a newer `curl_cffi` profile (e.g. `firefox148+`).
- **"No browser profile found"**: sign in to `claude.ai` in any supported browser, or pass `--cookie-file` explicitly.
- **`__cf_bm` missing or expired**: visit `claude.ai/settings/usage` in your browser within the last 30 minutes before running.

## Privacy & security

This script reads session cookies directly from your browser's local cookie store. Those cookies include your `sessionKey` (effectively a password for your `claude.ai` account). The cookies are never written to disk by this script and never transmitted anywhere except `claude.ai`.

A small amount of state is persisted locally:

- `history.jsonl`: your usage utilization snapshots (numeric percentages and timestamps; no conversation content). Used for the bar charts.
- `anonymous_id`: a randomly-generated UUID assigned per install, sent as a telemetry header to `claude.ai`. Each install gets its own; nothing ties one user's installation to another's.

Both files live under your platform's standard data directory:

- Windows: `%APPDATA%\claude_usage_tui\`
- macOS: `~/Library/Application Support/claude_usage_tui/`
- Linux: `~/.local/share/claude_usage_tui/` (or `$XDG_DATA_HOME/claude_usage_tui/`)

## Caveats

This script polls an undocumented internal endpoint. Anthropic could change it at any time and the script would stop working until updated. The TLS-fingerprint impersonation depends on `curl_cffi` keeping pace with current browsers; if Cloudflare tightens detection between curl_cffi releases, the script may need pinning to a newer profile.

This is a personal tool, not an Anthropic-supported integration. Use accordingly.

## License

MIT.
