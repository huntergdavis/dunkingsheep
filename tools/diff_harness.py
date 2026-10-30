#!/usr/bin/env python3
"""Differential harness for the Rust port: record, replay, golden (RUST_PORT 7).

The port is behaviour-for-behaviour, so the judge is not an opinion: it is the
same requests sent to the Python daemon and to the Rust one, with the responses
diffed. This file stays Python; it is the harness every port task runs.

    tools/diff_harness.py record --target ~/.config/dunkingsheep/dunkingsheep.sock \
        --listen /tmp/ds-proxy.sock --out rust/fixtures/traffic.jsonl --seconds 600
    tools/diff_harness.py replay --traffic rust/fixtures/traffic.jsonl \
        --against /tmp/ds-rs.sock --commands task_list,status
    tools/diff_harness.py golden --traffic rust/fixtures/traffic.jsonl \
        --state rust/fixtures/state

Wire protocol (dunk_server): one newline-delimited JSON request per connection,
one newline-delimited JSON reply.

Three rules this file keeps, because each is a defect this fleet has recorded:

1. **Scrub, never store a credential.** `service_token` is removed from recorded
   requests and `token`/`secret`/`password` values are removed from recorded
   responses (a fixture is committed to a PUBLIC repo: review t725 on #146 swept
   real-looking credentials into one). A scrubbed request records
   `"service_token_scrubbed": true`; replay re-injects a token only from a file
   the operator names (`--service-token-file`), never from the fixture.
2. **Mask what the daemon allocates, not what it decides.** Timestamps and
   freshly allocated ids differ between two runs of the SAME code; masked by
   `rust/fixtures/volatile.json`. Everything else must match byte for byte, and
   a diff is printed as a unified diff so it can be read, not summarised.
3. **Say when it could not measure.** A missing socket, a dead daemon or an
   unreadable fixture is reported as an error and a non-zero exit, never as
   "0 diffs".
"""
import argparse
import difflib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FIXTURES = os.path.join(ROOT, "rust", "fixtures")
DEFAULT_TRAFFIC = os.path.join(FIXTURES, "traffic.jsonl")
DEFAULT_VOLATILE = os.path.join(FIXTURES, "volatile.json")
DEFAULT_STATE = os.path.join(FIXTURES, "state")

SECRET_KEYS = frozenset({
    "service_token", "token", "tokens", "password", "secret", "api_token",
    "gh_token", "bearer",
})
SCRUB = "<scrubbed>"
MASK = "<volatile>"

# What a single connection may take. The daemon's own client default is 15 s; a
# replayed long command (poolrad, review) needs the 600 s the CLI uses, so the
# harness gives every request that ceiling and reports the latency it measured.
REQUEST_TIMEOUT_S = 600.0


# -- wire protocol -----------------------------------------------------------

def call(socket_path, request, timeout=REQUEST_TIMEOUT_S):
    """Send one request, return (response dict or None, latency_s)."""
    payload = (json.dumps(request, sort_keys=True) + "\n").encode("utf-8")
    started = time.time()
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(timeout)
    try:
        try:
            sock.connect(socket_path)
        except OSError:
            # No daemon on that path is a MEASUREMENT FAILURE, not a difference:
            # the caller reports it as an error line, never as "0 diffs".
            return None, time.time() - started
        sock.sendall(payload)
        reader = sock.makefile("rb")
        raw = reader.readline()
    finally:
        sock.close()
    latency = time.time() - started
    if not raw:
        return None, latency
    try:
        return json.loads(raw.decode("utf-8", "replace")), latency
    except ValueError:
        return {"ok": False, "error": "unparseable reply: "
                + raw[:200].decode("utf-8", "replace")}, latency


# -- scrubbing and masking ---------------------------------------------------

def scrub(value, key=None):
    """Remove credential-shaped values on the way INTO the fixture."""
    if isinstance(value, dict):
        out = {}
        for name, item in value.items():
            if str(name).lower() in SECRET_KEYS and item not in (None, "", [], {}):
                out[name] = SCRUB
            else:
                out[name] = scrub(item, str(name))
        return out
    if isinstance(value, list):
        return [scrub(item) for item in value]
    return value


