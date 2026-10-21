"""Test isolation: never touch the live Dunking Sheep state.

Must be imported before any `dunk_*` module, because those modules compute
their default paths from DUNKINGSHEEP_DIR at import time. Without this, the
Flock tests used the real tasks/inbox databases and their fake reaper requeued
real t6/t29 claims (the 2026-10-21 false-reap incident).
"""
import os
import tempfile

_TMP = tempfile.mkdtemp(prefix="dunkingsheep-tests-")
os.environ.setdefault("DUNKINGSHEEP_DIR", _TMP)
os.environ.setdefault("DUNKINGSHEEP_SOCKET", os.path.join(_TMP, "test.sock"))

# Tests must not shell out to `claude agents --json` (2.9 s per call).
os.environ.setdefault("DUNKINGSHEEP_CLAUDE_STATUS", "0")
os.environ.setdefault("DUNKINGSHEEP_MIN_POLL_S", "0")  # tests call the CLI repeatedly
