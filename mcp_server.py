#!/usr/bin/env python3
"""
mcp_server.py — MCP server exposing claude.ai usage to a Claude instance.

A companion to claude_usage_tui.py. The monitor writes the full latest usage
snapshot to latest.json each ping; this stdio MCP server reads that file on
demand and exposes two tools so a Claude agent can check its own usage and
throttle itself near a cap:

    get_usage()              — non-blocking read of the most recent ping
    wait(seconds, confirm)   — bounded-block-and-cycle throttle

It does no network/auth itself: it only reflects "what the stats were at the
most recent ping". Keep the monitor running to keep the data fresh.

Run standalone (for testing):       python mcp_server.py
Register with Claude Code:          claude mcp add claude-usage -- python mcp_server.py
or rely on the project-scoped .mcp.json in this repo.
"""

from __future__ import annotations

import math
import os
import sys
import time
from datetime import datetime, timezone
from typing import Any, Optional

# Make sibling modules (storage, paths, api) importable no matter the cwd the
# MCP host launches us from.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

try:
    from mcp.server.fastmcp import FastMCP
except ImportError:  # pragma: no cover - import guard
    print("missing dependency: mcp\ninstall with: pip install mcp", file=sys.stderr)
    sys.exit(2)

import storage


# Snapshot older than this (seconds) is flagged stale — the monitor is probably
# not running. Default comfortably exceeds the monitor's 30/60s poll intervals.
STALE_AFTER_SEC = int(os.environ.get("CLAUDE_USAGE_MCP_STALE_AFTER", "180"))

# Max seconds a single wait() call will block. Kept under the ~60s request
# timeout most MCP clients inherit (and that Claude Desktop hard-codes), so a
# long wait is done as several short, timeout-proof cycles instead.
MAX_BLOCK_SEC = max(1, int(os.environ.get("CLAUDE_USAGE_MCP_MAX_BLOCK", "50")))

# Hard ceiling for the diagnostic wait_test() tool. Generous (10 min, matching a
# typical API timeout) but bounded so a probe can never truly hang the client.
TEST_MAX_BLOCK_SEC = max(1, int(os.environ.get("CLAUDE_USAGE_MCP_TEST_MAX_BLOCK", "600")))

mcp = FastMCP("claude-usage")


# --- helpers (plain functions, unit-testable without the MCP runtime) -----

def _now() -> datetime:
    return datetime.now(timezone.utc)


def _parse_dt(s: Optional[str]) -> Optional[datetime]:
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00"))
    except ValueError:
        return None


def _humanize_until(dt: Optional[datetime]) -> Optional[str]:
    """Compact time-until string, e.g. '2d 4h', '1h 09m', '8m 30s', '14s'."""
    if dt is None:
        return None
    secs = int((dt - _now()).total_seconds())
    if secs <= 0:
        return "now"
    h, rem = divmod(secs, 3600)
    m, s = divmod(rem, 60)
    if h >= 24:
        d, h = divmod(h, 24)
        return f"{d}d {h}h"
    if h > 0:
        return f"{h}h {m:02d}m"
    if m > 0:
        return f"{m}m {s:02d}s"
    return f"{s}s"


def _advise(windows: list, stale: bool) -> tuple:
    """Return (recommendation, summary) from the windows. Keys off whichever
    window is closest to its cap."""
    usable = [w for w in windows if w.get("utilization_pct") is not None]
    if not usable:
        return ("No utilization data in the latest snapshot.",
                "No usage data available.")
    top = max(usable, key=lambda w: w["utilization_pct"])
    pct = top["utilization_pct"]
    label = top["label"]
    rin = top.get("resets_in_human") or "unknown"

    if pct >= 95:
        rec = (f"CRITICAL: {label} at {pct:.0f}%. You are at/near the cap. "
               f"Throttle now — call wait() in a loop until it resets (in {rin}), "
               f"or pause heavy work.")
    elif pct >= 80:
        rec = (f"WARNING: {label} at {pct:.0f}%. Approaching the cap; consider "
               f"slowing down or batching work. Resets in {rin}.")
    elif pct >= 50:
        rec = (f"{label} at {pct:.0f}%. Headroom is fine but worth watching. "
               f"Resets in {rin}.")
    else:
        rec = f"{label} at {pct:.0f}%. Usage is low; no need to throttle."

    if stale:
        rec = "(data may be stale — monitor possibly not running) " + rec
    summary = f"{label} {pct:.0f}% · resets in {rin}"
    return rec, summary


