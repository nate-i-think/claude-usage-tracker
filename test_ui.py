"""Standalone tests for ui.py projection math. Run: python test_ui.py

No framework required — plain asserts + a __main__ runner.
"""

import re
import sys
from datetime import datetime, timedelta, timezone

from rich.console import Console

from api import ExtraUsage, Snapshot, UsageWindow
from ui import (
    _fable_5h_ceilings, _fable_7d_ceilings, _orange_rows, render_bar_chart,
    render_burn_line, render_dashboard, render_headroom,
)


def _snap(pct: float, minutes_to_reset: float) -> Snapshot:
    resets = datetime.now(timezone.utc) + timedelta(minutes=minutes_to_reset)
    return Snapshot(
        fetched_at=datetime.now(timezone.utc),
        windows=[UsageWindow("5-hour", pct, resets)],
        extra=None,
    )


def _render_dashboard_plain(snap: Snapshot) -> str:
    panel = render_dashboard(
        snap=snap, history=[], five_hour_buckets=[], daily_history=[],
        burners=[], poll_sec=60, plan="Max", show_all=False,
    )
    console = Console(width=120)
    with console.capture() as cap:
        console.print(panel)
    return cap.get()


def _projected_pct(snap: Snapshot, rate: float) -> int:
    plain = render_headroom(snap, rate).plain
    m = re.search(r"Projected use:\s*(\d+)%", plain)
    assert m, f"could not parse projected use from: {plain!r}"
    return int(m.group(1))


def test_projected_use_includes_already_used_portion():
    # 21% already used, 0.675%/min pace, 144 min to reset.
    # Future draw alone = 0.675 * 144 ≈ 97%, but TOTAL projected use must add
    # the 21% already spent → ≈ 118%, i.e. on pace to blow past the cap.
    snap = _snap(pct=21, minutes_to_reset=144)
    projected = _projected_pct(snap, rate=0.675)
    assert projected > 100, f"expected cap breach (>100%), got {projected}%"
    assert projected == 118, f"expected 21 + round(0.675*144)=118, got {projected}%"


def test_eta_cap_uses_hours_and_minutes():
    # eta_min = (100-21)/0.675 ≈ 117 min → must read "1h 57m", not "117m",
    # to match the usage bars / "→ 100% in" formatting.
    snap = _snap(pct=21, minutes_to_reset=144)
    plain = render_headroom(snap, 0.675).plain
    assert "1h 57m" in plain, f"expected Xh XXm ETA, got: {plain!r}"
    assert "117m" not in plain, f"raw-minutes ETA still present: {plain!r}"


def test_safe_pace_projects_below_100():
    # 10% used, gentle 0.1%/min, resets in 100 min → 10 + 10 = 20% total.
    snap = _snap(pct=10, minutes_to_reset=100)
    projected = _projected_pct(snap, rate=0.1)
    assert projected == 20, f"expected 20%, got {projected}%"
    assert "safe" in render_headroom(snap, 0.1).plain


def test_extra_usage_shows_when_disabled_but_has_spend():
    # claude.ai reports is_enabled=False (e.g. disabled_reason "out_of_credits")
    # while real spend is still accrued for the period. The dashboard must keep
    # showing the spend instead of hiding the whole line.
    snap = _snap(pct=10, minutes_to_reset=120)
    snap.extra = ExtraUsage(
        enabled=False, monthly_limit=35.0, used_credits=22.63,
        utilization=64.66, currency="USD", disabled_reason="out_of_credits",
    )
    out = _render_dashboard_plain(snap)
    assert "Extra usage" in out, f"extra-usage line missing:\n{out}"
    assert "22.63" in out, f"spend amount missing:\n{out}"


def test_extra_usage_hidden_when_no_budget_and_no_spend():
    # No configured limit and nothing spent → nothing to show.
    snap = _snap(pct=10, minutes_to_reset=120)
    snap.extra = ExtraUsage(
        enabled=False, monthly_limit=0.0, used_credits=0.0,
        utilization=0.0, currency="USD", disabled_reason=None,
    )
    out = _render_dashboard_plain(snap)
    assert "Extra usage" not in out, f"extra-usage line shown unexpectedly:\n{out}"


def _history(points):
    """[(minutes_ago, pct), ...] -> [(datetime, pct), ...] oldest first."""
    now = datetime.now(timezone.utc)
    return [(now - timedelta(minutes=m), p) for m, p in points]


# --- Combined burn line ---------------------------------------------------

