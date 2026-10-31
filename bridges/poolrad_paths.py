"""Where the Poolrad checkouts live, independent of where this release is unpacked.

A release is an immutable tree under releases/<sha>/, so `ROOT.parent` is releases/,
not the workspace (t1128: the operations pane showed Poolrad "not found"). Resolution
order for every Poolrad path: the named environment variable, then
~/.config/dunkingsheep/poolrad.json, then ~/workspace/<default>. The tree's own files
(the vendored dashboard script) stay relative to the tree, never to its parent.
"""
import json
import os
from pathlib import Path

CONFIG = Path(os.environ.get("DUNKINGSHEEP_POOLRAD_CONFIG",
                             Path.home() / ".config" / "dunkingsheep" / "poolrad.json"))
DEFAULT_REPO = Path.home() / "workspace" / "poolrad-decomp"


def _configured(key):
    try:
        value = json.loads(CONFIG.read_text(encoding="utf-8")).get(key)
    except (OSError, ValueError, AttributeError):
        return None
    return Path(value).expanduser() if value else None


def repo(*env_names, key="repo"):
    """The Poolrad checkout: first set env var in `env_names`, else config, else default."""
    for name in env_names:
        if os.environ.get(name):
            return Path(os.environ[name]).expanduser()
    return _configured(key) or DEFAULT_REPO
