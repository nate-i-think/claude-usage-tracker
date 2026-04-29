#!/usr/bin/env python3
"""
claude_usage_tui.py — Real-time TUI for claude.ai Max-plan usage.

Polls https://claude.ai/api/organizations/{org_id}/usage and renders a Rich
dashboard: 5-hour / 7-day / Sonnet 7-day windows, current burn rate with
"100% in X" projection, side-by-side bar charts of the current 5h window
and the last 7 days, plus per-instance attribution from local Claude Code
and Cowork JSONLs.

Reads cookies from a Firefox-family browser (Firefox, Zen, LibreWolf,
Floorp, Waterfox); no secrets in config files. History is persisted to
%APPDATA%\\claude_usage_tui\\history.jsonl.

Setup:
    pip install --user curl_cffi rich browser-cookie3

Run:
    python claude_usage_tui.py                    # 60s polls, fullscreen
    python claude_usage_tui.py --interval 30s     # faster
    python claude_usage_tui.py --plan "Max 5x"    # custom plan label
    python claude_usage_tui.py --once             # one snapshot, exit
    python claude_usage_tui.py --debug            # diagnostic dump
"""

from __future__ import annotations

import argparse
import sys
import time
from collections import deque
from datetime import datetime, timedelta, timezone
from typing import Deque, Optional, Tuple

try:
    from curl_cffi import requests
    from rich.console import Console
    from rich.layout import Layout
    from rich.live import Live
    from rich.panel import Panel
except ImportError as e:
    print(f"missing dependency: {e.name}", file=sys.stderr)
    print("install with: pip install curl_cffi rich browser-cookie3", file=sys.stderr)
    sys.exit(2)

from api import Snapshot, fetch_usage
from debug import run_debug
from storage import (
    append_snapshot, attribute_recent, daily_history, five_hour_buckets,
)
from ui import render_dashboard, render_error


HISTORY_LEN = 240   # holds 15min of samples even at 5s polls
DEFAULT_POLL_SEC = 60


def parse_interval(s: str) -> int:
    """Parse '30', '30s', '2m', '1h' to seconds."""
    s = s.strip().lower()
    if not s:
        raise ValueError("empty interval")
    if s[-1] in "smh":
        n, unit = int(s[:-1]), s[-1]
        return {"s": n, "m": n * 60, "h": n * 3600}[unit]
    return int(s)


def _build_argparser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Real-time claude.ai usage TUI",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  python claude_usage_tui.py                    # 60s polls\n"
            "  python claude_usage_tui.py --interval 30s     # faster\n"
            "  python claude_usage_tui.py --plan 'Max 5x'    # custom label\n"
            "  python claude_usage_tui.py --once             # one snapshot\n"
            "  python claude_usage_tui.py --debug            # troubleshoot\n"
        ),
    )
    parser.add_argument("--interval", type=parse_interval,
                        default=DEFAULT_POLL_SEC, metavar="DURATION",
                        help="poll interval (e.g. 30, '30s', '2m', '1h')")
    parser.add_argument("--plan", default="Max",
                        help='plan label shown in title (e.g. "Max 5x")')
    parser.add_argument("--show-all", action="store_true",
                        help="show every bucket including 0%% / unknown")
    parser.add_argument("--no-fullscreen", action="store_true",
                        help="render inline instead of taking the full terminal")
    parser.add_argument("--once", action="store_true",
                        help="print one snapshot and exit")
    parser.add_argument("--cookie-file", default=None,
                        help="explicit path to cookies.sqlite")
    parser.add_argument("--debug", action="store_true",
                        help="diagnostic mode for troubleshooting")
    return parser


def _window_start(snap: Snapshot) -> datetime:
    if snap.five_hour_resets_at:
        return snap.five_hour_resets_at - timedelta(hours=5)
    return datetime.now(timezone.utc) - timedelta(hours=5)


def main() -> None:
    args = _build_argparser().parse_args()

    if args.debug:
        run_debug(args.cookie_file)
        return

    if args.interval < 5:
        print("warning: interval below 5s is wasteful — claude.ai caches at the edge.",
              file=sys.stderr)

    history: Deque[Tuple[datetime, Optional[float]]] = deque(maxlen=HISTORY_LEN)
    session = requests.Session()
    console = Console()

    def render(snap: Snapshot) -> Panel:
        ws = _window_start(snap)
        return render_dashboard(
            snap=snap,
            history=list(history),
            five_hour_buckets=five_hour_buckets(ws),
            daily_history=daily_history(),
            burners=attribute_recent(ws),
            poll_sec=args.interval,
            plan=args.plan,
            show_all=args.show_all,
        )

    if args.once:
        try:
            snap = fetch_usage(session, args.cookie_file)
            history.append((snap.fetched_at, snap.five_hour_pct))
            append_snapshot(snap)
            console.print(render(snap))
        except Exception as e:
            console.print(render_error(f"{type(e).__name__}: {e}"))
            sys.exit(1)
        return

    layout = Layout()
    layout.update(render_error("loading…"))
    with Live(layout, console=console, refresh_per_second=2,
              screen=not args.no_fullscreen) as live:
        while True:
            try:
                snap = fetch_usage(session, args.cookie_file)
                history.append((snap.fetched_at, snap.five_hour_pct))
                append_snapshot(snap)
                layout.update(render(snap))
                live.refresh()
            except KeyboardInterrupt:
                break
            except Exception as e:
                layout.update(render_error(
                    f"{type(e).__name__}: {e}\nretrying in {args.interval}s"
                ))
                live.refresh()
            try:
                time.sleep(args.interval)
            except KeyboardInterrupt:
                break


if __name__ == "__main__":
    main()
