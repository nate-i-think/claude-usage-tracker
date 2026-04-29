"""Platform-aware paths for config, history, and Cowork session data."""

from __future__ import annotations

import os
import platform
import uuid
from pathlib import Path


def platform_key() -> str:
    """Return 'win', 'mac', or 'linux'."""
    s = platform.system()
    if s == "Windows":
        return "win"
    if s == "Darwin":
        return "mac"
    return "linux"


def config_dir() -> Path:
    """Where we persist the per-install anonymous_id."""
    return _user_data_root() / "claude_usage_tui"


def history_file() -> Path:
    return config_dir() / "history.jsonl"


def anonymous_id_file() -> Path:
    return config_dir() / "anonymous_id"


def cowork_dir() -> Path:
    """Where the Cowork desktop app stores local agent session JSONLs."""
    key = platform_key()
    if key == "win":
        return Path(os.environ.get("APPDATA", "")) / "Claude" / "local-agent-mode-sessions"
    if key == "mac":
        return Path.home() / "Library" / "Application Support" / "Claude" / "local-agent-mode-sessions"
    return Path.home() / ".config" / "Claude" / "local-agent-mode-sessions"


def claude_code_dir() -> Path:
    """Claude Code project session JSONLs (consistent across platforms)."""
    return Path.home() / ".claude" / "projects"


def get_or_create_anonymous_id() -> str:
    """Per-install anonymous tracking ID. Generated once, persisted, never
    transmitted with PII. Each install gets its own; we never ship a value
    that ties one user to another's identity."""
    f = anonymous_id_file()
    if f.exists():
        try:
            value = f.read_text(encoding="utf-8").strip()
            if value:
                return value
        except OSError:
            pass
    value = f"claudeai.v1.{uuid.uuid4()}"
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(value, encoding="utf-8")
    return value


def _user_data_root() -> Path:
    key = platform_key()
    if key == "win":
        if appdata := os.environ.get("APPDATA"):
            return Path(appdata)
    if key == "mac":
        return Path.home() / "Library" / "Application Support"
    if xdg := os.environ.get("XDG_DATA_HOME"):
        return Path(xdg)
    return Path.home() / ".local" / "share"