def load_volatile(path=None):
    """The masking rules: key names, id patterns, and absolute-time bounds."""
    data = {}
    path = path or DEFAULT_VOLATILE
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        data = {}
    return {
        "keys": {str(key) for key in (data.get("keys") or [])},
        "patterns": {str(pat) for pat in (data.get("patterns") or [])},
        "epoch_keys": {str(key) for key in (data.get("epoch_keys") or [])},
    }


ISO_TS = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")
EPOCH_MIN = 1_000_000_000.0          # 2001-09-09: anything above is a clock


def mask(value, rules, key=None):
    """Replace the values two runs cannot agree on, and nothing else."""
    if isinstance(value, dict):
        return {name: (MASK if str(name) in rules["keys"]
                       else mask(item, rules, str(name)))
                for name, item in value.items()}
    if isinstance(value, list):
        return [mask(item, rules, key) for item in value]
    if isinstance(value, str):
        if key in rules["keys"]:
            return MASK
        if ISO_TS.match(value):
            return MASK
        for pattern in rules["patterns"]:
            if re.fullmatch(pattern, value):
                return MASK
        return value
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if key in rules["keys"] or key in rules["epoch_keys"]:
            return MASK
        if isinstance(value, float) and value >= EPOCH_MIN:
            return MASK
        return value
    return value


def canonical(value):
    """A stable text form for diffing: sorted keys, no float noise."""
    return json.dumps(value, indent=2, sort_keys=True, default=str)


def unified(expected, actual, label):
    return "\n".join(difflib.unified_diff(
        canonical(expected).splitlines(), canonical(actual).splitlines(),
        fromfile=f"recorded {label}", tofile=f"replayed {label}", lineterm=""))


# -- record ------------------------------------------------------------------

def _read_line(conn):
    reader = conn.makefile("rb")
    return reader.readline()


def record_connection(conn, target, out_path, lock, counter, wanted=None):
    """Forward one client connection and append what crossed it."""
    try:
        raw = _read_line(conn)
        if not raw:
            return
        try:
            request = json.loads(raw.decode("utf-8", "replace"))
        except ValueError:
            conn.sendall(b'{"ok": false, "error": "bad request json"}\n')
            return
        forwarded = dict(request)
        scrubbed_token = False
        if forwarded.get("service_token"):
            forwarded.pop("service_token", None)
            scrubbed_token = True
        response, latency = call(target, forwarded)
        reply = response if response is not None else {
            "ok": False, "error": "daemon closed the connection without replying"}
        entry = {
            "ts": round(time.time(), 3),
            "cmd": forwarded.get("cmd"),
            "args": scrub(forwarded.get("args") or {}),
            "self_pane_id": forwarded.get("self_pane_id"),
            "latency_ms": round(latency * 1000.0, 1),
            "response": scrub(reply),
        }
        if scrubbed_token:
            entry["service_token_scrubbed"] = True
        if wanted and forwarded.get("cmd") not in wanted:
            # Recorded, but not KEPT: an operator restricts what lands in a fixture
            # that may be committed. Host-derived commands (list_panes, lanes) answer
            # from the machine, not from the state copy, so they are never fixture
            # material (2026-10-30: a first recording carried real pane labels).
            conn.sendall((json.dumps(reply, sort_keys=True) + "\n").encode("utf-8"))
            return
        with lock:
            with open(out_path, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(entry, sort_keys=True) + "\n")
            counter["n"] += 1
        conn.sendall((json.dumps(reply, sort_keys=True) + "\n").encode("utf-8"))
    except Exception as error:      # noqa: BLE001 - a proxy must never die silently
        try:
            conn.sendall((json.dumps({"ok": False,
                                      "error": f"proxy error: {error}"}) + "\n").encode())
        except OSError:
            pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def record(target, listen, out, seconds=None, on_ready=None, commands=None):
    """Proxy `target` on `listen` for `seconds` (None: until interrupted)."""
    if os.path.exists(listen):
        os.unlink(listen)
    os.makedirs(os.path.dirname(os.path.abspath(out)) or ".", exist_ok=True)
    counter = {"n": 0}
    lock = threading.Lock()
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(listen)
    server.listen(64)
    server.settimeout(1.0)
    wanted = {cmd.strip() for cmd in (commands or []) if cmd.strip()} or None
    deadline = None if seconds is None else time.time() + float(seconds)
    if on_ready:
        on_ready(listen)
    try:
        while deadline is None or time.time() < deadline:
            try:
                conn, _addr = server.accept()
            except socket.timeout:
                continue
            thread = threading.Thread(target=record_connection,
                                      args=(conn, target, out, lock, counter, wanted),
                                      daemon=True)
            thread.start()
    except KeyboardInterrupt:
        pass
    finally:
        server.close()
        try:
            os.unlink(listen)
        except OSError:
            pass
    return counter["n"]


