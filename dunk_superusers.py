#!/usr/bin/env python3
"""
The superuser allowlist for destructive Dunking Sheep operations.

Only panes/agents on this list may close panes, tabs or workspaces, stop the
daemon, manage dunks, spawn agents, or use the raw herdr passthrough. Everyone
else communicates through the inbox and task queue. The 2026-10-18 incident
was a free agent closing the VP's tab and taking nine panes with it; a
non-empty allowlist is required before any destructive operation, so the
default is to refuse rather than to guess.

The file is ~/.config/dunkingsheep/superusers.json:

    {"panes": ["w9:p8"], "names": ["VP of agentry", "owner"]}

A flat {"superusers": ["w9:p8", "VP of agentry"]} list is also accepted;
entries containing ':' are pane ids, everything else is a name. Names match a
pane's own label, tab label or workspace label (case-insensitively), which is
how a pane that reconnects under a new pane id is still recognised.

The write is atomic and the file is mode 0600. This module only owns the
store; the daemon decides who may mutate it.
"""

import json
import os
import tempfile
import time

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
SUPERUSERS_FILE = os.path.join(CONFIG_DIR, "superusers.json")
DEFAULT_VP_NAME = os.environ.get("DUNKINGSHEEP_VP_NAME") or "VP of agentry"
DEFAULT_OWNER_NAME = os.environ.get("DUNKINGSHEEP_OWNER_NAME") or "owner"


class SuperuserError(ValueError):
    """A caller mistake (bad entry, unreadable allowlist). Safe to show."""


def _classify(entry):
    entry = str(entry or "").strip()
    return ("pane", entry) if ":" in entry else ("name", entry)


class Superusers:
    """Pane ids and names allowed to perform destructive operations."""

    def __init__(self, path=SUPERUSERS_FILE, create=True):
        self.path = path
        self.panes = set()
        self.names = set()
        self._stamp = None
        if create:
            self.load(create=True)

    def _file_stamp(self):
        try:
            stat = os.stat(self.path)
            return (stat.st_mtime_ns, stat.st_size)
        except OSError:
            return None

    def refresh(self):
        """Re-read the allowlist if the file changed under us.

        The VP edited the file and the daemon kept serving its in-memory copy,
        so a pane that was on the list was not a superuser until the next restart
        (2026-10-24). A stat per check costs nothing; a restart does not.
        """
        stamp = self._file_stamp()
        if stamp == self._stamp:
            return False
        self.load()
        self._stamp = stamp
        return True

    # -- loading ------------------------------------------------------------

    def load(self, create=True):
        if not os.path.exists(self.path):
            if create:
                self.seed_defaults()
            return
        try:
            with open(self.path, "r", encoding="utf-8") as handle:
                data = json.load(handle)
        except (OSError, ValueError) as error:
            raise SuperuserError(f"cannot read {self.path}: {error}") from None
        if not isinstance(data, dict):
            raise SuperuserError(f"{self.path} must contain a JSON object")
        panes = list(data.get("panes") or ())
        names = list(data.get("names") or ())
        for entry in data.get("superusers") or ():
            kind, value = _classify(entry)
            (panes if kind == "pane" else names).append(value)
        self.panes = {str(value).strip() for value in panes if str(value).strip()}
        self.names = {str(value).strip() for value in names if str(value).strip()}
        self._stamp = self._file_stamp()

    def seed_defaults(self):
        """First run: the VP name, the owner name, and any panes the setup set.

        The names mean the VP pane is recognised even after its pane id
        changes, which is what makes recovery work.
        """
        self.names = {DEFAULT_VP_NAME, DEFAULT_OWNER_NAME}
        self.panes = set()
        for variable in ("DUNKINGSHEEP_VP_PANE", "DUNKINGSHEEP_OWNER_PANE"):
            value = (os.environ.get(variable) or "").strip()
            if value:
                self.panes.add(value)
        self.save()

    # -- persisting ---------------------------------------------------------

    def save(self):
        directory = os.path.dirname(os.path.abspath(self.path))
        os.makedirs(directory, exist_ok=True)
        payload = {
            "panes": sorted(self.panes),
            "names": sorted(self.names),
            "updated_at": round(time.time(), 3),
        }
        fd, temporary = tempfile.mkstemp(prefix="superusers-", dir=directory)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump(payload, handle, indent=2)
                handle.write("\n")
            os.replace(temporary, self.path)
            try:
                os.chmod(self.path, 0o600)
            except OSError:
                pass
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    # -- queries ------------------------------------------------------------

    def entries(self):
        return sorted(self.panes) + sorted(self.names)

    def empty(self):
        return not self.panes and not self.names

    def is_superuser(self, pane_id, pane_info=None):
        if not pane_id:
            return False
        pane_id = str(pane_id).strip()
        if pane_id in self.panes:
            return True
        wanted = {name.strip().lower() for name in self.names if name.strip()}
        if not wanted:
            return False
        for field in ("label", "tab_label", "workspace_label"):
            value = str((pane_info or {}).get(field) or "").strip().lower()
            if value and value in wanted:
                return True
        return False

    def describe(self):
        return {
            "path": self.path,
            "panes": sorted(self.panes),
            "names": sorted(self.names),
            "empty": self.empty(),
        }

    # -- mutation (the daemon authorises the caller) ------------------------

    def add(self, entry):
        kind, value = _classify(entry)
        if not value:
            raise SuperuserError("a pane id or name is required")
        self.panes.add(value) if kind == "pane" else self.names.add(value)
        self.save()
        return value

    def remove(self, entry):
        kind, value = _classify(entry)
        if not value:
            raise SuperuserError("a pane id or name is required")
        before = self.entries()
        if kind == "pane":
            self.panes.discard(value)
        else:
            self.names.discard(value)
        self.save()
        return value in before
