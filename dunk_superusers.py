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

import contextlib
import fcntl
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


def _lock_path(path):
    """Sidecar lock file beside the allowlist (same directory/filesystem)."""
    return str(path) + ".lock"


@contextlib.contextmanager
def _locked(path, exclusive):
    """Serialize allowlist writers against authorization readers (t423).

    Writers (save) take EX across replacement; the task completion transaction
    takes SH across its privilege re-read through the mutation. Lock ordering
    is one-directional (SQLite writer lock, then this file lock; save takes
    only the file lock), so no deadlock cycle exists. Locks release on close,
    so a killed holder cannot wedge readers."""
    with open(_lock_path(path), "a+b") as handle:
        fcntl.flock(handle.fileno(),
                    fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH)
        try:
            yield handle
        finally:
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


@contextlib.contextmanager
def locked_allowlist(path):
    """Hold a shared allowlist lock across check-through-mutation (t423).

    Yields (panes, names) read under SH. Callers keep the block open until the
    authorized mutation (and its audit event) are recorded, so a writer
    revoking between the check and the mutation blocks instead of slipping
    through; refusal inside the block rolls back with no effects."""
    with _locked(path, False):
        with open(path, "r", encoding="utf-8") as handle:
            yield _parse_allowlist(json.load(handle))


def _parse_allowlist(data):
    """(panes, names) sets from decoded JSON; shared by load and locked reads.

    A roles file (dunk_roles: {"lanes": {pane: {"role": ...}}}) is accepted
    too: its owner lanes are the panes, and it never yields names."""
    if not isinstance(data, dict):
        raise SuperuserError("allowlist must contain a JSON object")
    if isinstance(data.get("lanes"), dict) and "panes" not in data:
        owners = {str(pane).strip() for pane, entry in data["lanes"].items()
                  if isinstance(entry, dict) and entry.get("role") == "owner"}
        return (owners, set())
    panes = list(data.get("panes") or ())
    names = list(data.get("names") or ())
    for entry in data.get("superusers") or ():
        kind, value = _classify(entry)
        (panes if kind == "pane" else names).append(value)
    return ({str(value).strip() for value in panes if str(value).strip()},
            {str(value).strip() for value in names if str(value).strip()})


def match_allowlist(panes, names, pane_id, pane_info=None):
    """Pure membership test shared by cached and freshly-read sets (t423)."""
    if not pane_id:
        return False
    pane_id = str(pane_id).strip()
    if pane_id in (panes or ()):
        return True
    wanted = {name.strip().lower() for name in (names or ()) if name.strip()}
    if not wanted:
        return False
    for field in ("label", "tab_label", "workspace_label"):
        value = str((pane_info or {}).get(field) or "").strip().lower()
        if value and value in wanted:
            return True
    return False


def read_panes_locked(path):
    """(panes, names) re-read from disk under a shared lock (t423).

    Unlike cached attributes (refreshed opportunistically), this read is
    serialized against save() replacement, so a completion transaction holding
    it across its privilege check through its mutation cannot admit a
    revocation landing in between. Raises on unreadable content: callers fail
    closed (a corrupt allowlist grants nothing)."""
    with _locked(path, False):
        with open(path, "r", encoding="utf-8") as handle:
            return _parse_allowlist(json.load(handle))


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
        try:
            self.panes, self.names = _parse_allowlist(data)
        except SuperuserError as error:
            raise SuperuserError(f"{self.path}: {error}") from None
        self._stamp = self._file_stamp()

    def read_panes_locked(self):
        """(panes, names) re-read from disk under a shared lock (t423).

        Unlike the cached attributes (refreshed opportunistically), this read
        is serialized against save() replacement, so a completion transaction
        holding it across its privilege check through its mutation cannot admit
        a revocation landing in between. Raises on unreadable content: callers
        fail closed (a corrupt allowlist grants nothing)."""
        with _locked(self.path, False):
            with open(self.path, "r", encoding="utf-8") as handle:
                return _parse_allowlist(json.load(handle))

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
        # EX across write+replace: authorization readers holding SH across
        # their check-then-mutate window cannot admit this replacement
        # in between (t423). Readers without the lock still see whole old-or-new
        # files (atomic replace), just without ordering.
        with _locked(self.path, True):
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
        return match_allowlist(self.panes, self.names, pane_id, pane_info)

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
