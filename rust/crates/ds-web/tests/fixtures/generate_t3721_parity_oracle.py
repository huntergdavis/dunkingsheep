"""Capture the Python desk's viewport, password, ops-board answer and ops-metrics refresh behaviour
as the t3721 parity oracle (t1702 leaf).

Run with the SERVED desk interpreter (releases/web-9c8f919-client499-fd1d498/.venv: FastAPI
0.162.0, Starlette 1.10.0, Pydantic 2.14.2, Python 3.14.4) from the repository root, with
scratch-only data and config dirs. httpx2 and httpcore2 are not in that venv, so put a scratch
directory holding only httpx2, httpcore2 and truststore (installed with --no-deps) on PYTHONPATH:

    VP_DATA_DIR=<scratch-data> VP_PASSWORD=<scratch-initial> T3721_NEW_PASSWORD=<scratch-new> \
    DUNKINGSHEEP_DIR=<scratch-config> PYTHONPATH=<scratch-pylib> \
        <served-python> rust/crates/ds-web/tests/fixtures/generate_t3721_parity_oracle.py

Safety: the generator refuses to run unless DUNKINGSHEEP_DIR and VP_DATA_DIR are scratch paths
and the daemon socket path that the desk's client would use is inside DUNKINGSHEEP_DIR. No live
owner database, token, config or daemon socket is read or written.

Secrets: the initial and new passwords are scratch-only literals supplied by the environment. The
initial password never appears in the oracle. The new password does: the pinned FastAPI 422 bodies
echo the submitted body, so `password_missing_current` records the new scratch literal in its
`input`. That echo is the pinned Python behaviour, recorded as the server sent it. The Rust port does
not echo it: owner decision i50225 redacts submitted passwords from validation errors, and the Rust
test records each redacted case as an intentional difference. The owner-credentials
file and the API token are recorded only as presence booleans. Responses are recorded as the server
sent them, except where a value is time-based or random (the session viewport timestamp, the pbkdf2
salt): those are recorded as presence only.

Daemon isolation (t3722, reviewer L2): with the scratch config the scratch socket path equals the
default socket path, so the daemon client would ask systemd about the live unit
(`systemctl --user is-active dunkingsheep.service`) and, for an inactive unit, start it. Both are
replaced in-process by `isolate_daemon_client()`. The no-daemon answers then come from the client's
own code, not from the host's service state, and no PATH or HOME trick is needed.
"""

import importlib.metadata
import json
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[5]  # fixtures -> tests -> ds-web -> crates -> rust -> repo
OUT = pathlib.Path(__file__).resolve().parent / "t3721_parity_oracle.json"
SCRATCH_PREFIX = "/home/hunter/workspace/.dunk-port-work/"

PASSWORD = os.environ["VP_PASSWORD"]
NEW_PASSWORD = os.environ["T3721_NEW_PASSWORD"]
CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR", "")
DATA_DIR = os.environ.get("VP_DATA_DIR", "")
if not (CONFIG_DIR.startswith(SCRATCH_PREFIX) and DATA_DIR.startswith(SCRATCH_PREFIX)):
    raise SystemExit("refusing: DUNKINGSHEEP_DIR and VP_DATA_DIR must be scratch paths")

sys.path.insert(0, str(ROOT))
from fastapi.testclient import TestClient  # noqa: E402

import web.app as desk  # noqa: E402  (VP_DATA_DIR and DUNKINGSHEEP_DIR must already point at scratch)
import dunk_client  # noqa: E402


def isolate_daemon_client():
    """Keep the desk's daemon client off the live systemd unit (t3722, reviewer L2).

    `_service_owns_socket` queries `systemctl --user is-active` for the production unit, and
    `start_daemon` runs `systemctl --user start` when the probe is absent. Both are the live
    service, not the scratch config, so both are replaced: the query raises, which the client
    reads as "not owned", and the autostart raises. The no-daemon result is then the client's own.
    """

    def systemctl_disabled(*args, **kwargs):
        raise OSError("scratch oracle: systemd is not queried")

    def autostart_disabled(self):
        raise RuntimeError("scratch oracle: daemon autostart disabled")

    dunk_client._systemctl = systemctl_disabled
    dunk_client.FlockClient.start_daemon = autostart_disabled


isolate_daemon_client()
if not str(dunk_client.SOCKET_PATH).startswith(CONFIG_DIR):
    raise SystemExit(f"refusing: the daemon client would use {dunk_client.SOCKET_PATH}")

