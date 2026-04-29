"""Diagnostic --debug mode: dump cookies, try profiles, surface auth issues."""

from __future__ import annotations

import re
import time
from typing import Optional

from curl_cffi import requests
from rich.console import Console
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from api import (
    DEBUG_PROFILES, USAGE_HOST, USAGE_PATH,
    build_headers, detect_org_id, load_cookie_jar, normalize_expires,
)


def _mask(value: Optional[str], keep: int = 6) -> str:
    if value is None:
        return "<none>"
    if len(value) <= keep * 2 + 3:
        return "*" * len(value)
    return f"{value[:keep]}…{value[-keep:]} ({len(value)} chars)"


def run_debug(cookie_file: Optional[str]) -> None:
    console = Console()
    console.rule("[bold]claude_usage_tui — debug")

    try:
        jar, diag = load_cookie_jar(cookie_file, return_diagnostic=True)
    except Exception as e:
        console.print(f"[red]cookie load failed:[/red] {type(e).__name__}: {e}")
        return

    auto_impersonate = _print_profiles(console, diag)
    _print_cookie_inventory(console, jar)

    try:
        org_id = detect_org_id(jar)
    except Exception as e:
        console.print(f"\n[red]org detection failed:[/red] {e}")
        return

    url = USAGE_HOST + USAGE_PATH.format(org_id=org_id)
    console.print(f"\n[bold]Org ID:[/bold] {org_id}\n[bold]URL:[/bold]    {url}")

    headers = build_headers(jar)
    console.print(f"\n[bold]Sending headers:[/bold] {sorted(headers.keys())}")
    console.print(f"[bold]Auto-selected impersonation:[/bold] {auto_impersonate}")

    success = _try_profiles(console, url, headers, jar, auto_impersonate)
    _print_recommendation(console, jar, success)


def _print_profiles(console, diag) -> Optional[str]:
    """Render profile table; return the auto-selected impersonate string."""
    now = time.time()
    label = {3: "[green]live[/green]", 2: "[orange3]expired[/orange3]",
             1: "[yellow]no sessionKey[/yellow]", 0: "[dim]no claude.ai[/dim]",
             -1: "[red]read error[/red]"}
    t = Table(show_header=True, header_style="bold cyan",
              title="Profiles considered (highest score wins)")
    t.add_column("browser")
    t.add_column("path", overflow="fold")
    t.add_column("score", justify="right")
    t.add_column("sessionKey")
    t.add_column("impersonate")

    auto: Optional[str] = None
    for browser, path, score, exp, impersonate in diag:
        if exp is None:
            sk = "—"
        else:
            d = exp - now
            sk = (f"[red]expired {-d/86400:.1f}d ago[/red]" if d < 0
                  else f"[green]valid {d/3600:.1f}h[/green]")
        if auto is None and score >= 1:
            auto = impersonate
        bname = browser.name if hasattr(browser, "name") else str(browser)
        t.add_row(bname, path, label.get(score, "?"), sk, impersonate)
    console.print(t)
    return auto


def _print_cookie_inventory(console, jar) -> None:
    important = {"cf_clearance", "__cf_bm", "sessionKey", "lastActiveOrg",
                 "anthropic-device-id"}
    notes = {
        "cf_clearance":        "Cloudflare challenge clearance",
        "__cf_bm":             "Cloudflare bot management (~30m TTL)",
        "sessionKey":          "claude.ai auth",
        "lastActiveOrg":       "active org UUID",
        "anthropic-device-id": "device ID (cross-checked with header)",
    }
    now = time.time()
    cookies = [c for c in jar if c.domain.endswith("claude.ai")]
    console.print(f"\n[bold]Selected profile has {len(cookies)} claude.ai cookies[/bold]")

    inv = Table(show_header=True, header_style="bold cyan")
    inv.add_column("name")
    inv.add_column("value (masked)")
    inv.add_column("expires in", justify="right")
    inv.add_column("note")
    for c in sorted(cookies, key=lambda c: (c.name not in important, c.name)):
        normalized = normalize_expires(c.expires)
        if normalized is None:
            exp = "[dim]session[/dim]"
        else:
            d = normalized - now
            if d < 0:
                exp = f"[red]EXPIRED {-d/60:.0f}m ago[/red]"
            elif d < 600:
                exp = f"[orange3]{d/60:.0f}m[/orange3]"
            elif d < 86400:
                exp = f"{d/3600:.1f}h"
            else:
                exp = f"{d/86400:.1f}d"
        is_important = c.name in important
        inv.add_row(
            Text(c.name, style="bold" if is_important else "dim"),
            _mask(c.value), exp, notes.get(c.name, ""),
        )
    console.print(inv)


