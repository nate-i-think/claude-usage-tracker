"""claude.ai usage API: cookies, headers, fetch, models, browser support."""

from __future__ import annotations

import glob
import os
import time
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Callable, Dict, List, Optional, Tuple

import browser_cookie3
from curl_cffi import requests

import paths


USAGE_HOST = "https://claude.ai"
USAGE_PATH = "/api/organizations/{org_id}/usage"

# anthropic-client-sha is the deployed web-client build hash. Stored in a
# sibling client_sha.txt file so users can refresh without editing source.
# Resolution order: env var (one-off override) > client_sha.txt > baked default.
# To refresh: open claude.ai/settings/usage with F12 → Network → Fetch/XHR,
# reload, find the "/usage" request, copy the anthropic-client-sha header,
# and paste it into client_sha.txt.
def _load_client_sha() -> str:
    if env := os.environ.get("CLAUDE_USAGE_TUI_CLIENT_SHA"):
        return env
    sha_file = os.path.join(os.path.dirname(__file__), "client_sha.txt")
    if os.path.exists(sha_file):
        try:
            value = open(sha_file, encoding="utf-8").read().strip()
            if value and len(value) >= 8:
                return value
        except OSError:
            pass
    return "dc0cedc76e6502966b76c5ddc9e3411719f69f54"


ANTHROPIC_CLIENT_SHA = _load_client_sha()

DEFAULT_IMPERSONATE = "firefox147"
DEBUG_PROFILES = ["firefox147", "firefox144", "chrome146", "chrome145", "chrome142"]

BUCKET_LABELS = [
    ("five_hour",            "5-hour"),
    ("seven_day",            "7-day all"),
    ("seven_day_sonnet",     "Sonnet 7d"),
    ("seven_day_opus",       "Opus 7d"),
    ("seven_day_cowork",     "Cowork 7d"),
    ("seven_day_oauth_apps", "OAuth 7d"),
    ("seven_day_omelette",   "Omelette 7d"),
    ("iguana_necktie",       "Iguana"),
    ("omelette_promotional", "Omelette promo"),
]
ALWAYS_SHOW = {"five_hour", "seven_day", "seven_day_sonnet"}


# --- Models ---------------------------------------------------------------

@dataclass
class UsageWindow:
    label: str
    utilization: Optional[float]
    resets_at: Optional[datetime]


@dataclass
class ExtraUsage:
    enabled: bool
    monthly_limit: float
    used_credits: float
    utilization: float
    currency: str


@dataclass
class Snapshot:
    fetched_at: datetime
    windows: List[UsageWindow]
    extra: Optional[ExtraUsage]

    def window(self, label: str) -> Optional[UsageWindow]:
        return next((w for w in self.windows if w.label == label), None)

    @property
    def five_hour_pct(self) -> Optional[float]:
        w = self.window("5-hour")
        return w.utilization if w else None

    @property
    def five_hour_resets_at(self) -> Optional[datetime]:
        w = self.window("5-hour")
        return w.resets_at if w else None


# --- Browser support ------------------------------------------------------

@dataclass
class BrowserDef:
    """A supported browser: where to find its cookies, how to load them, and
    which TLS fingerprint to impersonate so cf_clearance stays valid."""
    name: str
    loader: Callable
    impersonate: str
    paths: Dict[str, List[str]] = field(default_factory=dict)