def _current_usage() -> dict:
    """Build the structured 'most recent ping' view from latest.json."""
    data = storage.read_latest_snapshot()
    now = _now()

    if not data:
        return {
            "available": False,
            "stale": True,
            "note": ("No usage snapshot found. Start the monitor to produce one: "
                     "`python claude_usage_tui.py` (or `just start`)."),
            "windows": [],
            "five_hour": None,
            "extra": None,
            "recommendation": "No data yet — the monitor has not written a snapshot.",
            "summary": "No usage data available.",
        }

    fetched_at = _parse_dt(data.get("fetched_at"))
    age = int((now - fetched_at).total_seconds()) if fetched_at else None
    stale = age is None or age > STALE_AFTER_SEC

    windows = []
    for w in data.get("windows", []):
        resets_at = _parse_dt(w.get("resets_at"))
        rsec = int((resets_at - now).total_seconds()) if resets_at else None
        windows.append({
            "label": w.get("label"),
            "utilization_pct": w.get("utilization"),
            "resets_at": w.get("resets_at"),
            "resets_in_seconds": max(rsec, 0) if rsec is not None else None,
            "resets_in_human": _humanize_until(resets_at),
        })

    five_hour = next((w for w in windows if w["label"] == "5-hour"), None)
    recommendation, summary = _advise(windows, stale)

    result = {
        "available": True,
        "as_of": data.get("fetched_at"),
        "age_seconds": age,
        "stale": stale,
        "windows": windows,
        "five_hour": five_hour,
        "extra": data.get("extra"),
        "recommendation": recommendation,
        "summary": summary,
    }
    if stale:
        result["note"] = (
            f"Snapshot is {age}s old (> {STALE_AFTER_SEC}s threshold). The monitor "
            f"may not be running; values reflect the last successful ping.")
    return result


def _wait_logic(seconds: int, confirm: bool) -> dict:
    """Core of the wait tool — see the tool docstring for the contract."""
    seconds = max(0, int(seconds))
    block = min(seconds, MAX_BLOCK_SEC) if seconds else MAX_BLOCK_SEC

    usage = _current_usage()
    five = usage.get("five_hour")
    reset_in = five.get("resets_in_seconds") if five else None
    reset_at = five.get("resets_at") if five else None

    if not confirm:
        cycles = math.ceil(reset_in / block) if (reset_in and block) else None
        msg = f"This will BLOCK for up to {block}s per call. "
        if reset_in is not None:
            msg += (f"The 5-hour window resets in ~{reset_in}s"
                    + (f" (~{cycles} call(s) of {block}s)" if cycles else "") + ". ")
        msg += ("Re-call wait(seconds=..., confirm=true) to actually wait. Each "
                "call blocks only a bounded amount and then returns, so you are "
                "never trapped — loop until 'done' is true.")
        return {
            "blocked": False,
            "preview": True,
            "would_block_seconds": block,
            "requested_seconds": seconds,
            "max_block_seconds": MAX_BLOCK_SEC,
            "five_hour_reset_at": reset_at,
            "five_hour_resets_in_seconds": reset_in,
            "estimated_cycles_to_reset": cycles,
            "message": msg,
        }

    start = time.monotonic()
    time.sleep(block)
    slept = round(time.monotonic() - start, 1)

    after = _current_usage()
    five2 = after.get("five_hour")
    remaining = five2.get("resets_in_seconds") if five2 else None
    done = remaining is not None and remaining <= 0

    if done:
        next_step = "The 5-hour window has reset — you can resume normal work."
    elif remaining is not None:
        next_step = (f"~{remaining}s remain until the 5-hour reset. Call "
                     f"wait(seconds={remaining}, confirm=true) again to keep waiting.")
    else:
        next_step = ("Reset time unknown (monitor may not be running). Call "
                     "get_usage() to check, then decide whether to wait more.")

    return {
        "blocked": True,
        "slept_seconds": slept,
        "five_hour_reset_at": five2.get("resets_at") if five2 else None,
        "five_hour_resets_in_seconds": remaining,
        "five_hour_utilization_pct": five2.get("utilization_pct") if five2 else None,
        "done": bool(done),
        "next_step": next_step,
        "usage": after,
    }


