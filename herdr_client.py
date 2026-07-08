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


def _find_herdr():
    """Locate the herdr binary, preferring PATH then the usual install dir."""
    found = shutil.which("herdr")
    if found:
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

    # -- sending -----------------------------------------------------------

    def send_text_and_enter(self, pane_id, text):
        """Send literal text to a pane, then press Enter. Returns (ok, msg)."""
        if not pane_id:
            return False, "No target"
        code, _out, err = self._run(["pane", "send-text", pane_id, text])
        if code != 0:
            return False, (err.strip() or "send-text failed")
        code, _out, err = self._run(["pane", "send-keys", pane_id, "Enter"])
        if code != 0:
            return False, (err.strip() or "send-keys failed")
        return True, "sent"

    def notify(self, title, body=None):
        """Fire-and-forget herdr toast notification."""
        args = ["notification", "show", title]
        if body:
            args += ["--body", body]
        self._run(args, timeout=5)