# -- replay / golden ---------------------------------------------------------

def load_traffic(path):
    entries = []
    with open(path, encoding="utf-8") as handle:
        for number, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except ValueError as error:
                raise SystemExit(f"{path}:{number}: not JSON ({error})")
            if entry.get("_meta"):
                continue
            if not entry.get("cmd"):
                raise SystemExit(f"{path}:{number}: no cmd")
            entries.append(entry)
    return entries


def replay(entries, against, commands=None, volatile=None, service_token=None,
           verbose=True):
    """Send every recorded request to `against`; return (diffs, ratios, errors)."""
    rules = load_volatile(volatile)
    wanted = {cmd.strip() for cmd in (commands or []) if cmd.strip()}
    diffs, ratios, errors, sent = [], [], [], 0
    for entry in entries:
        if wanted and entry["cmd"] not in wanted:
            continue
        request = {"id": sent + 1, "cmd": entry["cmd"],
                   "args": entry.get("args") or {},
                   "self_pane_id": entry.get("self_pane_id")}
        if service_token:
            request["service_token"] = service_token
        actual, latency = call(against, request)
        sent += 1
        if actual is None:
            errors.append(f"{entry['cmd']}: no reply from {against}")
            continue
        # The top-level "id" is the REQUEST's sequence number echoed back, which is
        # the harness's own bookkeeping, not daemon behaviour: drop it on both sides
        # rather than mask a key named "id" everywhere (that would also hide a task
        # or message id the daemon really did get wrong).
        expected_reply = dict(entry.get("response") or {})
        actual_reply = dict(actual or {})
        expected_reply.pop("id", None)
        actual_reply.pop("id", None)
        expected_masked = mask(expected_reply, rules)
        actual_masked = mask(actual_reply, rules)
        if entry.get("latency_ms") and latency > 0:
            ratios.append(latency / max(entry["latency_ms"] / 1000.0, 1e-6))
        if actual_masked != expected_masked:
            diffs.append((entry["cmd"], unified(expected_masked, actual_masked,
                                               f"{entry['cmd']} @ {entry.get('ts')}")))
    if verbose:
        if ratios:
            ratios_sorted = sorted(ratios)
            print(f"latency: median ratio {ratios_sorted[len(ratios_sorted) // 2]:.2f}x "
                  f"over {len(ratios)} request(s)")
        for cmd, text in diffs:
            print(f"\n--- DIFF {cmd} ---")
            print(text)
    return {"sent": sent, "diffs": diffs, "ratios": ratios, "errors": errors}


# -- golden: a frozen state dir, no live daemon -----------------------------

def start_daemon(state_dir, socket_path, python=None, launcher=None, wait_s=30.0):
    """Start `dunkingsheep serve` against a COPY of `state_dir` and wait for it."""
    python = python or sys.executable
    launcher = launcher or os.path.join(ROOT, "dunkingsheep")
    workdir = tempfile.mkdtemp(prefix="ds-golden-state-")
    copy = os.path.join(workdir, "state")
    shutil.copytree(state_dir, copy)
    env = dict(os.environ)
    env["DUNKINGSHEEP_DIR"] = copy
    env["DUNKINGSHEEP_SOCKET"] = socket_path
    # A golden run must not touch the live fleet, including through its timers.
    env["DUNKINGSHEEP_NO_START_DAEMON"] = "1"
    log_path = os.path.join(workdir, "daemon.log")
    log = open(log_path, "wb")
    proc = subprocess.Popen([python, launcher, "serve", "--no-log-file"],
                            env=env, stdout=log, stderr=subprocess.STDOUT)
    deadline = time.time() + wait_s
    while time.time() < deadline:
        if proc.poll() is not None:
            log.close()
            text = open(log_path, "rb").read().decode("utf-8", "replace")[-800:]
            raise SystemExit(f"golden: the daemon exited ({proc.returncode}):\n{text}")
        if os.path.exists(socket_path):
            try:
                reply, _latency = call(socket_path, {"id": 0, "cmd": "status", "args": {}})
                if reply is not None:
                    return proc, workdir, log
            except OSError:
                pass
        time.sleep(0.1)
    proc.terminate()
    log.close()
    raise SystemExit("golden: the daemon did not answer 'status' in "
                     f"{wait_s}s (see {log_path})")