# Path patterns use $APPDATA, $LOCALAPPDATA, ~/, and shell-style globs. All
# expanded at runtime via os.path.expandvars + expanduser.
BROWSERS: List[BrowserDef] = [
    # Firefox-family — same cookies.sqlite format, same firefox TLS fingerprint
    BrowserDef("Firefox", browser_cookie3.firefox, "firefox147", paths={
        "win":   ["$APPDATA/Mozilla/Firefox/Profiles/*/cookies.sqlite"],
        "mac":   ["~/Library/Application Support/Firefox/Profiles/*/cookies.sqlite"],
        "linux": ["~/.mozilla/firefox/*/cookies.sqlite",
                  "~/snap/firefox/common/.mozilla/firefox/*/cookies.sqlite"],
    }),
    BrowserDef("Zen", browser_cookie3.firefox, "firefox147", paths={
        "win":   ["$APPDATA/zen/Profiles/*/cookies.sqlite",
                  "$APPDATA/Zen/Profiles/*/cookies.sqlite"],
        "mac":   ["~/Library/Application Support/zen/Profiles/*/cookies.sqlite"],
        "linux": ["~/.zen/*/cookies.sqlite"],
    }),
    BrowserDef("LibreWolf", browser_cookie3.librewolf, "firefox147", paths={
        "win":   ["$APPDATA/LibreWolf/Profiles/*/cookies.sqlite",
                  "$APPDATA/librewolf/Profiles/*/cookies.sqlite"],
        "mac":   ["~/Library/Application Support/LibreWolf/Profiles/*/cookies.sqlite"],
        "linux": ["~/.librewolf/*/cookies.sqlite"],
    }),
    BrowserDef("Floorp", browser_cookie3.firefox, "firefox147", paths={
        "win":   ["$APPDATA/Floorp/Profiles/*/cookies.sqlite"],
        "mac":   ["~/Library/Application Support/Floorp/Profiles/*/cookies.sqlite"],
        "linux": ["~/.floorp/*/cookies.sqlite"],
    }),
    BrowserDef("Waterfox", browser_cookie3.firefox, "firefox147", paths={
        "win":   ["$APPDATA/Waterfox/Profiles/*/cookies.sqlite"],
        "mac":   ["~/Library/Application Support/Waterfox/Profiles/*/cookies.sqlite"],
        "linux": ["~/.waterfox/*/cookies.sqlite"],
    }),
    # Chromium-family — different cookie storage, Chrome TLS fingerprint.
    # Cookies file moved from <profile>/Cookies to <profile>/Network/Cookies
    # in Chrome 96 (Nov 2021); we glob both locations.
    BrowserDef("Chrome", browser_cookie3.chrome, "chrome146", paths={
        "win":   ["$LOCALAPPDATA/Google/Chrome/User Data/*/Network/Cookies",
                  "$LOCALAPPDATA/Google/Chrome/User Data/*/Cookies"],
        "mac":   ["~/Library/Application Support/Google/Chrome/*/Network/Cookies",
                  "~/Library/Application Support/Google/Chrome/*/Cookies"],
        "linux": ["~/.config/google-chrome/*/Network/Cookies",
                  "~/.config/google-chrome/*/Cookies"],
    }),
    BrowserDef("Edge", browser_cookie3.edge, "chrome146", paths={
        "win":   ["$LOCALAPPDATA/Microsoft/Edge/User Data/*/Network/Cookies",
                  "$LOCALAPPDATA/Microsoft/Edge/User Data/*/Cookies"],
        "mac":   ["~/Library/Application Support/Microsoft Edge/*/Network/Cookies",
                  "~/Library/Application Support/Microsoft Edge/*/Cookies"],
        "linux": ["~/.config/microsoft-edge/*/Network/Cookies"],
    }),
    BrowserDef("Brave", browser_cookie3.brave, "chrome146", paths={
        "win":   ["$LOCALAPPDATA/BraveSoftware/Brave-Browser/User Data/*/Network/Cookies",
                  "$LOCALAPPDATA/BraveSoftware/Brave-Browser/User Data/*/Cookies"],
        "mac":   ["~/Library/Application Support/BraveSoftware/Brave-Browser/*/Network/Cookies"],
        "linux": ["~/.config/BraveSoftware/Brave-Browser/*/Network/Cookies"],
    }),
    # Safari (macOS only). Requires Full Disk Access for the running terminal.
    BrowserDef("Safari", browser_cookie3.safari, "safari260", paths={
        "mac": ["~/Library/Cookies/Cookies.binarycookies"],
    }),
]


def _expand(p: str) -> str:
    return os.path.expanduser(os.path.expandvars(p))


def find_cookie_files() -> List[Tuple[BrowserDef, str]]:
    """Enumerate (browser, cookie_file_path) for every profile on this OS."""
    key = paths.platform_key()
    found: List[Tuple[BrowserDef, str]] = []
    for browser in BROWSERS:
        for pattern in browser.paths.get(key, []):
            for path in glob.glob(_expand(pattern)):
                found.append((browser, path))
    return found


def _detect_browser_for_path(path: str) -> BrowserDef:
    """Pick a BrowserDef from filename (used with --cookie-file)."""
    name = os.path.basename(path).lower()
    if name.endswith(".sqlite"):
        return next(b for b in BROWSERS if b.name == "Firefox")
    if name.endswith(".binarycookies"):
        return next(b for b in BROWSERS if b.name == "Safari")
    if name == "cookies":
        return next(b for b in BROWSERS if b.name == "Chrome")
    return next(b for b in BROWSERS if b.name == "Firefox")  # safe default


# --- Cookie scoring & selection -------------------------------------------

def normalize_expires(ts) -> Optional[float]:
    """Detect ms vs s vs us and return seconds-since-epoch, or None."""
    if not ts:
        return None
    ts = float(ts)
    if ts > 1e14:
        return ts / 1_000_000
    if ts > 1e11:
        return ts / 1_000
    return ts


def score_profile(jar) -> Tuple[int, Optional[float]]:
    """3=live sessionKey, 2=expired, 1=no sessionKey, 0=no claude.ai cookies."""
    now = time.time()
    has_claude = False
    session_exp: Optional[float] = None
    for c in jar:
        if not c.domain.endswith("claude.ai"):
            continue
        has_claude = True
        if c.name == "sessionKey":
            session_exp = normalize_expires(c.expires)
    if not has_claude:
        return 0, None
    if session_exp is None:
        return 1, None
    return (3 if session_exp > now else 2), session_exp


def _try_load(browser: BrowserDef, path: str):
    """Call browser.loader, swallowing PermissionError (Safari w/o Full Disk Access)
    so we degrade gracefully and keep evaluating other profiles."""
    try:
        return browser.loader(cookie_file=path, domain_name="claude.ai"), None
    except PermissionError as e:
        return None, f"PermissionError: {e}"
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"


