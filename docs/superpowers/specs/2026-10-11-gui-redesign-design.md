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

## Implementation notes (as built)

Where the branch differs from the design text above, and why. Sources: the spike report and the task reports under `.superpowers/sdd/2026-10-11-gui-redesign/`.

### Window, persistence, background work

- **Hide to tray is minimize + hide.** `Visible(false)` alone keeps eframe running the full `ui()` pass on every repaint (`ViewportInfo::visible()` comes from minimized/occluded, not from `IsWindowVisible`). Hide sends `Minimized(true)` then `Visible(false)`; show sends `Visible(true)`, `Minimized(false)`, `Focus`. Only then does eframe take its logic-only path while hidden.
- **Own window persistence.** `persist_window: false`. The app stores its geometry in `Settings` (`full_rect`, `full_maximized`, `pill_pos`, `ui_mode`) and reads `app.ron` before `run_native` to build the `ViewportBuilder` for the saved mode, so a pill restart paints the pill on frame 1 with no flash. The spike showed eframe's own persistence is not flash-free.
  - Positions are the outer top-left in **physical pixels**; sizes are in points. On Windows the builder gets no position: `App::new` moves the still-hidden window with `SetWindowPos`, in two passes so a cross-DPI move is re-checked, and keeps it on the work area of the monitor its title band overlaps (`MonitorFromRect` + `rcWork`). Maximize goes through winit's `set_maximized` afterwards.
  - A stale eframe `"window"` key from earlier versions would still be applied at creation, so `keep_our_geometry` is installed as `NativeOptions::window_builder` to put our size and position back, and `remove_stale_window_key` deletes the key on the first `logic()` pass.
  - The runtime `MinInnerSize` from the design is only used when switching modes. At startup the builder is made for the saved mode (full: min 360x320; pill: no minimum, undecorated, fixed size, always on top).
- **`logic()` order**: track window → close request → drain poller events → tray sync → alert hook and toast clicks → settings sync (poller update, and an `app.ron` flush when the live settings differ from the saved ones) → 1 s tick. `ui()` only draws. The Settings panel's Apply only edits `self.settings`; the sync step does the rest.
- **Crash dialog (Task 1)**: recoverable regions run under `crash::catch_recoverable`, a `catch_unwind` wrapper that sets a thread-local flag the panic hook reads. Every panic is logged to `crash.log`; only unguarded ones get the message box, and the box is not re-entrant. This is a general mechanism, not a check on the poller thread's name.

### Theme

