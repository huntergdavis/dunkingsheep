"""Dead-man check for the exception watchdog (quis custodiet).

Deliberately independent of `dunk_watchdog`: it does not import it, read its
config, or evaluate any rule, so it still alerts when the watchdog is
disabled, misconfigured, crashing, or never ran. Its only input is the
heartbeat file the watchdog touches after every successful LIVE run, and its
parameters come from its own command line (set in its own systemd unit).

If now - mtime(heartbeat) > multiplier * cadence (or the file is missing),
it sends one `watchdog.deadman` alert to the VP inbox, at most once per
cooldown, tracked in its own small state file. No model is involved.
"""

import json
import os
import time

RULE_ID = "watchdog.deadman"
SYSTEM_SENDER = "watchdog"


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


def check(heartbeat, cadence_minutes, multiplier, to, state_path, cooldown_minutes, now,
          send=None, dry_run=False):
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
    body = (f"[critical] {RULE_ID}: the watchdog heartbeat {heartbeat} {what}. Fleet exceptions are "
            "NOT being watched until it runs again. Check `systemctl --user status "
            "dunkingsheep-watchdog.timer` and `dunkingsheep watchdog --dry-run`.")
    if dry_run:
        report["reason"] = "would alert (dry run)"
        return report
    if send is None:
        from dunk_client import FlockClient

        def send(target, subject, text):
            FlockClient(self_pane_id=None).call("inbox_send", to=target, subject=subject, body=text,
                                               system_sender=SYSTEM_SENDER)
    send(to, f"{RULE_ID}: watchdog heartbeat stale", body)
    state["last_alerted"] = now
    _save(state_path, state)
    report["alerted"] = True
    return report


def main(args):
    report = check(os.path.expanduser(args.heartbeat), args.cadence_minutes, args.multiplier, args.to,
                   os.path.expanduser(args.state), args.cooldown_minutes,
                   args.now if args.now is not None else time.time(), dry_run=args.dry_run)
    if args.json:
        print(json.dumps(report, sort_keys=True))
    else:
        status = "STALE" if report["stale"] else "fresh"
        print(f"{RULE_ID} {report['mode']}: heartbeat {status} age={report['age_minutes']} min "
              f"limit={report['limit_minutes']} min alerted={report['alerted']}"
              + (f" ({report['reason']})" if report["reason"] else ""))
    return 0
