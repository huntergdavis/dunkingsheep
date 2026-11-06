"""Write a SANITIZED whole-state dunks.json with the Python implementation.

Usage: python3 -I make_python_fixture.py <repo-root> <output.json>

Every value is synthetic (example pane ids, example paths, example text). The file is
produced by dunk_core.Flock._save() itself, so its bytes are exactly what the Python daemon
writes. The state directory is a fresh temp dir; no live config or herdr is touched.
"""
import os
import sys
import tempfile

REPO, OUT = sys.argv[1], sys.argv[2]
sys.path.insert(0, REPO)
import dunk_core  # noqa: E402

FAR_FUTURE = 4102444800.25  # 2100-01-01: a running dunk keeps this deadline on restore


class NoHerdr:
    """_save() never touches the terminal backend; a stub keeps the fixture hermetic."""


tmp = tempfile.mkdtemp(prefix="dunks-fixture-")
state = os.path.join(tmp, "dunks.json")
flock = dunk_core.Flock(herdr=NoHerdr(), state_file=state,
                        launches_file=os.path.join(tmp, "launches.json"), restore=False)
try:
    flock.dunkers = [
        dunk_core.Dunker(
            "d1", name="triage", target_pane_id="w9:p1", target_agent="claude",
            target_tab="example-tab", target_workspace="example-ws",
            target_cwd="/home/example/project", interval_minutes=15.0,
            text="continue", running=True, max_sends=5, send_count=2,
            created_at=1793000000.5, started_at=1793000100.25,
            last_sent_at=1793000400.125, next_send_at=FAR_FUTURE,
            skip_if_unconsumed=True, skip_count=1, total_skips=3,
            last_skipped_at=1793000200.0, last_sent_text="continue",
            hold_while_typing=True),
        dunk_core.Dunker(
            "d2", name="status", target_pane_id="w9:p2", target_agent="codex",
            target_cwd="/home/example/other", interval_minutes=2.5,
            text="status? — reply briefly", running=False, only_when="idle",
            created_at=1793000010.75, last_error="could not reach pane w9:p2",
            awaiting_consumption=True, unknown_skips=1),
        dunk_core.Dunker(
            "d3", name="quoted", interval_minutes=0.5,
            text='say "hi" and \\ then\tstop\x01', running=False,
            created_at=1793000020.0, hold_count=2, last_held_at=1793000030.5),
        dunk_core.Dunker(
            "d4", name="", target_pane_id="w9:p4", target_agent="pi",
            target_cwd="/home/example/pi", interval_minutes=0.001,
            text="check \U0001F600 build", running=False, max_sends=2, send_count=2,
            created_at=1793000040.1234567, last_sent_at=1793000050.0,
            last_sent_text="check \U0001F600 build"),
        dunk_core.Dunker(
            "d5", name="idle-check", target_pane_id="w9:p5", target_agent="claude",
            target_cwd="/home/example/five", interval_minutes=60.0, text="ping",
            running=False, only_when="idle", skip_if_unconsumed=True, skip_count=4,
            total_skips=9, last_skipped_at=1793000060.0, hold_count=1,
            last_held_at=1793000065.75, created_at=1793000055.0),
        dunk_core.Dunker(
            "d6", name="orphan", interval_minutes=30.0, text="continue",
            running=False, created_at=1793000070.0),
    ]
    flock.next_id = 7
    flock.next_message_id = 12
    flock.decided = {
        "m30": {"status": "delivered", "created_at": 1793000300.0},
        "m4": {"status": "cancelled", "created_at": 1793000040.25},
        "m17": {"status": "failed", "created_at": 1793000170.5},
        "m9": {"status": "delivered", "created_at": None},
        "m21": {"status": "skipped", "created_at": 1793000210.0},
    }
    assert flock._save() is True
finally:
    flock.close()
with open(state, "rb") as handle:
    data = handle.read()
os.makedirs(os.path.dirname(OUT), exist_ok=True)
with open(OUT, "wb") as handle:
    handle.write(data)
print("wrote", OUT, len(data), "bytes")
