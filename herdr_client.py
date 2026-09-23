#!/usr/bin/env python3
"""
Thin wrapper around the `herdr` CLI, which speaks to the running herdr server
over its unix socket and returns JSON. Dunking Sheep uses this the same way
Dunking Bird used ydotool/kdotool: shell out to a trusted local binary.

Everything here is best-effort and never raises to the caller; methods return
plain data or (ok, message) tuples so the TUI can render a status string.
"""

import json
import os
import shutil
import subprocess
import time


# Keep individual paste events below Codex's large-paste placeholder path.  A
# long placeholder can consume Enter instead of submitting; smaller ordered
# paste events reconstruct the same text without entering that state.
SEND_TEXT_CHUNK_CHARS = 900
SEND_TEXT_CHUNK_DELAY_S = 0.05

# Codex's paste-burst detector suppresses Enter for 120 ms after burst activity.
# Cross that window with margin before sending the submit key.
SEND_TEXT_SETTLE_DELAY_S = 0.2


def _text_chunks(text, max_chars=SEND_TEXT_CHUNK_CHARS):
    """Yield non-empty character chunks without splitting a CRLF pair."""
    if max_chars <= 0:
        raise ValueError("max_chars must be positive")
    start = 0
    while start < len(text):
        end = min(len(text), start + max_chars)
        if end < len(text) and text[end - 1:end + 1] == "\r\n":
            # Prefer moving the pair to the next chunk.  With a one-character
            # limit that would make no progress, so let this chunk exceed the
            # requested size by one instead.
            if end - start == 1:
                end += 1
            else:
                end -= 1
        yield text[start:end]
        start = end


GUARD_DIR = os.path.join(
    os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser("~/.config/dunkingsheep"),
    "bin",
)


def _find_herdr():
    """Locate the *real* herdr binary, preferring PATH then the usual install
    dir. `DUNKINGSHEEP_HERDR_BIN` overrides. The guard wrapper (see
    `dunkingsheep guard`) shadows herdr on agents' PATH and routes typing
    through this daemon, so resolving to it here would recurse; its directory
    is skipped."""
    override = os.environ.get("DUNKINGSHEEP_HERDR_BIN")
    if override and os.path.exists(override):
        return override
    guard_dir = os.path.realpath(GUARD_DIR)
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory or os.path.realpath(directory) == guard_dir:
            continue
        candidate = os.path.join(directory, "herdr")
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    found = shutil.which("herdr")
    if found and os.path.realpath(os.path.dirname(found)) != guard_dir:
        return found
    fallback = os.path.expanduser("~/.local/bin/herdr")
    if os.path.exists(fallback):
        return fallback
    return "herdr"  # let subprocess raise FileNotFoundError if truly missing


