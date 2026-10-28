"""Shared Telegram routing reader; never opens bot credentials or pairing data."""

import json
import os
from pathlib import Path


def resolve_telegram_channel(data_dir, *, channels_root=None):
    """Resolve environment > local routing config > cos, validating desk IDs.

    Both bridge and watchdog use this reader. telegram-routing.json contains
    only {"channel": "..."}; telegram.json remains credential-only. Services
    must share the routing directory and any environment overrides.
    """
    data_dir = Path(data_dir)
    root = Path(channels_root) if channels_root is not None else data_dir.parent
    routing = data_dir / "telegram-routing.json"
    config = {}
    if routing.exists():
        config = json.loads(routing.read_text(encoding="utf-8"))
        if not isinstance(config, dict) or set(config) - {"channel"}:
            raise ValueError("Telegram routing config must contain only channel")
    channel = os.environ.get("VP_TELEGRAM_CHANNEL", config.get("channel", "cos"))
    channels_path = os.environ.get("VP_CHANNELS_FILE")
    if channels_path:
        registry = Path(channels_path).expanduser()
    else:
        registry = root / "channels.json"
        if not registry.exists():
            registry = root / "channels.example.json"
    channels = json.loads(registry.read_text(encoding="utf-8"))
    if not isinstance(channels, list) or not all(
        isinstance(row, dict) and isinstance(row.get("id"), str) for row in channels
    ):
        raise ValueError("Telegram desk channel registry is invalid")
    if not isinstance(channel, str) or channel not in {row["id"] for row in channels}:
        raise ValueError("Telegram channel must name a configured desk channel")
    return channel