VIEWPORT = {"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920,
            "screenHeight": 1080, "devicePixelRatio": 2.0}
cases = []


def record(name, response, **extra):
    try:
        body = response.json()
    except ValueError:
        body = {"text": response.text}
    cases.append({"name": name, "status": response.status_code,
                  "content_type": response.headers.get("content-type"), "body": body, **extra})


def anon():
    return TestClient(desk.app, raise_server_exceptions=False)


def login(password):
    client = anon()
    response = client.post("/api/login", json={"password": password, "next": "/dash/cos"})
    return client, response


# --- owner session used by the viewport cases --------------------------------------------------
owner, login_response = login(PASSWORD)
record("login_owner", login_response)
owner_cookie = owner.cookies.get("vp_session")

# Viewport cases, in order. The side-effect probe is /api/channels, which reports the session's
# viewport dimensions (viewport_at is a timestamp, so only its presence is recorded).
viewport = anon()
record("viewport_ok", viewport.post("/api/viewport", json=VIEWPORT, cookies={"vp_session": owner_cookie}))
probe = owner.get("/api/channels")
rows = probe.json().get("viewports", [])
stored = [{k: v for k, v in row.items() if k != "viewport_at"} for row in rows]
cases.append({"name": "viewport_ok_probe_channels", "status": probe.status_code,
              "stored": stored, "viewport_at_present": [("viewport_at" in row) for row in rows]})
record("viewport_same_origin", viewport.post("/api/viewport", json=VIEWPORT,
                                              headers={"origin": "http://testserver"},
                                              cookies={"vp_session": owner_cookie}))
record("viewport_cross_origin", viewport.post("/api/viewport", json=VIEWPORT,
                                               headers={"origin": "https://evil.example"},
                                               cookies={"vp_session": owner_cookie}))
record("viewport_no_cookie", viewport.post("/api/viewport", json=VIEWPORT))
record("viewport_bogus_cookie", viewport.post("/api/viewport", json=VIEWPORT,
                                              cookies={"vp_session": "bogus-session"}))
for name, payload in [
    ("viewport_extra_key", {**VIEWPORT, "extra": 1}),
    ("viewport_missing_key", {k: v for k, v in VIEWPORT.items() if k != "screenHeight"}),
    ("viewport_string_int", {**VIEWPORT, "innerWidth": "1280"}),
    ("viewport_bool_int", {**VIEWPORT, "innerWidth": True}),
    ("viewport_float_int", {**VIEWPORT, "innerWidth": 1280.5}),
    ("viewport_zero_width", {**VIEWPORT, "innerWidth": 0}),
    ("viewport_width_over_max", {**VIEWPORT, "innerWidth": 32769}),
    ("viewport_dpr_zero", {**VIEWPORT, "devicePixelRatio": 0}),
    ("viewport_dpr_over_max", {**VIEWPORT, "devicePixelRatio": 17}),
    ("viewport_dpr_int", {**VIEWPORT, "devicePixelRatio": 2}),
]:
    record(name, viewport.post("/api/viewport", json=payload, cookies={"vp_session": owner_cookie}))
record("viewport_nan_dpr", viewport.post(
    "/api/viewport", content=b'{"innerWidth":1,"innerHeight":1,"screenWidth":1,"screenHeight":1,"devicePixelRatio":NaN}',
    headers={"content-type": "application/json"}, cookies={"vp_session": owner_cookie}))
record("viewport_not_json", viewport.post(
    "/api/viewport", content=b"not json", headers={"content-type": "application/json"},
    cookies={"vp_session": owner_cookie}))

