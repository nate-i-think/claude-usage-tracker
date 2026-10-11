# Plan: Timeline-first GUI redesign ("Claude warm")

## Context

The Rust egui port (merged to main at 5f19861) reproduces the old Rich TUI faithfully: text rows, burn lines, and two blocky bar charts. Nathan wants a redesign that goes beyond what a terminal could do. He parks the monitor in a screen corner and glances at it while agents run.

Decisions from Nathan:
- **Use:** glanceable corner widget first.
- **Lead direction:** timeline-first — a big smooth chart of the current 5-hour window from raw samples, with the Fable layer, a dashed projection, a cap line and a "you'd hit 100% at" marker; compact stats above, 7-day sparkline and burn/extra strip below.
- **Extras in scope:** compact pill mode, animations, tray icon with live %, threshold alerts.
- **Aesthetic:** Claude warm — warm charcoal dark / cream light, clay-orange accent, soft rounded corners.

Also fold in the open bug from the port's final review: the panic hook shows a blocking "crashed" box for poller panics that `catch_unwind` already recovers from (`src/crash.rs`, `src/poller.rs`).

## Design

### Layout (full mode, default ~420x460, min 360x320)

```
╭──────────────────────────────────────────╮
│ 5h 46% ▲+6   7d 13%   Fable 0%   ⟳ ◱ ⚙  │  stat chips; ◱ = pill toggle
│──────────────────────────────────────────│
│100%┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄✕ cap 4:10p│  hero timeline
│ 75%│                     ╱┄┄┄           │
│ 50%│               ╱‾‾‾‾╯  projection    │
│ 25%│        ╱‾‾‾‾‾╯                      │
│    │  ▁▁▂▓▓▓ fable layer                 │
│  0%└───┴────┴────┴───┃───┴────┴─────────│
│      11   12   13   now   15    reset 3:42p
│──────────────────────────────────────────│
│ 7d ▁▂▂▃▅▆▄   +0.21%/min · cap 5h28m   $23/$35 │  footer strip
│ ▸ Details                                │  collapsible
╰──────────────────────────────────────────╯
```

- **Stat chips:** each window as a rounded chip: label, big animated %, state color, pace delta for windows with a known reset (`▲+6` ahead of pace / `▼-4` behind). Pace = utilization − percent of window elapsed. Hover shows reset time ("resets 3:42 PM · in 1h 59m").
- **Hero timeline:** x = window start → reset (5 h), y = 0–100%.
  - Area under the 5-hour line from raw `history.jsonl` samples in the window (smooth polyline, gradient fill in the state color).
  - Fable layer: orange area = min(curve, rise-in-Fable × `FABLE_WEEKLY_TO_5H_RATIO`), same math as today's floor but continuous.
  - "Now" vertical marker; dashed projection from now at the 15-minute burn rate to reset or 100%; dashed 100% cap line; ✕ marker and label where the projection crosses 100% before reset (red), or "safe" end label (green).
  - Hover: crosshair with time, %, Fable share.
- **Footer strip:** 7-day sparkline (7 mini bars with Fable floor, today highlighted, hover per day), burn rate + ETA (from `analytics::burn_line`), extra usage mini-gauge.
- **Details (collapsible, remembered):** other windows (Iguana etc.), ALL/FABLE burn lines in full, projected-use advice, top burners with prettified project names (`C--Users-Nathan-Myers-…-foo` → `foo`), last polled. Error/persist/poller-stopped banners stay at the top as today.

### Pill mode (~300x44, borderless, always-on-top)

`◔ 46%  5h · 1h59m  ▲+6  ●` — mini ring, %, countdown, pace, state dot. Drag anywhere to move; click (or double-click) to expand to full mode. Toggled from the header button, tray menu, or a setting "start in pill mode". Pill and full positions remembered separately.

### Motion

- Numbers and ring/bars ease to new values on each poll (egui animate; idle CPU must return to ~0 after).
- Slow pulse on the 5-hour chip/pill dot only when ≥ 90% (low-rate repaint while pulsing, none otherwise).
- Projection line draws in on first data.

### Tray icon

Dynamic icon rendering the 5-hour % (two digits on a state-colored rounded square). Menu: Show/Hide, Pill mode, Refresh, Quit. Setting "close to tray" (default on when tray enabled): the window close button hides instead of quitting.

### Threshold alerts

Windows toast when the 5-hour window crosses 75% and 90%, and when the projection says you will hit the cap before reset (≥ 10 min warning). Edge-triggered and de-duplicated per window (keyed by `resets_at`), so each fires at most once per window. Settings toggles per alert type. Pure decision fn in analytics; delivery off the UI thread.

### Theme ("Claude warm")

One palette module (replaces scattered colors in `src/ui/mod.rs`):
- Dark: bg warm charcoal ~#262624, surface ~#30302E, raised ~#3A3936, text ~#F5F4EF, muted ~#A6A39A, accent clay #D97757.
- Light: bg cream ~#FAF9F5, surface ~#F0EEE6, text ~#141413, muted ~#6B6A65, accent darker clay #B04F30.
- State colors warmed (green/amber/orange/red) with contrast tests on both surfaces.
- Rounded corners (8–12 px), soft shadows on cards/chips, generous spacing, a bundled UI font (Inter, OFL) with non-jittering digits.

### Data plumbing

- `poller::Dashboard` gains `window_samples` (timestamped 5-hour and Fable values inside the current window), produced from the existing single history read (`storage::rollups`). No extra file reads.
- New pure analytics: `pace(window, now)`, `projection(samples/rate, now, reset)` → line endpoints + cap-crossing time, alert edge detection, project-name prettifier.