class HerdrClient:
    """Stateless helper. Each call is an independent `herdr ...` invocation."""

    def __init__(self, binary=None, default_timeout=8):
        self.binary = binary or _find_herdr()
        self.default_timeout = default_timeout

    # -- low level ---------------------------------------------------------

    def _run(self, args, timeout=None):
        """Run `herdr <args>`; return (returncode, stdout, stderr)."""
        try:
            proc = subprocess.run(
                [self.binary, *args],
                capture_output=True,
                text=True,
                timeout=timeout or self.default_timeout,
            )
            return proc.returncode, proc.stdout, proc.stderr
        except FileNotFoundError:
            return 127, "", "herdr binary not found"
        except subprocess.TimeoutExpired:
            return 124, "", "herdr command timed out"
        except Exception as e:  # pragma: no cover - defensive
            return 1, "", str(e)

    def _run_json(self, args, timeout=None):
        """Run a command whose stdout is a single JSON object; return the dict
        under `result`, or None on any failure."""
        code, out, _err = self._run(args, timeout=timeout)
        if code != 0 or not out.strip():
            return None
        try:
            payload = json.loads(out)
        except json.JSONDecodeError:
            return None
        if isinstance(payload, dict) and "error" in payload:
            return None
        if isinstance(payload, dict) and "result" in payload:
            return payload["result"]
        return payload

    def _run_result(self, args, timeout=None):
        """Run a JSON-printing herdr command; return (True, result) or
        (False, message) with herdr's own error text when it refuses."""
        code, out, err = self._run(args, timeout=timeout)
        payload = None
        for stream in (out, err):  # herdr prints JSON errors on stderr
            if stream.strip():
                try:
                    payload = json.loads(stream)
                    break
                except json.JSONDecodeError:
                    payload = None
        if isinstance(payload, dict) and isinstance(payload.get("error"), dict):
            error = payload["error"]
            return False, error.get("message") or error.get("code") or "herdr refused"
        if code != 0:
            return False, (err.strip() or out.strip() or f"herdr exited {code}")
        if isinstance(payload, dict) and "result" in payload:
            return True, payload["result"]
        return True, payload if payload is not None else {}

    # -- health ------------------------------------------------------------

    def is_available(self):
        """True if the herdr binary exists and the server is reachable."""
        code, out, err = self._run(["status", "server"], timeout=5)
        if code == 127:
            return False
        blob = (out + err).lower()
        return "running" in blob and "status" in blob

    def server_error_hint(self):
        """Human-readable reason the server looks unavailable."""
        if shutil.which("herdr") is None and not os.path.exists(
            os.path.expanduser("~/.local/bin/herdr")
        ):
            return "herdr not found - install herdr"
        return "herdr server not running - start herdr"

    # -- discovery ---------------------------------------------------------

    def list_panes(self):
        """Return a list of pane dicts (possibly empty)."""
        result = self._run_json(["pane", "list"])
        if not result:
            return []
        return result.get("panes", [])

    def get_pane(self, pane_id):
        """Return a single pane dict, or None if it no longer exists."""
        for pane in self.list_panes():
            if pane.get("pane_id") == pane_id:
                return pane
        return None

    def list_tabs(self):
        """Return a list of tab dicts (possibly empty)."""
        result = self._run_json(["tab", "list"])
        if not result:
            return []
        return result.get("tabs", [])

    def tab_labels(self):
        """Map of tab_id -> human label for annotating panes."""
        labels = {}
        for tab in self.list_tabs():
            tab_id = tab.get("tab_id")
            if tab_id:
                labels[tab_id] = tab.get("label") or tab.get("number") or tab_id
        return labels

    def list_panes_with_tabs(self):
        """Panes from `list_panes`, each annotated with a `tab_label` key."""
        labels = self.tab_labels()
        panes = self.list_panes()
        for pane in panes:
            pane["tab_label"] = labels.get(pane.get("tab_id"), pane.get("tab_id", "?"))
        return panes

    def list_workspaces(self):
        """Return a list of workspace dicts (possibly empty)."""
        result = self._run_json(["workspace", "list"])
        if not result:
            return []
        return result.get("workspaces", [])

    def list_panes_grouped(self):
        """Panes annotated with tab + workspace labels/numbers and sorted by
        (workspace number, tab number) so callers can group them for display.

        Each pane gains: `tab_label`, `tab_number`, `workspace_label`,
        `workspace_number`.
        """
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
        panes.sort(key=lambda p: (p.get("workspace_number", 0), p.get("tab_number", 0)))
        return panes

    # -- layout control ----------------------------------------------------
    # Thin wrappers over `herdr workspace|tab|pane|agent ...`. Each returns
    # (ok, result-or-message); `result` is herdr's own JSON (`workspace`,
    # `tab`, `root_pane`, `pane`, `agent` keys as the command provides).

    @staticmethod
    def _placement(cwd=None, label=None, focus=False, env=None):
        args = []
        if cwd:
            args += ["--cwd", str(cwd)]
        if label:
            args += ["--label", str(label)]
        for key, value in (env or {}).items():
            args += ["--env", f"{key}={value}"]
        args.append("--focus" if focus else "--no-focus")
        return args

    def create_workspace(self, label=None, cwd=None, focus=False, env=None):
        return self._run_result(
            ["workspace", "create", *self._placement(cwd, label, focus, env)]
        )

    def create_tab(self, workspace_id=None, label=None, cwd=None, focus=False, env=None):
        args = ["tab", "create"]
        if workspace_id:
            args += ["--workspace", workspace_id]
        return self._run_result(args + self._placement(cwd, label, focus, env))

    def split_pane(self, pane_id, direction="right", cwd=None, ratio=None, focus=False,
                   env=None):
        args = ["pane", "split", pane_id, "--direction", direction]
        if ratio:
            args += ["--ratio", str(ratio)]
        return self._run_result(args + self._placement(cwd, None, focus, env))

    def start_agent(self, name, argv, workspace_id=None, tab_id=None, split=None,
                    cwd=None, focus=False, env=None):
        """`herdr agent start`: open a pane running `argv` and register it as
        agent `name`, so herdr tracks it by that name."""
        args = ["agent", "start", str(name)]
        if workspace_id:
            args += ["--workspace", workspace_id]
        if tab_id:
            args += ["--tab", tab_id]
        if split:
            args += ["--split", split]
        args += self._placement(cwd, None, focus, env)
        return self._run_result(args + ["--", *argv], timeout=20)

    def rename(self, kind, target_id, label):
        return self._run_result([kind, "rename", target_id, str(label)])

    def close(self, kind, target_id):
        return self._run_result([kind, "close", target_id])

    def focus(self, kind, target_id):
        if kind == "pane":
            # `pane focus` is directional; the agent namespace focuses by id.
            return self._run_result(["agent", "focus", target_id])
        return self._run_result([kind, "focus", target_id])

    def run_command(self, pane_id, command):
        """Type a shell command + Enter into a pane (`herdr pane run`)."""
        code, out, err = self._run(["pane", "run", pane_id, str(command)])
        if code != 0:
            return False, (err.strip() or out.strip() or "pane run failed")
        return True, "ran"

    # -- sending -----------------------------------------------------------

    def send_text_and_enter(self, pane_id, text):
        """Send literal text to a pane, then press Enter. Returns (ok, msg)."""
        if not pane_id:
            return False, "No target"
        chunks = list(_text_chunks(text))
        for index, chunk in enumerate(chunks):
            code, _out, err = self._run(["pane", "send-text", pane_id, chunk])
            if code != 0:
                return False, (err.strip() or "send-text failed")
            if index + 1 < len(chunks):
                time.sleep(SEND_TEXT_CHUNK_DELAY_S)

        # Give terminal apps time to flush their paste/burst state.  Sending
        # Enter inside that state inserts a newline instead of submitting.
        time.sleep(SEND_TEXT_SETTLE_DELAY_S)
        code, _out, err = self._run(["pane", "send-keys", pane_id, "Enter"])
        if code != 0:
            return False, (err.strip() or "send-keys failed")
        return True, "sent"

    def send_keys(self, pane_id, keys):
        """Press one or more named keys in a pane (`herdr pane send-keys`)."""
        if not pane_id:
            return False, "No target"
        keys = [keys] if isinstance(keys, str) else [str(k) for k in keys]
        code, out, err = self._run(["pane", "send-keys", pane_id, *keys])
        if code != 0:
            return False, (err.strip() or out.strip() or "send-keys failed")
        return True, "sent"

    def send_text(self, pane_id, text):
        """Send literal text to a pane without pressing Enter. Returns (ok, msg)."""
        if not pane_id:
            return False, "No target"
        chunks = list(_text_chunks(text))
        for index, chunk in enumerate(chunks):
            code, _out, err = self._run(["pane", "send-text", pane_id, chunk])
            if code != 0:
                return False, (err.strip() or "send-text failed")
            if index + 1 < len(chunks):
                time.sleep(SEND_TEXT_CHUNK_DELAY_S)
        return True, "sent"

    # -- inspection --------------------------------------------------------

    def agent_status(self, pane_id):
        """Return the pane's agent_status (idle|working|blocked|unknown), or
        None if the pane cannot be found."""
        result = self._run_json(["pane", "get", pane_id])
        if not result:
            return None
        pane = result.get("pane") or {}
        return pane.get("agent_status") or "unknown"

    def read_pane(self, pane_id, lines=40, source=None, ansi=False):
        """Return the last `lines` lines of a pane's output as text, or None if
        the pane cannot be read. With `ansi=True` the text keeps its SGR
        styling (colours, dim), which is how the daemon tells typed input from
        an agent's dim placeholder hint.

        `herdr pane read` prints plain text (not JSON). The `recent` source
        includes scrollback but comes back empty for a pane herdr has never
        displayed, so fall back to the `visible` screen in that case."""
        sources = [source] if source else ["recent", "visible"]
        for src in sources:
            args = ["pane", "read", pane_id, "--source", src, "--lines", str(int(lines))]
            if ansi:
                args += ["--format", "ansi"]
            code, out, _err = self._run(args)
            if code != 0:
                return None
            if out.strip():
                # herdr pads the top with the empty rows above the content.
                return out.lstrip("\n")
        return ""

    @staticmethod
    def self_pane_id():
        """The pane this process runs in, if herdr launched it."""
        return os.environ.get("HERDR_PANE_ID") or None

    def notify(self, title, body=None):
        """Fire-and-forget herdr toast notification."""
        args = ["notification", "show", title]
        if body:
            args += ["--body", body]
        self._run(args, timeout=5)
