#!/usr/bin/env python3
"""
Terminal adapter seam.

Everything above this module (Flock, the task/inbox stores, the CLI, MCP)
asks for placements, panes, screen text and sends. It must not know which
terminal tool implements them. The first backend was herdr; tmux is the
second (owner decision 2026-10-18: "tmux is the way"), and it is selected with

    DUNKINGSHEEP_TERMINAL_BACKEND=tmux

Herdr stays the default until the tmux close guard has been demonstrated on
real lanes. Nothing about herdr is removed before that.

The seam deliberately keeps the *grammar* of the tool inside the backend:
herdr 0.9.5 starts an agent with `agent start NAME --kind KIND --pane ID`, while
tmux starts it with `new-window`/`split-window`. Callers only say "tab" or
"workspace" and "right"/"down".
"""

import json
import os
import re
import shlex
import shutil
import subprocess
import time

from herdr_client import (
    AGENT_KINDS,
    SEND_TEXT_CHUNK_CHARS,
    SEND_TEXT_CHUNK_DELAY_S,
    SEND_TEXT_SETTLE_DELAY_S,
    HerdrClient,
    _text_chunks,
)


DEFAULT_TMUX_REGISTRY = os.path.join(
    os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser("~/.config/dunkingsheep"),
    "tmux-panes.json",
)


class TerminalBackend:
    """The small contract every terminal backend implements.

    Only methods the rest of Dunking Sheep actually calls are listed here.
    `capabilities()` is honest about what this terminal can observe: tmux has
    no agent-status API, so working/idle is "unknown" there rather than a
    guess.
    """

    name = "base"
    # False for test doubles/fixtures; the task reaper refuses a pane list
    # from a non-live backend, so a fixture cannot move real work.
    live = False

    def capabilities(self):
        return frozenset()

    # -- annotations shared by every backend ------------------------------

    def tab_labels(self):
        labels = {}
        for tab in self.list_tabs():
            tab_id = tab.get("tab_id")
            if tab_id:
                labels[tab_id] = tab.get("label") or tab.get("number") or tab_id
        return labels

    def list_panes_with_tabs(self):
        labels = self.tab_labels()
        panes = self.list_panes()
        for pane in panes:
            pane["tab_label"] = labels.get(pane.get("tab_id"), pane.get("tab_id", "?"))
        return panes

    def list_panes_grouped(self):
        tabs = {t.get("tab_id"): t for t in self.list_tabs()}
        workspaces = {w.get("workspace_id"): w for w in self.list_workspaces()}
        panes = self.list_panes()
        for pane in panes:
            tab = tabs.get(pane.get("tab_id"), {})
            ws = workspaces.get(pane.get("workspace_id"), {})
            pane["tab_label"] = tab.get("label") or pane.get("tab_id", "?")
            pane["tab_number"] = tab.get("number", 0)
            pane["workspace_label"] = ws.get("label") or pane.get("workspace_id", "?")
            pane["workspace_number"] = ws.get("number", 0)
        panes.sort(key=lambda p: (p.get("workspace_number", 0),
                                  p.get("tab_number", 0)))
        return panes

    def agent_status(self, pane_id):
        return (self.get_pane(pane_id) or {}).get("agent_status")

    def agent_session(self, pane_id):
        return (self.get_pane(pane_id) or {}).get("agent_session")

    @staticmethod
    def self_pane_id():
        return os.environ.get("HERDR_PANE_ID") or os.environ.get("TMUX_PANE") or None

    # -- geometry ----------------------------------------------------------

    def resize(self, pane_id, cols, rows):
        """Backends that cannot size a pane say so instead of pretending."""
        return False, "this terminal backend cannot resize panes"


class HerdrBackend(TerminalBackend):
    """herdr, unchanged: the current default."""

    name = "herdr"

    def __init__(self, client=None):
        self._client = client if client is not None else HerdrClient()
        self.live = client is None or isinstance(client, HerdrClient)

    @property
    def client(self):
        return self._client

    def __getattr__(self, item):
        # The herdr client already has the full method set; do not copy it.
        return getattr(self._client, item)

    def capabilities(self):
        return frozenset({
            "screen", "scrollback", "send", "layout", "pane_identity",
            "agent_status", "agent_session", "close_guard", "typing_guard",
        })

    # These are richer on the herdr client than the generic pane-dict fallback
    # (agent_status is a live query, not a cached field). Delegate explicitly:
    # inheriting the base methods would shadow the client's implementations.
    def tab_labels(self):
        method = getattr(self._client, "tab_labels", None)
        return method() if callable(method) else super().tab_labels()

    def list_panes_with_tabs(self):
        method = getattr(self._client, "list_panes_with_tabs", None)
        return method() if callable(method) else super().list_panes_with_tabs()

    def list_panes_grouped(self):
        method = getattr(self._client, "list_panes_grouped", None)
        return method() if callable(method) else super().list_panes_grouped()

    def agent_status(self, pane_id):
        return self._client.agent_status(pane_id)

    def agent_session(self, pane_id):
        method = getattr(self._client, "agent_session", None)
        if callable(method):
            return method(pane_id)
        return super().agent_session(pane_id)


