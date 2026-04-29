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

from api import Snapshot
from storage import InstanceUsage


BLOCKS = "▁▂▃▄▅▆▇█"
ALWAYS_SHOWN_LABELS = {"5-hour", "7-day all", "Sonnet 7d"}


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
                 height: int = 8, title: str = ""):
        self.values = values
        self.height = height
        self.title = title

    def __rich_console__(self, console, options):
        if not self.values:
            yield Panel(Text("no data yet", style="dim"),
                        title=self.title, border_style="dim")
            return
        # Subtract panel chrome (2 border + 2 padding from padding=(0,1))
        # and 5 chars for y-axis labels + corner.
        avail = options.max_width - 4 - 5
        slot_w = max(2, min(4, avail // len(self.values)))
        text = Text("\n".join(self._rows(slot_w)))
        yield Panel(Align.center(text),
                    title=self.title, border_style="cyan", padding=(0, 1))

    def _rows(self, slot_w: int) -> List[str]:
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

        rows: List[str] = []
        for r in range(height, 0, -1):
            y_label = y_marks.get(r, "    ")
            bars: List[str] = []
            for _, pct in self.values:
                if pct is None:
                    bars.append(empty)
                    continue
                bar_h = pct * height / 100
                if bar_h >= r:
                    bars.append("█" * bar_w + gap)
                elif bar_h > r - 1:
                    idx = min(7, max(0, int((bar_h - (r - 1)) * 8)))
                    bars.append(BLOCKS[idx] * bar_w + gap)
                else:
                    bars.append(empty)
            rows.append(f"{y_label}┤{''.join(bars)}")

        label_w = slot_w  # labels fill full slot; use 2-char abbrevs at slot=2
        rows.append("  0%└" + "─" * (len(self.values) * slot_w))
        rows.append("     " + "".join(
            f"{label[:label_w]:<{label_w}}" for label, _ in self.values))
        return rows


def render_bar_chart(values: List[Tuple[str, Optional[float]]],
                     height: int = 8, title: str = "") -> BarChart:
    return BarChart(values, height, title)


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


def render_burn_lines(history, snap: Snapshot) -> Group:
    rate = compute_rate(history)
    pct = snap.five_hour_pct
    resets = snap.five_hour_resets_at

    if rate is None or pct is None:
        return Group(
            Text.from_markup(f"[bold]{BURN_WINDOW_MINUTES}m burn:[/bold]    "
                             "[dim]— (need 2 recent samples)[/dim]"),
            Text.from_markup("[bold]→ 100% in[/bold]    "
                             "[dim]— (waiting for data)[/dim]"),
        )

    sign = "+" if rate >= 0 else ""
    bcolor = ("red" if rate > 0.5 else "orange3" if rate > 0.2
              else "yellow" if rate > 0.05 else "green")
    recent = _recent_window(history)
    minutes = (recent[-1][0] - recent[0][0]).total_seconds() / 60
    burn = Text.from_markup(
        f"[bold]{BURN_WINDOW_MINUTES}m burn:[/bold]   [{bcolor}]{sign}{rate:.2f}%/min[/]   "
        f"[dim](over last {minutes:.0f}m, {len(recent)} samples)[/dim]"
    )

    if rate <= 0.01 or pct >= 100:
        proj = Text.from_markup(
            "[bold]→ 100% in[/bold]   [green]steady[/]      [dim](no projection)[/dim]"
        )
    else:
        eta_min = (100 - pct) / rate
        eta_dt = datetime.now(timezone.utc) + timedelta(minutes=eta_min)
        if resets and eta_dt > resets:
            reset_local = resets.astimezone().strftime("%H:%M")
            proj = Text.from_markup(
                f"[bold]→ 100% in[/bold]   [green]{_fmt_min(eta_min)}[/]   "
                f"[dim](won't hit cap before reset at {reset_local})[/dim]"
            )
        else:
            ecolor = "red" if eta_min < 30 else "orange3" if eta_min < 60 else "yellow"
            proj = Text.from_markup(
                f"[bold]→ 100% in[/bold]   [{ecolor}]{_fmt_min(eta_min)}[/]   "
                f"[dim](on pace to hit cap before reset)[/dim]"
            )
    return Group(burn, proj)


def render_headroom(snap: Snapshot, rate: Optional[float]) -> Text:
    pct = snap.five_hour_pct
    if pct is None:
        return Text("")
    headroom = 100 - pct
    resets = snap.five_hour_resets_at

    if rate is None or rate < 0.05:
        return Text.from_markup(
            f"[bold]Headroom:[/bold] [green]{headroom:.0f}%[/]  ·  "
            "[dim]idle, safe to spin up agents[/dim]"
        )

    eta_min = headroom / rate if rate > 0 else float("inf")
    minutes_to_reset = (
        (resets - datetime.now(timezone.utc)).total_seconds() / 60
        if resets else float("inf")
    )

    if eta_min > minutes_to_reset:
        return Text.from_markup(
            f"[bold]Headroom:[/bold] [green]{headroom:.0f}%[/]  ·  "
            "safe to spin up another agent"
        )
    if eta_min < 20:
        return Text.from_markup(
            f"[bold]Headroom:[/bold] [red]{headroom:.0f}%[/]  ·  "
            f"[red]ETA cap in {eta_min:.0f}m — pause some agents now[/red]"
        )
    return Text.from_markup(
        f"[bold]Headroom:[/bold] [orange3]{headroom:.0f}%[/]  ·  "
        f"[orange3]throttle if possible — ETA cap in {eta_min:.0f}m[/orange3]"
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


def render_summary(snap: Snapshot, show_all: bool) -> Table:
    table = Table.grid(expand=True, padding=(0, 1))
    table.add_column("window", style="bold", width=14)
    table.add_column("pct", width=7, justify="right")
    table.add_column("bar", ratio=1)
    table.add_column("resets in", width=10, justify="right")

    for w in snap.windows:
        if (not show_all and w.label not in ALWAYS_SHOWN_LABELS
                and (w.utilization or 0) <= 0):
            continue
        if w.utilization is None:
            table.add_row(w.label, Text("inactive", style="dim"),
                          Text(""), Text("—", style="dim"))
            continue
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
) -> Panel:
    summary = render_summary(snap, show_all)
    burn_group = render_burn_lines(history, snap)

    extras_text: Text = Text("")
    if snap.extra and snap.extra.enabled:
        e = snap.extra
        c = color_for(e.utilization)
        extras_text = Text.from_markup(
            f"[bold]Extra usage:[/bold] [{c}]${e.used_credits:.2f}[/] / "
            f"${e.monthly_limit:.0f} {e.currency}  ([{c}]{e.utilization:.1f}%[/])"
        )

    # Hour labels only at on-the-hour buckets to avoid duplicates like "10 10".
    five_h = render_bar_chart(
        [(t.strftime("%H") if t.minute == 0 else "", p)
         for t, p in five_hour_buckets],
        height=8, title="5-hour window",
    )
    seven_d = render_bar_chart(
        [(d.strftime("%a"), p) for d, p in daily_history],
        height=8, title="Last 7 days",
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

    body = Group(
        summary,        Text(""),
        burn_group,     Text(""),
        extras_text,    Text(""),
        chart_row,      Text(""),
        burners_text,   headroom_row,
    )
    title = (f"[bold cyan]claude.ai usage[/bold cyan]  [dim]·[/dim]  "
             f"[bold]{plan}[/bold]")
    return Panel(body, title=title, border_style="cyan")


def render_error(msg: str) -> Panel:
    return Panel(
        Text(msg, style="red"),
        title="[bold red]claude.ai usage — error[/bold red]",
        border_style="red",
    )