def load_cookie_jar(explicit_path: Optional[str] = None,
                    return_diagnostic: bool = False):
    """Load claude.ai cookies, picking the freshest profile across all browsers.

    Returns either jar or (jar, diagnostic_list). On --cookie-file, diagnostic
    is a single-entry list. On auto, it's all candidates with their scores.
    """
    if explicit_path:
        if not os.path.exists(explicit_path):
            raise RuntimeError(f"--cookie-file does not exist: {explicit_path}")
        browser = _detect_browser_for_path(explicit_path)
        jar, err = _try_load(browser, explicit_path)
        if jar is None:
            raise RuntimeError(f"Couldn't read {explicit_path}: {err}")
        if return_diagnostic:
            score, exp = score_profile(jar)
            return jar, [(browser, explicit_path, score, exp, browser.impersonate)]
        return jar, browser.impersonate

    candidates = find_cookie_files()
    if not candidates:
        raise RuntimeError(
            "Couldn't find any browser profile on this system.\n"
            "Tried: " + ", ".join(b.name for b in BROWSERS) + ".\n"
            "Pass --cookie-file PATH if your profile lives elsewhere."
        )

    scored = []
    for browser, path in candidates:
        jar, err = _try_load(browser, path)
        if jar is None:
            scored.append((-1, None, browser, path, None, err))
            continue
        score, exp = score_profile(jar)
        scored.append((score, exp, browser, path, jar, None))
    scored.sort(key=lambda t: (-t[0], -(t[1] or 0)))

    diagnostic = [(b, p, score, exp, b.impersonate)
                  for (score, exp, b, p, _, _) in scored]

    if scored and scored[0][0] >= 1 and scored[0][4] is not None:
        best_jar = scored[0][4]
        best_browser = scored[0][2]
        if return_diagnostic:
            return best_jar, diagnostic
        return best_jar, best_browser.impersonate

    raise RuntimeError(
        "No browser profile has usable claude.ai cookies. "
        "Sign in to claude.ai, then retry."
    )


def detect_org_id(jar) -> str:
    for c in jar:
        if c.name == "lastActiveOrg":
            return c.value
    raise RuntimeError("No lastActiveOrg cookie. Visit claude.ai/settings/usage.")


# --- Headers --------------------------------------------------------------

STATIC_HEADERS = {
    "Referer": f"{USAGE_HOST}/settings/usage",
    "anthropic-client-platform": "web_claude_ai",
    "anthropic-client-version": "1.0.0",
    "anthropic-client-sha": ANTHROPIC_CLIENT_SHA,
    "content-type": "application/json",
}


def build_headers(jar) -> dict:
    headers = dict(STATIC_HEADERS)
    headers["anthropic-anonymous-id"] = paths.get_or_create_anonymous_id()
    for c in jar:
        if c.name == "anthropic-device-id" and c.domain.endswith("claude.ai"):
            headers["anthropic-device-id"] = c.value
            break
    headers["x-activity-session-id"] = str(uuid.uuid4())
    return headers


# --- Fetch ----------------------------------------------------------------

def _parse_dt(s: Optional[str]) -> Optional[datetime]:
    if not s:
        return None
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def _parse_window(label: str, raw) -> UsageWindow:
    if raw is None:
        return UsageWindow(label, None, None)
    return UsageWindow(label, raw.get("utilization"),
                       _parse_dt(raw.get("resets_at")))


def fetch_usage(session: requests.Session,
                cookie_file: Optional[str] = None,
                impersonate: Optional[str] = None) -> Snapshot:
    """Fetch one usage snapshot. Auto-selects browser+impersonation profile."""
    jar, auto_impersonate = load_cookie_jar(cookie_file)
    org_id = detect_org_id(jar)
    url = USAGE_HOST + USAGE_PATH.format(org_id=org_id)

    resp = session.get(url, headers=build_headers(jar), cookies=jar,
                       timeout=15, impersonate=impersonate or auto_impersonate)
    if resp.status_code == 401:
        raise PermissionError("401 — session expired. Sign in to claude.ai.")
    if resp.status_code == 403:
        raise PermissionError(f"403 — auth or Cloudflare: {resp.text[:200]}")
    resp.raise_for_status()
    data = resp.json()

    windows = []
    for key, label in BUCKET_LABELS:
        raw = data.get(key)
        w = _parse_window(label, raw)
        if key in ALWAYS_SHOW or w.utilization is not None:
            windows.append(w)

    extra = None
    if (er := data.get("extra_usage")):
        # API returns dollar amounts in cents.
        extra = ExtraUsage(
            enabled=bool(er.get("is_enabled", False)),
            monthly_limit=float(er.get("monthly_limit") or 0) / 100,
            used_credits=float(er.get("used_credits") or 0) / 100,
            utilization=float(er.get("utilization") or 0),
            currency=er.get("currency") or "USD",
        )

    return Snapshot(datetime.now(timezone.utc), windows, extra)