### Feasibility findings (eframe 0.36.2 sources, Plan-agent investigation)

- **Pill:** `ViewportCommand::{MinInnerSize(ZERO), Decorations(false), Resizable(false), InnerSize, OuterPosition, WindowLevel}` all apply at runtime, in that order. Drag via `StartDrag` (needs focus). eframe persists one window rect, so store `ui_mode`, `pill_pos`, `dashboard_rect` in our `Settings` and drop the builder's fixed `with_min_inner_size` in favour of a runtime `MinInnerSize` chosen from the saved mode. No transparency (wgpu DX12 is opaque-only); round the borderless pill with Windows 11 DWM corner preference (`DwmSetWindowAttribute`, windows-sys `Win32_Graphics_Dwm`).
- **Tray:** `tray-icon = 0.26.1` (windows-sys only, nasm-safe). Create it in `App::new` on the main thread. Event handlers push into a channel and call `ctx.request_repaint()`, filtering out Move/Enter/Leave events. eframe 0.36 has `App::logic`, which keeps running while the window is hidden (repaints throttled to ≥100 ms). So **move poll draining, tray updates and the 1 s tick from `ui()` into `logic()`**, using `Instant` rather than `ctx.input().time` there. Close-to-tray via `close_requested()` → `CancelClose` + `Visible(false)`; Quit sets a flag then `Close`. Render the icon RGBA ourselves (pixel digit font, `SM_CXSMICON` size) and update it only when the whole-number % changes.
- **Toasts:** `tauri-winrt-notification = 0.8.1` (`windows 0.62` already in the lock file). Register a per-user AUMID (`HKCU\Software\Classes\AppUserModelId\ClaudeUsageMonitor`, DisplayName + IconUri) and call `SetCurrentProcessExplicitAppUserModelID` before `run_native`. Fall back to `Toast::POWERSHELL_APP_ID` if that fails. Show toasts from a dedicated notifier thread (Toast is `!Send`).
- **Motion:** `animate_value_with_time` repaints only until it lands, so only feed it targets that change per poll. Pulse is computed from time with `request_repaint_after(66 ms)` and only at ≥ 90% while visible.
- **Font:** bundle Inter (variable, OFL) for text. egui can't enable OpenType `tnum`, so numbers render through a fixed-width digit-cell helper to avoid jitter.
- **Timeline area:** a `Mesh` triangle strip between the line and the baseline (vertex colors give the gradient), with a stroked line on top. Segments break at sample gaps and resets, thinned to about one point per pixel column.

### What stays

All non-UI layers (model, cookies, fetch, storage, analytics core, poller, CLI, MCP) and the settings panel's logic. `ui/rows.rs` text formatters remain for the CLI's `text_report` and the Details section.

## Execution (subagent-driven, same process as the port)

Spike first (throwaway, in scratchpad), in the risk order the investigation recommended. Then tasks run sequentially with a review after each:

| # | Task | Tier |
|---|------|------|
| 0 | Spike: (a) tray icon + hide-to-tray + `logic()` wakeups + idle CPU; (b) pill toggle sequence, drag, DWM rounding, restart in pill mode; (c) toast with registered AUMID vs PowerShell fallback from a background thread | Fable |
| 1 | Fix panic-hook dialog for recovered poller panics (always log; box only for non-poller threads) | Opus |
| 2 | Move polling/tick work from `ui()` into `App::logic()`; runtime min-size from saved mode; Settings gains ui_mode, pill_pos, dashboard_rect, tray/alert toggles | Opus |
| 3 | Theme module ("Claude warm" palettes, contrast tests), bundled Inter, fixed-width digit helper; restyle existing widgets onto it | Opus |
| 4 | Data: `window_samples` in Dashboard from the existing single history read; analytics pace, projection + cap crossing, project-name prettifier, alert edge detection — all pure with tests | Opus |
| 5 | Hero timeline widget (mesh area, Fable layer, now marker, projection, cap line/marker, hover crosshair) + pure geometry tests | Opus |
| 6 | New layout: stat chips header, footer strip (7-day sparkline, burn, extra), collapsible Details; old rows/bar charts leave the main view | Opus |
| 7 | Motion: eased numbers/bars, ≥ 90% pulse, projection draw-in; idle-CPU check | Opus |
| 8 | Pill mode (toggle, drag, expand, DWM rounding, separate positions, "start in pill" setting) | Opus |
| 9 | Tray icon (rendered % icon, menu, close-to-tray, settings) | Opus |
| 10 | Threshold alerts (AUMID registration, notifier thread, settings toggles, dedupe per window) | Opus |
| 11 | README + screenshots; final whole-branch review | Opus / Fable review |

Work on a branch (`gui-redesign`) in a worktree, and push after every commit (Nathan's standing preference). Execution step 1 is committing this design as `docs/superpowers/specs/2026-10-11-gui-redesign-design.md` and the plan under `docs/superpowers/plans/`.

Open questions I'll rule on during execution unless Nathan objects:
- **Default full-mode size:** 420x460.
- **Close-to-tray:** on by default only when the tray is enabled.
- **Alert thresholds:** 75% and 90%, plus a projected-cap warning.

## Verification

- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` clean after every task.
- Screenshots of full mode (dark + light), narrow minimum width, pill mode, tray icon, and a toast, checked by the controller.
- Idle CPU over 60 s in full mode, pill mode, and hidden-to-tray, near zero outside animations; pulse only at ≥ 90%.
- Alerts: unit tests for edge triggering; one forced manual toast.
- Tray: hide to tray, keep polling (icon % updates), restore, quit from menu.
