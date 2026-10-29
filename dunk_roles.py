#!/usr/bin/env python3
"""Roles: who a caller is, bound to a process, and what that role may call.

Replaces the superuser allowlist (`dunk_superusers.py`) and the desk credential
(`dunk_desk.py`). Design: `docs/FLAT_FLEET.md` section 4.

Three ideas, kept separate on purpose:

1. **Binding.** A pane's role is bound to its root process, `(pane_id,
   pane_pid, start)`, recorded by the launcher at the moment the lane starts
   (`register`). It is never matched by label: a tab title is one `tmux
   select-pane -T owner` away for any lane, and `%N` ids are reused after a
   tmux restart. The server re-verifies the binding on every request with
   `dunk_callers.caller_pane_auth_failure`; a pane whose pid or start no
   longer match is `anonymous` until re-registered.

2. **Service tokens.** A process with no pane (a systemd unit, the web desk)
   proves a role with a token from `service-tokens.json` (sha256 -> role).
   A no-pane request without one is `anonymous`, always: "no TTY means
   system" would be a privilege grant any lane reaches with `setsid -f`.

3. **The allowlist.** `ALLOW` names, per role, the commands it may call.
   `owner` may call everything. Checked once, in `dunk_api.dispatch`, which
   is the single choke point for the CLI, MCP and the socket.

What this does and does not enforce: every lane runs as the same Unix user as
the daemon, so a lane that reads this file, the token files or the owner's own
credentials can bypass all of it. These rules hold against confusion,
generalisation and bugs - the failure modes actually observed - and not
against a lane that sets out to break them (FLAT_FLEET.md 4.4).
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import logging
import os
import secrets
import tempfile
import time
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("DUNKINGSHEEP_DIR",
                                 str(Path.home() / ".config" / "dunkingsheep")))
ROLES_FILE = str(CONFIG_DIR / "roles.json")
TOKENS_FILE = str(CONFIG_DIR / "service-tokens.json")

log = logging.getLogger("dunkingsheep.roles")

ROLES = ("owner", "system", "web", "desk", "planner", "worker", "reviewer",
         "anonymous")
LANE_ROLES = ("desk", "planner", "worker", "reviewer", "owner")
TOKEN_ROLES = ("web", "system", "owner")
DEFAULT_ROLE = "anonymous"
# With no registry file at all (a fresh install, the test suite) a bound pane
# is a worker, so an un-set-up install still works. The first registration
# writes the file with default "anonymous": from then on, unregistered means
# read-only (FLAT_FLEET.md 4.2).
UNCONFIGURED_DEFAULT = "worker"
DEFAULT_REVIEWER_PROVIDERS = ("claude", "codex")

# Read-only commands: every role may call these.
READ_ONLY = frozenset({
    "status", "poll", "health", "help", "doctor", "whoami", "roster",
    "message_state", "reply_stats", "project_summary", "state_latest",
    "task_list", "task_read", "task_stats", "task_metrics", "task_followers",
    "metrics", "inbox_count", "inbox_stats", "inbox_status", "list_panes",
    "list_workspaces", "list_dunks", "get_dunk", "delivery",
    "delivery_capability", "lanes", "memory_recall", "memory_search",
    "memory_context", "memory_stats", "memory_status", "owner_stats",
    "superuser_status", "pm_digest", "pm_routing", "list_messages",
    "get_message", "dispatch", "review_list", "poolrad_progress",
    "owner_receipts",      # delivery receipts the web desk reconciles (read-only)
    # Bootstrap paths: the handlers gate these (first owner may self-register).
    "superuser_recover", "lane_register",
})

# A lane that does work: pulls from the queue, reads and sends mail, notes.
_WORKER = READ_ONLY | frozenset({
    "task_next", "task_heartbeat", "task_release", "task_complete", "task_note",
    "task_block", "task_unblock", "task_follow", "task_unfollow",
    "task_add",            # unassigned work for itself or the queue; see h_task_add
    "task_update",         # title/body/priority only; status and assignees are stripped
    "inbox_read", "inbox_send", "inbox_ack", "inbox_archive", "inbox_fetch",
    "inbox_list", "inbox_delete", "inbox_cancel", "inbox_recover",
    "send_text", "send_keys", "flush_box",   # own pane only (require_send_self)
    "turn_start", "turn_end", "read_pane", "state_capture", "gh_token",
    "context_add", "context_supersede",     # facts under the lane's own authority
})

ALLOW = {
    "owner": None,   # everything
    "system": READ_ONLY | frozenset({
        "task_add", "task_assign", "task_cancel", "task_update", "task_note",
        "task_block", "task_unblock", "task_reap", "inbox_send", "inbox_read",
        "inbox_fetch", "respawn_pane", "send_keys", "flush_box", "read_pane",
        "owner_push", "owner_take", "owner_poll", "owner_receipts",
        "state_capture", "lane_register", "recover", "turn_start", "turn_end",
        "gh_token", "review_sync", "context_add", "poolrad_sync",
    }),
    # The owner's transport (the web desk): everything but the
    # fleet-destructive verbs, which stay on a human in a terminal.
    "web": None,
    "desk": READ_ONLY | frozenset({
        "task_add", "task_note", "inbox_read", "inbox_fetch", "inbox_ack",
        "owner_take", "owner_poll", "owner_receipts", "owner_reply",
        # The owner mailbox is the desk's whole job (FLAT_FLEET.md 7.2): read it,
        # answer it. A lane reads its own mailbox, never a person's conversation.
        "owner_conversation",
        "read_pane", "turn_start", "turn_end",
    }),
    "planner": READ_ONLY | frozenset({
        "task_add", "task_note", "inbox_read", "turn_start", "turn_end",
    }),
    "worker": _WORKER,
    "reviewer": _WORKER,
    "anonymous": READ_ONLY,
}

# Verbs no token role may call: closing, shaping and stopping the fleet stays
# with a human in a terminal (the 2026-10-18 incident, and `web` is a web request).
WEB_DENIED = frozenset({
    "close", "shutdown", "respawn_pane", "start_agent", "create_workspace",
    "create_tab", "split_pane", "rename", "focus", "herdr", "add_dunk",
    "update_dunk", "remove_dunk", "start_dunk", "stop_dunk", "toggle_dunk",
    "stop_all", "fire_dunk", "superuser_add", "superuser_remove",
    "superuser_recover", "memory_ingest", "memory_backfill", "lane_set",
    "lane_register", "token_issue", "token_revoke", "gh_token", "clear_box",
    "retire_pane",
})

# Result caps for the desk: the latency mechanism (FLAT_FLEET.md 4.3).
DESK_LIMIT = 5
DESK_RESULT_BYTES = 4096


class RoleError(RuntimeError):
    """A named refusal."""


def allowed(role, command):
    """May `role` call `command`?"""
    if role == "web":
        return command not in WEB_DENIED
    permitted = ALLOW.get(role, READ_ONLY)
    if permitted is None:
        return True
    return command in permitted


def _hash(token):
    return hashlib.sha256(str(token).encode("utf-8")).hexdigest()


def _read_json(path):
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except OSError:
        return {}
    except ValueError:
        raise RoleError(f"{path} is not valid JSON; fix it or remove it") from None
    return data if isinstance(data, dict) else {}


def _write_json(path, payload):
    directory = os.path.dirname(os.path.abspath(path))
    os.makedirs(directory, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".roles-", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(payload, handle, indent=1, sort_keys=True)
            handle.write("\n")
        os.replace(temporary, path)
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _stamp(path):
    try:
        st = os.stat(path)
        return (st.st_mtime_ns, st.st_size)
    except OSError:
        return None


class Roles:
    """The lane registry plus the service-token table.

    Both files are re-read when they change, so a registration or revocation
    takes effect on the next request without a daemon restart.
    """

    def __init__(self, path=None, tokens_path=None, default=None):
        self.path = path or os.environ.get("DUNKINGSHEEP_ROLES_FILE") or ROLES_FILE
        self.tokens_path = (tokens_path or os.environ.get("DUNKINGSHEEP_TOKENS_FILE")
                            or TOKENS_FILE)
        self._default_override = default
        self.lanes = {}
        self.reviewer_providers = list(DEFAULT_REVIEWER_PROVIDERS)
        self.default = default or DEFAULT_ROLE
        self._tokens = {}
        self._stamp = None
        self._tokens_stamp = None
        self.refresh(force=True)

    # -- persistence ----------------------------------------------------------

    def refresh(self, force=False):
        stamp = _stamp(self.path)
        if force or stamp != self._stamp:
            data = _read_json(self.path)
            lanes = data.get("lanes") or {}
            self.lanes = {str(k): dict(v) for k, v in lanes.items()
                          if isinstance(v, dict) and v.get("role") in ROLES}
            providers = data.get("reviewer_providers")
            self.reviewer_providers = (list(providers) if isinstance(providers, list)
                                       else list(DEFAULT_REVIEWER_PROVIDERS))
            configured = data.get("default")
            if self._default_override:
                self.default = self._default_override
            elif configured in ROLES:
                self.default = configured
            else:
                self.default = DEFAULT_ROLE if data else UNCONFIGURED_DEFAULT
            self._stamp = stamp
        tstamp = _stamp(self.tokens_path)
        if force or tstamp != self._tokens_stamp:
            data = _read_json(self.tokens_path)
            tokens = data.get("tokens") or {}
            self._tokens = {str(k): dict(v) for k, v in tokens.items()
                            if isinstance(v, dict) and v.get("role") in TOKEN_ROLES}
            self._tokens_stamp = tstamp

    def save(self):
        _write_json(self.path, {
            "lanes": self.lanes,
            "reviewer_providers": self.reviewer_providers,
            "default": self.default,
            "updated_at": round(time.time(), 3),
        })
        self._stamp = _stamp(self.path)

    def _save_tokens(self):
        _write_json(self.tokens_path, {"tokens": self._tokens,
                                       "updated_at": round(time.time(), 3)})
        self._tokens_stamp = _stamp(self.tokens_path)

    # -- registry -------------------------------------------------------------

    def register(self, pane_id, role, pane_pid=None, start=None, provider=None,
                 model=None, by=None, label=None):
        pane_id = str(pane_id or "").strip()
        if not pane_id:
            raise RoleError("a registration needs a pane id")
        if role not in LANE_ROLES:
            raise RoleError(f"role must be one of {', '.join(LANE_ROLES)}; got {role!r}")
        self.refresh()
        if not self._default_override and _stamp(self.path) is None:
            self.default = DEFAULT_ROLE      # the first registration ends bootstrap
        entry = {"role": role, "registered_at": round(time.time(), 3)}
        for key, value in (("pane_pid", pane_pid), ("start", start),
                           ("provider", provider), ("model", model),
                           ("by", by), ("label", label)):
            if value not in (None, ""):
                entry[key] = int(value) if key == "pane_pid" else str(value)
        self.lanes[pane_id] = entry
        self.save()
        log.info("role registered pane=%s role=%s pid=%s by=%s", pane_id, role,
                 entry.get("pane_pid"), by)
        return dict(entry, pane_id=pane_id)

    def unregister(self, pane_id):
        self.refresh()
        entry = self.lanes.pop(str(pane_id), None)
        if entry is not None:
            self.save()
        return entry

    def entry(self, pane_id):
        self.refresh()
        return self.lanes.get(str(pane_id or ""))

    def owners(self):
        self.refresh()
        return sorted(p for p, e in self.lanes.items() if e.get("role") == "owner")

    def has_owner(self):
        return bool(self.owners()) or any(
            t.get("role") == "owner" for t in self._tokens.values())

    def role_of(self, pane_id, pane_info=None):
        """The registered role for a pane, or the default.

        When the registration recorded a pane pid and the live pane record
        carries one, they must match: a reused `%N` after a tmux restart is a
        different pane and gets the default role until re-registered.
        """
        self.refresh()
        if not pane_id:
            return DEFAULT_ROLE
        entry = self.lanes.get(str(pane_id))
        if not entry:
            return self.default
        want_pid = entry.get("pane_pid")
        if want_pid is not None:
            # The registration recorded a root pid: the pane must still exist
            # and carry it. A gone pane, or a reused id with a new root, gets the
            # default role until the owner re-registers it.
            live_pid = (pane_info or {}).get("pane_pid")
            if live_pid in (None, "", "None"):
                log.warning("role binding unverifiable pane=%s registered_pid=%s (pane gone?)",
                            pane_id, want_pid)
                return self.default
            try:
                if int(live_pid) != int(want_pid):
                    log.warning("role binding stale pane=%s registered_pid=%s live_pid=%s",
                                pane_id, want_pid, live_pid)
                    return self.default
            except (TypeError, ValueError):
                return self.default
        return entry["role"]

    def provider_of(self, pane_id):
        entry = self.entry(pane_id) or {}
        return (entry.get("provider") or "").strip().lower() or None

    def may_review(self, pane_id):
        """Reviewer role AND a provider on the reviewer list (free models never review)."""
        if self.role_of(pane_id) != "reviewer":
            return False
        provider = self.provider_of(pane_id)
        return bool(provider and provider in {p.lower() for p in self.reviewer_providers})

    # -- service tokens -------------------------------------------------------

    def issue_token(self, role, label=None, by=None):
        if role not in TOKEN_ROLES:
            raise RoleError(f"a token role must be one of {', '.join(TOKEN_ROLES)}")
        self.refresh()
        token = secrets.token_urlsafe(32)
        self._tokens[_hash(token)] = {"role": role, "label": str(label or role),
                                      "issued_at": round(time.time(), 3),
                                      "by": str(by or "")}
        self._save_tokens()
        return token

    def revoke_token(self, label):
        self.refresh()
        before = len(self._tokens)
        self._tokens = {h: t for h, t in self._tokens.items() if t.get("label") != label}
        if len(self._tokens) != before:
            self._save_tokens()
        return before - len(self._tokens)

    def token_role(self, token):
        if not token:
            return None
        self.refresh()
        entry = self._tokens.get(_hash(token))
        return entry.get("role") if entry else None

    def tokens(self):
        self.refresh()
        return [{"label": t.get("label"), "role": t.get("role"),
                 "issued_at": t.get("issued_at")} for t in self._tokens.values()]

    def describe(self):
        self.refresh()
        return {"path": self.path, "tokens_path": self.tokens_path,
                "lanes": {p: dict(e) for p, e in self.lanes.items()},
                "reviewer_providers": list(self.reviewer_providers),
                "default": self.default, "tokens": self.tokens()}


def clamp_desk_args(command, args):
    """Daemon-side result caps for the desk role (FLAT_FLEET.md 4.3)."""
    args = dict(args or {})
    if "limit" in args or command in ("memory_recall", "memory_search", "memory_context",
                                      "task_list", "inbox_read", "inbox_list"):
        try:
            limit = int(args.get("limit") or DESK_LIMIT)
        except (TypeError, ValueError):
            limit = DESK_LIMIT
        args["limit"] = max(1, min(limit, DESK_LIMIT))
    if command in ("read_pane",):
        try:
            lines = int(args.get("lines") or 40)
        except (TypeError, ValueError):
            lines = 40
        args["lines"] = max(1, min(lines, 40))
    return args


def clamp_desk_result(result):
    """Truncate a result the desk could not read in time anyway."""
    try:
        text = json.dumps(result)
    except (TypeError, ValueError):
        return result
    if len(text) <= DESK_RESULT_BYTES:
        return result
    return {"truncated": True, "bytes": len(text), "cap": DESK_RESULT_BYTES,
            "head": text[:DESK_RESULT_BYTES],
            "note": "desk results are capped; ask a narrower question "
                    "(task read <id>, task metrics) or file a task"}