def _safe_name(label, fallback="dunkingsheep"):
    """tmux session/window names cannot contain `:`, `.` or whitespace."""
    name = re.sub(r"[^A-Za-z0-9_@+-]+", "-", str(label or "").strip()).strip("-")
    return (name or fallback)[:60]


def _agent_kind(command, title):
    """Best-effort agent kind from tmux's foreground command and pane title."""
    first = (str(command or "").strip().split() or [""])[0]
    base = os.path.basename(first).lower()
    for kind in AGENT_KINDS:
        if base == kind or base.startswith(kind):
            return kind
    text = str(title or "").lower()
    for kind in sorted(AGENT_KINDS, key=len, reverse=True):
        if re.search(r"(^|[^a-z0-9])" + re.escape(kind) + r"([^a-z0-9]|$)", text):
            return kind
    return ""


class TmuxBackend(TerminalBackend):
    """tmux backend: sessions are workspaces, windows are tabs, panes are panes.

    Screen and scrollback come from `capture-pane`; literal typing from
    `send-keys -l` plus Enter with the same settle delay herdr uses. Pane
    identity is tmux's own `%N` id (also exported to the pane as TMUX_PANE),
    so callers get a stable registry key without asking a server.

    Close protection is two-layered:
      * this backend refuses to close any pane/window/session it did not
        create (recorded in a small registry);
      * `dunk_tmux_guard.py` shadows tmux for agent PATHs and routes
        kill-pane/kill-window/kill-session through the daemon, which applies
        the superuser/self policy before this backend sees it.
    """

    name = "tmux"
    live = True
    PANE_SEP = "\x1f"
    # The last two fields are APPENDED, never inserted: every earlier field keeps
    # its position, and _parse_panes still accepts a 12-field line (a fixture, an
    # older capture) with cols/rows absent rather than guessing a size.
    PANE_FORMAT = PANE_SEP.join([
        "#{pane_id}", "#{session_id}", "#{window_id}", "#{pane_current_path}",
        "#{pane_current_command}", "#{pane_title}", "#{pane_active}",
        "#{session_name}", "#{window_name}", "#{window_index}",
        "#{session_index}", "#{pane_pid}", "#{pane_width}", "#{pane_height}",
    ])
    TAB_FORMAT = PANE_SEP.join([
        "#{window_id}", "#{session_id}", "#{window_name}", "#{window_index}",
        "#{window_active}",
    ])
    WORKSPACE_FORMAT = PANE_SEP.join([
        "#{session_id}", "#{session_name}", "#{session_index}",
    ])

    def __init__(self, socket_name=None, registry_path=None, tmux_bin=None,
                 guard_dir=None):
        self.socket_name = socket_name if socket_name is not None else \
            (os.environ.get("DUNKINGSHEEP_TMUX_SOCKET") or None)
        self.tmux = tmux_bin or os.environ.get("DUNKINGSHEEP_TMUX_BIN") \
            or shutil.which("tmux") or "tmux"
        self.registry_path = registry_path or os.environ.get(
            "DUNKINGSHEEP_TMUX_REGISTRY") or DEFAULT_TMUX_REGISTRY
        config_dir = (os.environ.get("DUNKINGSHEEP_DIR")
                      or os.path.expanduser("~/.config/dunkingsheep"))
        self.guard_dir = guard_dir or os.environ.get(
            "DUNKINGSHEEP_TMUX_GUARD_DIR") or os.path.join(config_dir, "tmux-guard")

    def capabilities(self):
        return frozenset({
            "screen", "scrollback", "send", "layout", "pane_identity",
            "pane_pid", "close_guard", "resize", "respawn",
        })

    # -- process -----------------------------------------------------------

    def _run(self, args, timeout=None):
        argv = [self.tmux]
        if self.socket_name:
            argv += ["-L", self.socket_name]
        argv += [str(arg) for arg in args]
        try:
            proc = subprocess.run(argv, capture_output=True, text=True,
                                  timeout=timeout or 20)
            return proc.returncode, proc.stdout, proc.stderr
        except FileNotFoundError:
            return 127, "", "tmux binary not found"
        except subprocess.TimeoutExpired:
            return 124, "", "tmux command timed out"
        except Exception as error:  # pragma: no cover - defensive
            return 1, "", str(error)

    def _tmux(self, *args, timeout=None):
        return self._run(args, timeout=timeout)

    def version(self):
        code, out, err = self._tmux("-V", timeout=5)
        text = (out or err).strip()
        return text or None

    def is_available(self):
        code, out, err = self._tmux("list-sessions", timeout=5)
        blob = (out + err).lower()
        return code == 0 or "no server running" in blob or "no sessions" in blob

    def server_error_hint(self):
        if not shutil.which(self.tmux) and not os.path.exists(self.tmux):
            return "tmux not found - install tmux"
        return "no tmux server/session - create one with `tmux new -d -s dunkingsheep`"

    # -- registry ----------------------------------------------------------

    def _load_registry(self):
        try:
            with open(self.registry_path, "r", encoding="utf-8") as handle:
                data = json.load(handle)
        except (OSError, ValueError):
            data = {}
        for key in ("panes", "tabs", "workspaces"):
            data.setdefault(key, {})
        return data

    def _save_registry(self, data):
        directory = os.path.dirname(self.registry_path) or "."
        os.makedirs(directory, exist_ok=True)
        temp = self.registry_path + ".tmp"
        with open(temp, "w", encoding="utf-8") as handle:
            json.dump(data, handle, indent=2, sort_keys=True)
        os.chmod(temp, 0o600)
        os.replace(temp, self.registry_path)

    COLLECTIONS = {"pane": "panes", "tab": "tabs", "workspace": "workspaces"}

    def _remember(self, kind, item, **extra):
        if not item or not item.get(f"{kind}_id"):
            return
        data = self._load_registry()
        record = {"at": int(time.time()), **extra}
        data[self.COLLECTIONS[kind]][item[f"{kind}_id"]] = record
        self._save_registry(data)

    def _forget(self, kind, item_id):
        data = self._load_registry()
        if data[self.COLLECTIONS[kind]].pop(item_id, None) is not None:
            self._save_registry(data)

    def _managed(self, kind, item_id, panes=None):
        data = self._load_registry()
        if item_id not in data[self.COLLECTIONS[kind]]:
            return False
        if kind == "pane":
            return True
        # A window/session may contain panes someone split into it by hand;
        # closing the container would kill those too. Only close it when every
        # pane inside is one we created.
        panes = panes if panes is not None else self.list_panes()
        inside = [p for p in panes
                  if p.get(f"{kind}_id") == item_id]
        return all(p.get("pane_id") in data["panes"] for p in inside)

    # -- discovery ---------------------------------------------------------

    def _parse_panes(self, out):
        data = self._load_registry()
        panes = []
        for line in (out or "").splitlines():
            fields = line.split(self.PANE_SEP)
            if len(fields) < 12:
                continue
            (pane_id, session_id, window_id, cwd, command, title, active,
             session_name, window_name, window_index, session_index,
             pane_pid) = fields[:12]
            kind = _agent_kind(command, title)
            record = data["panes"].get(pane_id) or {}
            # tmux does not expose pane titles through #{pane_title} on every
            # config; the registry name we recorded at start_agent is the
            # stable, human-chosen label the inbox resolves by.
            label = record.get("name") or title.strip() or window_name
            def _int(value, default=0):
                try:
                    return int(str(value).strip())
                except (TypeError, ValueError):
                    return default

            # Each field is parsed on its own. The old single try/except meant a BLANK
            # session_index (tmux leaves it empty when the session has no index set)
            # raised on the first int() and silently nulled pane_pid with it - which is
            # why capture could not identify a lane's agent process, and therefore why
            # session_id was null for every live pane on 2026-10-24.
            number = _int(window_index, 0)
            workspace_number = _int(session_index, 0)
            pid = None if str(pane_pid).strip() in ("", "None") else _int(pane_pid, None)
            # Geometry, so a caller can report the cell grid a lane is being drawn
            # into (and, later, ask for a resize). None when the line did not carry it.
            cols = _int(fields[12], None) if len(fields) > 12 else None
            rows = _int(fields[13], None) if len(fields) > 13 else None
            panes.append({
                "pane_id": pane_id,
                "terminal_id": pane_id,
                "tab_id": window_id,
                "workspace_id": session_id,
                "cwd": cwd,
                "foreground_cwd": cwd,
                "agent": kind,
                "agent_status": "unknown",
                "agent_session": None,
                "focused": active == "1",
                "label": label,
                "terminal_title_stripped": label,
                "terminal_title": title,
                "pane_pid": pid,
                "cols": cols,
                "rows": rows,
                "managed": pane_id in data["panes"],
                "tab_label": window_name,
                "workspace_label": session_name,
                "tab_number": number,
                "workspace_number": workspace_number,
            })
        return panes

    def list_panes(self):
        code, out, err = self._tmux("list-panes", "-a", "-F", self.PANE_FORMAT)
        blob = (out + err).lower()
        if code != 0:
            if "no server running" in blob or "no sessions" in blob:
                return []
            return []
        return self._parse_panes(out)

    def get_pane(self, pane_id):
        for pane in self.list_panes():
            if pane.get("pane_id") == pane_id:
                return pane
        return None

    def list_tabs(self):
        code, out, err = self._tmux("list-windows", "-a", "-F", self.TAB_FORMAT)
        if code != 0:
            return []
        tabs = []
        for line in (out or "").splitlines():
            fields = line.split(self.PANE_SEP)
            if len(fields) < 5:
                continue
            tab_id, workspace_id, label, number, active = fields[:5]
            try:
                number = int(number)
            except ValueError:
                number = 0
            tabs.append({"tab_id": tab_id, "workspace_id": workspace_id,
                         "label": label, "number": number,
                         "active": active == "1"})
        return tabs

    def list_workspaces(self):
        code, out, err = self._tmux("list-sessions", "-F", self.WORKSPACE_FORMAT)
        if code != 0:
            return []
        tabs = self.list_tabs()
        workspaces = []
        for line in (out or "").splitlines():
            fields = line.split(self.PANE_SEP)
            if len(fields) < 3:
                continue
            workspace_id, label, number = fields[:3]
            try:
                number = int(number)
            except ValueError:
                number = 0
            active = next((t["tab_id"] for t in tabs
                           if t.get("workspace_id") == workspace_id
                           and t.get("active")), None)
            workspaces.append({"workspace_id": workspace_id, "label": label,
                               "number": number, "active_tab_id": active})
        return workspaces

    # -- layout ------------------------------------------------------------

    def _env_args(self, env):
        args = []
        for key, value in (env or {}).items():
            args += ["-e", f"{key}={value}"]
        return args

    def _session_target(self, workspace_id=None):
        if workspace_id:
            return workspace_id
        own = self.self_pane_id()
        if own:
            pane = self.get_pane(own)
            if pane:
                return pane.get("workspace_id")
        workspaces = self.list_workspaces()
        return workspaces[0]["workspace_id"] if workspaces else None

    def create_workspace(self, label=None, cwd=None, focus=False, env=None):
        name = _safe_name(label)
        args = ["new-session", "-d", "-s", name]
        if cwd:
            args += ["-c", str(cwd)]
        args += self._env_args(env)
        code, out, err = self._tmux(*args)
        if code != 0:
            return False, (err.strip() or out.strip() or "tmux new-session failed")
        workspace = next((w for w in self.list_workspaces()
                          if w.get("label") == name), None)
        if not workspace:
            return False, "tmux created the session but it is not visible"
        self._remember("workspace", workspace, label=label or name)
        tab = next((t for t in self.list_tabs()
                    if t.get("workspace_id") == workspace["workspace_id"]), None)
        pane = next((p for p in self.list_panes()
                     if p.get("workspace_id") == workspace["workspace_id"]), None)
        if tab:
            self._remember("tab", tab, label=tab.get("label"))
        if pane:
            self._remember("pane", pane, label=pane.get("label"))
        return True, {"workspace": workspace, "tab": tab, "root_pane": pane}

    def create_tab(self, workspace_id=None, label=None, cwd=None, focus=False, env=None):
        target = self._session_target(workspace_id)
        if not target:
            return False, "no tmux session to create a tab in"
        name = _safe_name(label or "tab")
        args = ["new-window", "-d", "-P", "-F",
                self.PANE_SEP.join(["#{window_id}", "#{pane_id}"]),
                "-t", target, "-n", name]
        if cwd:
            args += ["-c", str(cwd)]
        args += self._env_args(env)
        code, out, err = self._tmux(*args)
        if code != 0:
            return False, (err.strip() or out.strip() or "tmux new-window failed")
        fields = (out or "").strip().split(self.PANE_SEP)
        window_id = fields[0] if fields else None
        pane_id = fields[1] if len(fields) > 1 else None
        tab = next((t for t in self.list_tabs() if t.get("tab_id") == window_id), None)
        pane = self.get_pane(pane_id) if pane_id else None
        if not tab or not pane:
            return False, "tmux created the window but it is not visible"
        self._remember("tab", tab, label=label or name)
        self._remember("pane", pane, label=pane.get("label"))
        return True, {"tab": tab, "root_pane": pane}

    def split_pane(self, pane_id, direction="right", cwd=None, ratio=None,
                   focus=False, env=None, command=None):
        args = ["split-window", "-t", pane_id,
                "-h" if str(direction).lower() == "right" else "-v",
                "-P", "-F", "#{pane_id}"]
        if not focus:
            args.append("-d")
        if cwd:
            args += ["-c", str(cwd)]
        if ratio:
            percent = int(float(ratio) * 100) if float(ratio) <= 1 else int(ratio)
            args += ["-p", str(percent)]
        args += self._env_args(env)
        if command:
            args.append(command)
        code, out, err = self._tmux(*args)
        if code != 0:
            return False, (err.strip() or out.strip() or "tmux split-window failed")
        pane = self.get_pane((out or "").strip())
        if not pane:
            return False, "tmux created the split but it is not visible"
        self._remember("pane", pane, label=pane.get("label"))
        return True, {"pane": pane}

    def _agent_command(self, argv):
        argv = [str(part) for part in argv]
        if not argv:
            return None, "command is required"
        kind = os.path.basename(argv[0])
        if kind not in AGENT_KINDS:
            return None, f"unsupported agent kind {kind!r}"
        return kind, shlex.join(argv)

    @staticmethod
    def _agent_launch(command):
        """Wrap an agent command so its exit is visible rather than invisible.

        The pane runs this as its process; without the wrapper a fast exit takes the
        pane and the error with it, and `remain-on-exit` is a window option the caller
        has no handle on. Printing the status and dropping to a shell keeps both.
        """
        return ("sh -c " + shlex.quote(
            f"{command}; printf '\\n[agent exited rc=%s]\\n' \"$?\"; exec sh"))

    def start_agent(self, name, argv, workspace_id=None, tab_id=None, split=None,
                    cwd=None, focus=False, env=None):
        kind, command = self._agent_command(argv)
        if kind is None:
            return False, command
        # Only the agent panes we start get the tmux guard first on PATH; the
        # user's own shell keeps the real tmux. Prefix the command as well as
        # the pane env: the login shell may reset PATH before running it.
        env = dict(env or {})
        guard_wrapper = os.path.join(self.guard_dir, "tmux")
        if os.path.isfile(guard_wrapper):
            env["PATH"] = self.guard_dir + os.pathsep + (
                env.get("PATH") or os.environ.get("PATH", ""))
            command = "PATH=%s %s" % (shlex.quote(env["PATH"]), command)
        if tab_id:
            panes = [p for p in self.list_panes() if p.get("tab_id") == tab_id]
            if not panes:
                return False, f"tab {tab_id} has no pane to split"
            ok, result = self.split_pane(
                panes[0]["pane_id"], direction=split or "right", cwd=cwd,
                focus=focus, env=env, command=self._agent_launch(command))
            if not ok:
                return ok, result
            pane = result["pane"]
        else:
            target = self._session_target(workspace_id)
            if not target:
                return False, "no tmux session to start the agent in"
            args = ["new-window", "-d", "-P", "-F", "#{pane_id}",
                    "-t", target, "-n", _safe_name(name)]
            if cwd:
                args += ["-c", str(cwd)]
            args += self._env_args(env)
            # AN AGENT THAT DIES MUST LEAVE EVIDENCE. Running the command as the
            # window process with remain-on-exit off means any fast exit destroys the
            # pane AND the error text with it: a start that printed "started agent in
            # %24" left nothing three seconds later (VP t115). The wrapper keeps the
            # pane, prints the exit status, and drops to a shell so the failure can be
            # read instead of guessed at.
            args.append(self._agent_launch(command))
            code, out, err = self._tmux(*args)
            if code != 0:
                return False, (err.strip() or out.strip()
                               or "tmux new-window failed")
            pane = self.get_pane((out or "").strip())
            if not pane:
                return False, "tmux started the agent but the pane is not visible"
            tab = next((t for t in self.list_tabs()
                        if t.get("tab_id") == pane["tab_id"]), None)
            if tab:
                self._remember("tab", tab, label=name)
        # Label the pane with the agent name so inbox targeting by name works.
        self._tmux("select-pane", "-t", pane["pane_id"], "-T", str(name))
        pane = self.get_pane(pane["pane_id"]) or pane
        self._remember("pane", pane, agent=name, agent_kind=kind, name=name)
        agent = dict(pane, name=str(name), agent=kind, agent_status="unknown")
        return True, {"agent": agent, "argv": list(argv), "pane_id": pane["pane_id"]}

    # -- mutations ---------------------------------------------------------

    def respawn_pane(self, pane_id, argv, expected_pid=None, expected_start=None):
        """Replace a managed pane's process in place, with an argv command.

        This is the guarded same-pane primitive the fleet activation needs.  It
        refuses a pane Dunking Sheep did not create, a pid/start that no longer
        matches, and a start-time mismatch.  The caller supplies an argv list;
        this method joins it with shlex and wraps it in the same visible-exit
        shell as start_agent, so no caller string is interpreted as shell code.
        """
        if not pane_id:
            return False, "a pane id is required"
        if not argv:
            return False, "an argv command is required"
        pane = self.get_pane(pane_id)
        if pane is None:
            return False, f"pane {pane_id} is not visible"
        if not self._managed("pane", pane_id):
            return False, (f"refusing to respawn pane {pane_id}: Dunking Sheep did "
                           "not create it")
        old_pid = pane.get("pane_pid")
        try:
            old_pid_int = int(old_pid)
        except (TypeError, ValueError):
            return False, f"pane {pane_id} has no pane process id"
        if expected_pid is not None and old_pid_int != int(expected_pid):
            return False, (f"refusing: pane {pane_id} pid is {old_pid_int}, "
                           f"expected {expected_pid}")
        if expected_start is not None:
            from dunk_callers import process_start
            actual_start = process_start(old_pid_int)
            if str(actual_start) != str(expected_start):
                return False, (f"refusing: pane {pane_id} process start is "
                               f"{actual_start!r}, expected {expected_start!r}")
        command = self._agent_launch(shlex.join([str(part) for part in argv]))
        # FINAL identity re-check immediately before the kill/spawn.  The caller
        # has already checked its snapshot; this closes the window in which the
        # pane could have been replaced while idle/mail/queue checks ran.
        current = self.get_pane(pane_id)
        if not current or current.get("pane_pid") is None:
            return False, f"pane {pane_id} disappeared before respawn"
        try:
            current_pid = int(current.get("pane_pid"))
        except (TypeError, ValueError):
            return False, f"pane {pane_id} lost its pane pid before respawn"
        if current_pid != old_pid_int:
            return False, (f"refusing: pane {pane_id} was replaced between check and "
                           f"mutation (pid {old_pid_int} -> {current_pid})")
        if expected_start is not None:
            from dunk_callers import process_start
            if str(process_start(current_pid)) != str(expected_start):
                return False, (f"refusing: pane {pane_id} process changed between check "
                               "and mutation")
        code, out, err = self._tmux("respawn-pane", "-k", "-t", pane_id, command)
        if code != 0:
            return False, (err.strip() or out.strip() or "tmux respawn-pane failed")
        deadline = time.time() + 3.0
        while time.time() < deadline:
            current = self.get_pane(pane_id)
            if current and current.get("pane_pid"):
                try:
                    new_pid = int(current["pane_pid"])
                except (TypeError, ValueError):
                    new_pid = None
                if new_pid is not None and new_pid != old_pid_int:
                    return True, {"pane_id": pane_id, "old_pid": old_pid_int,
                                  "pane_pid": new_pid, "argv": list(argv)}
            time.sleep(0.05)
        return False, (f"pane {pane_id} did not show a new process after respawn")

    def rename(self, kind, target_id, label):
        commands = {
            "workspace": ["rename-session", "-t", target_id, str(label)],
            "tab": ["rename-window", "-t", target_id, str(label)],
            "pane": ["select-pane", "-t", target_id, "-T", str(label)],
        }
        if kind not in commands:
            return False, f"unknown kind {kind!r}"
        code, out, err = self._tmux(*commands[kind])
        if code != 0:
            return False, (err.strip() or out.strip() or f"tmux rename {kind} failed")
        item = (self.get_pane(target_id) if kind == "pane" else
                next((x for x in (self.list_tabs() if kind == "tab"
                                  else self.list_workspaces())
                      if x.get(f"{kind}_id") == target_id), None))
        if kind == "pane":
            self._remember("pane", item, label=label) if item else None
        return True, {kind: item}

    def close(self, kind, target_id):
        if not self._managed(kind, target_id):
            return False, (
                f"refusing to close {kind} {target_id}: Dunking Sheep did not "
                "create it (or it contains panes it did not create)")
        command = {"pane": "kill-pane", "tab": "kill-window",
                   "workspace": "kill-session"}[kind]
        code, out, err = self._tmux(command, "-t", target_id)
        if code != 0:
            return False, (err.strip() or out.strip() or f"tmux {command} failed")
        self._forget(kind, target_id)
        return True, {}

    def focus(self, kind, target_id):
        commands = {
            "pane": ["select-pane", "-t", target_id],
            "tab": ["select-window", "-t", target_id],
            "workspace": ["switch-client", "-t", target_id],
        }
        if kind not in commands:
            return False, f"unknown kind {kind!r}"
        code, out, err = self._tmux(*commands[kind])
        if code != 0:
            return False, (err.strip() or out.strip() or f"tmux focus {kind} failed")
        return True, {}

    def run_command(self, pane_id, command):
        return self.send_text_and_enter(pane_id, command)

    # -- geometry ----------------------------------------------------------

    # tmux's own refusal thresholds are the authority ('width too small', 'width too
    # large'), measured on 2026-10-28: -x 0 -> rc 1 'width too small', -x 99999 -> rc 1
    # 'width too large', -x 1 -y 1 -> accepted. These bounds only stop nonsense before
    # it reaches tmux; they are not a claim about what tmux allows.
    RESIZE_MIN = 1
    RESIZE_MAX = 1000

    def resize(self, pane_id, cols, rows):
        """Size the pane's WINDOW with tmux, then report what tmux actually did.

        tmux has no absolute per-pane size: `resize-window -x/-y` sets the window grid
        and every pane in it re-lays-out. So the return value carries the geometry
        READ BACK after the call, never the request echoed.

        An attached client is reported, not treated as an error: measured on
        2026-10-28, `resize-window` succeeds with a client attached (80x24 client,
        window became 120x40). With window-size=latest that client's next resize event
        will win, so the caller is told `clients_attached` rather than being refused -
        and never told a size held when it may not.
        """
        try:
            cols, rows = int(cols), int(rows)
        except (TypeError, ValueError):
            return False, "cols and rows must be integers"
        if not (self.RESIZE_MIN <= cols <= self.RESIZE_MAX
                and self.RESIZE_MIN <= rows <= self.RESIZE_MAX):
            return False, (f"cols and rows must be between {self.RESIZE_MIN} "
                           f"and {self.RESIZE_MAX}")
        before = self.get_pane(pane_id)
        if before is None:
            return False, f"no such pane {pane_id}"
        window = before.get("tab_id")
        geometry_before = self._window_geometry(window)
        attached = self._client_count(before.get("workspace_id"))
        code, out, err = self._tmux("resize-window", "-t", pane_id,
                                    "-x", str(cols), "-y", str(rows))
        if code != 0:
            # tmux refuses a size before touching the layout (measured: dims unchanged
            # after 'width too small' and 'width too large'), so a refusal reports no
            # geometry at all rather than a before/after that implies a partial result.
            return False, (err.strip() or out.strip() or "tmux refused the resize")
        geometry_after = self._window_geometry(window)
        target = geometry_after.get(pane_id) or {}
        return True, {
            "pane_id": pane_id,
            "requested": {"cols": cols, "rows": rows},
            "cols": target.get("cols"),
            "rows": target.get("rows"),
            # A resize sizes the WINDOW, so say which panes the re-layout actually
            # moved. A caller that asked about one pane otherwise cannot tell that its
            # neighbours changed shape too.
            "window_pane_count": len(geometry_after),
            "resized_panes": [
                {"pane_id": pid, "cols": size["cols"], "rows": size["rows"],
                 "cols_before": (geometry_before.get(pid) or {}).get("cols"),
                 "rows_before": (geometry_before.get(pid) or {}).get("rows")}
                for pid, size in geometry_after.items()
                if size != geometry_before.get(pid)
            ],
            "clients_attached": attached,
            "overridable_by_client": bool(attached),
        }

    def _window_geometry(self, window_id):
        """Measured cols/rows of every pane in one window, keyed by pane id."""
        if not window_id:
            return {}
        return {pane["pane_id"]: {"cols": pane.get("cols"), "rows": pane.get("rows")}
                for pane in self.list_panes() if pane.get("tab_id") == window_id}

    def _client_count(self, session_id):
        """How many clients are attached to the pane's session (unknown -> None)."""
        if not session_id:
            return None
        code, out, err = self._tmux("list-clients", "-t", session_id,
                                    "-F", "#{client_name}")
        if code != 0:
            return None
        return len([line for line in out.splitlines() if line.strip()])

    # -- sending -----------------------------------------------------------

    def send_text(self, pane_id, text):
        if not pane_id:
            return False, "No target"
        chunks = list(_text_chunks(text))
        for index, chunk in enumerate(chunks):
            code, out, err = self._tmux("send-keys", "-t", pane_id, "-l", "--", chunk)
            if code != 0:
                return False, (err.strip() or out.strip() or "tmux send-keys failed")
            if index + 1 < len(chunks):
                time.sleep(SEND_TEXT_CHUNK_DELAY_S)
        return True, "sent"

    def send_text_and_enter(self, pane_id, text):
        ok, message = self.send_text(pane_id, text)
        if not ok:
            return ok, message
        time.sleep(SEND_TEXT_SETTLE_DELAY_S)
        code, out, err = self._tmux("send-keys", "-t", pane_id, "Enter")
        if code != 0:
            return False, (err.strip() or out.strip() or "tmux send-keys Enter failed")
        return True, "sent"

    def send_keys(self, pane_id, keys):
        if not pane_id:
            return False, "No target"
        keys = [keys] if isinstance(keys, str) else [str(key) for key in keys]
        code, out, err = self._tmux("send-keys", "-t", pane_id, *keys)
        if code != 0:
            return False, (err.strip() or out.strip() or "tmux send-keys failed")
        return True, "sent"

    # -- inspection --------------------------------------------------------

    def read_pane(self, pane_id, lines=40, source=None, ansi=False):
        args = ["capture-pane", "-p", "-t", pane_id]
        if source != "visible":
            args += ["-S", f"-{abs(int(lines))}"]
        if ansi:
            args.append("-e")
        code, out, err = self._tmux(*args)
        if code != 0:
            return None
        return out.lstrip("\n")