def _try_profiles(console, url, headers, jar,
                  auto_impersonate: Optional[str]) -> Optional[str]:
    profiles = DEBUG_PROFILES.copy()
    if auto_impersonate and auto_impersonate not in profiles:
        profiles.insert(0, auto_impersonate)

    console.print(f"\n[bold]Testing {len(profiles)} impersonation profiles[/bold]")
    results = Table(show_header=True, header_style="bold cyan")
    results.add_column("profile")
    results.add_column("status", justify="right")
    results.add_column("cf-ray")
    results.add_column("server")
    results.add_column("body sniff")

    success: Optional[str] = None
    for profile in profiles:
        try:
            resp = requests.Session().get(
                url, headers=headers, cookies=jar, timeout=15,
                impersonate=profile,
            )
            status = (f"[green]{resp.status_code}[/green]"
                      if resp.status_code == 200
                      else f"[red]{resp.status_code}[/red]")
            body = re.sub(r"sk-ant-\w{6,}", "sk-ant-***",
                          resp.text[:120].replace("\n", " "))
            results.add_row(profile, status,
                            resp.headers.get("cf-ray", "—")[:24],
                            resp.headers.get("server", "—"), body)
            if resp.status_code == 200 and success is None:
                success = profile
        except Exception as e:
            results.add_row(profile, "[red]ERR[/red]", "—", "—",
                            f"{type(e).__name__}: {e}"[:120])
    console.print(results)
    return success


def _print_recommendation(console, jar, success: Optional[str]) -> None:
    if success:
        console.print(Panel(
            Text.from_markup(
                f"[bold green]Success with profile:[/bold green] {success}\n"
                "Live mode will auto-select the matching profile based on which "
                "browser cookies came from — no edits required."
            ),
            border_style="green",
        ))
        return

    cookies = [c for c in jar if c.domain.endswith("claude.ai")]
    sess = next((c for c in cookies if c.name == "sessionKey"), None)
    cf_bm = next((c for c in cookies if c.name == "__cf_bm"), None)
    cf_clear = next((c for c in cookies if c.name == "cf_clearance"), None)
    now = time.time()

    lines = ["[bold red]All profiles failed.[/bold red]\n"]
    sess_exp = normalize_expires(sess.expires) if sess else None
    if sess and sess_exp and sess_exp < now:
        d = (now - sess_exp) / 86400
        lines.append(
            f"• [red]sessionKey is EXPIRED ({d:.0f}d ago)[/red] — "
            "this is the cause of 'Invalid authorization'.\n"
            "  Sign out of claude.ai, sign back in, visit "
            "claude.ai/settings/usage, then re-run."
        )
    elif not sess:
        lines.append("• [red]No sessionKey cookie.[/red] Sign in to claude.ai.")
    cf_bm_exp = normalize_expires(cf_bm.expires) if cf_bm else None
    if not cf_bm or (cf_bm_exp and cf_bm_exp < now):
        lines.append(
            "• [orange3]__cf_bm missing or expired[/orange3] — "
            "visit claude.ai/settings/usage to refresh.")
    if not cf_clear:
        lines.append(
            "• [orange3]cf_clearance missing[/orange3] — "
            "visit claude.ai to clear Cloudflare.")
    lines.append(
        "\nIf the body sniff says 'permission_error' but cookies look fresh, "
        "your anthropic-client-sha may be stale — refresh client_sha.txt.\n"
        "If it says something Cloudflare-y, your TLS impersonation may need "
        "updating — see DEBUG_PROFILES in api.py."
    )

    console.print(Panel(Text.from_markup("\n".join(lines)), border_style="red"))
