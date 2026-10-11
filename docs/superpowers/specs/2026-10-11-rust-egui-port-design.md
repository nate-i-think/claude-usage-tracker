# Plan: Port claude-usage-monitor to a Rust egui desktop widget

## Context

The current tool is a ~2,000-line Python Rich TUI (`claude_usage_tui.py`, `api.py`, `storage.py`, `ui.py`, `paths.py`, `debug.py`) plus a Python MCP server (`mcp_server.py`). It polls claude.ai's internal `/api/organizations/{org}/usage` endpoint using browser cookies and curl_cffi TLS impersonation, and renders usage windows, burn rate, projections, and two bar charts.

Nathan wants a fast native Rust egui program instead. Decisions already made:

- **MCP server is ported to Rust** too. Python goes away entirely after parity.
- **Firefox-family cookies only** (Zen, Firefox, LibreWolf, Floorp, Waterfox), plus a `--cookie-file` fallback. Zen and Firefox profiles exist on this machine.
- **Compact widget window**: small, resizable, optional always-on-top, remembers size and position.
- **Windows first, portable code**: verify on Windows only, keep paths and cookie code cross-platform.
- **Cloudflare**: spike first. Try plain reqwest+rustls; switch to `wreq` impersonation only if blocked.
- **Same repo**, Cargo project at the root. Python deleted in a final commit once parity is verified.
- **Subagent tiers**: Fable for the spike and the analytics math port; Opus for everything else; Sonnet for scaffolding.

Environment: rustc/cargo 1.96, cmake 3.29 present, **nasm absent** (matters only if wreq/BoringSSL is needed). No existing `%APPDATA%\claude_usage_tui\history.jsonl` on this machine.

## Process note

Brainstorming's architectural path asks for a committed spec at `docs/superpowers/specs/2026-10-11-rust-egui-port-design.md`. Plan mode forbids writing it now, so **execution step 1 is: copy the Design section below into that spec file and commit it**, then run `superpowers:writing-plans` / `superpowers:subagent-driven-development` for execution.

## Design

### Crate layout (one crate, lib + two bins)

```
Cargo.toml
src/
  lib.rs
  paths.rs       data dir, Claude Code / Cowork dirs, anonymous_id
  config.rs      client SHA resolution, persisted app settings
  model.rs       UsageWindow, ExtraUsage, Snapshot + JSON parsing
  cookies.rs     Firefox-family profile discovery, sqlite read, scoring
  fetch.rs       HTTP client + headers (stack chosen by spike)
  storage.rs     history.jsonl, latest.json, rollups, attribution scan
  analytics.rs   burn rate, reset detection, ETA, projected use, colors, humanize, Fable ceilings
  poller.rs      background thread: fetch + persist + rollups → channel
  mcp.rs         get_usage / wait / wait_test logic + rmcp stdio server
  debug.rs       diagnostic dump
  ui/
    app.rs       eframe::App, layout, error banner
    rows.rs      window rows, burn lines, extra usage, burners, projected-use line
    charts.rs    5-hour and 7-day bar charts with Fable floor
    settings.rs  settings panel
  bin/
    claude-usage.rs      GUI (#![windows_subsystem = "windows"])
    claude-usage-cli.rs  console: `once`, `debug`, `mcp`
```

Two binaries because a Windows GUI-subsystem exe has no console, so `debug`/`once` output would vanish. The CLI bin also serves MCP over stdio. The `.mcp.json` points at `claude-usage-cli mcp`.

### Behavior to preserve exactly (port from Python, with tests)