- Palette roles beyond the design: `control_border` (input, checkbox and button outlines at ≥ 3:1), `accent_text` (clay text that passes 4.5:1 on cards), `bar` (neutral chart bars that stand apart from the Fable clay), plus `field`, `raised_hover`, `text_faint`, `grid`, `track` and `shadow`. `high` became #ECA04A (dark) and #864700 (light), so the high state is at least 1.3:1 in luminance from the Fable clay. `danger` and `fable` still differ by hue only.
- Accent and Fable are the same clay, so non-Fable items (the ALL tag, chart bars, today's sparkline outline) use neutral colors.
- Inter v4.1 (variable) is bundled; the semibold family is the same file pinned to wght 600 via `FontTweak::coords`. Numbers go through a fixed-width digit-cell helper (`ui::digits`).
- **Theme selector.** Settings has a Theme row (System / Dark / Light radio buttons) that calls `ctx.set_theme` the moment one is picked, like Always on top; it is not a `Settings` field because egui persists `theme_preference` in its own memory. The first launch (no egui memory yet) still defaults to Dark.
- **✕ is drawn with two painter lines.** No font in the chain renders U+2715.

### Timeline

- **Gap bridging.** A gap of more than 10 minutes without samples still ends the solid segment, but segments separated only by time are joined by a dimmed dotted line over a faint area (and a faint Fable fill when both ends have a value). Hovering a bridge reads "no samples 1:10–1:45 PM". Only a reset (a drop of more than 5 points) or missing values leave a real break. Without this, sparse polling (app closed, laptop asleep) fragmented the chart.
- Thinning keeps the min and max per pixel column (up to two points), so spikes survive. An isolated single sample is drawn as a dot.
- The Fable layer has a 1 px surface-colored separator along its top, so it does not rely on hue next to the orange high state.
- **End label placement**: "cap h:mmp" / "safe" sits on a surface-colored pad at the first of six candidate spots that neither the dashed line nor the ✕ keep-out box touches, and the dash stops short of the label. With an unknown reset there is no ✕ and no label (the line is still drawn), and the footer shows only the rate.
- Hover picks, in order: the projection beyond now, the nearest sample within 12 px, a bridge, else just the time.

### Layout

- The "claude.ai usage · plan" title and the 📌 button are gone from the header. Always-on-top lives in Settings only; the plan label moved to the Details footer line ("Max plan · last polled Ns ago").
- Chip % is floored (99.6 never reads 100). The pace delta is muted below 10 points; ahead is amber, behind green. Chips wrap to a second row when they don't fit (the Fable chip wraps at 360 pt).
- **The sparkline uses a fixed 0–100% scale.** A scale relative to the week's maximum made a 47% day look near the cap.
- **Footer burn copy**: "+0.21%/min · cap in 1h 48m" when the cap lands before the reset, **"+0.34%/min · safe till reset"** when it lands after (`Eta::Minutes` carries a `lands_after_reset` flag from the shared helper), and the rate alone when the reset is unknown.
- **Burner names come from the session JSONL `cwd`** (the last path component of the latest timestamped entry), with `prettify_project` only as the fallback: the folder-name encoding is ambiguous (`__tomos` → `tomos-…`). Long names are cut at the front so the tail stays.
- The content column is capped at 720 pt and centered; `layout::pack_rows` wraps chips and footer items.

### Motion

- Tweens are 0.45 s ease-out and only start from poll data. A value not drawn for more than 2 s (hidden window, collapsed Details) snaps instead of gliding. Reduce motion snaps everything.
- The projection draw-in (0.6 s) runs on the first projection, and again only when a **new poll** (keyed by `fetched_at`) moves the end by more than 5 points. The end mark and label fade in over the last 30%.
- **Pulse: three breaths, then steady.** At ≥ 90% the 5-hour chip (and the pill dot) breathes three times over about 5 s, then holds the peak level with no extra repaints beyond the 1 s tick. It re-arms only below **88%** or in a new window. "Keep pulsing near cap" keeps it breathing at a 66 ms repaint; that measured about 9.35% of one core against about 2.5% idle, so it is opt-in.
- **Jitter-tolerant window identity.** The API's `resets_at` jitters from poll to poll (usually sub-second, occasionally minutes). `analytics::alerts::same_window` rounds both values to the minute and treats keys within 5 minutes as the same window. The alert state and the pulse tracker both use it.

### Pill

- 300x44 pt, with DWM rounded corners (`DWMWCP_ROUND`, set once at startup).
- **Right-click shows an inline action row** (Expand / Refresh / Settings / Quit) in place of the pill's content, not a popup menu: an egui popup cannot leave the 44 pt window. Esc, a background click or focus loss closes it. The hover tooltip is two short lines for the same reason.
- Position sanity: positions more than 16384 px from the origin are dropped (three 4K monitors reach x = 11520), not 10000 as briefed. With no usable position, the pill goes to the bottom-right corner of the saved full rect. The required visible strip is capped at the window's own size, so a pill parked flush on the taskbar edge stays put.
- Entering the pill closes the Settings panel. Pill → Settings expands first and then opens the panel.

### Tray

- Left-click toggles show/hide (the design only had the menu item). The menu is Show / Hide, Pill mode (check), Refresh now, a separator, Quit. The app never calls `show_menu()`: without a user gesture it hangs `logic()`.
- The icon is drawn at `SM_CXSMICON` (36 px at 225%) with a 3x5 pixel font in whole-pixel blocks, on the light palette's state colors whatever the app theme, with white digits. Single digits render as "08"; ≥ 100% shows "!!", unknown "–". The tooltip adds "· last poll failed" while polls fail.
- Quit sets a `quitting` flag before `Close`, so close-to-tray lets it through; the pill's Quit uses the same path.
- tray-icon and muda keep their event handlers in a `OnceCell`, so the handlers are installed once per process in `App::new` and the channel belongs to the app. Re-creating the icon (toggling "Show tray icon") reuses it.

### Alerts

- Alert copy floors the percentage ("5-hour usage at 89%" for 89.5) and carries the usage at firing time, so a 75% alert fired at 95% (with the 90% toggle off) says 95%.
- An edge reached while its toggle is off still counts as consumed. A projected cap less than 10 minutes away is skipped and not recorded, so it can fire later. The edge state is persisted in `Settings::alert_state`, so a restart does not re-fire.
- Toasts go out under the registered AUMID `ClaudeUsageMonitor`, with `DisplayName` and an `IconUri` pointing at a generated coral "%" PNG (`app-icon.png` in the data dir), from a dedicated notifier thread. If registration fails, one line goes to `crash.log` and toasts fall back to `Toast::POWERSHELL_APP_ID`. Settings has a "Send test notification" button.
- Clicking a toast is wired (`on_activated` → show the window) but unverified. The explicit AUMID also changes taskbar grouping, so an older taskbar pin of the exe may need re-pinning (the README says so).

### Still open for a human check

Real tray clicks, pill drag feel and click-vs-drag, the pill's right-click row, hover tooltips and the crosshair, a toast banner with Do not disturb off, toast click → show, and mixed-DPI pill placement. Each task report lists the exact steps.