def test_all_burn_line_is_single_combined_line():
    # burn + projection collapse into ONE line with the tag, rate, ETA, samples.
    hist = _history([(10, 20.0), (2, 24.0)])  # 0.5%/min
    line = render_burn_line(
        "ALL", hist, pct=24.0,
        resets=datetime.now(timezone.utc) + timedelta(hours=3),
        tag_style="bold cyan").plain
    assert line.count("\n") == 0, f"expected single line, got:\n{line}"
    for token in ("ALL:", "15m burn", "%/min", "→ 100% in", "(over last"):
        assert token in line, f"missing {token!r} in: {line!r}"


def test_all_burn_no_data_single_line():
    line = render_burn_line("ALL", [], pct=None, resets=None,
                            tag_style="bold cyan").plain
    assert line.count("\n") == 0, f"expected single line, got:\n{line}"
    assert "ALL:" in line
    assert "need 2 recent samples" in line


def test_fable_burn_uses_its_own_history():
    all_hist = _history([(10, 20.0), (2, 24.0)])   # 0.50%/min
    fable_hist = _history([(10, 8.0), (2, 8.2)])   # ~0.03%/min
    all_line = render_burn_line("ALL", all_hist, 24.0, None, "bold cyan").plain
    fable_line = render_burn_line("FABLE", fable_hist, 8.2, None,
                                  "bold #d97757").plain
    assert "FABLE:" in fable_line
    assert "0.50%/min" in all_line, all_line
    assert "0.50%/min" not in fable_line, fable_line  # independent sourcing


def test_reason_notes_dropped_from_combined_line():
    hist = _history([(10, 20.0), (2, 24.0)])
    resets = datetime.now(timezone.utc) + timedelta(minutes=5)  # won't-hit-cap path
    line = render_burn_line("ALL", hist, 24.0, resets, "bold cyan").plain
    assert "won't hit cap" not in line, line
    assert "on pace to hit cap" not in line, line


# --- Layout order ---------------------------------------------------------

def test_summary_order_bars_grouped_then_burns():
    snap = _snap_with_windows(
        UsageWindow("7-day all", 14.0,
                    datetime.now(timezone.utc) + timedelta(days=5)),
        UsageWindow("Fable", 8.0, None),
    )
    out = _render_dashboard_plain(snap)
    lines = out.splitlines()
    def idx(sub):
        return next(i for i, l in enumerate(lines) if sub in l)
    # Bars first (5-hour, 7-day all, Fable), then the burn lines (ALL, FABLE).
    assert idx("7-day all") < idx("Fable") < idx("ALL:") < idx("FABLE:"), \
        f"bad order:\n{out}"


# --- Orange Fable floor on charts -----------------------------------------

def test_orange_rows_math():
    assert _orange_rows(25.0, 16) == 4    # bottom 25% of 16 rows
    assert _orange_rows(0.0, 16) == 0
    assert _orange_rows(None, 16) == 0


def test_fable_5h_ceilings_scale_change_since_window_start():
    # Fable is a WEEKLY metric; the 5h chart should show only the rise since the
    # window start, scaled by R (weekly cap / 5h cap) to reach 5h-budget terms.
    now = datetime.now(timezone.utc)
    buckets = [(now, 4.0), (now, 5.0), (now, 6.0)]  # window start at 4%
    assert _fable_5h_ceilings(buckets, 2.0) == [0.0, 2.0, 4.0]


def test_fable_5h_ceilings_all_none_when_no_fable_data():
    now = datetime.now(timezone.utc)
    assert _fable_5h_ceilings([(now, None), (now, None)], 2.0) == [None, None]


def test_fable_7d_ceilings_scale_by_weekly_cap_fraction():
    # fable% is a % of the Fable cap, which is only ~50% of the 7-day-all cap, so
    # its share of the 7-day-all bar is fable% x 0.5 (None days -> 0, no orange).
    now = datetime.now(timezone.utc)
    daily = [(now, 10.0), (now, None), (now, 4.0)]
    assert _fable_7d_ceilings(daily, 0.5) == [5.0, 0.0, 2.0]


def test_fable_5h_ceilings_never_negative():
    # A reset/None dip must not produce a negative (upward-inverted) floor.
    now = datetime.now(timezone.utc)
    assert _fable_5h_ceilings([(now, 5.0), (now, 4.0)], 3.0) == [0.0, 0.0]


