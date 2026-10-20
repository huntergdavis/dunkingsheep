"""Dead-man check for the exception watchdog (quis custodiet).

Deliberately independent of `dunk_watchdog`: it does not import it, read its
config, or evaluate any rule, so it still alerts when the watchdog is
disabled, misconfigured, crashing, or never ran. Its only input is the
heartbeat file the watchdog touches after every successful LIVE run, and its
parameters come from its own command line (set in its own systemd unit).

If now - mtime(heartbeat) > multiplier * cadence (or the file is missing),
it sends one `watchdog.deadman` alert to the VP inbox, at most once per
cooldown, tracked in its own small state file. No model is involved.

Two more duties (VP i19817 / i19821):
* A watchdog that refuses to start writes its raw config error to a fixed
  error file. The dead-man carries that text in its alert, so the VP gets the
  raw error through a path that does not depend on the broken config.
* Delivery never depends on the daemon it may be reporting (VP i19837):
  1. the daemon (`inbox_send` as system sender `watchdog`);
  2. only if that fails, a DIRECT INSERT into the inbox store
     (~/.config/dunkingsheep/inbox.db, through dunk_inbox.Inbox.send: the same
     columns the daemon reads), verified by reading the row back. The daemon
     shows it when it returns; if it never returns, a human finds it with
     sqlite3. One row per distinct alert: the incident key is recorded, and the
     same incident is never written twice;
  3. only if that fails too, a status file and a journal line, and exit 1, so
     the unit shows in `systemctl --user --failed`. A desk banner does not exist
     yet; the status file is the interim the desk can show.
  The dead-man is its own process, so it never writes the store while the
  watchdog is reading it in the same run.
"""

import json
import os
import sys
import time

RULE_ID = "watchdog.deadman"
SYSTEM_SENDER = "watchdog"
CAUSES = ("daemon_down", "config_bad", "other")


def daemon_path_problem(daemon_path, expected_dir):
    """A misconfigured daemon path raises the same FileNotFoundError as a dead
    daemon, so it is checked BEFORE anything is sent (VP i19851, w8:p8 i19854).
    Returns None when the path is usable, else the reason it is config_bad:
    the path is empty, its parent directory is missing or not a directory, or
    the parent is not the expected one."""
    if not daemon_path or not str(daemon_path).strip():
        return "daemon path is empty"
    parent = os.path.dirname(os.path.abspath(daemon_path))
    if not os.path.isdir(parent):
        return f"daemon path parent {parent} does not exist or is not a directory"
    if not expected_dir or os.path.realpath(parent) != os.path.realpath(expected_dir):
        return f"daemon path parent {parent} is not the expected directory {expected_dir}"
    return None


def classify_daemon_error(error, daemon_path, expected_dir):
    """daemon_down | config_bad | other. Only daemon_down may write directly.

    * ConnectionRefusedError: the path exists and nobody accepts. daemon_down.
    * FileNotFoundError / NotADirectoryError: daemon_down ONLY if the path is
      configured, its parent is the expected existing directory, and the error
      (if it names a file) names the daemon path itself. Otherwise config_bad.
    * Anything else: other. A timeout, an error reply from a live daemon, or
      DaemonUnavailable may mean the alert is already queued. DaemonUnavailable
      is a deliberate exclusion: FlockClient.call raises it only after the
      request was sent (w8:p8 i19841, VP i19851)."""
    if isinstance(error, ConnectionRefusedError):
        return "daemon_down"
    if isinstance(error, (FileNotFoundError, NotADirectoryError)):
        if daemon_path_problem(daemon_path, expected_dir):
            return "config_bad"
        named = getattr(error, "filename", None)
        if named is not None and os.path.abspath(str(named)) != os.path.abspath(daemon_path):
            return "config_bad"
        if isinstance(error, NotADirectoryError):
            return "config_bad"
        return "daemon_down"
    return "other"




def heartbeat_age(path, now):
    """Seconds since the heartbeat, or None if it does not exist."""
    try:
        return now - os.stat(path).st_mtime
    except FileNotFoundError:
        return None


