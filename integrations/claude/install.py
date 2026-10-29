#!/usr/bin/env python3
"""Install the in-pane Claude turn producer into a Claude home.

This does not install Claude or change the shared checkout.  It copies the
producer next to `settings.json` and, only when the turn hooks are absent,
adds the three entries.  An existing settings file that already points at
`dunkingsheep-turn.sh` is left byte-for-byte alone, and unrelated hooks are
preserved.  A backup (`settings.json.bak-<epoch>`) is written whenever hooks
are changed.

Usage:
    python3 integrations/claude/install.py [--home ~/.claude] [--check]

Only ever run this for a TEST lane home after independent review.  It never
touches production settings on its own: `--home` selects the target, and
`--check` reports without changing anything.

Hook wiring (fixed by this installer, do not rewire by hand):
  UserPromptSubmit -> start   (opens a daemon turn, captures its token)
  Stop             -> end     (closes it with that token; Stop-shaped only)
  SubagentStop     -> noop    (a finishing subagent must never end the turn)

The producer is only *in-pane* when the hook process runs under the pane's
own Claude process: the daemon authenticates the caller by kernel ancestry
under the pane root and refuses anything else, so a detached adapter that
merely asserts a pane label is refused by design.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
WRAPPER = "dunkingsheep-turn.sh"
HELPER = "dunkingsheep-turn.py"
HOOK_ACTIONS = {"UserPromptSubmit": "start", "Stop": "end",
                "SubagentStop": "noop"}


def _entry(event: str, action: str, home: Path) -> dict:
    # Absolute path: hooks run through a shell, and a quoted $HOME is not
    # expanded.  The installer keeps the same absolute form on reinstall.
    # The wrapper takes start/end/noop, not Claude's event name.
    return {"hooks": [{
        "type": "command",
        "command": f"bash '{home / WRAPPER}' {action}",
        "timeout": 15,
    }]}


def _expected_command(event: str, action: str, home: Path) -> str:
    # The exact command this installer writes.  Matching is by equality, not
    # substring: a different absolute path (or a legacy hook that merely
    # mentions the wrapper name) must NOT count as wired, or --check would
    # bless a hook that runs someone else's producer.
    return f"bash '{home / WRAPPER}' {action}"


def _entry_commands(entry: dict) -> list:
    if not isinstance(entry, dict):
        return []
    hooks = entry.get("hooks")
    if not isinstance(hooks, list):
        return []
    return [hook.get("command") for hook in hooks if isinstance(hook, dict)]


def _already_wired(document: dict, home: Path) -> bool:
    hooks = document.get("hooks") if isinstance(document, dict) else None
    if not isinstance(hooks, dict):
        return False
    for event, action in HOOK_ACTIONS.items():
        entries = hooks.get(event)
        if not isinstance(entries, list):
            return False
        want = _expected_command(event, action, home)
        if not any(cmd == want
                   for entry in entries for cmd in _entry_commands(entry)):
            return False
    return True


def _merge(document: dict, home: Path) -> dict:
    document = dict(document)
    hooks = dict(document.get("hooks") or {})
    for event, action in HOOK_ACTIONS.items():
        existing = hooks.get(event)
        kept = []
        if isinstance(existing, list):
            for entry in existing:
                commands = " ".join(
                    str(hook.get("command", ""))
                    for hook in (entry.get("hooks") or [])
                    if isinstance(hook, dict))
                if WRAPPER not in commands:
                    kept.append(entry)
        hooks[event] = kept + [_entry(event, action, home)]
    document["hooks"] = hooks
    return document


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", default=os.path.expanduser("~/.claude"),
                        help="Claude home directory (default: ~/.claude). "
                             "Only ever point this at a TEST lane home until "
                             "the producer is independently accepted.")
    parser.add_argument("--check", action="store_true",
                        help="report what is installed; change nothing")
    args = parser.parse_args(argv)
    home = Path(args.home).expanduser()
    wrapper = home / WRAPPER
    helper = home / HELPER
    settings_path = home / "settings.json"

    helper_ok = helper.is_file() and wrapper.is_file()
    document = None
    error = None
    if settings_path.is_file():
        try:
            document = json.loads(settings_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            error = str(exc)
    wired = bool(document and _already_wired(document, home))
    if args.check:
        print(json.dumps({
            "home": str(home),
            "producer_files": helper_ok,
            "settings_json": settings_path.is_file(),
            "settings_parse_error": error,
            "turn_hooks": wired,
            "launch_requirement": "hook processes under the pane's own Claude process",
        }, indent=2, sort_keys=True))
        return 0 if (helper_ok and wired) else 1

    home.mkdir(parents=True, exist_ok=True)
    for name in ("turn-generation.py", "dunkingsheep-turn.sh"):
        shutil.copy(HERE / name, home / ("dunkingsheep-turn.py"
                                         if name == "turn-generation.py"
                                         else name))
    try:
        wrapper.chmod(0o755)
    except OSError:
        pass
    if document is None:
        if settings_path.is_file():
            print(f"settings.json unreadable ({error}); files installed, "
                  f"hooks not merged")
            return 0
        document = {}
    if _already_wired(document, home):
        print("turn hooks already wired; left unchanged")
        return 0
    backup = home / f"settings.json.bak-{int(time.time())}"
    if settings_path.is_file():
        shutil.copy(settings_path, backup)
    merged = _merge(document, home)
    settings_path.write_text(json.dumps(merged, indent=2) + "\n",
                             encoding="utf-8")
    print(f"turn hooks installed (backup: {backup.name if backup.is_file() else 'none'})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