def _wait_test_logic(seconds: int) -> dict:
    """Core of the wait_test diagnostic — see the tool docstring for the contract."""
    requested = max(0, int(seconds))
    block = min(requested, TEST_MAX_BLOCK_SEC)
    capped = block < requested

    start = time.monotonic()
    time.sleep(block)
    slept = round(time.monotonic() - start, 1)

    return {
        "requested_seconds": requested,
        "blocked_seconds": block,
        "capped_to_ceiling": capped,
        "test_max_block_seconds": TEST_MAX_BLOCK_SEC,
        "slept_seconds": slept,
        "result": (
            f"Returned after blocking ~{slept}s. If you can read this, your MCP "
            f"client's single tool-call timeout is at least {block}s. If instead "
            f"the call errored or timed out, the client's limit is below {block}s — "
            f"keep wait()'s CLAUDE_USAGE_MCP_MAX_BLOCK under that."
        ),
    }


# --- MCP tools ------------------------------------------------------------

@mcp.tool()
def get_usage() -> dict[str, Any]:
    """Report Claude's current claude.ai usage from the most recent monitor ping.

    Read-only and non-blocking. Use this to decide whether to slow down or keep
    going. Returns, for each usage window (5-hour, 7-day, Fable, ...):
    utilization percent, when it resets, and a human-readable time-to-reset —
    plus the data's age, a `stale` flag (true if the monitor isn't updating),
    the extra-usage block, and a `recommendation`/`summary` you can act on.

    Data is whatever the running `claude_usage_tui.py` monitor last fetched; if
    the monitor isn't running the result is marked stale.
    """
    return _current_usage()


@mcp.tool()
def wait(seconds: int = MAX_BLOCK_SEC, confirm: bool = False) -> dict[str, Any]:
    """Throttle yourself by sleeping in short, bounded cycles near a usage cap.

    BLOCKS the call while it sleeps, but only up to ~50s per call (MAX_BLOCK_SEC)
    — deliberately short so no MCP client times out and you can never get
    trapped in a long sleep. To wait longer (e.g. until the 5-hour window
    resets), call this repeatedly until the result's `done` is true.

    Two-step, so you never block by accident:
      - confirm=false (default): PREVIEW only. Nothing sleeps. Returns how long
        a real call would block and roughly how many cycles reaching the reset
        would take.
      - confirm=true: actually sleep one bounded cycle (min(seconds,
        MAX_BLOCK_SEC)), then return how much time remains until the 5-hour
        reset and whether to call again (`next_step`).

    Typical loop: get_usage() → if near the cap, wait(confirm=false) to preview,
    then wait(seconds=<remaining>, confirm=true) repeatedly until done.
    """
    return _wait_logic(seconds, confirm)


@mcp.tool()
def wait_test(seconds: int = 90) -> dict[str, Any]:
    """DIAGNOSTIC: block for `seconds` to discover your MCP client's tool-call timeout.

    Unlike wait(), this is NOT capped at ~50s — it blocks for the full requested
    duration (up to a 600s hard ceiling) and immediately returns how long it
    actually slept. Use it to probe how long your client (e.g. Claude Desktop)
    lets a single tool call run: if this returns normally, the client's timeout
    is at least that long; if the client kills the call first, you've found the
    ceiling, and you can set wait()'s CLAUDE_USAGE_MCP_MAX_BLOCK just under it.

    For real throttling use wait() instead, which stays under the common 60s
    limit and cycles. Default 90s — just over wait()'s 50s cap, a good first probe.
    """
    return _wait_test_logic(seconds)


if __name__ == "__main__":
    mcp.run(transport="stdio")
