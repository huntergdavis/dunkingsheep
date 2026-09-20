#!/usr/bin/env python3
"""
Dunking Sheep core: the flock of dunkers and their timers.

This module is the single mechanism behind every control surface. The TUI's
keyboard, the unix-socket API, the command-line tool and the MCP server all
end up calling methods on `Flock`. Nothing in here knows about curses, sockets
or JSON-RPC; it only knows dunkers, herdr panes and time.

A *dunker* (or *dunk*) is: a target herdr pane, a text, an interval, and a
running flag. While running, a per-dunker thread counts down and delivers the
text (plus Enter) to the pane through `HerdrClient`, then starts over.

Orchestration extras on top of the classic Dunking Bird model:

- text templates: `{id}`, `{name}`, `{count}`, `{target}`, `{interval}`,
  `{time}` and `{date}` expand at send time (other braces are left alone),
  so a dunk can tell the agent it lands on which dunk to remove.
- `only_when="idle"`: defer a send while herdr reports the target agent as
  working or blocked; re-check every few seconds.
- `max_sends=N`: remove the dunk automatically after its N-th send.
- `skip_if_unconsumed=True`: backpressure. A scheduled send becomes a no-op
  when the previous send is still sitting unread in the target's input box (see
  `Flock._previous_send_consumed`). Judged by reading the pane, never by whether
  the agent is merely busy.
- `hold_while_typing=True` (the default): never type over a human. Before a
  send the daemon reads the target's input box with its styling and, if the box
  holds text someone is composing, holds the send and re-checks every minute
  until the box is clear (see `Flock._human_typing`).
- persistence: the flock is saved to a JSON file on every structural change
  and restored (running dunks included) when a new flock is created.
- herdr layout control: `list_workspaces`, `create_workspace`, `create_tab`,
  `split_pane`, `start_agent`, `rename`, `focus`, `close`, plus the
  `herdr_command` passthrough for anything else the herdr CLI can do. An admin
  agent builds its team here, then dunks on it.
"""

import json
import logging
import os
import re
import shlex
import threading
import time

from herdr_client import HerdrClient

VERSION = "2.3.0"

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
STATE_FILE = os.path.join(CONFIG_DIR, "dunks.json")

# Agent states during which an `only_when="idle"` dunk holds its send.
BUSY_STATUSES = ("working", "blocked")
IDLE_POLL_S = 5
# A restored dunk whose scheduled send is already in the past fires after this.
RESTORE_GRACE_S = 5
DEFAULT_TEXT = "continue"
DEFAULT_INTERVAL_MINUTES = 10.0
ONLY_WHEN_CHOICES = (None, "idle")
# How many trailing pane lines to inspect for a still-unconsumed previous send,
# and how much of that text to look for (whitespace-collapsed).
# Reading the pane's visible screen to decide whether a send was consumed.
CONSUMPTION_READ_LINES = 40
CONSUMPTION_PREFIX_CHARS = 80
# Prompt sigils that mark the start of an agent's input box (Claude ❯, Codex ›).
INPUT_BOX_SIGILS = ("\u276f", "\u203a", ">")
# Substrings an agent shows *inside the input box* when it has collapsed a long
# or pasted input it has not yet submitted. Matched only within the box region,
# so transcript-collapse chips ("ctrl + t to view transcript") never count.
COLLAPSED_INPUT_MARKERS = (
    "ctrl+o to expand",
    "paste again to expand",
    "pasted text",
    "pasted content",
    "[image",
)
# While a human is typing in the target's input box, a dunk re-checks this often.
TYPING_RECHECK_S = 60
# A queued direct message re-checks this often (it is waiting to be delivered,
# not scheduled for later, so it should land soon after the human finishes).
MESSAGE_RECHECK_S = 5
# The box must read clear on two checks this far apart before anything is
# sent, so a pause between thoughts is not mistaken for being done.
TYPING_SETTLE_S = 1.0
# Delivered / failed / cancelled messages kept for list_messages.
MESSAGE_HISTORY = 50

TEMPLATE_RE = re.compile(r"\{(id|name|count|target|interval|time|date)\}")
# CSI escape sequences (colours, dim, cursor moves) as `herdr pane read
# --format ansi` emits them.
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
# Characters agents draw around the input box that are never typed input:
# braille animation dots, box-drawing rules and block glyphs, whitespace.
DECORATION_RE = re.compile(r"[─-▟⠀-⣿\s]+")

log = logging.getLogger("dunkingsheep")


class DunkError(ValueError):
    """A caller mistake (bad id, bad interval, ambiguous target). Safe to show."""


def format_interval(minutes):
    """Human form of an interval: '10', '1.5', or '30s' for under a minute.
    Every form round-trips through parse_interval."""
    minutes = float(minutes)
    if 0 < minutes < 1:
        return f"{minutes * 60:g}s"
    if minutes == int(minutes):
        return str(int(minutes))
    return f"{minutes:g}"


def parse_interval(value):
    """Accept minutes as a number or a string like '60', '1.5h', '90s', '2m'."""
    if isinstance(value, bool):
        raise DunkError("interval must be a number of minutes")
    if isinstance(value, (int, float)):
        minutes = float(value)
    else:
        text = str(value).strip().lower()
        match = re.fullmatch(r"([0-9]*\.?[0-9]+)\s*(s|sec|secs|m|min|mins|h|hr|hrs|)", text)
        if not match:
            raise DunkError(f"bad interval {value!r} (try 10, 90s, 1.5h)")
        number, unit = float(match.group(1)), match.group(2)
        if unit.startswith("s"):
            minutes = number / 60.0
        elif unit.startswith("h"):
            minutes = number * 60.0
        else:
            minutes = number
    if not minutes > 0:
        raise DunkError("interval must be positive")
    return minutes


def expand_template(text, dunker):
    """Expand the known `{placeholders}` in `text`; leave other braces alone."""
    now = time.localtime()
    values = {
        "id": dunker.id,
        "name": dunker.name or dunker.id,
        "count": str(dunker.send_count + 1),
        "target": dunker.target_label(),
        "interval": format_interval(dunker.interval_minutes),
        "time": time.strftime("%H:%M:%S", now),
        "date": time.strftime("%Y-%m-%d", now),
    }
    return TEMPLATE_RE.sub(lambda m: values[m.group(1)], text)