def stop_daemon(proc, log):
    try:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    finally:
        log.close()


# -- CLI ---------------------------------------------------------------------

def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="action", required=True)

    rec = sub.add_parser("record", help="proxy the live daemon and write scrubbed traffic")
    rec.add_argument("--target", required=True, help="the daemon socket to forward to")
    rec.add_argument("--listen", required=True, help="the socket path to listen on")
    rec.add_argument("--out", default=DEFAULT_TRAFFIC)
    rec.add_argument("--seconds", type=float, default=600.0,
                     help="how long to record (the task's acceptance says 10 min)")
    rec.add_argument("--append", action="store_true")
    rec.add_argument("--commands", default="",
                     help="comma separated: KEEP only these commands (host-derived "
                          "answers belong in a per-host recording, not a fixture)")

    rep = sub.add_parser("replay", help="replay recorded traffic against a daemon")
    rep.add_argument("--traffic", default=DEFAULT_TRAFFIC)
    rep.add_argument("--against", required=True)
    rep.add_argument("--commands", default="", help="comma separated command names")
    rep.add_argument("--volatile", default=None)
    rep.add_argument("--service-token-file", default=None,
                     help="re-inject a token at replay time; never read from the fixture")

    gold = sub.add_parser("golden", help="replay against a frozen state snapshot offline")
    gold.add_argument("--traffic", default=DEFAULT_TRAFFIC)
    gold.add_argument("--state", default=DEFAULT_STATE)
    gold.add_argument("--commands", default="")
    gold.add_argument("--volatile", default=None)
    gold.add_argument("--socket", default=None)

    args = parser.parse_args(argv)

    if args.action == "record":
        if not args.append and os.path.exists(args.out):
            os.unlink(args.out)
        if not os.path.exists(os.path.dirname(os.path.abspath(args.out))):
            os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
        print(f"recording {args.target} -> {args.listen} for {args.seconds:g}s "
              f"into {args.out} (point agents at {args.listen})")
        count = record(args.target, args.listen, args.out, seconds=args.seconds,
                       commands=args.commands.split(",") if args.commands else None)
        print(f"recorded {count} request(s) in {args.out}")
        return 0

    if args.action == "replay":
        token = None
        if args.service_token_file:
            with open(args.service_token_file, encoding="utf-8") as handle:
                token = handle.read().strip()
        entries = load_traffic(args.traffic)
        commands = args.commands.split(",") if args.commands else None
        result = replay(entries, args.against, commands=commands,
                        volatile=args.volatile, service_token=token)
        print(f"replayed {result['sent']} request(s): {len(result['diffs'])} diff(s), "
              f"{len(result['errors'])} error(s)")
        for error in result["errors"]:
            print(f"ERROR {error}", file=sys.stderr)
        return 0 if not result["diffs"] and not result["errors"] else 1

    if args.action == "golden":
        if not os.path.isdir(args.state):
            print(f"golden: no state snapshot at {args.state}", file=sys.stderr)
            return 2
        entries = load_traffic(args.traffic)
        socket_path = args.socket or os.path.join(
            tempfile.mkdtemp(prefix="ds-golden-sock-"), "daemon.sock")
        proc, workdir, log = start_daemon(args.state, socket_path)
        try:
            commands = args.commands.split(",") if args.commands else None
            result = replay(entries, socket_path, commands=commands,
                            volatile=args.volatile)
        finally:
            stop_daemon(proc, log)
        print(f"golden: replayed {result['sent']} request(s) offline: "
              f"{len(result['diffs'])} diff(s), {len(result['errors'])} error(s) "
              f"(state copy {workdir})")
        for error in result["errors"]:
            print(f"ERROR {error}", file=sys.stderr)
        return 0 if not result["diffs"] and not result["errors"] else 1

    return 2


if __name__ == "__main__":
    raise SystemExit(main())