# --- password cases ----------------------------------------------------------------------------
record("password_no_cookie", anon().post("/api/password", json={"current_password": PASSWORD, "new_password": NEW_PASSWORD}))
record("password_wrong_current", owner.post("/api/password", json={"current_password": "not-the-password", "new_password": NEW_PASSWORD}))
record("password_short_new", owner.post("/api/password", json={"current_password": PASSWORD, "new_password": "short"}))
record("password_missing_current", owner.post("/api/password", json={"new_password": NEW_PASSWORD}))
# Ordering and shape cases (t3722). They change no state: the body is refused before the handler.
record("password_no_cookie_invalid_body", anon().post("/api/password", json={"new_password": "short"}))
record("password_wrong_current_short_new", owner.post("/api/password", json={"current_password": "not-the-password", "new_password": "short"}))
record("password_missing_both", owner.post("/api/password", json={}))
record("password_current_not_string", owner.post("/api/password", json={"current_password": 12345678901234, "new_password": NEW_PASSWORD}))
record("password_new_too_long", owner.post("/api/password", json={"current_password": PASSWORD, "new_password": "x" * 201}))
record("password_not_json", owner.post("/api/password", content=b"not json", headers={"content-type": "application/json"}))
record("password_list_body", owner.post("/api/password", json=[1, 2]))
# Empty body: FastAPI sees no body at all (a 'missing' error with input null, not a decode error).
record("password_empty_body", owner.post("/api/password", content=b"", headers={"content-type": "application/json"}))
# NaN in a JSON body: json.loads accepts it, and the pinned Python desk answers 500 (its 422 cannot
# serialise the NaN input). Recorded as the server answered it; a decision is pending (t3722).
record("password_nan_body", owner.post("/api/password", content=b'{"current_password": NaN, "new_password": "x"}', headers={"content-type": "application/json"}))

api_token = desk._auth_record()["token"]  # scratch token, read in-process, never written to the oracle
record("password_api_identity_invalid_body", anon().post("/api/password", json={"current_password": PASSWORD, "new_password": "short"},
                                                         headers={"authorization": f"Bearer {api_token}"}))
record("password_api_identity",anon().post("/api/password", json={"current_password": PASSWORD, "new_password": NEW_PASSWORD},
                                            headers={"authorization": f"Bearer {api_token}"}))
del api_token

# A second owner session, to observe the session sweep the change performs.
other, other_login = login(PASSWORD)
record("login_second_session", other_login)
other_cookie = other.cookies.get("vp_session")

# The desk rewrites the owner-credentials file only when it exists, so create it as a desk would.
desk.CREDENTIALS.write_text(f"Password: {PASSWORD}\nAPI token: {desk._auth_record()['token']}\n", encoding="utf-8")

record("password_ok", owner.post("/api/password", json={"current_password": PASSWORD, "new_password": NEW_PASSWORD}))
creds_text = desk.CREDENTIALS.read_text(encoding="utf-8")
cases.append({"name": "password_ok_credentials_file",
              "first_line_is_new_password": creds_text.splitlines()[0] == f"Password: {NEW_PASSWORD}",
              "has_api_token_line": any(line.startswith("API token: ") for line in creds_text.splitlines())})
# Status only: the body carries session timestamps that differ per run (the viewport_at column).
current_probe = owner.get("/api/channels")
cases.append({"name": "password_ok_probe_current_session", "status": current_probe.status_code})
record("password_ok_probe_other_session", anon().get("/api/channels", cookies={"vp_session": other_cookie}))
record("login_new_password", anon().post("/api/login", json={"password": NEW_PASSWORD, "next": "/dash/cos"}))
record("login_old_password", anon().post("/api/login", json={"password": PASSWORD, "next": "/dash/cos"}))

# --- ops-board answer cases (owner session, after the password change) -------------------------
board = owner
record("answer_no_cookie", anon().post("/api/ops-board/answer", json={"task_id": "t1", "reply": "ok"}))
record("answer_list_payload", board.post("/api/ops-board/answer", json=[1, 2]))
record("answer_empty_payload", board.post("/api/ops-board/answer", json={}))
record("answer_blank_task", board.post("/api/ops-board/answer", json={"task_id": "   ", "reply": "ok"}))
record("answer_missing_reply", board.post("/api/ops-board/answer", json={"task_id": "t1"}))
record("answer_blank_reply", board.post("/api/ops-board/answer", json={"task_id": "t1", "reply": "   "}))
record("answer_no_daemon", board.post("/api/ops-board/answer", json={"task_id": "t1", "reply": "ok"}),
       note="deterministic: the daemon client is isolated (no systemd query, no autostart)")

# --- ops-metrics refresh (auth gate only; the owner path calls gh and reads host state) ---------
record("metrics_refresh_no_cookie", anon().post("/api/ops-metrics/refresh"))

versions = {package: importlib.metadata.version(package) for package in ("fastapi", "starlette", "pydantic")}
payload = {
    "oracle": {"interpreter": sys.version.split()[0], **versions},
    "scratch": {"config_dir": "scratch", "data_dir": "scratch"},
    "cases": cases,
}
OUT.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
print(f"wrote {OUT} with {len(cases)} cases")