# The backend name the daemon uses when nothing is set. One constant, because a refusal message
# that repeats a default is a message that can drift from the default (it did: "herdr" survived
# here long after tmux became the default, so GATE 5 could name a backend nobody was running).
DEFAULT_BACKEND_NAME = "tmux"


def backend_name():
    """The backend that would be selected now: the env override, else the default.

    The final `or` is not redundant: a WHITESPACE-ONLY value ("   ") strips to "", and
    get_backend() maps "" to the default - so without it this function would report "" while the
    daemon ran tmux, and GATE 5 would name a backend that is not what runs (reviewer on #40).
    """
    return ((os.environ.get("DUNKINGSHEEP_TERMINAL_BACKEND") or "").strip().lower()
            or DEFAULT_BACKEND_NAME)


def get_backend(herdr=None):
    """Return the configured terminal backend.

    Passing an existing herdr client/fake (tests, the daemon's constructor)
    wraps it in `HerdrBackend`; otherwise `DUNKINGSHEEP_TERMINAL_BACKEND`
    selects herdr (default) or tmux. An unknown value is a named refusal, not
    a silent fallback.
    """
    if herdr is not None:
        if isinstance(herdr, TerminalBackend):
            return herdr
        return HerdrBackend(herdr)
    # TMUX IS THE DEFAULT (owner decision on t50, 2026-10-24). herdr was the default
    # for the whole life of the daemon, so the flip depended on one systemd drop-in
    # setting the variable: any unit that lost that drop-in silently moved the daemon
    # back onto a backend with no agents on it (herdr pane list: 47 panes, 0 agents).
    # A default that needs a drop-in to be correct is not a default.
    #
    # herdr is still selectable, explicitly, for the week-long rollback:
    #   DUNKINGSHEEP_TERMINAL_BACKEND=herdr
    name = backend_name()
    if name in ("", "tmux"):
        return TmuxBackend()
    if name == "herdr":
        return HerdrBackend()
    raise ValueError(
        f"unknown terminal backend {name!r}; expected 'tmux' or 'herdr'")