class Dunker:
    """One dunk. Plain data plus a little presentation logic."""

    FIELDS = (
        "id", "name", "target_pane_id", "target_agent", "target_tab",
        "target_workspace", "target_cwd", "interval_minutes", "text",
        "running", "only_when", "max_sends", "send_count", "created_at",
        "started_at", "last_sent_at", "next_send_at", "last_error",
        "skip_if_unconsumed", "skip_count", "last_skipped_at",
        "awaiting_consumption", "last_sent_text",
        "hold_while_typing", "hold_count", "last_held_at",
    )

    def __init__(self, id, **kwargs):
        self.id = id
        self.name = kwargs.get("name") or ""
        self.target_pane_id = kwargs.get("target_pane_id")
        self.target_agent = kwargs.get("target_agent")
        self.target_tab = kwargs.get("target_tab")
        self.target_workspace = kwargs.get("target_workspace")
        self.target_cwd = kwargs.get("target_cwd")
        self.interval_minutes = float(
            kwargs.get("interval_minutes") or DEFAULT_INTERVAL_MINUTES
        )
        self.text = kwargs.get("text", DEFAULT_TEXT)
        if self.text is None:
            self.text = DEFAULT_TEXT
        self.running = bool(kwargs.get("running", False))
        self.only_when = kwargs.get("only_when") or None
        self.max_sends = kwargs.get("max_sends") or None
        self.send_count = int(kwargs.get("send_count") or 0)
        self.created_at = kwargs.get("created_at") or time.time()
        self.started_at = kwargs.get("started_at")
        self.last_sent_at = kwargs.get("last_sent_at")
        self.next_send_at = kwargs.get("next_send_at")
        self.last_error = kwargs.get("last_error")
        # Backpressure (all persisted; old state files simply lack them).
        self.skip_if_unconsumed = bool(kwargs.get("skip_if_unconsumed", False))
        self.skip_count = int(kwargs.get("skip_count") or 0)
        self.last_skipped_at = kwargs.get("last_skipped_at")
        # True from a successful send until the target is seen consuming it.
        self.awaiting_consumption = bool(kwargs.get("awaiting_consumption", False))
        self.last_sent_text = kwargs.get("last_sent_text")
        # Never type over a human (persisted; old state files default to on).
        self.hold_while_typing = bool(kwargs.get("hold_while_typing", True))
        self.hold_count = int(kwargs.get("hold_count") or 0)
        self.last_held_at = kwargs.get("last_held_at")
        # Volatile (not persisted).
        self.status = kwargs.get("status", "Ready")
        self.stop_event = None
        self.thread = None

    # -- presentation -------------------------------------------------------

    def target_label(self):
        if not self.target_pane_id:
            return "(no target)"
        tab = self.target_tab or self.target_pane_id
        if self.target_agent:
            return f"{tab} / {self.target_agent}"
        return tab

    def set_target(self, pane):
        """Point at a herdr pane dict (as returned by HerdrClient.list_panes_grouped)."""
        self.target_pane_id = pane.get("pane_id")
        self.target_agent = pane.get("agent")
        self.target_cwd = pane.get("cwd")
        self.target_tab = pane.get("tab_label") or pane.get("tab_id")
        self.target_workspace = pane.get("workspace_label") or pane.get("workspace_id")

    def to_dict(self):
        data = {field: getattr(self, field) for field in self.FIELDS}
        data["status"] = self.status
        data["target_label"] = self.target_label()
        if self.running and self.next_send_at:
            data["next_send_in_s"] = max(0, int(round(self.next_send_at - time.time())))
        else:
            data["next_send_in_s"] = None
        return data

    @classmethod
    def from_dict(cls, data):
        data = dict(data)
        dunk_id = data.pop("id")
        data.pop("status", None)
        data.pop("target_label", None)
        data.pop("next_send_in_s", None)
        return cls(dunk_id, **data)


class Message:
    """One direct message waiting in (or delivered from) a pane's outbox."""

    def __init__(self, id, pane_id, target_label, text, source, deliver):
        self.id = id
        self.pane_id = pane_id
        self.target_label = target_label
        self.text = text
        self.source = source          # "send_text" or the dunk id for a fired dunk
        self.deliver = deliver        # callable -> (ok, detail)
        self.status = "queued"        # queued | sent | failed | cancelled
        self.detail = None
        self.holds = 0
        self.created_at = time.time()
        self.finished_at = None
        self.done = threading.Event()

    def _queued_message(self):
        if self.holds:
            return f"queued: someone is typing in {self.target_label}"
        return f"queued for {self.target_label}, delivering once the input box is clear"

    def to_dict(self):
        queued = self.status == "queued"
        return {
            "message_id": self.id, "pane_id": self.pane_id, "target_label": self.target_label,
            "text": self.text, "source": self.source, "status": self.status,
            "queued": queued, "ok": None if queued else self.status == "sent",
            "message": (self._queued_message() if queued else self.detail or self.status),
            "holds": self.holds, "created_at": self.created_at, "finished_at": self.finished_at,
            "waiting_s": int(time.time() - self.created_at) if queued else None,
        }


