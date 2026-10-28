#!/usr/bin/env python3
"""Install the in-pane Codex turn producer into a Codex home.

This does not install Codex or change the shared checkout.  It copies the
producer next to `hooks.json` and, only when the turn hooks are absent, adds the
two entries.  An existing hooks.json that already points at
`dunkingsheep-turn.sh` is left byte-for-byte alone: Codex persists a trust hash
for that file, and rewriting it would make every lane need a fresh approval.

Usage:
    python3 integrations/codex/install.py [--home ~/.codex] [--check]

The producer is only *in-pane* when Codex is launched with `--no-daemon`.  With
the default shared app-server daemon the hook has no ancestry under the pane and
the daemon refuses the event, correctly.  The installer prints that requirement;
it never edits launch commands or restarts anything.
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
HOOK_ACTIONS = {"UserPromptSubmit": "start", "Stop": "end"}


def _entry(event: str, action: str, home: Path) -> dict:
    # Absolute path: hooks run through a shell, and a quoted $HOME is not
    # expanded.  The existing installation uses the same absolute form.
    # The wrapper takes start/end, not Codex's event name.
    return {"hooks": [{
        "command": f"bash '{home / WRAPPER}' {action}",
        "timeout": 15,
        "type": "command",
    }]}


def _already_wired(document: dict) -> bool:
    hooks = document.get("hooks") if isinstance(document, dict) else None
    if not isinstance(hooks, dict):
        return False
    for event in HOOK_ACTIONS:
        entries = hooks.get(event)
        if not isinstance(entries, list):
            return False
        commands = " ".join(
            str(hook.get("command", ""))
            for entry in entries if isinstance(entry, dict)
            for hook in (entry.get("hooks") or []) if isinstance(hook, dict)
        )
        if WRAPPER not in commands:
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
    parser.add_argument("--home", default=os.path.expanduser("~/.codex"),
                        help="Codex home directory (default: ~/.codex)")
    parser.add_argument("--check", action="store_true",
                        help="report what is installed; change nothing")
    args = parser.parse_args(argv)
    home = Path(args.home).expanduser()
    wrapper = home / WRAPPER
    helper = home / HELPER
    hooks_path = home / "hooks.json"

    helper_ok = helper.is_file() and wrapper.is_file()
    document = None
    error = None
    if hooks_path.is_file():
        try:
            document = json.loads(hooks_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            error = str(exc)
    wired = bool(document and _already_wired(document))
    if args.check:
        print(json.dumps({
            "home": str(home),
            "producer_files": helper_ok,
            "hooks_json": hooks_path.is_file(),
            "hooks_parse_error": error,
            "turn_hooks": wired,
            "launch_requirement": "codex --no-daemon (in-pane app-server)",
        }, indent=2, sort_keys=True))
        return 0 if (helper_ok and wired) else 1

    home.mkdir(parents=True, exist_ok=True)
    for source, target in ((HERE / "turn-generation.py", helper),
                           (HERE / WRAPPER, wrapper)):
        shutil.copyfile(source, target)
        os.chmod(target, 0o700)
    if document is None:
        document = {"hooks": {}}
    if error:
        raise SystemExit(f"refusing to rewrite unparsable {hooks_path}: {error}")
    if not wired:
        stamp = time.strftime("%Y%m%d-%H%M%S")
        backup = home / f"hooks.json.bak-dunkingsheep-{stamp}"
        if hooks_path.is_file():
            shutil.copyfile(hooks_path, backup)
        hooks_path.write_text(json.dumps(_merge(document, home), indent=2) + "\n",
                              encoding="utf-8")
        changed = f"added UserPromptSubmit/Stop hooks (backup: {backup.name})"
    else:
        changed = "hooks.json already wired; left unchanged (trust hash preserved)"
    print(f"installed {helper} and {wrapper}")
    print(changed)
    print("launch requirement: codex --no-daemon  # puts the app-server and hooks "
          "inside the pane's process ancestry")
    print("note: a fresh hooks.json needs one approval in the Codex TUI; "
          "replacing only the script keeps the existing approval")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
