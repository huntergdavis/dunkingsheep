#!/usr/bin/env python3
"""Install the in-pane OpenCode turn producer.

Copies `dunkingsheep-turn.js` into <config>/opencode/plugins/.  OpenCode loads
every file in that directory as a server plugin, so no config edit is needed.
The plugin calls the daemon without a pane label; the daemon authenticates the
spawned CLI by ancestry under the pane root.  A shared/detached OpenCode server
is refused and no turn is recorded.

Usage:
    python3 integrations/opencode/install.py [--config-dir ~/.config] [--check]
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
from pathlib import Path

HERE = Path(__file__).resolve().parent
NAME = "dunkingsheep-turn.js"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config-dir",
                        default=os.environ.get("XDG_CONFIG_HOME")
                        or os.path.expanduser("~/.config"),
                        help="XDG config root (default: $XDG_CONFIG_HOME or ~/.config)")
    parser.add_argument("--check", action="store_true",
                        help="report status; change nothing")
    args = parser.parse_args(argv)
    target = Path(args.config_dir).expanduser() / "opencode" / "plugins" / NAME
    if args.check:
        print(json.dumps({
            "plugin": str(target),
            "installed": target.is_file(),
            "disconnect": "in-memory token lost on restart; daemon TTL is the backstop",
        }, indent=2, sort_keys=True))
        return 0 if target.is_file() else 1
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(HERE / NAME, target)
    os.chmod(target, 0o644)
    print(f"installed {target}")
    print("no config edit: OpenCode loads plugins/ automatically")
    print("disconnect: an OpenCode restart loses the token; the daemon TTL "
          "expires the turn instead of guessing idle")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