- **Parsing** (`api.py`): `BUCKET_LABELS`, `ALWAYS_SHOW`, per-model windows from the `limits` array via `scope.model.display_name` and `percent`, `extra_usage` cents ÷ 100, ISO datetimes with `Z`.
- **Headers** (`api.py` `STATIC_HEADERS`, `build_headers`): referer, client platform/version/sha, anonymous id, device id from cookie, fresh `x-activity-session-id` per request. 401/403 error messages.
- **Client SHA** resolution: env `CLAUDE_USAGE_TUI_CLIENT_SHA` > settings value > `client_sha.txt` next to the exe > clear error. New: editable in the settings panel, since it rotates every few weeks.
- **Cookies** (`api.py` `score_profile`, `normalize_expires`, `load_cookie_jar`): score 3/2/1/0, sort by score then expiry, `lastActiveOrg` → org id. Copy `cookies.sqlite` plus `-wal` to a temp dir before opening, because Firefox holds a lock.
- **Storage** (`storage.py`): same paths (`%APPDATA%\claude_usage_tui\`), same `history.jsonl` schema (`ts, five_hour, seven_day, fable_7d, extra_pct`), tolerant reads, atomic `latest.json` via temp + rename with identical JSON shape. 30-minute buckets with carry-forward, daily max over 7 local days, Fable variants, attribution scan of `~/.claude/projects` and Cowork dirs.
- **Analytics** (`ui.py`): 15-min burn window, >5% drop = reset, rate thresholds and colors, "steady" when rate ≤ 0.01, green ETA when it lands after reset, projected use = pct + rate × minutes-to-reset, color thresholds 50/75/90, `humanize_until`, `_fmt_min`, `FABLE_WEEKLY_TO_5H_RATIO = 5.9`, `FABLE_TO_WEEKLY_ALL_RATIO = 0.5`, orange-floor ceilings. In-memory history holds 240 samples.
- **MCP** (`mcp_server.py`): tools `get_usage`, `wait(seconds, confirm)`, `wait_test(seconds)` with the same result JSON fields, `_advise` thresholds, and env vars `CLAUDE_USAGE_MCP_MAX_BLOCK` (50), `CLAUDE_USAGE_MCP_STALE_AFTER` (180), `CLAUDE_USAGE_MCP_TEST_MAX_BLOCK` (600).
- **Debug** (`debug.py`): profile table with scores, masked cookie inventory, org id, URL, header names, request result with body sniff, recommendation.

### Runtime architecture

- **UI thread** never does I/O. It owns the latest `Snapshot`, derived chart data, and a 240-sample history ring.
- **Poller thread** (std thread, blocking HTTP) loops every interval: load cookies, fetch, append history, write `latest.json`, compute rollups and attribution scan, send `PollResult::{Ok(Dashboard), Err(String)}` over a channel, call `ctx.request_repaint()`. A "refresh now" message wakes it early. Cookies are reloaded each poll like the Python, so a re-login is picked up.
- UI calls `request_repaint_after(1s)` so "last polled Xs ago" and reset countdowns tick without refetching.
- **Errors** show as a red banner above the last good dashboard, instead of replacing the whole view like the TUI does. A `permission_error` body gets a hint to refresh the client SHA with a button that opens settings.

### UI (compact widget)

Top to bottom, matching the TUI's content:
1. Title bar text "claude.ai usage · {plan}", refresh button, settings gear, pin (always-on-top) toggle.
2. Window rows: label, colored %, progress bar, reset countdown. 5-hour, 7-day all, Fable first, then others. Hidden-if-zero rule unless "show all".
3. Burn lines ALL (cyan) and FABLE (#d97757).
4. Extra usage line with disabled reason.
5. Charts: 5-hour window (30-min buckets, hour labels on the hour) and last 7 days (weekday labels), Fable floor drawn as an orange bottom segment clipped to the bar. Side by side when wide, stacked when narrow. Drawn with `egui_plot` stacked `BarChart`s for axes and hover values.
6. Top burners (5h).
7. Projected-use line with advice, and "last polled Xs ago" right-aligned.

Settings panel: plan label, poll interval (min 5s), show-all, always-on-top, cookie file override, client SHA. Persisted with eframe's storage (`persistence` feature), which also remembers window geometry.

### Dependencies (pin exact versions at scaffold time)

`eframe`/`egui`, `egui_plot`, `serde`/`serde_json`, `chrono`, `rusqlite` (bundled), `glob`, `uuid`, `clap` (CLI bin), `rmcp` (stdio server; brings tokio, used only in the CLI `mcp` path), `anyhow`/`thiserror`, `reqwest` (blocking, rustls) **or** `wreq` per spike, `tempfile` (dev + cookie copy).

## Execution tasks

| # | Task | Tier | Depends on |
|---|------|------|-----------|
| 0 | **Spike (throwaway)**: read Zen/Firefox cookies with rusqlite from a temp copy; hit the usage endpoint with reqwest+rustls and Firefox-like headers. If Cloudflare returns 403 HTML, try `wreq` with a Firefox profile (install nasm/LLVM if needed). Report which stack works. | Fable | — |
| 1 | Commit spec doc; scaffold crate, two bins, deps, update `.gitignore` (`target/`) and `justfile` (`cargo run --bin ...`). | Sonnet | 0 |
| 2 | `paths`, `config`, `model` parsing + tests (port `test_api.py` fixtures). | Opus | 1 |
| 3 | `cookies` + tests against a synthetic `moz_cookies` sqlite. | Opus | 2 |
| 4 | `storage`: history, latest.json, rollups, attribution + tests with temp dirs. | Opus | 2 |
| 5 | `analytics` port + tests (port every case in `test_ui.py`). | Fable | 2 |
| 6 | `fetch` + `poller` thread. | Opus | 3, 4, 5 |
| 7 | GUI: app shell, rows, burn lines, extra, burners, projected use, error banner. | Opus | 6 |
| 8 | GUI charts with Fable floor and responsive layout. | Opus | 7 |
| 9 | Settings panel, persistence, always-on-top. | Opus | 7 |
| 10 | CLI bin: `once` (text summary) and `debug`. | Opus | 6 |
| 11 | `mcp` module + `claude-usage-cli mcp`; update `.mcp.json`. | Opus | 4 |
| 12 | Parity check vs the Python app, README rewrite, delete Python files in a final commit. | Opus | all |

Tasks 3, 4, 5 run in parallel. Tasks 10 and 11 run in parallel with the GUI tasks. Each task gets a review pass per subagent-driven-development.

## Verification

- `cargo test` covers parsing, cookie scoring, rollups, analytics, and MCP result shapes. Cases ported from `test_api.py` and `test_ui.py` must pass with the same expected values.
- `cargo clippy -- -D warnings` clean.
- Run `claude-usage-cli once` against the live endpoint and compare numbers with `python claude_usage_tui.py --once` run at the same moment (before Python is deleted).
- Run `claude-usage-cli debug` and confirm the profile table picks the Zen/Firefox profile with a live `sessionKey`.
- Launch the GUI, confirm it polls, the countdown ticks, charts fill after a few polls, resizing to narrow stacks the charts, always-on-top and window geometry persist across restarts.
- Break the client SHA in settings, confirm the error banner and hint appear and the last good data stays visible.
- Register `claude-usage-cli mcp` in Claude Code via `.mcp.json`, call `get_usage`, `wait(confirm=false)`, and a short `wait(seconds=5, confirm=true)`.
- Check release binary startup time and idle CPU (should be near zero between repaints).