def _load(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (FileNotFoundError, ValueError):
        return {}


def _save(path, data):
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    temp = f"{path}.tmp.{os.getpid()}"
    with open(temp, "w", encoding="utf-8") as handle:
        json.dump(data, handle, sort_keys=True)
    os.replace(temp, path)


def _read_error(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read().strip()[:2000] or None
    except (FileNotFoundError, IsADirectoryError):
        return None


def daemon_sender(daemon_path):
    def send(target, subject, text):
        from dunk_client import FlockClient
        FlockClient(daemon_path, self_pane_id=None).call(
            "inbox_send", to=target, subject=subject, body=text, system_sender=SYSTEM_SENDER)
    return send


def direct_write(inbox_path, target, subject, text):
    """The fallback: one row straight into the inbox store, read back to verify."""
    from dunk_inbox import Inbox
    inbox = Inbox(path=inbox_path, blob_dir=os.path.join(os.path.dirname(inbox_path), "attachments"))
    try:
        message_id = inbox.send(target, text, subject=subject, sender=SYSTEM_SENDER)["message_id"]
        back = inbox.get(message_id)
        if (back.get("sender"), back.get("mailbox"), back.get("body")) != (SYSTEM_SENDER, target, text):
            raise RuntimeError(f"direct write of {message_id} did not read back intact")
        return message_id
    finally:
        inbox.close()


def check(heartbeat, cadence_minutes, multiplier, to, state_path, cooldown_minutes, now,
          send=None, dry_run=False, error_file=None, status_path=None, inbox_path=None,
          daemon_path=None, expected_dir=None):
    """Return a report; send at most one alert. `send(to, subject, body)`."""
    if cadence_minutes <= 0 or multiplier <= 0 or cooldown_minutes < 0:
        raise ValueError("cadence_minutes and multiplier must be > 0, cooldown_minutes >= 0")
    limit = multiplier * cadence_minutes * 60
    age = heartbeat_age(heartbeat, now)
    stale = age is None or age > limit
    report = {"rule": RULE_ID, "heartbeat": heartbeat, "age_minutes": None if age is None else round(age / 60, 1),
              "limit_minutes": round(limit / 60, 1), "stale": stale, "alerted": False,
              "reason": None, "mode": "dry-run" if dry_run else "live"}
    if not stale:
        return report
    state = _load(state_path)
    last = state.get("last_alerted")
    if last is not None and now - last < cooldown_minutes * 60:
        report["reason"] = "cooldown"
        return report
    what = "is missing (the watchdog has never completed a live run)" if age is None else \
        f"is {round(age / 60, 1)} min old (limit {round(limit / 60, 1)} min = {multiplier} x cadence)"
    config_error = _read_error(error_file) if error_file else None
    report["config_error"] = config_error
    body = (f"[critical] {RULE_ID}: the watchdog heartbeat {heartbeat} {what}. Fleet exceptions are "
            "NOT being watched until it runs again. Check `systemctl --user status "
            "dunkingsheep-watchdog.timer` and `dunkingsheep watchdog --dry-run`.")
    if config_error:
        body += f"\nThe watchdog refused to start with this raw config error:\n{config_error}"
    if dry_run:
        report["reason"] = "would alert (dry run)"
        return report
    # The caller (the CLI) resolves daemon_path exactly as dunk_client does; an
    # unresolved one is config_bad below, never a guess.
    send = send or daemon_sender(daemon_path)
    subject = f"{RULE_ID}: watchdog heartbeat stale"
    # One incident = one stale heartbeat (its mtime, or "missing"); a direct row is
    # written at most once per incident.
    incident = "missing" if age is None else f"mtime:{now - age:.0f}"
    def fail(error, attempted, cause):
        report["delivery_error"] = error
        report["cause"] = cause
        if status_path:
            _save(status_path, {"rule": RULE_ID, "at": now, "error": error, "cause": cause, "heartbeat": heartbeat,
                                "target": to, "daemon_path": daemon_path, "attempted": attempted,
                                "config_error": config_error})
        print(f"{RULE_ID}: VP alert NOT delivered [{cause}] ({error}); status written to {status_path}",
              file=sys.stderr)
        return report

    problem = daemon_path_problem(daemon_path, expected_dir)
    if problem:
        # A CONFIG refusal: nothing is sent and nothing is written around a daemon
        # that may be perfectly healthy at the path it really uses.
        return fail(f"CONFIG: {problem}", "none", "config_bad")
    try:
        send(to, subject, body)
        report["delivered_via"] = "daemon"
    except Exception as daemon_error:  # noqa: BLE001 - classified below; never re-raised
        report["daemon_error"] = f"{type(daemon_error).__name__}: {daemon_error}"
        cause = classify_daemon_error(daemon_error, daemon_path, expected_dir)
        if cause == "config_bad":
            return fail(f"CONFIG: daemon path {daemon_path}: {report['daemon_error']}", "daemon", cause)
        if cause != "daemon_down":
            return fail(f"daemon send failed without proof the daemon is down: {report['daemon_error']}",
                        "daemon", cause)
        if state.get("direct_incident") != incident:
            try:
                report["direct_message_id"] = direct_write(inbox_path, to, subject, body)
            except Exception as direct_error:  # noqa: BLE001 - not alerted; the next run retries
                return fail(f"no daemon ({report['daemon_error']}); direct write failed: "
                            f"{type(direct_error).__name__}: {direct_error}", "direct", "daemon_down")
            state["direct_incident"] = incident
        report["delivered_via"] = "direct"
    state["last_alerted"] = now
    _save(state_path, state)
    report["alerted"] = True
    return report


def main(args):
    report = check(os.path.expanduser(args.heartbeat), args.cadence_minutes, args.multiplier, args.to,
                   os.path.expanduser(args.state), args.cooldown_minutes,
                   args.now if args.now is not None else time.time(), dry_run=args.dry_run,
                   error_file=os.path.expanduser(args.error_file), status_path=os.path.expanduser(args.status),
                   inbox_path=os.path.expanduser(args.inbox), daemon_path=args.daemon_path,
                   expected_dir=args.expected_dir)
    if args.json:
        print(json.dumps(report, sort_keys=True))
    else:
        status = "STALE" if report["stale"] else "fresh"
        print(f"{RULE_ID} {report['mode']}: heartbeat {status} age={report['age_minutes']} min "
              f"limit={report['limit_minutes']} min alerted={report['alerted']}"
              + (f" ({report['reason']})" if report["reason"] else ""))
    if report.get("cause") == "config_bad":
        return 2
    return 1 if report.get("delivery_error") else 0
