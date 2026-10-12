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
  until the box is clear (see `Flock._human_typing`). The last of those reads
  happens with the send lock held (`Flock.send_guarded`), so a draft that
  appears while this send queues behind another one still stops it.
- confirmed submit: after typing, the daemon reads the input box back and
  presses Enter again while its own text is still sitting there (see
  `Flock.submit_text`), because one Enter does not reliably submit a long
  message. A send that never leaves the box is reported as failed, not sent.
- persistence: the flock is saved to a JSON file on every structural change
  and restored (running dunks included) when a new flock is created.
- herdr layout control: `list_workspaces`, `create_workspace`, `create_tab`,
  `split_pane`, `start_agent`, `rename`, `focus`, `close`, plus the
  `herdr_command` passthrough for anything else the herdr CLI can do. An admin
  agent builds its team here, then dunks on it.
"""

import functools
import json
import logging
import os
import re
import shlex
import threading
import time

from dunk_inbox import Inbox, InboxError
from herdr_client import HerdrClient

VERSION = "2.8.1"

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
# Pressing Enter once does not reliably submit. Measured on a live Codex pane
# 2026-10-10: a ~2.2k-character message stayed in the input box 3 times out of
# 3, at every pre-Enter delay from 0.2 s to 2 s and every paste chunk size from
# 120 to 900 characters, and a second Enter submitted it every time. So the
# submit is confirmed by reading the box back rather than timed, and Enter is
# repeated while our own text is still sitting there. A blind second Enter
# would be unsafe: if the first one worked, it would submit whatever the human
# typed next.
# Quiet period before each Enter press. Measured on a live Codex pane: what
# decides whether a big paste submits is how long the pane has been quiet, not
# the key itself, and pressing Enter again straight away does not help. With
# these gaps a 2.2k message submits; the long tail only costs time when a send
# is actually stuck, since a normal one is confirmed on the first press.
SUBMIT_GAPS = (0.2, 1.0, 3.0, 6.0)
# How many recent sends per pane to recognise as ours. Remembering only the
# last one meant a single mis-read lost track of our own text for good, and
# every later message queued behind it until a human ran `flush`.
REMEMBERED_SENDS = 4
# How long our own text may sit unsubmitted in a pane's input box while
# messages wait behind it before the daemon presses Enter again to clear it.
# Text the daemon did not type is never submitted this way (see
# `Flock._unwedge`), so this only bounds how long we wait on ourselves, and a
# notification an agent is waiting for should not take longer than this.
STALE_BOX_S = 30.0
# Phrases an agent only shows while a full-screen panel or dialog is open
# (/usage, settings, a confirmation). Such a screen is not an input box: its
# selected row is drawn with the same prompt sigil, which made the daemon read
# a modal as "someone is typing". That cost a Poolrad worker a /usage read on
# 2026-10-12 - the command had run, the panel was open, and the daemon spent
# four Enter presses on the panel before reporting the send failed, while a
# follow-up Escape queued behind the modal for ever.
MODAL_MARKERS = (
    "esc to cancel",
    "esc to close",
    "esc to exit",
    "enter to confirm",
    "press enter to continue",
    "to go back",
)

# Every inbox notice starts with this, which is how one is recognised as
# replaceable rather than stacked behind another.
NOTICE_PREFIX = "[inbox] "

# Key names that mean "submit this".
ENTER_KEYS = ("Enter", "enter", "Return", "return", "KP_Enter")
SUBMIT_ATTEMPTS = len(SUBMIT_GAPS)
SUBMIT_CONFIRM_S = 2.0
# Most submits land at once, so look soon and back off rather than paying a
# fixed delay on every send.
SUBMIT_POLL_S = (0.1, 0.2, 0.4, 0.8, 1.0)
# A box can read clear for an instant while an agent redraws. Declaring the
# send done on that one reading stranded a message in the VP pane on
# 2026-10-11 and blocked everything queued behind it. Require two later,
# readable checks; an absent input box during redraw cannot prove delivery.
SUBMIT_VERIFY_GAPS = (0.4, 1.0)

TEMPLATE_RE = re.compile(r"\{(id|name|count|target|interval|time|date)\}")
# CSI escape sequences (colours, dim, cursor moves) as `herdr pane read
# --format ansi` emits them.
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
# Characters agents draw around the input box that are never typed input:
# braille animation dots, box-drawing rules and block glyphs, whitespace.
DECORATION_RE = re.compile(r"[─-▟⠀-⣿\s]+")

log = logging.getLogger("dunkingsheep")


class _NoticeFolded(Exception):
    """Internal: a notice was merged into one already waiting."""


class DunkError(ValueError):
    """A caller mistake (bad id, bad interval, ambiguous target). Safe to show."""


class AmbiguousTarget(DunkError):
    """A target matched more than one pane. Distinct from "no such pane",
    because a mailbox may be a name with no pane behind it, but it must never
    be a name that stands for several panes at once."""


def _as_dunk_error(method):
    """An unknown message id is a caller mistake like any other, so it should
    come back as a plain message rather than as an internal error."""
    @functools.wraps(method)
    def wrapper(*args, **kwargs):
        try:
            return method(*args, **kwargs)
        except InboxError as error:
            raise DunkError(str(error)) from None
    return wrapper


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
        self.hold_while_typing = True
        self.kind = "text"            # "keys" presses are how a panel is dismissed
        self.is_notice = False        # an inbox notice: one per pane, never stacked
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
                 restore=True, inbox=None):
        self.herdr = herdr or HerdrClient()
        self.state_file = state_file
        self.persist_enabled = persist
        self._lock = threading.RLock()
        # One lock per pane, not one for the whole flock. A send holds its
        # lock across the confirm-and-retry sequence, which can run for tens of
        # seconds when an agent will not take Enter. With a single lock that
        # stalled every other pane's send behind it, and a caller waiting on
        # the daemon saw a timeout: that is what a Poolrad /usage read hit on
        # 2026-10-12. Panes are independent; their sends should be too.
        self.send_lock = threading.Lock()          # kept: tests and callers hold it
        self._pane_locks = {}
        self.dunkers = []
        self.next_id = 1
        self.started_at = time.time()
        self.last_message = ""
        self.closed = False
        # Direct-message outbox: pane_id -> [Message], plus the per-pane
        # delivery thread and a bounded history of finished messages.
        self.outbox = {}
        self.outbox_threads = {}
        # The last text we typed into each pane, so text stranded by a failed
        # submit is recognised as ours and flushed instead of being mistaken
        # for a human's draft (which would stall that pane's queue for good).
        self.sent_text_by_pane = {}
        self._warned_wedged = {}
        self.inbox = Inbox() if inbox is None else inbox
        self.quarantined_mailboxes = self._quarantine_shared_mailboxes()
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
            "inbox_unread": sum(m["unread"] for m in self.inbox.stats()),
            "quarantined_mailboxes": self.quarantined_mailboxes or None,
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

    # -- inboxes ------------------------------------------------------------
    # Machine-to-machine messaging that never types into a terminal. See
    # dunk_inbox.py for why, and for the storage.

    def _quarantine_shared_mailboxes(self):
        """Move mail that an older resolver filed under an agent *type* out of
        reach. Those mailboxes were shared by every pane of that type, so the
        messages in them may have been readable by the wrong agent and we
        cannot tell who they were meant for. They are renamed so no pane
        resolves to them, and reported, rather than delivered to a guess."""
        try:
            types = {str(p.get("agent")).lower() for p in self.herdr.list_panes()
                     if p.get("agent")}
        except Exception:  # noqa: BLE001 - herdr may be down; nothing to do yet
            return []
        moved = []
        for mailbox in types:
            try:
                count = self.inbox.rename_mailbox(mailbox, f"unrouted:{mailbox}")
            except InboxError:
                continue
            if count:
                moved.append({"was": mailbox, "now": f"unrouted:{mailbox}",
                              "messages": count})
                log.warning("inbox: %d message(s) were filed under the shared mailbox "
                            "%r by an older version and may have been visible to the "
                            "wrong agent; moved to 'unrouted:%s' for review",
                            count, mailbox, mailbox)
        return moved

    def resolve_mailbox(self, spec, self_pane_id=None):
        """Turn a target into (mailbox key, label, pane id).

        The key has to identify one pane and keep identifying it. The pane's
        own label comes first, since that is a name someone chose; then the tab
        label; then the pane id, which is always unique but changes if the pane
        moves. Each candidate is taken only if exactly one pane carries it.

        The agent *type* is never a key. Keying on it was a bug: every Codex
        pane shared one mailbox, so a message meant for the Poolrad PM was
        readable from the website and worker panes too.

        A spec matching no pane is still a valid mailbox name, so mail can be
        left for an agent that has not started. A spec matching several panes
        is refused rather than pooled."""
        spec = str(spec or "").strip()
        if not spec:
            raise DunkError("a target mailbox is required")
        try:
            pane = self.resolve_target(spec, self_pane_id)
        except AmbiguousTarget:
            raise
        except DunkError:
            if spec.lower() in ("self", "me", "here"):
                raise
            if self._is_agent_type(spec):
                raise DunkError(
                    f"{spec!r} is an agent type, not one agent: it would be a mailbox "
                    "several panes share. Use a pane id, a pane or tab label, or a "
                    "name of your own.")
            return spec, spec, None
        return self._mailbox_for_pane(pane)

    def _is_agent_type(self, spec):
        """True if `spec` names a kind of agent (claude, codex) rather than
        one of them, so it must never become a mailbox."""
        low = str(spec).strip().lower()
        return any(str(p.get("agent") or "").lower() == low
                   for p in self.herdr.list_panes())

    def _mailbox_for_pane(self, pane):
        panes = self.herdr.list_panes_grouped()

        def unique(field):
            value = pane.get(field)
            if not value:
                return None
            matches = [p for p in panes if p.get(field) == value]
            return value if len(matches) == 1 else None

        key = unique("label") or unique("tab_label") or pane["pane_id"]
        label = pane.get("label") or pane.get("tab_label") or pane["pane_id"]
        if label != pane["pane_id"]:
            label = f"{label} ({pane['pane_id']})"
        return str(key), label, pane["pane_id"]

    def _own_mailbox(self, self_pane_id):
        key, _label, _pane = self.resolve_mailbox("self", self_pane_id)
        return key

    def _mailbox_or_self(self, spec, self_pane_id):
        if spec:
            return self.resolve_mailbox(spec, self_pane_id)[0]
        if not self_pane_id:
            raise DunkError("no mailbox given and no caller pane to default to")
        return self._own_mailbox(self_pane_id)

    @_as_dunk_error
    def inbox_send(self, to, body, subject=None, attach=None, notify=True,
                   self_pane_id=None):
        """Put a message in another agent's inbox. Nothing is typed into their
        terminal except, when `notify`, a short line telling them they have
        mail - which is the one send short enough to always submit."""
        mailbox, label, pane_id = self.resolve_mailbox(to, self_pane_id)
        sender = None
        if self_pane_id:
            try:
                sender = self._own_mailbox(self_pane_id)
            except DunkError:
                sender = self_pane_id
        attachments = [attach] if isinstance(attach, str) else list(attach or ())
        message = self.inbox.send(mailbox, body, subject=subject, sender=sender,
                                  mailbox_label=label, pane_id=pane_id,
                                  attachments=attachments)
        message["notified"] = False
        if notify and pane_id:
            unread = self.inbox.count(mailbox)["unread"]
            text = (f"{NOTICE_PREFIX}{unread} unread. Read with the dunkingsheep MCP "
                    f"tool inbox_read, or: dunkingsheep inbox read")
            try:
                existing = self._refresh_queued_notice(pane_id, text)
                if existing is not None:
                    # A notice for this pane is already waiting. Say the new
                    # count rather than stacking a second one: an agent that
                    # reads its inbox once has handled both messages.
                    self.inbox.record_notify(message["message_id"], "queued",
                                             f"folded into {existing}")
                    message["notified"] = True
                    raise _NoticeFolded
                nudge = self.send_now(pane_id, text, wait_s=0)
                self.inbox.record_notify(message["message_id"],
                                         "queued" if nudge.get("queued") else "sent",
                                         nudge.get("message"))
                message["notified"] = True
            except _NoticeFolded:
                pass
            except DunkError as error:
                self.inbox.record_notify(message["message_id"], "failed", str(error))
        with self._lock:
            self.last_message = f"Inbox message to {label}"
        log.info("inbox %s -> %s (%s)", message["message_id"], mailbox,
                 "notified" if message["notified"] else "no nudge")
        return message

    @_as_dunk_error
    def _refresh_queued_notice(self, pane_id, text):
        """If an inbox notice for this pane is still waiting, update it to the
        new unread count and return its id. Otherwise None.

        Stacking one notice per message means an agent that was away comes
        back to a pile of identical lines, each of which it has to submit.
        One notice carrying the current count is the whole of the information.
        """
        with self._lock:
            for queued in self.outbox.get(pane_id) or []:
                if queued.status == "queued" and queued.is_notice:
                    queued.text = text
                    queued.deliver = lambda: self.submit_text(pane_id, text)
                    return queued.id
        return None

    def inbox_count(self, mailbox=None, self_pane_id=None):
        return self.inbox.count(self._mailbox_or_self(mailbox, self_pane_id))

    @_as_dunk_error
    def inbox_list(self, mailbox=None, unread_only=True, limit=50, self_pane_id=None):
        key = self._mailbox_or_self(mailbox, self_pane_id)
        return {"mailbox": key,
                "messages": self.inbox.list(key, unread_only=unread_only, limit=limit)}

    @_as_dunk_error
    def inbox_read(self, mailbox=None, limit=10, peek=False, self_pane_id=None):
        key = self._mailbox_or_self(mailbox, self_pane_id)
        return {"mailbox": key, "messages": self.inbox.read(key, limit=limit, peek=peek)}

    @_as_dunk_error
    def inbox_status(self, message_id):
        return self.inbox.status(message_id)

    @_as_dunk_error
    def inbox_ack(self, message_ids):
        return self.inbox.ack(message_ids)

    @_as_dunk_error
    def inbox_cancel(self, message_ids):
        return self.inbox.cancel(message_ids)

    @_as_dunk_error
    def inbox_delete(self, message_ids):
        return self.inbox.delete(message_ids)

    @_as_dunk_error
    def inbox_stats(self):
        return {"mailboxes": self.inbox.stats()}

    @_as_dunk_error
    def inbox_fetch(self, attachment_id, dest=None):
        return self.inbox.fetch(attachment_id, dest or os.getcwd())

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
            ran, message = self.submit_text(pane["pane_id"], command)
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
    # Typing into a pane must go through send_text, which waits for the human
    # to stop typing. Reaching these through the passthrough would bypass every
    # protection in this module; `pane send-keys ... Enter` is the worst of
    # them, because it submits whatever half-finished sentence is in the box.
    HERDR_TYPING = {
        ("pane", "send-text"): "send_text(target=..., text=...)",
        ("pane", "send-keys"): "send_text(target=..., text=...)",
        ("pane", "run"): "send_text(target=..., text=...)",
        ("agent", "send"): "send_text(target=..., text=...)",
    }

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
        instead = self.HERDR_TYPING.get(tuple(argv[:2]))
        if instead:
            raise DunkError(
                f"'herdr {' '.join(argv[:2])}' types into a pane, which would land on top of "
                f"whatever a human is writing there. Use {instead} instead: it waits until "
                "the input box is clear, keeps ordering, and reports back. Pass "
                "hold_while_typing=false if you really must type over a draft.")
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
                raise AmbiguousTarget(f"target {spec!r} is ambiguous: {_describe(exact)}")
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
        raise AmbiguousTarget(f"target {spec!r} is ambiguous: {_describe(matches)}")

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
            dunker.status = "Sending..."
            sent, ok, message = self.send_guarded(
                dunker.target_pane_id, lambda: self._do_send(dunker),
                dunker.last_sent_text, dunker.awaiting_consumption,
                hold_while_typing=dunker.hold_while_typing)
            if not sent:
                queued = self._new_message(
                    dunker.target_pane_id, dunker.target_label(), dunker.text,
                    source=dunker.id, deliver=lambda: self._do_send(dunker))
                dunker.status = "Queued (typing)"
                self._enqueue(queued)
                return {"id": dunker.id, "ok": None, "queued": True,
                        "message_id": queued.id,
                        "message": f"queued: someone is typing in {dunker.target_label()}"}
            stamp = time.strftime("%H:%M:%S")
            dunker.status = f"Tested {stamp}" if ok else "Test failed"
            return {"id": dunker.id, "ok": ok, "queued": False, "message": message}
        except Exception as error:  # noqa: BLE001 - keep the thread alive
            dunker.status = "Test failed"
            dunker.last_error = str(error)
            log.exception("test send failed for %s", dunker.id)
            return {"id": dunker.id, "ok": False, "message": str(error)}

    def send_now(self, target, text, self_pane_id=None, hold_while_typing=True, wait_s=0,
                 press_enter=True, is_notice=False):
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
        message = self._new_message(
            pane["pane_id"], label, str(text), source="send_text",
            deliver=lambda: self.submit_text(pane["pane_id"], message.text,
                                             press_enter=press_enter))
        message.is_notice = is_notice or str(text).startswith(NOTICE_PREFIX)
        if not _check_bool(hold_while_typing, "hold_while_typing"):
            message.hold_while_typing = False
            self._deliver(message)
            return message.to_dict()
        self._enqueue(message)
        if wait_s:
            message.done.wait(min(float(wait_s), 3600))
        return message.to_dict()

    def send_keys_now(self, target, keys, self_pane_id=None, hold_while_typing=True,
                      wait_s=0):
        """Press keys in a pane, queued behind a human who is typing exactly
        like a message. `pane send-keys <pane> Enter` is how a half-written
        sentence gets submitted by accident, so it takes the same path."""
        pane = self.resolve_target(target, self_pane_id)
        keys = [keys] if isinstance(keys, str) else [str(k) for k in keys]
        keys = [k for k in keys if str(k).strip()]
        if not keys:
            raise DunkError("keys are required, e.g. 'Enter' or 'ctrl+c'")
        label = pane.get("tab_label") or pane["pane_id"]
        message = self._new_message(pane["pane_id"], label, " ".join(keys), source="send_keys",
                                    deliver=lambda: self.press_keys(pane["pane_id"], keys))
        message.kind = "keys"
        if not _check_bool(hold_while_typing, "hold_while_typing"):
            message.hold_while_typing = False
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
        """Send one message, re-checking the box under the send lock. Returns
        False when a draft appeared at the last moment and the message must
        stay queued."""
        with self._lock:
            if message.status == "cancelled":
                return True
        sent, ok, detail = self.send_guarded(
            message.pane_id, message.deliver,
            hold_while_typing=message.hold_while_typing,
            allow_modal=(message.kind == "keys"))
        if not sent:
            with self._lock:
                message.holds += 1
            return False
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
        return True

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

    def _unwedge(self, pane_id, content, since):
        """Last resort when a box has held the same text, unchanged, for
        STALE_BOX_S while messages wait behind it.

        Only text we typed is ever submitted this way. Unchanged text we do
        not recognise is far more likely to be a person who started a sentence
        and walked away, and submitting half a thought on their behalf is
        worse than a queue that waits: the wedge is visible in `messages` and
        `flush` resolves it deliberately. Our own stranded text is a different
        matter - it is ours to clean up, and recognising it is what the
        remembered-sends list exists for."""
        if not content or time.time() - since < STALE_BOX_S:
            return False
        if not self._looks_like_ours(pane_id, content):
            if not self._warned_wedged.get(pane_id):
                self._warned_wedged[pane_id] = True
                log.warning("%s: input box has held text we did not type for %ds with "
                            "messages waiting. Not submitting someone's draft; run "
                            "`dunkingsheep flush %s` to release it: %r",
                            pane_id, int(time.time() - since), pane_id, content[:80])
            return False
        self._warned_wedged.pop(pane_id, None)
        log.warning("%s: our own text has sat unsubmitted for %ds with messages "
                    "waiting; submitting it to unwedge the queue: %r",
                    pane_id, int(time.time() - since), content[:80])
        ok, detail = self._press_until_submitted(pane_id, content)
        if not ok:
            log.error("%s: could not unwedge the input box: %s", pane_id, detail)
        return ok

    def _outbox_worker(self, pane_id):
        """Deliver a pane's queued messages in order, each only once the box
        has been clear for TYPING_SETTLE_S; while someone types, wait and
        re-check every MESSAGE_RECHECK_S. A box that never clears at all is
        unwedged rather than waited on for ever."""
        held_since = None
        seen, seen_since = None, time.time()
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
                    if self._deliver(message):
                        with self._lock:
                            if message in queue:
                                queue.remove(message)
                        seen, seen_since = None, time.time()
                        continue
                    # Refused under the lock. This used to loop straight back,
                    # and when the two checks disagreed - the box holding our
                    # own text that would not submit, which `_clear_to_send`
                    # forgives and `_ensure_box_clear` does not - the worker
                    # span for 27 minutes without delivering or ever reaching
                    # the recovery below. Fall through to it instead.
                    held_since = held_since or time.time()
                elif held_since is None:
                    held_since = time.time()
                    with self._lock:
                        message.holds += 1
                        self.last_message = f"Holding message for {message.target_label}: someone is typing"
                    log.info("message %s -> %s: holding, someone is typing in the input box",
                             message.id, pane_id)
                # Watch what is in the box. While it changes someone is
                # writing and we wait; while it does not, we are wedged.
                content = self._box_text(pane_id) or ""
                if content != seen:
                    seen, seen_since = content, time.time()
                elif self._unwedge(pane_id, seen, seen_since):
                    seen, seen_since = None, time.time()
                    held_since = None
                    continue
                time.sleep(MESSAGE_RECHECK_S)
        except Exception:  # noqa: BLE001 - never kill the daemon
            log.exception("outbox worker for %s crashed", pane_id)
        finally:
            with self._lock:
                if self.outbox_threads.get(pane_id) is threading.current_thread():
                    self.outbox_threads.pop(pane_id, None)

    def list_messages(self, why_blocked=True):
        """Queued messages (oldest first) and the recent delivered/failed ones.
        A queued message also says what its pane's box is holding, so a wedge
        is visible instead of looking like an idle queue."""
        with self._lock:
            queued = [m.to_dict() for queue in self.outbox.values() for m in queue
                      if m.status == "queued"]
            recent = [m.to_dict() for m in self.message_history]
        queued.sort(key=lambda m: m["created_at"])
        if why_blocked:
            boxes = {}
            for message in queued:
                pane = message["pane_id"]
                if pane not in boxes:
                    content = self._box_text(pane)
                    boxes[pane] = ("" if content is None else content)
                content = boxes[pane]
                message["blocked_by"] = (
                    None if not content
                    else "guarded send pending" if self._looks_like_ours(pane, content)
                    else "input box contains a draft")
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
        ok, message = self.submit_text(dunker.target_pane_id, text)
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
        if _modal_open(screen):
            # A panel or dialog, not a half-written sentence. Its selected row
            # carries the same sigil as the prompt, which is what made a
            # queued Escape wait for ever behind an open /usage panel.
            return False
        typed = _typed_text(screen)
        if not typed:
            return False
        if _is_our_send(typed, last_sent_text, awaiting_consumption):
            return False
        # Text we typed and failed to submit is ours to clean up, not a draft
        # to wait behind; `_ensure_box_clear` flushes it before the next send.
        return not self._looks_like_ours(pane_id, typed)

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

    def _box_holds(self, pane_id, text):
        """True if `text` still sits in the pane's input box, False if nothing
        of ours is waiting there, None if the pane could not be read.

        A pane that reads fine but draws no input box is a bare shell: there
        is no box for text to be stranded in, so there is nothing to confirm
        and the send stands. Only a pane we cannot read at all is inconclusive,
        and that is the case the strict rule is for. Conflating the two made
        every send to a shell pane report failure."""
        screen = self.herdr.read_pane(pane_id, lines=CONSUMPTION_READ_LINES,
                                      source="visible", ansi=True)
        if screen is None:
            return None
        if _modal_open(screen):
            # A panel or dialog is open, so our text left the box and opened
            # it. Pressing Enter again would act on the panel instead.
            return False
        shown = _typed_text(screen)
        if not shown:            # empty box, or no box at all
            return False
        return _is_our_send(shown, text, True)

    def _press_until_submitted(self, pane_id, text):
        """Press Enter until `text` has left the input box. Returns
        (ok, detail). Only ever presses again while our own text is still
        there, so it can never submit something a human typed afterwards."""
        for attempt, gap in enumerate(SUBMIT_GAPS, start=1):
            # Let the pane fall quiet first; a burst of Enters does not submit.
            if gap:
                time.sleep(gap)
            ok, detail = self.herdr.send_keys(pane_id, "Enter")
            if not ok:
                return False, detail
            deadline = time.time() + SUBMIT_CONFIRM_S
            last_hold = None
            for wait in SUBMIT_POLL_S:
                if time.time() >= deadline:
                    break
                time.sleep(wait)
                last_hold = self._box_holds(pane_id, text)
                if last_hold is False:
                    for verify_gap in SUBMIT_VERIFY_GAPS:
                        time.sleep(verify_gap)
                        last_hold = self._box_holds(pane_id, text)
                        if last_hold is not False:
                            break    # it came back, or the pane became unreadable
                    else:
                        # Only repeated readable checks prove submission. A
                        # missing input box during redraw proves nothing.
                        if attempt > 1:
                            log.info("%s: submitted after %d Enter presses", pane_id, attempt)
                        return True, "sent" if attempt == 1 else f"sent (Enter x{attempt})"
                    if last_hold is True:
                        break        # our text returned; another Enter is safe
            if last_hold is not True:
                return False, "submission unconfirmed: input box unreadable or changing"
            log.info("%s: still in the input box after Enter #%d; pressing again",
                     pane_id, attempt)
        return False, (f"text did not submit after {SUBMIT_ATTEMPTS} Enter presses; "
                       "it is still in the pane's input box")

    def _remember_typed(self, pane_id, text):
        with self._lock:
            recent = self.sent_text_by_pane.setdefault(pane_id, [])
            if text in recent:
                recent.remove(text)
            recent.append(text)
            del recent[:-REMEMBERED_SENDS]

    def _looks_like_ours(self, pane_id, shown):
        """True if what is in the box is text we typed. We keep the last few
        sends rather than only the newest, and never forget one just because a
        send reported success: a single mis-read used to orphan our own text,
        and then nothing could tell it from a human's draft."""
        if not shown:
            return False
        with self._lock:
            recent = list(self.sent_text_by_pane.get(pane_id, []))
        return any(_is_our_send(shown, text, True) for text in recent)

    def _box_text(self, pane_id):
        """Whatever is sitting in the pane's input box: '' when empty, None
        when there is no box to read."""
        screen = self.herdr.read_pane(pane_id, lines=CONSUMPTION_READ_LINES,
                                      source="visible", ansi=True)
        if screen is None:
            return None
        return _typed_text(screen)

    def press_keys(self, pane_id, keys):
        """Press keys in a pane. When the last key is Enter and the box holds
        text, the press is confirmed the same way a text send is: Enter does
        not reliably submit, so pressing it once and reporting success was how
        agent-to-agent messages were ending up stranded in the box."""
        keys = [keys] if isinstance(keys, str) else [str(k) for k in keys]
        if not keys or keys[-1] not in ENTER_KEYS:
            return self.herdr.send_keys(pane_id, keys)
        if len(keys) > 1:
            ok, detail = self.herdr.send_keys(pane_id, keys[:-1])
            if not ok:
                return ok, detail
        pending = self._box_text(pane_id)
        if not pending:
            # Nothing waiting: a bare Enter is just a key press.
            return self.herdr.send_keys(pane_id, [keys[-1]])
        return self._press_until_submitted(pane_id, pending)

    def submit_text(self, pane_id, text, press_enter=True):
        """Type `text` into a pane and confirm it was actually submitted."""
        ok, detail = self.herdr.send_text(pane_id, text)
        if not ok:
            return False, detail
        self._remember_typed(pane_id, text)
        if not press_enter:
            return True, "sent"
        pane = next((item for item in self.herdr.list_panes()
                     if item.get("pane_id") == pane_id), None)
        if pane is not None and not pane.get("agent"):
            # A bare shell has no chat input box to read back.
            return self.herdr.send_keys(pane_id, "Enter")
        return self._press_until_submitted(pane_id, text)

    def flush_box(self, target, self_pane_id=None):
        """Submit whatever is sitting unsent in a pane's input box.

        The daemon flushes its own stranded text automatically, but text left
        by something else (an agent typing through the herdr CLI, an older
        daemon, a person who walked away) looks exactly like a draft, so every
        message to that pane queues behind it for ever. This is the explicit
        "yes, send that" for those cases."""
        pane = self.resolve_target(target, self_pane_id)
        pane_id = pane["pane_id"]
        screen = self.herdr.read_pane(pane_id, lines=CONSUMPTION_READ_LINES,
                                      source="visible", ansi=True)
        shown = _typed_text(screen) if screen is not None else None
        if shown is None:
            raise DunkError(f"could not read the input box of {pane_id}")
        label = pane.get("tab_label") or pane_id
        if not shown:
            return {"pane_id": pane_id, "target_label": label, "flushed": False,
                    "text": None, "message": "the input box is already empty"}
        ok, detail = self._press_until_submitted(pane_id, shown)
        log.info("flush %s: %s (%s)", pane_id, "submitted" if ok else "FAILED", detail)
        return {"pane_id": pane_id, "target_label": label, "flushed": ok,
                "text": shown, "message": detail}

    def _ensure_box_clear(self, pane_id, allow_modal=False):
        """Make the input box ready to type into. Our own text stranded by a
        failed submit is flushed with Enter; a human's draft is left alone."""
        screen = self.herdr.read_pane(pane_id, lines=CONSUMPTION_READ_LINES,
                                      source="visible", ansi=True)
        if screen is None:
            return True                      # unreadable: nothing to go on
        if _modal_open(screen):
            # Typing into an open panel does nothing useful and its keys mean
            # something else, so text waits for it to go. Key presses are how
            # a panel is dismissed, so they go through.
            return allow_modal
        shown = _typed_text(screen)
        if shown is None or not shown:
            return True
        if self._looks_like_ours(pane_id, shown):
            log.info("%s: our previous send is stranded in the box; submitting it first",
                     pane_id)
            ok, _detail = self._press_until_submitted(pane_id, shown)
            return ok
        return False                         # someone is writing

    def _lock_for(self, pane_id):
        with self._lock:
            lock = self._pane_locks.get(pane_id)
            if lock is None:
                lock = self._pane_locks[pane_id] = threading.Lock()
            return lock

    def send_guarded(self, pane_id, send, last_sent_text=None, awaiting_consumption=False,
                     hold_while_typing=True, allow_modal=False):
        """Run `send` (a callable that types into `pane_id`) with the send lock
        held, after one final look at the input box under that same lock.

        The settle check alone is not enough: between `_clear_to_send` returning
        and the keystrokes actually going out, this send has to wait for the
        global `send_lock`, which every other pane's send also holds. On a busy
        flock that gap ran into seconds, and a human who started typing inside
        it got our text spliced onto the end of their sentence (reproduced
        2026-09-23). The final check closes the gap to one pane read.

        Returns (sent, ok, detail): sent=False means a draft appeared and
        nothing was typed."""
        with self._lock_for(pane_id):
            if hold_while_typing and not self._ensure_box_clear(pane_id, allow_modal):
                log.info("%s: draft appeared while waiting for the send lock; not typing",
                         pane_id)
                return False, None, "someone started typing"
            try:
                ok, detail = send()
            except Exception as error:  # noqa: BLE001 - report, never crash a caller
                return True, False, str(error)
            return True, ok, detail

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
                if stop_event.is_set():
                    return
                dunker.status = "Sending..."
                sent, ok, _message = self.send_guarded(
                    dunker.target_pane_id, lambda: self._do_send(dunker),
                    dunker.last_sent_text, dunker.awaiting_consumption,
                    hold_while_typing=dunker.hold_while_typing)
                if not sent:
                    # They started typing while we waited for the send lock.
                    with self._lock:
                        dunker.hold_count += 1
                        dunker.last_held_at = time.time()
                        dunker.status = "Held (typing)"
                        self._save()
                    if stop_event.wait(TYPING_RECHECK_S):
                        return
                    continue

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


def _modal_open(screen):
    """True if the pane is showing a panel or dialog rather than an input box.
    Nothing can be typed into one, and nothing is waiting in it."""
    if not screen:
        return False
    low = _strip_ansi(screen).lower()
    return any(marker in low for marker in MODAL_MARKERS)


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