def test_orange_rows_at_least_one_when_nonzero():
    # Any nonzero Fable usage must show at least one orange block, even if the
    # floor would otherwise round down to zero rows (e.g. Fable 6% -> 3% floor).
    assert _orange_rows(3.0, 16) == 1
    assert _orange_rows(0.4, 16) == 1


def test_all_and_fable_burn_tags_align():
    hist = _history([(10, 20.0), (2, 24.0)])
    all_line = render_burn_line("ALL", hist, 24.0, None, "bold cyan").plain
    fable_line = render_burn_line("FABLE", hist, 24.0, None, "bold #d97757").plain
    assert all_line.index("15m burn") == fable_line.index("15m burn"), \
        f"tags misaligned:\n{all_line!r}\n{fable_line!r}"


def _capture_colored(renderable) -> str:
    console = Console(force_terminal=True, color_system="truecolor", width=60)
    with console.capture() as cap:
        console.print(renderable)
    return cap.get()


def test_chart_has_claude_orange_when_ceiling_set():
    chart = render_bar_chart([("a", 50.0), ("b", 50.0)], height=16, title="t",
                             orange_ceilings=[25.0, 25.0])
    assert "217;119;87" in _capture_colored(chart), "expected #d97757 truecolor"


def test_chart_has_no_orange_when_ceiling_zero():
    chart = render_bar_chart([("a", 50.0)], height=16, title="t",
                             orange_ceilings=[0.0])
    assert "217;119;87" not in _capture_colored(chart)


def _snap_with_windows(*windows: UsageWindow) -> Snapshot:
    """A snapshot with a live 5-hour window plus whatever extra windows a test
    wants to assert on (e.g. a Fable or Sonnet row)."""
    resets = datetime.now(timezone.utc) + timedelta(minutes=120)
    return Snapshot(
        fetched_at=datetime.now(timezone.utc),
        windows=[UsageWindow("5-hour", 10.0, resets), *windows],
        extra=None,
    )


def test_fable_row_pinned_even_at_zero_percent():
    # Fable replaces Sonnet as the always-shown model row. At 0% a non-pinned row
    # would be filtered out, so this proves "Fable" is in the always-shown set.
    snap = _snap_with_windows(UsageWindow("Fable", 0.0, None))
    out = _render_dashboard_plain(snap)
    assert "Fable" in out, f"Fable row missing at 0%:\n{out}"


def test_sonnet_row_no_longer_pinned():
    # Sonnet is unpinned: an inactive (None) Sonnet window must not render.
    snap = _snap_with_windows(UsageWindow("Sonnet 7d", None, None))
    out = _render_dashboard_plain(snap)
    assert "Sonnet" not in out, f"Sonnet still pinned when inactive:\n{out}"


def _interior_lines(out: str) -> list:
    """Panel rows with the border pipes and surrounding whitespace stripped."""
    return [ln.strip().strip("|").strip() for ln in out.splitlines()]


def test_no_blank_spacer_rows_between_sections():
    snap = _snap(pct=10, minutes_to_reset=120)
    snap.extra = ExtraUsage(
        enabled=True, monthly_limit=35.0, used_credits=10.0,
        utilization=28.6, currency="USD",
    )
    lines = _interior_lines(_render_dashboard_plain(snap))
    burn_idx = next(i for i, l in enumerate(lines) if "100% in" in l)
    extra_idx = next(i for i, l in enumerate(lines) if "Extra usage" in l)
    assert extra_idx == burn_idx + 1, (
        f"expected Extra usage on the row right after the burn line (no blank "
        f"spacer), got burn@{burn_idx} extra@{extra_idx}")


def test_charts_have_sixteen_bar_rows():
    now = datetime.now(timezone.utc)
    snap = _snap(pct=10, minutes_to_reset=120)
    panel = render_dashboard(
        snap=snap, history=[], five_hour_buckets=[(now, 50.0)],
        daily_history=[], burners=[], poll_sec=60, plan="Max", show_all=False,
    )
    console = Console(width=120)
    with console.capture() as cap:
        console.print(panel)
    out = cap.get()
    bar_rows = sum(1 for ln in out.splitlines() if "┤" in ln)  # '┤'
    assert bar_rows == 16, f"expected 16 bar rows, got {bar_rows}:\n{out}"


if __name__ == "__main__":
    # Rendered dashboards embed unicode (arrows, block glyphs); keep the runner
    # from crashing on a legacy-codepage console when a failure dumps them.
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, OSError):
        pass
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS {name}")
            except AssertionError as e:
                failures += 1
                print(f"FAIL {name}: {e}")
    raise SystemExit(failures)
