"""Local persistence: history JSONL, rollups, and per-instance attribution."""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import List, Optional, Tuple

from api import Snapshot
import paths


@dataclass
class HistoryEntry:
    ts: float
    five_hour: Optional[float]
    seven_day: Optional[float]
    sonnet_7d: Optional[float]
    extra_pct: Optional[float]


@dataclass
class InstanceUsage:
    name: str
    source: str   # "claude-code" or "cowork"
    tokens: int
    last_active: datetime


# --- History append/read --------------------------------------------------

def append_snapshot(snap: Snapshot) -> None:
    f = paths.history_file()
    f.parent.mkdir(parents=True, exist_ok=True)
    seven = snap.window("7-day all")
    sonnet = snap.window("Sonnet 7d")
    entry = HistoryEntry(
        ts=snap.fetched_at.timestamp(),
        five_hour=snap.five_hour_pct,
        seven_day=seven.utilization if seven else None,
        sonnet_7d=sonnet.utilization if sonnet else None,
        extra_pct=snap.extra.utilization if snap.extra else None,
    )
    with f.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(asdict(entry)) + "\n")


def read_recent(since: datetime) -> List[HistoryEntry]:
    f = paths.history_file()
    if not f.exists():
        return []
    cutoff = since.timestamp()
    out: List[HistoryEntry] = []
    with f.open(encoding="utf-8") as fh:
        for line in fh:
            try:
                d = json.loads(line)
                if d.get("ts", 0) >= cutoff:
                    out.append(HistoryEntry(**d))
            except (json.JSONDecodeError, TypeError):
                continue
    return out


# --- Rollups for charts ---------------------------------------------------

def five_hour_buckets(window_start: datetime,
                      bucket_minutes: int = 30
                      ) -> List[Tuple[datetime, Optional[float]]]:
    """Bucket the current 5h window. Future buckets stay None; past buckets
    with no sample carry forward (utilization is monotonic within a window).
    """
    n = 300 // bucket_minutes
    times = [window_start + timedelta(minutes=bucket_minutes * i) for i in range(n)]
    entries = read_recent(window_start)
    now_ts = datetime.now(timezone.utc).timestamp()
    window_end_ts = (window_start + timedelta(hours=5)).timestamp()
    buckets: List[Optional[float]] = [None] * n

    for e in entries:
        if e.five_hour is None:
            continue
        if e.ts < window_start.timestamp() or e.ts >= window_end_ts:
            continue
        idx = int((e.ts - window_start.timestamp()) / 60 / bucket_minutes)
        if 0 <= idx < n:
            cur = buckets[idx]
            buckets[idx] = e.five_hour if cur is None else max(cur, e.five_hour)

    last: Optional[float] = None
    for i in range(n):
        if times[i].timestamp() > now_ts:
            break
        if buckets[i] is None and last is not None:
            buckets[i] = last
        elif buckets[i] is not None:
            last = buckets[i]

    return list(zip(times, buckets))


def daily_history(days: int = 7) -> List[Tuple[datetime, Optional[float]]]:
    """Last N days of 7-day utilization, max per day, oldest first."""
    today = datetime.now().replace(hour=0, minute=0, second=0, microsecond=0)
    since = today - timedelta(days=days - 1)
    entries = read_recent(since)
    by_day: dict = {}
    for e in entries:
        if e.seven_day is None:
            continue
        day = datetime.fromtimestamp(e.ts).replace(
            hour=0, minute=0, second=0, microsecond=0)
        by_day[day] = max(by_day.get(day, 0), e.seven_day)
    return [(today - timedelta(days=i),
             by_day.get(today - timedelta(days=i)))
            for i in reversed(range(days))]


# --- Per-instance attribution from local JSONLs --------------------------

def attribute_recent(window_start: datetime) -> List[InstanceUsage]:
    cutoff = window_start.timestamp()
    out: List[InstanceUsage] = []

    cc_dir = paths.claude_code_dir()
    if cc_dir.exists():
        for project_dir in cc_dir.iterdir():
            if not project_dir.is_dir():
                continue
            tokens, last = _scan_jsonls(list(project_dir.glob("*.jsonl")), cutoff)
            if tokens > 0:
                out.append(InstanceUsage(
                    project_dir.name, "claude-code", tokens, last))

    cw_dir = paths.cowork_dir()
    if cw_dir.exists():
        # Cowork groups by user-uuid/org-uuid/local_session-uuid.
        for session_dir in cw_dir.glob("*/*/local_*"):
            if not session_dir.is_dir():
                continue
            tokens, last = _scan_jsonls(list(session_dir.rglob("*.jsonl")), cutoff)
            if tokens > 0:
                out.append(InstanceUsage(
                    f"cowork:{session_dir.name[6:14]}", "cowork", tokens, last))

    return sorted(out, key=lambda i: -i.tokens)


def _scan_jsonls(file_paths, cutoff: float) -> Tuple[int, datetime]:
    tokens = 0
    last_ts = cutoff
    for path in file_paths:
        try:
            with open(path, encoding="utf-8", errors="replace") as f:
                for line in f:
                    try:
                        d = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    ts = _extract_ts(d)
                    if ts is None or ts < cutoff:
                        continue
                    tokens += _extract_tokens(d)
                    if ts > last_ts:
                        last_ts = ts
        except OSError:
            continue
    return tokens, datetime.fromtimestamp(last_ts, tz=timezone.utc)


def _extract_ts(d: dict) -> Optional[float]:
    for key in ("timestamp", "created_at", "ts"):
        v = d.get(key)
        if isinstance(v, (int, float)):
            return float(v)
        if isinstance(v, str):
            try:
                return datetime.fromisoformat(v.replace("Z", "+00:00")).timestamp()
            except ValueError:
                continue
    return None


def _extract_tokens(d: dict) -> int:
    usage = d.get("usage") or d.get("message", {}).get("usage", {})
    if not isinstance(usage, dict):
        return 0
    return sum(int(usage.get(k, 0) or 0) for k in (
        "input_tokens", "output_tokens",
        "cache_read_input_tokens", "cache_creation_input_tokens",
    ))
