#!/usr/bin/env python3
"""A desk credential: exactly one permission, and it is not a superuser entry.

The web desk must be able to type into a lane the owner is looking at. Granting it
the superuser allowlist would also let it close panes, start agents, reshape
workspaces and edit dunks - far more than "send this line", and all of it one bug
away from a web request.

So the credential is separate and narrow:

    ~/.config/dunkingsheep/desk.json
    {"token": "<random>", "panes": ["%1", "%2", "%4", "%9"]}

What it permits: send_text / send_keys to a pane in `panes`, nothing else.
What it refuses: every other command, every other pane, and every destructive verb -
with a named reason that says which rule refused. Each use is logged with the target
and the outcome, because a credential that cannot be audited is a hole with a name.

A token is not a security boundary against another local process (the socket already
trusts the filesystem); it is a boundary against the DESK being handed powers it does
not need, and it is checked here rather than in the web app so the web app cannot
widen it.
"""

from __future__ import annotations

import hmac
import json
import logging
import os
import secrets
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("DUNKINGSHEEP_DIR",
                                 str(Path.home() / ".config" / "dunkingsheep")))
PATH = CONFIG_DIR / "desk.json"      # tests override with DUNKINGSHEEP_DESK_FILE

# The whole permission. Anything not in this set is refused for a desk credential.
DESK_VERBS = frozenset({"send_text", "send_keys"})

log = logging.getLogger("dunkingsheep.desk")


class DeskError(RuntimeError):
    """A named refusal: the caller needs to know which rule stopped it."""


def _read(path=PATH):
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except OSError:
        return {}
    except ValueError:
        raise DeskError(f"{path} is not valid JSON; fix it or remove it") from None
    return data if isinstance(data, dict) else {}


class DeskCredential:
    """Token -> allowed pane set. Read on every check so an edit takes effect."""

    def __init__(self, path=None):
        self.path = Path(path or os.environ.get("DUNKINGSHEEP_DESK_FILE") or PATH)

    def issue(self, panes):
        """Create (or rotate) the token for this pane set. Returns the token once."""
        panes = [str(pane).strip() for pane in panes if str(pane).strip()]
        if not panes:
            raise DeskError("a desk credential must be given at least one pane")
        token = secrets.token_urlsafe(32)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = str(self.path) + ".tmp"
        with open(temporary, "w", encoding="utf-8") as handle:
            json.dump({"token": token, "panes": panes}, handle, indent=1)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, self.path)
        os.chmod(self.path, 0o600)
        return token

    def panes(self):
        return [str(p) for p in (_read(self.path).get("panes") or [])]

    def has_token(self):
        return bool((_read(self.path).get("token") or "").strip())

    def permits(self, command, target, token):
        """True only for a listed verb on a listed pane with the right token.

        Raises DeskError naming the rule for anything else, so a refused desk send
        reads as a permission boundary rather than as a broken button.
        """
        expected = (_read(self.path).get("token") or "").strip()
        if not expected or not token or not hmac.compare_digest(str(token), expected):
            raise DeskError("desk credential is not valid")
        if command not in DESK_VERBS:
            raise DeskError(
                f"a desk credential may only {', '.join(sorted(DESK_VERBS))}; "
                f"{command} needs a superuser")
        allowed = {str(pane) for pane in (_read(self.path).get("panes") or [])}
        if str(target) not in allowed:
            raise DeskError(
                f"a desk credential may not type into {target!r}; "
                f"allowed: {', '.join(sorted(allowed)) or 'none'}")
        log.info("desk-send %s -> %s", command, target)
        return True
