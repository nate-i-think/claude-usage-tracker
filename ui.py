"""TUI rendering: panels, charts, dashboard composition."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import List, Optional, Tuple

from rich.align import Align
from rich.console import Group
from rich.panel import Panel
from rich.progress_bar import ProgressBar
from rich.table import Table
from rich.text import Text

from api import Snapshot, UsageWindow
from storage import InstanceUsage


BLOCKS = "▁▂▃▄▅▆▇█"
ALWAYS_SHOWN_LABELS = {"5-hour", "7-day all", "Fable"}
CHART_HEIGHT = 16   # bar-chart rows; minor ticks (25/50/75/100%) land on 4/8/12/16

# Claude coral/orange for the Fable overlay. Rich renders truecolor and
# downgrades gracefully on 256/16-color terminals.
FABLE_ORANGE = "#d97757"
# Fable is a WEEKLY metric, so its % is a share of the weekly Fable cap. To plot
# Fable's contribution on the *5-hour* chart we take the rise in Fable% since the
# window start and scale it by this ratio = (weekly Fable cap) / (5-hour cap),
# turning a weekly-% into a 5-hour-budget-%. It is NOT in the API.
#
# Calibrated directly from an all-Fable session: when every bit of usage is Fable,
# both meters track the same usage, so R = (5-hour burn) / (Fable weekly burn).
# Measured 1.06%/min ÷ 0.18%/min ≈ 5.9. One-line knob; re-measure the same way
# (during a pure-Fable burst) if your plan changes.
FABLE_WEEKLY_TO_5H_RATIO = 5.9

# On the 7-day chart the bars are 7-day-*all* utilization. Fable% is a % of the
# Fable cap, which is ~50% of the 7-day-all cap, so Fable's share of a 7-day-all
# bar is fable% × this. (Without it, Fable is drawn 2× too tall — it looked like
# a whole bar when it should be a slice.)
FABLE_TO_WEEKLY_ALL_RATIO = 0.5


class LastPolled:
    """Right-aligned freshness indicator. Re-renders every Live tick so the
    seconds-counter increments live without re-fetching from the server."""

    def __init__(self, fetched_at: datetime):
        self.fetched_at = fetched_at

    def __rich__(self) -> Text:
        secs = int((datetime.now(timezone.utc) - self.fetched_at).total_seconds())
        return Text(f"last polled {secs}s ago", style="dim")


# --- Color & formatting helpers -------------------------------------------

def color_for(pct: Optional[float]) -> str:
    if pct is None: return "dim"
    if pct >= 90:   return "red"
    if pct >= 75:   return "orange3"
    if pct >= 50:   return "yellow"
    return "green"


def humanize_until(dt: Optional[datetime]) -> str:
    if dt is None:
        return "—"
    secs = int((dt - datetime.now(timezone.utc)).total_seconds())
    if secs <= 0:
        return "now"
    h, rem = divmod(secs, 3600)
    if h >= 24:
        d, h = divmod(h, 24)
        return f"{d}d {h}h"
    return f"{h}h {rem // 60:02d}m"


def _fmt_min(m: float) -> str:
    h, mm = divmod(int(m), 60)
    return f"{h}h {mm:02d}m" if h else f"{mm}m"


# --- Bar chart ------------------------------------------------------------

class BarChart:
    """Vertical bar chart that adapts its slot width to fit the terminal.

    Slot widths chosen at render time based on available space:
      4 — full breathing room ("██  ", labels with trailing space)
      3 — compact ("██ ", labels touch)
      2 — dense ("█ ", thin bars, 2-letter labels)
    Always falls back gracefully so the chart never overflows its panel.
    """

    def __init__(self, values: List[Tuple[str, Optional[float]]],
                 height: int = 8, title: str = "",
                 orange_ceilings: Optional[List[Optional[float]]] = None):
        self.values = values
        self.height = height
        self.title = title
        # Per-slot y-axis % up to which the bar's bottom renders in Fable orange.
        self.orange_ceilings = orange_ceilings

    def __rich_console__(self, console, options):
        if not self.values:
            yield Panel(Text("no data yet", style="dim"),
                        title=self.title, border_style="dim")
            return
        # Subtract panel chrome (2 border + 2 padding from padding=(0,1))
        # and 5 chars for y-axis labels + corner.
        avail = options.max_width - 4 - 5
        slot_w = max(2, min(4, avail // len(self.values)))
        yield Panel(Align.center(self._render_text(slot_w)),
                    title=self.title, border_style="cyan", padding=(0, 1))

    def _render_text(self, slot_w: int) -> Text:
        height = self.height
        y_marks = {
            height:                       "100%",
            max(1, round(height * 0.75)): " 75%",
            max(1, round(height * 0.50)): " 50%",
            max(1, round(height * 0.25)): " 25%",
        }
        bar_w = max(1, slot_w - 1)
        gap = " " * (slot_w - bar_w)
        empty = " " * slot_w

        ceilings = self.orange_ceilings or []
        # Number of bottom rows to paint orange, per slot (0 when no Fable floor).
        orange = [_orange_rows(ceilings[i] if i < len(ceilings) else None, height)
                  for i in range(len(self.values))]

        text = Text()
        for r in range(height, 0, -1):
            text.append(f"{y_marks.get(r, '    ')}┤")
            for i, (_, pct) in enumerate(self.values):
                if pct is None:
                    text.append(empty)
                    continue
                bar_h = pct * height / 100
                # The bottom `orange[i]` filled rows carry the Fable-orange style;
                # cells above the bar stay blank, so it clips to the bar height.
                style = FABLE_ORANGE if r <= orange[i] else None
                if bar_h >= r:
                    text.append("█" * bar_w, style=style)
                    text.append(gap)
                elif bar_h > r - 1:
                    idx = min(7, max(0, int((bar_h - (r - 1)) * 8)))
                    text.append(BLOCKS[idx] * bar_w, style=style)
                    text.append(gap)
                else:
                    text.append(empty)
            text.append("\n")

        label_w = slot_w  # labels fill full slot; use 2-char abbrevs at slot=2
        text.append("  0%└" + "─" * (len(self.values) * slot_w) + "\n")
        text.append("     " + "".join(
            f"{label[:label_w]:<{label_w}}" for label, _ in self.values))
        return text


def _fable_5h_ceilings(fable_buckets: List[Tuple[datetime, Optional[float]]],
                       ratio: float) -> List[Optional[float]]:
    """Per-bucket orange ceilings (y-axis %) for the 5-hour chart.

    Fable% is a weekly figure, so only the *rise since the window start* reflects
    Fable used inside this 5-hour window. Scale that rise by `ratio` (weekly Fable
    cap / 5-hour cap) to express it on the 5-hour budget. Never negative; buckets
    with no Fable sample stay None. Clipping to each bar happens downstream.
    """
    baseline = next((v for _, v in fable_buckets if v is not None), None)
    if baseline is None:
        return [None for _ in fable_buckets]
    return [None if v is None else max(0.0, v - baseline) * ratio
            for _, v in fable_buckets]


def _fable_7d_ceilings(fable_daily: List[Tuple[datetime, Optional[float]]],
                       ratio: float) -> List[Optional[float]]:
    """Per-day orange ceilings (y-axis %) for the 7-day chart: each day's Fable%
    converted from the Fable cap to the 7-day-all cap via `ratio`. No-data days
    contribute 0 (no orange). Clipping to each bar happens downstream."""
    return [ratio * (v or 0) for _, v in fable_daily]


def _orange_rows(ceiling: Optional[float], height: int) -> int:
    """How many bottom chart rows fall within the orange Fable floor.

    `ceiling` is a y-axis percentage (0–100); returns the count of the lowest
    rows to paint orange, capped at the chart height. 0/None → no orange.
    """
    if not ceiling or ceiling <= 0:
        return 0
    # Guarantee at least one visible orange block for any nonzero Fable usage,
    # even when the floor would otherwise round down to zero rows.
    return max(1, min(height, round(ceiling * height / 100)))


def render_bar_chart(values: List[Tuple[str, Optional[float]]],
                     height: int = 8, title: str = "",
                     orange_ceilings: Optional[List[Optional[float]]] = None
                     ) -> BarChart:
    return BarChart(values, height, title, orange_ceilings)


# --- Burn rate, projection, headroom --------------------------------------

BURN_WINDOW_MINUTES = 15


def _recent_window(history, window_minutes: int = BURN_WINDOW_MINUTES):
    """Recent samples, trimmed to post-reset only.

    Utilization is monotonic-non-decreasing within a window, so a drop in
    samples can only mean we crossed a reset. After a reset the pre-reset
    samples are no longer meaningful for burn rate, so we anchor to the
    most recent reset point.
    """
    cutoff = datetime.now(timezone.utc) - timedelta(minutes=window_minutes)
    recent = [(t, p) for t, p in history if t >= cutoff and p is not None]
    reset_idx = 0
    for i in range(1, len(recent)):
        if recent[i][1] + 5 < recent[i - 1][1]:   # >5% drop = reset
            reset_idx = i
    return recent[reset_idx:]


def compute_rate(history: List[Tuple[datetime, Optional[float]]],
                 window_minutes: int = BURN_WINDOW_MINUTES) -> Optional[float]:
    """Burn rate (%/min) over the most recent `window_minutes` of history."""
    recent = _recent_window(history, window_minutes)
    if len(recent) < 2:
        return None
    minutes = (recent[-1][0] - recent[0][0]).total_seconds() / 60
    if minutes <= 0:
        return None
    return (recent[-1][1] - recent[0][1]) / minutes


def render_burn_line(tag: str, history, pct: Optional[float],
                     resets: Optional[datetime], tag_style: str) -> Text:
    """One combined line: '{TAG}: 15m burn {rate}, → 100% in {eta} (samples)'.

    Window-agnostic: pass the 5-hour history/pct/resets for the ALL line, or the
    Fable history/pct/resets for the FABLE line. The reason notes ("won't hit cap
    …" / "on pace …") are intentionally dropped — the ETA color still signals
    safe (green) vs at-risk (yellow/orange/red).
    """
    rate = compute_rate(history)
    # Pad the tag to the widest ("FABLE:") so the shorter "ALL:" still lines up.
    label = f"{tag}:"
    prefix = f"[{tag_style}]{label}[/]" + " " * (len("FABLE:") - len(label) + 1)

    if rate is None or pct is None:
        return Text.from_markup(
            f"{prefix}[bold]{BURN_WINDOW_MINUTES}m burn[/bold] [dim]—[/dim], "
            f"[bold]→ 100% in[/bold] [dim]— (need 2 recent samples)[/dim]"
        )

    sign = "+" if rate >= 0 else ""
    bcolor = ("red" if rate > 0.5 else "orange3" if rate > 0.2
              else "yellow" if rate > 0.05 else "green")
    recent = _recent_window(history)
    minutes = (recent[-1][0] - recent[0][0]).total_seconds() / 60

    if rate <= 0.01 or pct >= 100:
        eta = "[green]steady[/]"
    else:
        eta_min = (100 - pct) / rate
        eta_dt = datetime.now(timezone.utc) + timedelta(minutes=eta_min)
        if resets and eta_dt > resets:            # won't hit cap before reset
            eta = f"[green]{_fmt_min(eta_min)}[/]"
        else:
            ecolor = "red" if eta_min < 30 else "orange3" if eta_min < 60 else "yellow"
            eta = f"[{ecolor}]{_fmt_min(eta_min)}[/]"

    return Text.from_markup(
        f"{prefix}[bold]{BURN_WINDOW_MINUTES}m burn[/bold] "
        f"[{bcolor}]{sign}{rate:.2f}%/min[/], "
        f"[bold]→ 100% in[/bold] {eta} "
        f"[dim](over last {minutes:.0f}m, {len(recent)} samples)[/dim]"
    )


def render_headroom(snap: Snapshot, rate: Optional[float]) -> Text:
    """Lead with *projected use*: total 5h budget you'd have consumed by the time
    the window resets if the current 15m burn pace holds (already-used pct + rate
    %/min × minutes-to-reset). >100% means you're on pace to hit the cap first.
    Coloring/advisory match the cap-ETA thresholds (green safe, orange throttle,
    red imminent)."""
    pct = snap.five_hour_pct
    if pct is None:
        return Text("")
    headroom = 100 - pct
    resets = snap.five_hour_resets_at

    if rate is None or rate < 0.05:
        return Text.from_markup(
            f"[bold]Projected use:[/bold] [green]0%[/]  ·  "
            "[dim]idle, safe to spin up agents[/dim]"
        )

    eta_min = headroom / rate if rate > 0 else float("inf")
    minutes_to_reset = (
        (resets - datetime.now(timezone.utc)).total_seconds() / 60
        if resets else float("inf")
    )

    # Total projected use at reset = already-used + additional draw if the pace
    # holds. Can exceed 100% (intentional — signals how hard you'd blow the cap);
    # "—" if reset unknown.
    projected = (f"{pct + rate * minutes_to_reset:.0f}%"
                 if minutes_to_reset != float("inf") else "—")

    if eta_min > minutes_to_reset:
        return Text.from_markup(
            f"[bold]Projected use:[/bold] [green]{projected}[/]  ·  "
            "safe to spin up another agent"
        )
    if eta_min < 20:
        return Text.from_markup(
            f"[bold]Projected use:[/bold] [red]{projected}[/]  ·  "
            f"[red]ETA cap in {_fmt_min(eta_min)} — pause some agents now[/red]"
        )
    return Text.from_markup(
        f"[bold]Projected use:[/bold] [orange3]{projected}[/]  ·  "
        f"[orange3]throttle if possible — ETA cap in {_fmt_min(eta_min)}[/orange3]"
    )


# --- Top burners, summary table -------------------------------------------

def render_top_burners(burners: List[InstanceUsage], n: int = 5) -> Text:
    if not burners:
        return Text.from_markup(
            "[bold]Top burners (5h):[/bold]  [dim]no local activity yet[/dim]"
        )
    parts = [f"{b.name[:20]} [bold]{b.tokens / 1000:.0f}k[/bold]"
             for b in burners[:n]]
    return Text.from_markup("[bold]Top burners (5h):[/bold]  " + "  ·  ".join(parts))


def render_window_row(w: UsageWindow, show_all: bool) -> Optional[Table]:
    """One summary row as a single-row grid, or None if it should be hidden.

    Each row is its own grid with identical fixed column widths, so separate
    rows still align when stacked — which lets the dashboard interleave burn
    lines between them.
    """
    if (not show_all and w.label not in ALWAYS_SHOWN_LABELS
            and (w.utilization or 0) <= 0):
        return None
    table = Table.grid(expand=True, padding=(0, 1))
    table.add_column("window", style="bold", width=14)
    table.add_column("pct", width=7, justify="right")
    table.add_column("bar", ratio=1)
    table.add_column("resets in", width=10, justify="right")

    if w.utilization is None:
        table.add_row(w.label, Text("inactive", style="dim"),
                      Text(""), Text("—", style="dim"))
    else:
        c = color_for(w.utilization)
        bar = ProgressBar(total=100, completed=w.utilization,
                          complete_style=c, finished_style="red")
        table.add_row(w.label, Text(f"{w.utilization:5.1f}%", style=c),
                      bar, humanize_until(w.resets_at))
    return table


# --- Top-level dashboard --------------------------------------------------

def render_dashboard(
    snap: Snapshot,
    history: List[Tuple[datetime, Optional[float]]],
    five_hour_buckets: List[Tuple[datetime, Optional[float]]],
    daily_history: List[Tuple[datetime, Optional[float]]],
    burners: List[InstanceUsage],
    poll_sec: int,
    plan: str,
    show_all: bool,
    fable_history: Optional[List[Tuple[datetime, Optional[float]]]] = None,
    fable_daily: Optional[List[Tuple[datetime, Optional[float]]]] = None,
    fable_buckets: Optional[List[Tuple[datetime, Optional[float]]]] = None,
) -> Panel:
    fable_history = fable_history or []
    fable_daily = fable_daily or []
    fable_buckets = fable_buckets or []
    by_label = {w.label: w for w in snap.windows}
    fable_w = by_label.get("Fable")
    PRIMARY = ("5-hour", "7-day all", "Fable")

    # Bars grouped first (5-hour, 7-day all, Fable), then the burn lines
    # (ALL, FABLE), then any other windows.
    rows: List = []
    for label in ("5-hour", "7-day all"):
        w = by_label.get(label)
        if w is not None and (row := render_window_row(w, show_all)) is not None:
            rows.append(row)
    if fable_w is not None and (row := render_window_row(fable_w, show_all)) is not None:
        rows.append(row)
    rows.append(render_burn_line(
        "ALL", history, snap.five_hour_pct, snap.five_hour_resets_at,
        tag_style="bold cyan"))
    if fable_w is not None:
        rows.append(render_burn_line(
            "FABLE", fable_history, fable_w.utilization, fable_w.resets_at,
            tag_style=f"bold {FABLE_ORANGE}"))
    for w in snap.windows:
        if w.label in PRIMARY:
            continue
        if (row := render_window_row(w, show_all)) is not None:
            rows.append(row)

    # Show whenever there's a configured budget or accrued spend — claude.ai
    # flips is_enabled to false (e.g. disabled_reason "out_of_credits") while
    # the period's spend is still meaningful, so don't gate on `enabled`.
    extras_text: Text = Text("")
    e = snap.extra
    if e and (e.monthly_limit > 0 or e.used_credits > 0):
        c = color_for(e.utilization)
        extras_text = Text.from_markup(
            f"[bold]Extra usage:[/bold] [{c}]${e.used_credits:.2f}[/] / "
            f"${e.monthly_limit:.0f} {e.currency}  ([{c}]{e.utilization:.1f}%[/])"
        )
        if not e.enabled:
            reason = (e.disabled_reason or "off").replace("_", " ")
            extras_text.append(f"  (disabled: {reason})", style="dim")

    # Orange Fable floor. 5-hour chart: the rise in Fable% since the window start,
    # scaled from weekly-% to 5h-% by FABLE_WEEKLY_TO_5H_RATIO (see constant).
    # 7-day chart: each day's Fable% plotted directly — already on the weekly scale.
    five_ceilings = _fable_5h_ceilings(fable_buckets, FABLE_WEEKLY_TO_5H_RATIO)

    # Hour labels only at on-the-hour buckets to avoid duplicates like "10 10".
    five_h = render_bar_chart(
        [(t.strftime("%H") if t.minute == 0 else "", p)
         for t, p in five_hour_buckets],
        height=CHART_HEIGHT, title="5-hour window",
        orange_ceilings=five_ceilings,
    )
    seven_d = render_bar_chart(
        [(d.strftime("%a"), p) for d, p in daily_history],
        height=CHART_HEIGHT, title="Last 7 days",
        orange_ceilings=_fable_7d_ceilings(fable_daily, FABLE_TO_WEEKLY_ALL_RATIO),
    )

    chart_row = Table.grid(expand=True, padding=(0, 1))
    chart_row.add_column(ratio=1)
    chart_row.add_column(ratio=1)
    chart_row.add_row(five_h, seven_d)

    rate = compute_rate(history)
    headroom = render_headroom(snap, rate)
    burners_text = render_top_burners(burners)

    headroom_row = Table.grid(expand=True)
    headroom_row.add_column(justify="left", ratio=1)
    headroom_row.add_column(justify="right")
    headroom_row.add_row(headroom, LastPolled(snap.fetched_at))

    parts = list(rows)
    if extras_text.plain:
        parts.append(extras_text)
    parts += [chart_row, burners_text, headroom_row]
    body = Group(*parts)
    title = (f"[bold cyan]claude.ai usage[/bold cyan]  [dim]·[/dim]  "
             f"[bold]{plan}[/bold]")
    return Panel(body, title=title, border_style="cyan")


def render_error(msg: str) -> Panel:
    return Panel(
        Text(msg, style="red"),
        title="[bold red]claude.ai usage — error[/bold red]",
        border_style="red",
    )
