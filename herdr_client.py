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

# The agent kinds `herdr agent start --kind` accepts (0.9.5). The command in a
# start_agent request is the agent itself; its basename must be one of these.
AGENT_KINDS = frozenset({
    "pi", "claude", "codex", "gemini", "cursor", "devin", "agy", "cline",
    "omp", "mastracode", "opencode", "copilot", "kimi", "kiro", "droid",
    "amp", "grok", "hermes", "kilo", "qodercli", "qwen", "letta", "maki",
    "muse",
})


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


GUARD_MARKER = b"dunkingsheep-herdr-guard"


def _is_guard(path):
    """True if `path` is the Dunking Sheep herdr wrapper rather than herdr."""
    try:
        with open(path, "rb") as handle:
            return GUARD_MARKER in handle.read(400)
    except OSError:
        return False


def _find_herdr():
    """Locate the *real* herdr binary, preferring PATH then the usual install
    dir. `DUNKINGSHEEP_HERDR_BIN` overrides. The guard wrapper (see
    `dunkingsheep guard`) shadows herdr on agents' PATH and routes typing
    through this daemon, so resolving to it here would recurse; its directory
    is skipped."""
    override = os.environ.get("DUNKINGSHEEP_HERDR_BIN")
    if override and os.path.exists(override) and not _is_guard(override):
        return override
    guard_dir = os.path.realpath(GUARD_DIR)
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory or os.path.realpath(directory) == guard_dir:
            continue
        candidate = os.path.join(directory, "herdr")
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            if _is_guard(candidate):
                # The guard shadows herdr in place; the real one sits beside it.
                sibling = os.path.join(directory, "herdr-real")
                if os.path.isfile(sibling) and os.access(sibling, os.X_OK):
                    return sibling
                continue
            return candidate
    for fallback in (os.path.expanduser("~/.local/bin/herdr-real"),
                     os.path.expanduser("~/.local/bin/herdr")):
        if os.path.exists(fallback) and not _is_guard(fallback):
            return fallback
    return "herdr"  # let subprocess raise FileNotFoundError if truly missing


class HerdrClient:
    """Stateless helper. Each call is an independent `herdr ...` invocation."""

    def __init__(self, binary=None, default_timeout=8):
        self.binary = binary or _find_herdr()
        self.default_timeout = default_timeout

    # -- low level ---------------------------------------------------------

    def _run(self, args, timeout=None):
        """Run `herdr <args>`; return (returncode, stdout, stderr).

        DUNKINGSHEEP_GUARD marks this as a call made *by* the daemon, so if
        `self.binary` turns out to be the guard wrapper the wrapper execs the
        real herdr instead of routing back into the daemon. Without it a
        long-running daemon that resolved `herdr` before the guard was
        installed recurses into itself: every internal send becomes a new
        `dunkingsheep send`, which times out after 8 s and backs the queue up.
        That happened on 2026-10-11 and is what this flag prevents."""
        env = dict(os.environ, DUNKINGSHEEP_GUARD="1")
        try:
            proc = subprocess.run(
                [self.binary, *args],
                capture_output=True,
                text=True,
                env=env,
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

    def version(self):
        """The herdr CLI version string (e.g. 'herdr 0.9.5'), or None."""
        for args in (["--version"], ["version"]):
            code, out, err = self._run(args, timeout=5)
            text = (out or "").strip() or (err or "").strip()
            if code != 0 or not text:
                continue
            return text.splitlines()[-1].strip()
        return None

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
        """Start agent `name` in a new shell pane.

        herdr 0.9.5's grammar is

            herdr agent start <NAME> --kind <KIND> --pane <ID> [OPTIONS] [-- ARG...]

        It starts an agent in an *existing* pane at a shell prompt; it has no
        `--workspace`, `--tab`, `--split` or `--cwd`. So create/locate the pane
        first (a split inside `tab_id`, otherwise a new tab in `workspace_id`),
        then call that exact shape. Keeping this grammar here, below the
        terminal-adapter seam, is the point of the seam: callers ask for a
        placement, not for herdr flags.
        """
        argv = [str(part) for part in argv]
        if not argv:
            return False, "command is required"
        kind = os.path.basename(argv[0])
        if kind not in AGENT_KINDS:
            return False, (
                f"unsupported agent kind {kind!r}; create the pane and start it "
                "yourself, or use one of: " + ", ".join(sorted(AGENT_KINDS))
            )

        pane_id = None
        if tab_id:
            panes = [pane for pane in self.list_panes()
                     if pane.get("tab_id") == tab_id]
            if not panes:
                return False, f"tab {tab_id} has no pane to split"
            ok, result = self.split_pane(
                panes[0]["pane_id"], direction=split or "right", cwd=cwd,
                focus=focus, env=env)
            if not ok:
                return ok, result
            pane_id = (result.get("pane") or {}).get("pane_id")
        elif workspace_id:
            ok, result = self.create_tab(
                workspace_id=workspace_id, label=name, cwd=cwd, focus=focus,
                env=env)
            if not ok:
                return ok, result
            pane_id = (result.get("root_pane") or {}).get("pane_id")
        else:
            return False, "workspace or tab is required"
        if not pane_id:
            return False, "herdr did not return the new pane's id"

        args = ["agent", "start", str(name), "--kind", kind,
                "--pane", pane_id, "--timeout", "120000", "--", *argv[1:]]
        ok, result = self._run_result(args, timeout=150)
        if not ok:
            # Do not leave the shell we just created behind as an orphan.
            self.close("pane", pane_id)
            return ok, result
        agent = dict((result or {}).get("agent") or {})
        agent.setdefault("pane_id", pane_id)
        agent.setdefault("name", str(name))
        # A unique pane label makes the new agent addressable by name (the
        # inbox resolves exact labels before agent kinds). Best effort: the
        # agent is already running even if the rename is refused.
        self.rename("pane", pane_id, name)
        return True, {"agent": agent, "argv": argv, "pane_id": pane_id}

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
