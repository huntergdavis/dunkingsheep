"""Durable, deterministic notification for delivery-blocking dialogs.

Never sends keys or answers a prompt. Owner delivery uses the web desk directly,
so a blocked coordinator cannot swallow the alert. Failed sinks retry separately.
"""
import hashlib
from contextlib import closing
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import threading
import time

_LOCK = threading.Lock()
RETRY_S = 60


def post_owner(body):
    command = (os.environ.get("DUNKINGSHEEP_VPCTL") or shutil.which("vpctl") or
               str(Path.home() / "workspace/vp-of-agentry/vpctl"))
    subprocess.run([command, "brief", "--channel", "cos"], input=body,
                   text=True, capture_output=True, timeout=15, check=True)


def notify(pane_id, generation, signature, *, owner_sink, vp_sink, path=None, now=None):
    """Notify each sink per pane/process/dialog; retry failures after 60s.

    Successful alerts have a 30-minute cooldown, so a recurring identical dialog
    cannot be silenced forever during a long-lived agent session.

    At-least-once: a crash after delivery but before commit can repeat an alert.
    It cannot silently mark an unsuccessful delivery complete.
    """
    now = time.time() if now is None else now
    if not generation or not signature:
        return []
    path = path or str(Path(os.environ.get("DUNKINGSHEEP_DIR",
                                         str(Path.home() / ".config/dunkingsheep"))) /
                       "dialog-alerts.db")
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    identity = hashlib.sha256(f"{pane_id}\0{generation}\0{signature}".encode()).hexdigest()
    body = (f"Dunking Sheep: a dialog in {pane_id} is blocking queued messages. "
            "Open that terminal to inspect the question. The VP may handle routine "
            "questions within existing authority; approvals needing the owner remain "
            "for the owner. No answer or key has been sent automatically.\n\n"
            f"Visible dialog: {signature[:500]}")
    results = []
    with _LOCK, closing(sqlite3.connect(path, timeout=5)) as db:
        db.execute("CREATE TABLE IF NOT EXISTS alerts "
                   "(identity TEXT, sink TEXT, last_attempt REAL, delivered REAL, "
                   "PRIMARY KEY(identity,sink))")
        for name, sink in (("owner", owner_sink), ("vp", vp_sink)):
            row = db.execute("SELECT last_attempt,delivered FROM alerts WHERE identity=? AND sink=?",
                             (identity, name)).fetchone()
            # Re-alert a still-blocked or reopened identical dialog after 30m.
            # This bounds permanent suppression without trusting redraws as closes.
            if row and ((row[1] is not None and now - row[1] < 1800)
                        or now - row[0] < RETRY_S):
                continue
            db.execute("INSERT INTO alerts VALUES (?,?,?,NULL) ON CONFLICT(identity,sink) "
                       "DO UPDATE SET last_attempt=excluded.last_attempt, delivered=NULL", (identity, name, now))
            db.commit()
            try:
                result = sink(body)
                if isinstance(result, dict) and result.get("outcome") == "undeliverable":
                    raise RuntimeError("notification target is unavailable")
            except Exception as exc:
                results.append((name, False, type(exc).__name__))
            else:
                db.execute("UPDATE alerts SET delivered=? WHERE identity=? AND sink=?",
                           (now, identity, name))
                db.commit()
                results.append((name, True, None))
    return results
