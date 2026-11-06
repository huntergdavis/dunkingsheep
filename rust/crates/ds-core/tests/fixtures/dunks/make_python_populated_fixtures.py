"""Write two SANITIZED populated state files with the Python implementation (t3309).

Usage: python3 -I make_python_populated_fixtures.py <repo-root> <out-dir>

Writes two files into <out-dir>:

- python-written-populated-outbox.json: queued outbox rows with holds, a notice with its
  notify_for ids and a box hold signature, and a modal hold signature.
- python-written-populated-stuck.json: stuck candidates recorded by the real
  _note_stuck_candidate and _reconcile_stuck, a stuck tracker with per-pane records and the
  spent ledger, and a non-default stuck_config.

Both bytes come from dunk_core.Flock._save() itself, so they are exactly what the Python
daemon writes. Every value is synthetic: example pane ids (w9:pN), example labels and text,
and fixed timestamps, so the files are deterministic. The stuck config is pinned, because the
default one reads the environment. No herdr, terminal or live config is touched; each file is
written from a fresh temp dir that is removed afterwards.
"""
import os
import shutil
import sys
import tempfile

REPO, OUT_DIR = sys.argv[1], sys.argv[2]
sys.path.insert(0, REPO)
import dunk_core  # noqa: E402
import dunk_stuck  # noqa: E402


class NoHerdr:
    """_save() never touches the terminal backend; a stub keeps the fixtures hermetic."""


def make_message(id, pane_id, label, text, source, created_at, status="queued",
                 attempt_generation=None):
    message = dunk_core.Message(id, pane_id, label, text, source, lambda: (True, ""))
    message.created_at = created_at
    message.status = status
    message.attempt_generation = attempt_generation
    return message


def populate_outbox(flock):
    first = make_message("m5", "w9:p1", "example-lane", "please pick up the example task",
                         "send_text", 1793000500.5)
    notice = make_message("m6", "w9:p1", "example-lane", "2 new inbox messages",
                          "send_text", 1793000510.0)
    notice.is_notice = True
    notice.notify_for = ["i14", "i15"]
    notice.holds = 2
    notice.next_attempt_at = 1793000600.25
    notice.hold_signature = ("box", "> draft left in the box")
    other = make_message("m7", "w9:p2", "example-two", "status? — reply briefly", "d2",
                         1793000520.75)
    other.hold_while_typing = False
    other.holds = 1
    other.hold_signature = ("modal", "Save changes? [y/N]")
    flock.outbox = {"w9:p1": [first, notice], "w9:p2": [other]}
    flock.stuck_config = dunk_stuck.Config()
    flock.next_id = 3
    flock.next_message_id = 9
    flock.decided = {"m4": {"status": "delivered", "created_at": 1793000400.25}}


def populate_stuck(flock):
    # The daemon keeps the order it first saw each pane, so these are NOT sorted by pane id.
    failed = make_message("m7", "w9:p2", "example-two", "status? — reply briefly", "d2",
                          1793000520.75, status="failed", attempt_generation="gen-7f3a")
    failed.typed_at = 1793000521.5
    sent = make_message("m3", "w9:p1", "example-lane", "please pick up the example task",
                        "send_text", 1793000500.5, status="sent", attempt_generation="gen-2b9c")
    sent.scheduled = True
    flock._note_stuck_candidate(failed)
    flock._note_stuck_candidate(sent)
    flock._reconcile_stuck("w9:p1", "m3", "exhausted",
                           "2 recovery attempt(s) did not clear the box", created_at=1793000500.5)
    # _reconcile_stuck stamps time.time(); pin it so the fixture is reproducible.
    flock.stuck_candidates["w9:p1"]["recovered_at"] = 1793000580.125
    flock.stuck = dunk_stuck.Tracker(
        records={
            "w9:p2": {"message_id": "m7", "generation": "gen-7f3a", "fingerprint": "fp-41",
                      "first_seen": 1793000522.0, "last_attempt_at": 1793000560.5,
                      "attempts": 1, "terminal": None},
            "w9:p1": {"message_id": "m3", "generation": "gen-2b9c", "fingerprint": "fp-07",
                      "first_seen": 1793000501.0, "last_attempt_at": 1793000540.0,
                      "attempts": 2,
                      "terminal": "2 recovery attempt(s) did not clear the box"},
        },
        spent={
            dunk_stuck.Tracker.ledger_key("w9:p2", "m7", "gen-7f3a", "fp-41"): 1,
            dunk_stuck.Tracker.ledger_key("w9:p1", "m3", "gen-2b9c", "fp-07"): 2,
        },
    )
    flock.stuck_config = dunk_stuck.Config.from_mapping({
        "enabled": True, "threshold_s": 90, "max_attempts": 2, "cooldown_s": 45,
        "observation_interval_s": 15})
    flock.next_id = 8
    flock.next_message_id = 9


def write_state(populate, out_path):
    tmp = tempfile.mkdtemp(prefix="populated-fixture-")
    try:
        state = os.path.join(tmp, "dunks.json")
        flock = dunk_core.Flock(herdr=NoHerdr(), state_file=state,
                                launches_file=os.path.join(tmp, "launches.json"), restore=False)
        try:
            populate(flock)
            assert flock._save() is True
        finally:
            flock.close()
        with open(state, "rb") as handle:
            data = handle.read()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "wb") as handle:
        handle.write(data)
    print("wrote", out_path, len(data), "bytes")


write_state(populate_outbox, os.path.join(OUT_DIR, "python-written-populated-outbox.json"))
write_state(populate_stuck, os.path.join(OUT_DIR, "python-written-populated-stuck.json"))
