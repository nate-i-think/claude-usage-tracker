"""Standalone tests for api.py packet parsing. Run: python test_api.py

No framework required — plain asserts + a __main__ runner (mirrors test_ui.py).
"""

import sys
from datetime import datetime

from api import parse_model_limits


# A trimmed copy of a real /usage packet (2026-07-01), when per-model limits
# moved out of the top-level seven_day_* keys into the `limits` array. Fable is
# the only model-scoped entry; session + weekly_all carry no model scope.
FABLE_PACKET = {
    "five_hour": {"utilization": 12.0, "resets_at": "2026-07-01T21:10:01+00:00"},
    "seven_day": {"utilization": 13.0, "resets_at": "2026-07-07T06:00:01+00:00"},
    "seven_day_sonnet": None,
    "limits": [
        {"kind": "session", "percent": 12,
         "resets_at": "2026-07-01T21:10:01+00:00", "scope": None},
        {"kind": "weekly_all", "percent": 13,
         "resets_at": "2026-07-07T06:00:01+00:00", "scope": None},
        {"kind": "weekly_scoped", "percent": 0, "resets_at": None,
         "scope": {"model": {"display_name": "Fable", "id": None},
                   "surface": None}},
    ],
}


def test_extracts_fable_window_from_limits_array():
    windows = parse_model_limits(FABLE_PACKET)
    labels = [w.label for w in windows]
    assert labels == ["Fable"], f"expected exactly [Fable], got {labels}"


def test_fable_zero_percent_maps_to_zero_utilization_not_none():
    # percent 0 must render as a real 0.0% bar, not "inactive" (which is what a
    # None utilization renders as). This is the whole point of "show it as a bar".
    fable = parse_model_limits(FABLE_PACKET)[0]
    assert fable.utilization == 0.0, f"expected 0.0, got {fable.utilization!r}"


def test_null_resets_at_parses_to_none():
    fable = parse_model_limits(FABLE_PACKET)[0]
    assert fable.resets_at is None, f"expected None, got {fable.resets_at!r}"


def test_active_model_limit_parses_percent_and_resets():
    data = {"limits": [
        {"kind": "weekly_scoped", "percent": 42,
         "resets_at": "2026-07-07T06:00:01+00:00",
         "scope": {"model": {"display_name": "Fable"}}},
    ]}
    fable = parse_model_limits(data)[0]
    assert fable.utilization == 42.0, f"expected 42.0, got {fable.utilization!r}"
    assert isinstance(fable.resets_at, datetime), \
        f"expected datetime, got {fable.resets_at!r}"


def test_ignores_session_and_weekly_all_entries():
    # Only model-scoped entries become windows; session/weekly_all have scope None.
    windows = parse_model_limits(FABLE_PACKET)
    assert all(w.label == "Fable" for w in windows), \
        f"non-model limit leaked in: {[w.label for w in windows]}"


def test_missing_limits_key_returns_empty_list():
    assert parse_model_limits({"five_hour": {"utilization": 5.0}}) == []


def test_limits_none_returns_empty_list():
    assert parse_model_limits({"limits": None}) == []


if __name__ == "__main__":
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
