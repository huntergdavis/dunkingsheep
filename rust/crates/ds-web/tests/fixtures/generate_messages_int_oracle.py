"""Capture the Python desk's POST /api/brief and send-now behaviour as the integer oracle (t1702).

Run with the SERVED desk interpreter (releases/web-9c8f919-client499-fd1d498/.venv: FastAPI
0.162.0, Starlette 1.10.0, Pydantic 2.14.2, Python 3.14.4) from the repository root, with a
fresh scratch data dir so no live desk state is touched. httpx2 and httpcore2 are not in that
venv: put a scratch directory holding only httpx2, httpcore2 and truststore (installed with
--no-deps) on PYTHONPATH.

Each case is a real request through the desk's app against a real on-disk messages.sqlite3.
Before and after each request the message, attachment and answer state is read back, so the
rollback behaviour is recorded from the database, not inferred. Exceptions are recorded as
the server answered them (500), not raised into the generator.
"""

import base64
import importlib.metadata
import json
import os
import pathlib
import sqlite3
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[5]
OUT = pathlib.Path(__file__).resolve().parent / "messages_int_oracle.json"
HUGE = 2**100

sys.path.insert(0, str(ROOT))
from fastapi.testclient import TestClient  # noqa: E402

import web.app as desk  # noqa: E402  (VP_DATA_DIR must already point at scratch)

# Answer inserts are scoped to the owner's question, so seed one owner question first.
with sqlite3.connect(desk.DB) as connection:
    connection.execute(
        "INSERT INTO messages(channel, role, body) VALUES('cos', 'owner', 'question?')")
    question_id = connection.execute("SELECT max(id) FROM messages").fetchone()[0]

client = TestClient(desk.app, raise_server_exceptions=False)


def as_identity(identity):
    desk.app.dependency_overrides[desk.require_auth] = lambda: identity


def db_state():
    with sqlite3.connect(desk.DB) as connection:
        messages = connection.execute("SELECT count(*) FROM messages").fetchone()[0]
        attachments = connection.execute("SELECT count(*) FROM attachments").fetchone()[0]
        answered = connection.execute(
            "SELECT count(*) FROM messages WHERE answer_id IS NOT NULL").fetchone()[0]
        newest = connection.execute("SELECT max(id) FROM messages").fetchone()[0]
    return {"messages": messages, "attachments": attachments, "answered": answered,
            "newest_id": newest}


def shot(response):
    try:
        body = response.json()
    except ValueError:
        body = {"text": response.text}
    return {"status": response.status_code, "body": body}


def run(name, identity, method, url, payload=None, attach=None):
    as_identity(identity)
    before = db_state()
    if method == "POST":
        response = client.post(url, json=payload)
    else:
        response = client.request(method, url)
    after = db_state()
    return {"name": name, "identity": identity, "method": method, "url": url,
            "payload": payload, **shot(response), "db_before": before, "db_after": after}


cases = [
    run("brief_base", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "reply", "answers": []}),
    run("brief_answers_owner_question", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "answer", "answers": [question_id]}),
    run("brief_answer_too_large", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "huge", "answers": [HUGE]}),
    run("brief_answer_overflow_64bit", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "huge64", "answers": [2**63]}),
    run("brief_answer_zero", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "zero", "answers": [0]}),
    run("brief_answer_bool", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "bool", "answers": [True]}),
    run("brief_answer_float", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "float", "answers": [1.0]}),
    run("brief_empty_body", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "", "answers": []}),
    run("brief_unknown_channel", "api", "POST", "/api/brief",
        {"channel": "nope", "body": "x", "answers": []}),
    run("brief_missing_attachment", "api", "POST", "/api/brief",
        {"channel": "cos", "body": "x", "answers": [], "attachment_ids": ["missing"]}),
    run("brief_owner_identity_refused", "owner", "POST", "/api/brief",
        {"channel": "cos", "body": "x", "answers": []}),
    run("send_now_owner_not_found", "owner", "POST", "/api/messages/999/send-now"),
    run("send_now_api_refused", "api", "POST", "/api/messages/999/send-now"),
    run("send_now_huge_id", "owner", "POST", f"/api/messages/{HUGE}/send-now"),
]

versions = {package: importlib.metadata.version(package)
            for package in ("fastapi", "starlette", "pydantic")}
revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True,
                          text=True).stdout.strip()
payload = {
    "oracle": {"interpreter": sys.version.split()[0], "revision": revision, **versions},
    "seed": {"owner_question_id": question_id, "huge": str(HUGE)},
    "cases": cases,
}
OUT.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
print(f"wrote {OUT} with {len(cases)} cases")