class Flock:
    """Owns every dunker and its timer thread. Thread-safe."""

    def __init__(self, herdr=None, state_file=STATE_FILE, persist=True,
                 restore=True):
        self.herdr = herdr or HerdrClient()
        self.state_file = state_file
        self.persist_enabled = persist
        self._lock = threading.RLock()
        self.send_lock = threading.Lock()
        self.dunkers = []
        self.next_id = 1
        self.started_at = time.time()
        self.last_message = ""
        self.closed = False
        # Direct-message outbox: pane_id -> [Message], plus the per-pane
        # delivery thread and a bounded history of finished messages.
        self.outbox = {}
        self.outbox_threads = {}
        self.message_history = []
        self.next_message_id = 1
        if restore:
            self._restore()

    # -- persistence --------------------------------------------------------

    def _restore(self):
        if not self.persist_enabled or not os.path.exists(self.state_file):
            return
        try:
            with open(self.state_file, "r", encoding="utf-8") as handle:
                payload = json.load(handle)
        except (OSError, ValueError) as error:
            log.warning("could not read %s: %s", self.state_file, error)
            return
        with self._lock:
            self.next_id = int(payload.get("next_id") or 1)
            for data in payload.get("dunkers", []):
                try:
                    dunker = Dunker.from_dict(data)
                except Exception as error:  # noqa: BLE001 - skip bad rows
                    log.warning("skipping corrupt dunk %r: %s", data, error)
                    continue
                dunker.status = "Restored"
                self.dunkers.append(dunker)
                self.next_id = max(self.next_id, _numeric_id(dunker.id) + 1)
            for dunker in list(self.dunkers):
                if dunker.running:
                    now = time.time()
                    if not dunker.next_send_at or dunker.next_send_at < now:
                        dunker.next_send_at = now + RESTORE_GRACE_S
                    self._spawn_timer(dunker)
        log.info("restored %d dunk(s) from %s", len(self.dunkers), self.state_file)

    def _save(self):
        if not self.persist_enabled:
            return
        with self._lock:
            payload = {
                "version": 1,
                "next_id": self.next_id,
                "dunkers": [
                    {field: getattr(d, field) for field in Dunker.FIELDS}
                    for d in self.dunkers
                ],
            }
        try:
            os.makedirs(os.path.dirname(self.state_file), exist_ok=True)
            tmp = self.state_file + ".tmp"
            with open(tmp, "w", encoding="utf-8") as handle:
                json.dump(payload, handle, indent=2)
            os.replace(tmp, self.state_file)
        except OSError as error:
            log.warning("could not save %s: %s", self.state_file, error)

    # -- lookup ---------------------------------------------------------------

    def _find(self, dunk_id):
        dunk_id = str(dunk_id).strip()
        for dunker in self.dunkers:
            if dunker.id == dunk_id:
                return dunker
        # Be forgiving: accept a bare number or a row number.
        if dunk_id.isdigit():
            for dunker in self.dunkers:
                if dunker.id == f"d{dunk_id}":
                    return dunker
        raise DunkError(f"no dunk with id {dunk_id!r}")

    def get(self, dunk_id):
        with self._lock:
            return self._find(dunk_id).to_dict()

    def list(self):
        with self._lock:
            return [d.to_dict() for d in self.dunkers]

    def status(self):
        with self._lock:
            running = sum(1 for d in self.dunkers if d.running)
            total = len(self.dunkers)
        available = self.herdr.is_available()
        return {
            "version": VERSION,
            "pid": os.getpid(),
            "dunkers": total,
            "running": running,
            "herdr_available": available,
            "herdr_hint": None if available else self.herdr.server_error_hint(),
            "state_file": self.state_file if self.persist_enabled else None,
            "uptime_s": int(time.time() - self.started_at),
            "queued_messages": sum(len(q) for q in self.outbox.values()),
            "message": self.last_message,
        }

    def summary(self):
        with self._lock:
            n = len(self.dunkers)
            running = sum(1 for d in self.dunkers if d.running)
        label = f"{n} dunker{'s' if n != 1 else ''}"
        return f"{label} ({running} running)" if running else label

    # -- targets ----------------------------------------------------------------

    def list_panes(self, self_pane_id=None):
        panes = self.herdr.list_panes_grouped()
        for pane in panes:
            pane["is_self"] = bool(self_pane_id) and pane.get("pane_id") == self_pane_id
        return panes

    # -- herdr layout control -----------------------------------------------
    # Workspaces, tabs and panes can be created, renamed, focused and closed
    # through the same surfaces that schedule dunks, so an admin agent can
    # spin up a team (a workspace per project, a tab per agent) and then dunk
    # on it. Everything here is a thin, resolved call into HerdrClient.

    LAYOUT_KINDS = ("workspace", "tab", "pane")

    def list_workspaces(self):
        """Workspaces with their tabs nested, sorted by number."""
        tabs = self.herdr.list_tabs()
        for tab in tabs:  # older herdr omits workspace_id; it is the id's prefix
            tab.setdefault("workspace_id", str(tab.get("tab_id", "")).split(":")[0])
        workspaces = self.herdr.list_workspaces()
        for workspace in workspaces:
            workspace["tabs"] = sorted(
                (t for t in tabs if t.get("workspace_id") == workspace.get("workspace_id")),
                key=lambda t: t.get("number", 0),
            )
        workspaces.sort(key=lambda w: w.get("number", 0))
        return workspaces

    def _self_pane(self, self_pane_id):
        if not self_pane_id:
            raise DunkError("'self' needs HERDR_PANE_ID (run from inside a herdr pane)")
        pane = self.herdr.get_pane(self_pane_id)
        if pane is None:
            raise DunkError(f"own pane {self_pane_id} not found in herdr")
        return pane

    @staticmethod
    def _pick(items, spec, kind, id_key, label_key="label"):
        """Resolve `spec` against a list: exact id, exact label (case-
        insensitive), then unique substring of the label."""
        needle = str(spec).strip()
        if not needle:
            raise DunkError(f"{kind} is required")
        for item in items:
            if item.get(id_key) == needle:
                return item
        low = needle.lower()
        exact = [i for i in items if str(i.get(label_key) or "").lower() == low]
        if len(exact) == 1:
            return exact[0]
        if len(exact) > 1:
            raise DunkError(f"{kind} {spec!r} is ambiguous: "
                            + ", ".join(i[id_key] for i in exact))
        partial = [i for i in items if low in str(i.get(label_key) or "").lower()]
        if len(partial) == 1:
            return partial[0]
        if partial:
            raise DunkError(f"{kind} {spec!r} is ambiguous: "
                            + ", ".join(f"{i[id_key]} ({i.get(label_key)})" for i in partial))
        raise DunkError(f"no herdr {kind} matches {spec!r}")

    def resolve_workspace(self, spec, self_pane_id=None):
        """Workspace dict from an id ('w9'), a label, a unique label substring
        or 'self' (the caller's own workspace)."""
        spec = str(spec or "").strip()
        if spec.lower() in ("self", "me", "here"):
            spec = self._self_pane(self_pane_id).get("workspace_id")
        workspaces = self.herdr.list_workspaces()
        if not workspaces:
            raise DunkError(self.herdr.server_error_hint())
        return self._pick(workspaces, spec, "workspace", "workspace_id")

    def resolve_tab(self, spec, self_pane_id=None):
        """Tab dict from an id ('w9:t2'), a label, a unique label substring or
        'self' (the caller's own tab)."""
        spec = str(spec or "").strip()
        if spec.lower() in ("self", "me", "here"):
            spec = self._self_pane(self_pane_id).get("tab_id")
        tabs = self.herdr.list_tabs()
        if not tabs:
            raise DunkError(self.herdr.server_error_hint())
        return self._pick(tabs, spec, "tab", "tab_id")

    def _resolve_kind(self, kind, target, self_pane_id=None):
        kind = str(kind or "").strip().lower()
        if kind not in self.LAYOUT_KINDS:
            raise DunkError("kind must be 'workspace', 'tab' or 'pane'")
        if kind == "workspace":
            item = self.resolve_workspace(target, self_pane_id)
            return kind, item["workspace_id"], item
        if kind == "tab":
            item = self.resolve_tab(target, self_pane_id)
            return kind, item["tab_id"], item
        item = self.resolve_target(target, self_pane_id)
        return kind, item["pane_id"], item

    def _after_create(self, ok, result, command, what):
        if not ok:
            raise DunkError(f"herdr could not create the {what}: {result}")
        pane = result.get("root_pane") or result.get("pane") or result.get("agent") or {}
        out = {
            "workspace": result.get("workspace"),
            "tab": result.get("tab"),
            "pane": pane,
            "pane_id": pane.get("pane_id"),
            "ran": None,
        }
        if command and pane.get("pane_id"):
            ran, message = self.herdr.run_command(pane["pane_id"], command)
            out["ran"] = command if ran else None
            if not ran:
                out["error"] = f"created, but running the command failed: {message}"
        with self._lock:
            self.last_message = f"Created {what}"
        log.info("created %s -> %s%s", what, pane.get("pane_id"),
                 f" running {command!r}" if command else "")
        return out

    def create_workspace(self, label=None, cwd=None, command=None, focus=False):
        """New herdr workspace (with its first tab and pane). `command`, if
        given, is typed + Enter into that pane once it exists."""
        ok, result = self.herdr.create_workspace(label=label, cwd=cwd, focus=focus)
        return self._after_create(ok, result, command, f"workspace {label or ''}".strip())

    def create_tab(self, workspace=None, label=None, cwd=None, command=None, focus=False,
                   self_pane_id=None):
        """New tab. `workspace` is an id, label or 'self'; omitted means the
        caller's own workspace when known, else herdr's focused one."""
        workspace_id = None
        if workspace:
            workspace_id = self.resolve_workspace(workspace, self_pane_id)["workspace_id"]
        elif self_pane_id:
            own = self.herdr.get_pane(self_pane_id)
            workspace_id = own.get("workspace_id") if own else None
        ok, result = self.herdr.create_tab(workspace_id=workspace_id, label=label, cwd=cwd,
                                           focus=focus)
        return self._after_create(ok, result, command, f"tab {label or ''}".strip())

    def split_pane(self, target, direction="right", cwd=None, command=None, focus=False,
                   self_pane_id=None):
        """Split an existing pane (any pane target) right or down."""
        direction = str(direction or "right").lower()
        if direction not in ("right", "down"):
            raise DunkError("direction must be 'right' or 'down'")
        pane = self.resolve_target(target, self_pane_id)
        ok, result = self.herdr.split_pane(pane["pane_id"], direction=direction, cwd=cwd,
                                           focus=focus)
        return self._after_create(ok, result, command, "pane")

    def start_agent(self, name, command, workspace=None, tab=None, split=None, cwd=None,
                    focus=False, self_pane_id=None):
        """Launch `command` in a new pane registered with herdr as agent `name`
        (`herdr agent start`). Placement: inside `tab` (splitting it when
        `split` is right/down), else in `workspace` (id, label or 'self';
        herdr picks the tab), else wherever herdr puts it."""
        name = str(name or "").strip()
        if not name:
            raise DunkError("agent name is required")
        argv = shlex.split(str(command or ""))
        if not argv:
            raise DunkError("command is required (e.g. 'claude' or 'codex --model gpt-5')")
        workspace_id = tab_id = None
        if tab:
            tab_id = self.resolve_tab(tab, self_pane_id)["tab_id"]
        if workspace:
            workspace_id = self.resolve_workspace(workspace, self_pane_id)["workspace_id"]
        if split and split not in ("right", "down"):
            raise DunkError("split must be 'right' or 'down'")
        ok, result = self.herdr.start_agent(name, argv, workspace_id=workspace_id,
                                            tab_id=tab_id, split=split, cwd=cwd, focus=focus)
        if not ok:
            raise DunkError(f"herdr could not start agent {name!r}: {result}")
        agent = result.get("agent") or {}
        with self._lock:
            self.last_message = f"Started agent {name}"
        log.info("started agent %s -> %s: %s", name, agent.get("pane_id"), argv)
        return {"agent": agent, "pane_id": agent.get("pane_id"), "name": name,
                "argv": result.get("argv") or argv}

    def rename(self, kind, target, label, self_pane_id=None):
        label = str(label or "").strip()
        if not label:
            raise DunkError("label is required")
        kind, target_id, _item = self._resolve_kind(kind, target, self_pane_id)
        ok, result = self.herdr.rename(kind, target_id, label)
        if not ok:
            raise DunkError(f"herdr could not rename {kind} {target_id}: {result}")
        log.info("renamed %s %s -> %r", kind, target_id, label)
        return {"kind": kind, "id": target_id, "label": label,
                kind: (result or {}).get(kind)}

    def focus(self, kind, target, self_pane_id=None):
        kind, target_id, _item = self._resolve_kind(kind, target, self_pane_id)
        ok, result = self.herdr.focus(kind, target_id)
        if not ok:
            raise DunkError(f"herdr could not focus {kind} {target_id}: {result}")
        return {"kind": kind, "id": target_id, "focused": True}

    def close_target(self, kind, target, self_pane_id=None):
        """Close a workspace, tab or pane, killing whatever runs inside.
        Refuses to close the caller's own pane or its containers, since a
        process closing its own terminal is almost always a mistake; do it
        from another pane. Dunks aimed into the closed area are stopped."""
        kind, target_id, item = self._resolve_kind(kind, target, self_pane_id)
        own = self.herdr.get_pane(self_pane_id) if self_pane_id else None
        if own and own.get({"workspace": "workspace_id", "tab": "tab_id",
                            "pane": "pane_id"}[kind]) == target_id:
            raise DunkError(f"refusing to close the {kind} this caller runs in "
                            f"({target_id}); target it from another pane")
        doomed = [p["pane_id"] for p in self.herdr.list_panes()
                  if p.get({"workspace": "workspace_id", "tab": "tab_id",
                            "pane": "pane_id"}[kind]) == target_id]
        ok, result = self.herdr.close(kind, target_id)
        if not ok:
            raise DunkError(f"herdr could not close {kind} {target_id}: {result}")
        stopped = []
        with self._lock:
            for dunker in self.dunkers:
                if dunker.running and dunker.target_pane_id in doomed:
                    self._halt(dunker)
                    dunker.running = False
                    dunker.next_send_at = None
                    dunker.status = "Target closed"
                    stopped.append(dunker.id)
            if stopped:
                self._save()
            self.last_message = f"Closed {kind} {item.get('label') or target_id}"
        log.info("closed %s %s (panes %s); stopped dunks %s", kind, target_id, doomed, stopped)
        return {"kind": kind, "id": target_id, "closed_panes": doomed,
                "stopped_dunks": stopped}

    # Passthrough refusals: commands that would take herdr itself down or need
    # an interactive terminal. Everything else herdr offers is fair game.
    HERDR_BLOCKED = (
        (), ("server", "stop"), ("update",), ("channel", "set"), ("session",),
        ("completion",), ("agent", "attach"), ("integration",),
    )

    def herdr_command(self, command, self_pane_id=None):
        """Run any `herdr <subcommand ...>` and return its parsed result. This
        is the escape hatch for everything without a dedicated command:
        `herdr_command("tab focus w9:t2")`, `herdr_command("api schema --json")`.
        The word `self` as an argument becomes the caller's pane id (its tab
        or workspace id for `tab ...` / `workspace ...` commands). herdr wants
        ids here, not labels; the typed commands above resolve labels. Returns
        {"ok", "result" (parsed JSON when herdr printed JSON, else None),
        "text" (raw stdout), "error" (herdr's message, if any)}."""
        argv = shlex.split(str(command or "")) if not isinstance(command, list) \
            else [str(a) for a in command]
        if argv and argv[0] == "herdr":
            argv = argv[1:]
        if not argv:
            raise DunkError("command is required, e.g. 'workspace list' (see `herdr --help`)")
        for blocked in self.HERDR_BLOCKED:
            if blocked and tuple(argv[:len(blocked)]) == blocked:
                raise DunkError(f"'herdr {' '.join(blocked)}' is not allowed from here "
                                "(it would stop, replace or attach herdr itself)")
        if any(a.startswith("-") and a in ("--remote", "--session") for a in argv):
            raise DunkError("launching or attaching a herdr session is not allowed from here")
        if self_pane_id and "self" in argv:
            # `self` means the caller's tab for tab commands, its workspace
            # for workspace commands, and its pane otherwise.
            own_id = self_pane_id
            if argv[0] in ("tab", "workspace"):
                own = self.herdr.get_pane(self_pane_id) or {}
                own_id = own.get(f"{argv[0]}_id") or self_pane_id
            argv = [own_id if a == "self" else a for a in argv]
        code, out, err = self.herdr._run(argv, timeout=30)
        payload = None
        for stream in (out, err):  # herdr prints JSON errors on stderr
            if stream.strip():
                try:
                    payload = json.loads(stream)
                    break
                except ValueError:
                    payload = None
        error = None
        text = out if out.strip() else err
        if isinstance(payload, dict) and isinstance(payload.get("error"), dict):
            error = payload["error"].get("message") or payload["error"].get("code")
        elif code != 0 and not _looks_like_usage(text):
            error = err.strip() or out.strip() or f"herdr exited {code}"
        result = payload.get("result") if isinstance(payload, dict) and "result" in payload \
            else payload
        log.info("herdr %s -> %s", " ".join(argv), "ok" if not error else f"error: {error}")
        return {"ok": error is None, "argv": argv, "result": result if error is None else None,
                "text": text or "", "error": error, "exit_code": code}

    def herdr_help(self, topic=None):
        """herdr's own usage text: the command groups (`topic` empty) or one
        group's subcommands (`topic` = 'pane', 'workspace', 'tab', 'agent',
        'wait', 'notification', ...). This is how an agent discovers what the
        `herdr` passthrough can do; `herdr api schema --json` has the full
        machine-readable schema."""
        topic = str(topic or "").strip().split()
        argv = topic[:1] if topic else ["--help"]
        code, out, err = self.herdr._run(argv, timeout=10)
        text = out if out.strip() else err
        if not text.strip():
            raise DunkError(f"herdr printed no help for {' '.join(argv)!r} (exit {code})")
        return {"topic": " ".join(topic) or None, "text": text}

    def resolve_target(self, spec, self_pane_id=None):
        """Turn a target spec into a pane dict.

        `spec` may be a pane dict, a pane id (`w8:p3`), the word `self` (the
        pane of the calling process), a terminal id, or a case-insensitive
        substring of a tab label, workspace label, agent name, directory or
        terminal title. Exactly one pane must match.
        """
        if isinstance(spec, dict):
            if not spec.get("pane_id"):
                raise DunkError("target dict has no pane_id")
            panes = self.herdr.list_panes_grouped()
            for pane in panes:
                if pane.get("pane_id") == spec["pane_id"]:
                    return pane
            return spec
        spec = str(spec or "").strip()
        if not spec:
            raise DunkError("target is required")
        if spec.lower() in ("self", "me", "here"):
            if not self_pane_id:
                raise DunkError(
                    "target 'self' needs HERDR_PANE_ID (run from inside a herdr pane)"
                )
            spec = self_pane_id
        panes = self.herdr.list_panes_grouped()
        if not panes:
            raise DunkError(self.herdr.server_error_hint())
        for pane in panes:
            if pane.get("pane_id") == spec or pane.get("terminal_id") == spec:
                return pane
        needle = spec.lower()
        # Exact label matches beat substring matches, so a tab named
        # "Dunking Sheep" wins over a pane whose title merely contains it.
        for key in ("tab_label", "label", "workspace_label", "agent"):
            exact = [p for p in panes if str(p.get(key) or "").lower() == needle]
            if len(exact) == 1:
                return exact[0]
            if len(exact) > 1:
                agents = [p for p in exact if p.get("agent")]
                if len(agents) == 1:
                    return agents[0]
                raise DunkError(f"target {spec!r} is ambiguous: {_describe(exact)}")
        matches = []
        for pane in panes:
            haystack = " ".join(
                str(pane.get(key) or "")
                for key in ("tab_label", "workspace_label", "agent", "cwd",
                            "terminal_title_stripped", "label")
            ).lower()
            if needle in haystack:
                matches.append(pane)
        if len(matches) == 1:
            return matches[0]
        if not matches:
            raise DunkError(f"no herdr pane matches {spec!r}")
        # Prefer a single agent pane among several matches (a tab label often
        # matches both the agent pane and a shell beside it).
        agents = [p for p in matches if p.get("agent")]
        if len(agents) == 1:
            return agents[0]
        raise DunkError(f"target {spec!r} is ambiguous: {_describe(matches)}")

    # -- mutation -----------------------------------------------------------

    def add(self, target=None, text=DEFAULT_TEXT, interval_minutes=DEFAULT_INTERVAL_MINUTES,
            name=None, start=False, only_when=None, max_sends=None,
            skip_if_unconsumed=False, hold_while_typing=True, self_pane_id=None):
        interval = parse_interval(interval_minutes)
        only_when = _check_only_when(only_when)
        max_sends = _check_max_sends(max_sends)
        pane = self.resolve_target(target, self_pane_id) if target else None
        with self._lock:
            dunker = Dunker(
                f"d{self.next_id}",
                name=name,
                interval_minutes=interval,
                text=text if text is not None else DEFAULT_TEXT,
                only_when=only_when,
                max_sends=max_sends,
                skip_if_unconsumed=_check_bool(skip_if_unconsumed, "skip_if_unconsumed"),
                hold_while_typing=_check_bool(hold_while_typing, "hold_while_typing"),
            )
            self.next_id += 1
            if pane:
                dunker.set_target(pane)
                dunker.status = "Target set"
            self.dunkers.append(dunker)
            if start and pane:
                self._start(dunker)
            elif start:
                dunker.status = "No target - press c"
            self._save()
            self.last_message = f"Added {dunker.id}"
            return dunker.to_dict()

    def update(self, dunk_id, target=None, text=None, interval_minutes=None,
               name=None, only_when=..., max_sends=..., skip_if_unconsumed=None,
               hold_while_typing=None, self_pane_id=None):
        """Change fields on a dunk. Pass `only_when=None` / `max_sends=None`
        explicitly to clear them; omit them to leave them alone."""
        pane = self.resolve_target(target, self_pane_id) if target else None
        interval = parse_interval(interval_minutes) if interval_minutes is not None else None
        if only_when is not ...:
            only_when = _check_only_when(only_when)
        if max_sends is not ...:
            max_sends = _check_max_sends(max_sends)
        with self._lock:
            dunker = self._find(dunk_id)
            changed = []
            if pane:
                dunker.set_target(pane)
                changed.append("target")
            if text is not None:
                dunker.text = str(text)
                changed.append("text")
            if interval is not None and interval != dunker.interval_minutes:
                dunker.interval_minutes = interval
                if dunker.running:
                    anchor = dunker.last_sent_at or dunker.started_at or time.time()
                    dunker.next_send_at = max(time.time() + 1, anchor + interval * 60)
                changed.append("interval")
            if name is not None:
                dunker.name = str(name)
                changed.append("name")
            if only_when is not ...:
                dunker.only_when = only_when
                changed.append("only_when")
            if max_sends is not ...:
                dunker.max_sends = max_sends
                changed.append("max_sends")
            if skip_if_unconsumed is not None:
                dunker.skip_if_unconsumed = _check_bool(skip_if_unconsumed, "skip_if_unconsumed")
                changed.append("skip_if_unconsumed")
            if hold_while_typing is not None:
                dunker.hold_while_typing = _check_bool(hold_while_typing, "hold_while_typing")
                changed.append("hold_while_typing")
            if changed:
                dunker.status = f"{changed[-1].replace('_', ' ').capitalize()} set"
                self._save()
            return dunker.to_dict()

    def remove(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            self._halt(dunker)
            dunker.running = False
            self.dunkers.remove(dunker)
            self._save()
            self.last_message = f"Removed {dunker.id}"
            return dunker.to_dict()

    def start(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            self._start(dunker)
            self._save()
            return dunker.to_dict()

    def stop(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            self._halt(dunker)
            dunker.running = False
            dunker.next_send_at = None
            dunker.status = "Stopped"
            self._save()
            return dunker.to_dict()

    def toggle(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            return self.stop(dunk_id) if dunker.running else self.start(dunk_id)

    def stop_all(self):
        with self._lock:
            for dunker in self.dunkers:
                if dunker.running:
                    self._halt(dunker)
                    dunker.running = False
                    dunker.next_send_at = None
                    dunker.status = "Stopped"
            self._save()
            return [d.to_dict() for d in self.dunkers]

    def _start(self, dunker):
        if not dunker.target_pane_id:
            dunker.status = "No target - press c"
            raise DunkError(f"{dunker.id} has no target")
        if dunker.running and dunker.thread and dunker.thread.is_alive():
            return
        now = time.time()
        dunker.running = True
        dunker.started_at = now
        dunker.next_send_at = now + dunker.interval_minutes * 60
        dunker.last_error = None
        self._spawn_timer(dunker)

    def _spawn_timer(self, dunker):
        dunker.stop_event = threading.Event()
        dunker.thread = threading.Thread(
            target=self._timer_loop,
            args=(dunker, dunker.stop_event),
            name=f"dunk-{dunker.id}",
            daemon=True,
        )
        dunker.thread.start()

    def _halt(self, dunker):
        """Stop the timer thread without touching the persisted running flag."""
        if dunker.stop_event:
            dunker.stop_event.set()
        dunker.stop_event = None
        dunker.thread = None

    # -- sending ------------------------------------------------------------

    def fire(self, dunk_id, countdown_s=0, wait=True):
        """Send a dunk's text right now (the classic 't' test send). Does not
        disturb the running countdown."""
        with self._lock:
            dunker = self._find(dunk_id)
            if not dunker.target_pane_id:
                dunker.status = "No target - press c"
                raise DunkError(f"{dunker.id} has no target")
        if wait:
            return self._fire_worker(dunker, countdown_s)
        threading.Thread(
            target=self._fire_worker, args=(dunker, countdown_s), daemon=True
        ).start()
        return {"id": dunker.id, "ok": None, "message": "test send scheduled"}

    def _fire_worker(self, dunker, countdown_s):
        try:
            for remaining in range(int(countdown_s), 0, -1):
                dunker.status = f"Test in {remaining}..."
                time.sleep(1)
            if dunker.hold_while_typing and not self._clear_to_send(
                    dunker.target_pane_id, dunker.last_sent_text, dunker.awaiting_consumption):
                # Same rule as a scheduled send: never type over a human. The
                # send waits in the pane's outbox and lands when the box clears.
                message = self._new_message(
                    dunker.target_pane_id, dunker.target_label(), dunker.text,
                    source=dunker.id, deliver=lambda: self._do_send(dunker))
                dunker.status = "Queued (typing)"
                self._enqueue(message)
                return {"id": dunker.id, "ok": None, "queued": True, "message_id": message.id,
                        "message": f"queued: someone is typing in {dunker.target_label()}"}
            with self.send_lock:
                dunker.status = "Sending..."
                ok, message = self._do_send(dunker)
            stamp = time.strftime("%H:%M:%S")
            dunker.status = f"Tested {stamp}" if ok else "Test failed"
            return {"id": dunker.id, "ok": ok, "queued": False, "message": message}
        except Exception as error:  # noqa: BLE001 - keep the thread alive
            dunker.status = "Test failed"
            dunker.last_error = str(error)
            log.exception("test send failed for %s", dunker.id)
            return {"id": dunker.id, "ok": False, "message": str(error)}

    def send_now(self, target, text, self_pane_id=None, hold_while_typing=True, wait_s=0):
        """One-off send to any pane, no dunk involved. Like a dunk, it never
        types over a human: if someone is composing in the target's box (or
        earlier messages to that pane are still waiting), the message is queued
        in the daemon and delivered, in order, once the box has been clear for
        TYPING_SETTLE_S. `wait_s` > 0 blocks up to that long for delivery
        before returning, so a short hold still comes back as sent."""
        pane = self.resolve_target(target, self_pane_id)
        if not str(text or "").strip():
            raise DunkError("text is required")
        label = pane.get("tab_label") or pane["pane_id"]
        message = self._new_message(pane["pane_id"], label, str(text), source="send_text",
                                    deliver=lambda: self.herdr.send_text_and_enter(
                                        pane["pane_id"], str(text)))
        if not _check_bool(hold_while_typing, "hold_while_typing"):
            self._deliver(message)
            return message.to_dict()
        self._enqueue(message)
        if wait_s:
            message.done.wait(min(float(wait_s), 3600))
        return message.to_dict()

    # -- direct-message outbox ---------------------------------------------

    def _new_message(self, pane_id, label, text, source, deliver):
        with self._lock:
            message = Message(f"m{self.next_message_id}", pane_id, label, text, source, deliver)
            self.next_message_id += 1
        return message

    def _deliver(self, message):
        """Send one message now (under the shared send lock) and record it."""
        with self.send_lock:
            if message.status == "cancelled":
                return
            try:
                ok, detail = message.deliver()
            except Exception as error:  # noqa: BLE001 - report, never crash a queue
                ok, detail = False, str(error)
        with self._lock:
            message.status = "sent" if ok else "failed"
            message.detail = detail
            message.finished_at = time.time()
            self.last_message = (f"Sent to {message.target_label}" if ok
                                 else f"Send failed: {detail}")
            self._remember(message)
        log.info("message %s -> %s (%s): %s", message.id, message.pane_id, message.source,
                 "sent" if ok else f"FAILED {detail}")
        message.done.set()

    def _remember(self, message):
        self.message_history.append(message)
        del self.message_history[:-MESSAGE_HISTORY]

    def _enqueue(self, message):
        """Queue a message for its pane and make sure a delivery thread runs.
        Delivery is immediate when nothing is queued and nobody is typing."""
        with self._lock:
            queue = self.outbox.setdefault(message.pane_id, [])
            queue.append(message)
            thread = self.outbox_threads.get(message.pane_id)
            if thread is None or not thread.is_alive():
                thread = threading.Thread(target=self._outbox_worker, args=(message.pane_id,),
                                          name=f"outbox-{message.pane_id}", daemon=True)
                self.outbox_threads[message.pane_id] = thread
                thread.start()

    def _outbox_worker(self, pane_id):
        """Deliver a pane's queued messages in order, each only once the box
        has been clear for TYPING_SETTLE_S; while someone types, wait and
        re-check every MESSAGE_RECHECK_S."""
        held_since = None
        try:
            while not self.closed:
                with self._lock:
                    queue = self.outbox.get(pane_id) or []
                    queue[:] = [m for m in queue if m.status == "queued"]
                    message = queue[0] if queue else None
                    if message is None:
                        self.outbox.pop(pane_id, None)
                        return
                if self._clear_to_send(pane_id):
                    if held_since is not None:
                        log.info("message %s -> %s: box clear after %ds, delivering",
                                 message.id, pane_id, int(time.time() - held_since))
                        held_since = None
                    with self._lock:
                        if message in queue:
                            queue.remove(message)
                    self._deliver(message)
                    continue
                if held_since is None:
                    held_since = time.time()
                    with self._lock:
                        message.holds += 1
                        self.last_message = f"Holding message for {message.target_label}: someone is typing"
                    log.info("message %s -> %s: holding, someone is typing in the input box",
                             message.id, pane_id)
                time.sleep(MESSAGE_RECHECK_S)
        except Exception:  # noqa: BLE001 - never kill the daemon
            log.exception("outbox worker for %s crashed", pane_id)
        finally:
            with self._lock:
                if self.outbox_threads.get(pane_id) is threading.current_thread():
                    self.outbox_threads.pop(pane_id, None)

    def list_messages(self):
        """Queued messages (oldest first) and the recent delivered/failed ones."""
        with self._lock:
            queued = [m.to_dict() for queue in self.outbox.values() for m in queue
                      if m.status == "queued"]
            recent = [m.to_dict() for m in self.message_history]
        queued.sort(key=lambda m: m["created_at"])
        return {"queued": queued, "recent": recent[::-1]}

    def get_message(self, message_id):
        with self._lock:
            for queue in self.outbox.values():
                for message in queue:
                    if message.id == message_id:
                        return message.to_dict()
            for message in self.message_history:
                if message.id == message_id:
                    return message.to_dict()
        raise DunkError(f"no message with id {message_id!r}")

    def cancel_message(self, message_id):
        """Drop a queued message before it is delivered."""
        with self._lock:
            for pane_id, queue in self.outbox.items():
                for message in queue:
                    if message.id == message_id:
                        if message.status != "queued":
                            raise DunkError(f"message {message_id} already {message.status}")
                        message.status = "cancelled"
                        message.finished_at = time.time()
                        queue.remove(message)
                        self._remember(message)
                        message.done.set()
                        log.info("message %s -> %s cancelled", message.id, pane_id)
                        return message.to_dict()
            for message in self.message_history:
                if message.id == message_id:
                    raise DunkError(f"message {message_id} already {message.status}")
        raise DunkError(f"no queued message with id {message_id!r}")

    def read_pane(self, target, lines=40, self_pane_id=None):
        pane = self.resolve_target(target, self_pane_id)
        text = self.herdr.read_pane(pane["pane_id"], lines=lines)
        if text is None:
            raise DunkError(f"could not read pane {pane['pane_id']}")
        return {"pane_id": pane["pane_id"], "agent": pane.get("agent"),
                "agent_status": pane.get("agent_status"),
                "tab_label": pane.get("tab_label"), "text": text}

    def _do_send(self, dunker):
        text = expand_template(dunker.text, dunker).strip()
        if not text:
            return True, "empty text, nothing sent"
        ok, message = self.herdr.send_text_and_enter(dunker.target_pane_id, text)
        with self._lock:
            if ok:
                dunker.send_count += 1
                dunker.last_sent_at = time.time()
                dunker.last_error = None
                dunker.last_sent_text = text
                # Backpressure looks at this text in the pane on the next send.
                dunker.awaiting_consumption = True
                self.last_message = f"Sent to {dunker.target_label()}"
            else:
                dunker.last_error = message
                self.last_message = f"Send failed: {message}"
        log.info("%s -> %s: %s (%s)", dunker.id, dunker.target_pane_id,
                 "sent" if ok else "FAILED", message)
        return ok, message

    def _previous_send_consumed(self, dunker):
        """Decide, at send time, whether the previous send has left the target's
        input box. Returns True (consumed, go ahead) or False (still pending or
        we could not tell, so hold).

        The signal is the pane's visible input box, not the agent's busy state:
        a pane idling at a prompt with unread text reports idle, and a pane busy
        on an earlier turn tells us nothing about the newest send. We read the
        box and treat the send as still pending if it shows a collapsed-input
        placeholder (how Claude Code renders a long queued paste) or the sent
        text verbatim. An unreadable or unrecognisable box is inconclusive, and
        for this opt-in feature the safe answer to "not sure" is to hold: a
        missed nudge is recoverable next interval, a stacked pile-up is not.

        Blind spots, stated honestly: we detect our own queued send (verbatim, or
        the collapse placeholder a long one becomes) but not arbitrary unrelated
        short text a human left in the box, because agents show rotating idle
        hints there that cannot be told apart from typed text - so a short
        unrelated line reads as "clear" and the send proceeds. A short prompt
        whose echo lingers in the box region can hold one extra interval. This
        assumes an agent pane that draws an input box (Claude/Codex), not a bare
        shell."""
        if dunker.send_count == 0 or not dunker.last_sent_text:
            return True  # nothing sent yet; the first send always proceeds
        tail = self.herdr.read_pane(
            dunker.target_pane_id, lines=CONSUMPTION_READ_LINES, source="visible"
        )
        pending = _input_pending(tail, dunker.last_sent_text)
        if pending is None:
            log.info("%s: could not read %s input box; holding to avoid stacking",
                     dunker.id, dunker.target_pane_id)
            return False
        if not pending:
            with self._lock:
                dunker.awaiting_consumption = False
        return not pending

    def _typing_in(self, pane_id, last_sent_text=None, awaiting_consumption=False):
        """True if someone is composing text in `pane_id`'s input box. See
        `_human_typing` for the reasoning; this is the pane-level form used by
        dunks and by queued direct messages alike."""
        screen = self.herdr.read_pane(pane_id, lines=CONSUMPTION_READ_LINES,
                                      source="visible", ansi=True)
        if screen is None:
            return False
        typed = _typed_text(screen)
        if not typed:
            return False
        return not _is_our_send(typed, last_sent_text, awaiting_consumption)

    def _clear_to_send(self, pane_id, last_sent_text=None, awaiting_consumption=False,
                       stop_event=None):
        """True once the box has read clear on two checks TYPING_SETTLE_S
        apart, so a pause between thoughts is not mistaken for being done.
        False as soon as either check sees typing (or the wait is stopped)."""
        if self._typing_in(pane_id, last_sent_text, awaiting_consumption):
            return False
        if stop_event is not None:
            if stop_event.wait(TYPING_SETTLE_S):
                return False
        else:
            time.sleep(TYPING_SETTLE_S)
        return not self._typing_in(pane_id, last_sent_text, awaiting_consumption)

    def _human_typing(self, dunker):
        """True if someone is composing text in the target's input box right
        now, so a send would splice our text into the middle of theirs and
        Enter would submit the mangled result.

        Reads the pane *with styling*: both Claude Code and Codex draw their
        empty-box placeholder hints dim (SGR 2) and typed text at full
        intensity, so "non-dim text after the prompt sigil" is a human's draft
        without having to know every rotating hint. Our own leftover send
        (verbatim, or the collapse placeholder a long one becomes while we are
        still awaiting its consumption) is not typing; that is backpressure's
        business. An unreadable pane or one with no input box (a bare shell) is
        inconclusive and does not hold, since this check guards the human's
        draft rather than the agent's queue."""
        return self._typing_in(dunker.target_pane_id, dunker.last_sent_text,
                               dunker.awaiting_consumption)

    def _timer_loop(self, dunker, stop_event):
        try:
            while not stop_event.is_set():
                with self._lock:
                    total = max(1.0, dunker.interval_minutes * 60)
                    if not dunker.next_send_at:
                        dunker.next_send_at = time.time() + total

                # Countdown, re-reading next_send_at so interval edits apply.
                while True:
                    remaining = dunker.next_send_at - time.time()
                    if remaining <= 0:
                        break
                    minutes, seconds = divmod(int(remaining + 0.999), 60)
                    dunker.status = f"Next: {minutes:02d}:{seconds:02d}"
                    if stop_event.wait(min(1.0, remaining)):
                        return

                # Gates. First the optional idle gate, then never type over a
                # human. After a typing hold the idle gate runs again, because
                # finishing typing usually means submitting, which makes the
                # agent busy.
                held_since = None
                while True:
                    if dunker.only_when == "idle":
                        while True:
                            agent_status = self.herdr.agent_status(dunker.target_pane_id)
                            if agent_status is None:
                                dunker.status = "Target gone"
                                dunker.last_error = "target pane not found"
                                break
                            if agent_status not in BUSY_STATUSES:
                                break
                            dunker.status = f"Busy ({agent_status})"
                            if stop_event.wait(IDLE_POLL_S):
                                return
                    if not dunker.hold_while_typing or self._clear_to_send(
                            dunker.target_pane_id, dunker.last_sent_text,
                            dunker.awaiting_consumption, stop_event):
                        break
                    if stop_event.is_set():
                        return
                    if held_since is None:
                        held_since = time.time()
                        with self._lock:
                            dunker.hold_count += 1
                            dunker.last_held_at = held_since
                            self.last_message = f"Holding {dunker.id}: someone is typing"
                            self._save()
                        log.info("%s -> %s: holding, someone is typing in the input box",
                                 dunker.id, dunker.target_pane_id)
                    dunker.status = "Held (typing)"
                    if stop_event.wait(TYPING_RECHECK_S):
                        return
                if held_since is not None:
                    log.info("%s -> %s: input box clear after %ds, resuming",
                             dunker.id, dunker.target_pane_id, int(time.time() - held_since))

                # Backpressure: a no-op when the previous send was never taken.
                if dunker.skip_if_unconsumed and not self._previous_send_consumed(dunker):
                    with self._lock:
                        if stop_event.is_set():
                            return
                        dunker.next_send_at = time.time() + max(1.0, dunker.interval_minutes * 60)
                        dunker.skip_count += 1
                        dunker.last_skipped_at = time.time()
                        dunker.status = "Skipped (unconsumed)"
                        self.last_message = f"Skipped {dunker.id}: previous send unconsumed"
                        self._save()
                    log.info("%s -> %s: skipped, previous send not consumed",
                             dunker.id, dunker.target_pane_id)
                    if stop_event.wait(1.0):
                        return
                    continue

                dunker.status = "Waiting..."
                with self.send_lock:
                    if stop_event.is_set():
                        return
                    dunker.status = "Sending..."
                    ok, _message = self._do_send(dunker)

                with self._lock:
                    if stop_event.is_set():
                        return
                    dunker.next_send_at = time.time() + max(1.0, dunker.interval_minutes * 60)
                    stamp = time.strftime("%H:%M:%S")
                    dunker.status = f"Sent {stamp}" if ok else "Send failed!"
                    done = bool(dunker.max_sends) and dunker.send_count >= dunker.max_sends
                    self._save()
                if done:
                    log.info("%s reached max_sends=%s, removing", dunker.id, dunker.max_sends)
                    self.remove(dunker.id)
                    return
                if stop_event.wait(1.0):
                    return
        except Exception as error:  # noqa: BLE001 - never kill the daemon
            dunker.status = "Error"
            dunker.last_error = str(error)
            log.exception("timer loop for %s crashed", dunker.id)

    # -- lifecycle ----------------------------------------------------------

    def close(self):
        """Halt every timer thread, keeping persisted running flags so a new
        Flock resumes them."""
        with self._lock:
            self.closed = True
            for dunker in self.dunkers:
                self._halt(dunker)
            for queue in self.outbox.values():
                for message in queue:
                    message.done.set()


def _looks_like_usage(text):
    """herdr prints usage for a bare command group (exit non-zero); that is an
    answer, not an error."""
    head = (text or "").lstrip()[:200].lower()
    return head.startswith("usage:") or head.startswith("herdr") and "commands:" in head


def _describe(panes):
    return ", ".join(
        f"{p.get('pane_id')} ({p.get('workspace_label')}/{p.get('tab_label')}"
        f"{' ' + p['agent'] if p.get('agent') else ''})"
        for p in panes
    )


def _numeric_id(dunk_id):
    match = re.fullmatch(r"d(\d+)", str(dunk_id))
    return int(match.group(1)) if match else 0


def _check_only_when(value):
    if value in ("", "none", "None", "any", "always"):
        value = None
    if value not in ONLY_WHEN_CHOICES:
        raise DunkError("only_when must be 'idle' or empty")
    return value


def _collapse(text):
    """Whitespace-insensitive form of text, so wrapped lines still match."""
    return " ".join(str(text).split())


def _input_area(tail):
    """Return the agent's input-box region: everything from the last prompt
    sigil line (Claude ❯, Codex ›) to the end of the visible screen. None if no
    such line is present, meaning we cannot isolate the box. The box sits at the
    very bottom of the screen, so the *last* sigil line starts it; transcript
    text above it (including transcript-collapse chips) is excluded."""
    if not tail:
        return None
    lines = tail.splitlines()
    last = None
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped[:1] in INPUT_BOX_SIGILS:
            last = index
    if last is None:
        return None
    return "\n".join(lines[last:])


def _input_pending(tail, last_sent_text):
    """True if the target's input box still holds unconsumed input, False if it
    is clear, None if we cannot tell (unreadable pane or no recognisable box)."""
    if tail is None:
        return None
    area = _input_area(tail)
    if area is None:
        return None
    low = area.lower()
    if any(marker in low for marker in COLLAPSED_INPUT_MARKERS):
        return True
    needle = _collapse(last_sent_text)[:CONSUMPTION_PREFIX_CHARS].lower()
    if needle and needle in _collapse(area).lower():
        return True
    return False


def _strip_ansi(text):
    """Plain text of an ANSI-styled screen."""
    return ANSI_RE.sub("", text)


def _sgr_dim(params, dim):
    """Apply one SGR parameter list to the current dim state."""
    if not params:
        return False
    index = 0
    while index < len(params):
        code = params[index]
        if code in ("", "0"):
            dim = False
        elif code == "2":
            dim = True
        elif code == "22":
            dim = False
        elif code in ("38", "48", "58"):
            # Extended colour: skip its sub-parameters so "38;2;r;g;b" is not
            # mistaken for dim.
            if index + 1 < len(params) and params[index + 1] == "5":
                index += 1
            elif index + 1 < len(params) and params[index + 1] == "2":
                index += 4
        index += 1
    return dim


def _bright_text(line):
    """The characters of one ANSI-styled line that are not rendered dim."""
    out = []
    dim = False
    position = 0
    for match in ANSI_RE.finditer(line):
        if not dim:
            out.append(line[position:match.start()])
        sequence = match.group(0)
        if sequence.endswith("m"):
            dim = _sgr_dim(sequence[2:-1].split(";"), dim)
        position = match.end()
    if not dim:
        out.append(line[position:])
    return "".join(out)


def _typed_text(screen):
    """What a human has typed into the target's input box, judged from an
    ANSI-styled screen: the non-dim text on the last prompt-sigil line, minus
    the sigil and box decoration. '' when the box is empty or shows only a dim
    placeholder hint; None when no input box can be found. Only the sigil line
    is inspected: any draft starts there, and continuation lines are hard to
    tell from an agent's footer."""
    if not screen:
        return None
    sigil_line = None
    for line in screen.splitlines():
        plain = _strip_ansi(line).strip()
        if plain[:1] in INPUT_BOX_SIGILS:
            sigil_line = line
    if sigil_line is None:
        return None
    bright = DECORATION_RE.sub(" ", _bright_text(sigil_line)).strip()
    if bright[:1] in INPUT_BOX_SIGILS:
        bright = bright[1:]
    return _collapse(bright)


def _is_our_send(typed, last_sent_text, awaiting_consumption):
    """True if the text in the box is our own previous send rather than a
    human's draft: the sent text (the visible line is a prefix of it, or starts
    with its prefix), or a collapsed-paste placeholder while our last send is
    still unaccounted for."""
    low = _collapse(typed).lower()
    if last_sent_text:
        full = _collapse(last_sent_text).lower()
        needle = full[:CONSUMPTION_PREFIX_CHARS]
        if low and (full.startswith(low) or low.startswith(needle)):
            return True
    if awaiting_consumption and any(marker in low for marker in COLLAPSED_INPUT_MARKERS):
        return True
    return False


def _check_bool(value, name):
    if isinstance(value, bool):
        return value
    if value in (None, "", 0, 1):
        return bool(value)
    text = str(value).strip().lower()
    if text in ("1", "true", "yes", "y", "on"):
        return True
    if text in ("0", "false", "no", "n", "off"):
        return False
    raise DunkError(f"{name} must be true or false")


def _check_max_sends(value):
    if value in (None, "", 0, "0"):
        return None
    try:
        number = int(value)
    except (TypeError, ValueError):
        raise DunkError("max_sends must be a positive integer") from None
    if number <= 0:
        raise DunkError("max_sends must be a positive integer")
    return number
