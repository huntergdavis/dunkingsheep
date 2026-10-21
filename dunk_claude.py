#!/usr/bin/env python3
"""Claude Code session status from the tool itself (owner #1173).

`claude agents --json` returns one object per live session with
pid/cwd/kind/startedAt/sessionId/name and **status: idle|busy**. That is the
busy/idle signal tmux does not expose and the one the tmux trial's gate 2 was
missing. The call costs seconds, so the result is cached briefly; a caller that
needs certainty at that instant passes fresh=True. If the command fails or no
session matches the pane's cwd, the status is UNKNOWN (None), never a guess.
"""

import json
import os
import subprocess
import time

DEFAULT_TTL_S = float(os.environ.get("DUNKINGSHEEP_CLAUDE_STATUS_TTL_S", "20"))


class ClaudeStatus:
    def __init__(self, ttl=DEFAULT_TTL_S, binary=None, timeout=10):
        self.ttl = float(ttl)
        self.binary = binary or os.environ.get("DUNKINGSHEEP_CLAUDE_BIN") or "claude"
        self.timeout = timeout
        self._cached_at = 0.0
        self._agents = None

    def agents(self, fresh=False):
        """The live session list, cached for `ttl`; None when unavailable."""
        now = time.time()
        if not fresh and self._agents is not None and now - self._cached_at < self.ttl:
            return self._agents
        try:
            proc = subprocess.run([self.binary, "agents", "--json"],
                                  capture_output=True, text=True,
                                  timeout=self.timeout)
            if proc.returncode != 0 or not proc.stdout.strip():
                data = None
            else:
                data = json.loads(proc.stdout)
                if not isinstance(data, list):
                    data = None
        except (OSError, ValueError, subprocess.SubprocessError):
            data = None
        self._agents = data
        self._cached_at = now
        return data

    def session_for_pane(self, pane, fresh=False):
        """The one session whose cwd is the pane's cwd, or None.

        cwd is unique per lane here; a duplicate cwd is ambiguous, so it is
        reported as no match (UNKNOWN) rather than picking one.
        """
        agents = self.agents(fresh=fresh)
        if not agents:
            return None
        cwd = (pane or {}).get("cwd")
        if not cwd:
            return None
        matches = [agent for agent in agents if agent.get("cwd") == cwd]
        return matches[0] if len(matches) == 1 else None

    def status_for_pane(self, pane, fresh=False):
        session = self.session_for_pane(pane, fresh=fresh)
        if not session:
            return None
        status = session.get("status")
        return status if status in ("idle", "busy") else None
