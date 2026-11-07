"""Capture the Python desk's GET /api/uploads/{id} behaviour as the parity oracle (t1702).

Run with the oracle interpreter (FastAPI and Starlette as served on this host), from the
repository root, with a scratch data dir so no live desk state is touched:

    VP_DATA_DIR=<scratch> PYTHONPATH=<scratch-pylib-with-httpx2> \
        <oracle-python> rust/crates/ds-web/tests/fixtures/generate_uploads_range_oracle.py

It writes uploads_range_oracle.json beside this file. The multipart boundary is random per
response, so it is replaced by the literal BOUNDARY in both the header and the body before
it is recorded; everything else is recorded as the server sent it.
"""

import base64
import importlib.metadata
import json
import os
import pathlib
import sqlite3
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[5]  # fixtures -> tests -> ds-web -> crates -> rust -> repo
OUT = pathlib.Path(__file__).resolve().parent / "uploads_range_oracle.json"
CONTENT = b"0123456789ABCDEFGHIJ"  # 20 bytes: the Range arithmetic below is hand-checkable
FILENAME = "notes.txt"
MIME = "text/plain"
MTIME = 1_700_000_000  # fixed, so Last-Modified and the ETag are stable across runs

sys.path.insert(0, str(ROOT))
from fastapi.testclient import TestClient  # noqa: E402

import web.app as desk  # noqa: E402  (VP_DATA_DIR must already point at scratch)

desk.app.dependency_overrides[desk.require_auth] = lambda: "owner"
client = TestClient(desk.app)

upload_dir = desk.UPLOADS
upload_dir.mkdir(parents=True, exist_ok=True)
attachment_id = "oracle0000000000000000000000000001"
path = upload_dir / f"{attachment_id}_{FILENAME}"
path.write_bytes(CONTENT)
os.utime(path, (MTIME, MTIME))
with sqlite3.connect(desk.DB) as connection:
    connection.execute(
        "INSERT OR REPLACE INTO attachments(id,filename,mime,size,path) VALUES(?,?,?,?,?)",
        (attachment_id, FILENAME, MIME, len(CONTENT), str(path)),
    )

URL = f"/api/uploads/{attachment_id}"
KEPT = ("content-type", "content-length", "content-range", "accept-ranges", "etag",
        "last-modified", "content-disposition", "x-content-type-options", "allow", "cache-control")


def normalise(response):
    """Status, the headers the parity test compares, and the body (boundary made literal)."""
    headers = {}
    for name in KEPT:
        value = response.headers.get(name)
        if value is not None:
            headers[name] = value
    body = response.content
    boundary = None
    if "multipart/byteranges" in headers.get("content-type", ""):
        boundary = headers["content-type"].split("boundary=", 1)[1]
        headers["content-type"] = headers["content-type"].replace(boundary, "BOUNDARY")
        body = body.replace(boundary.encode(), b"BOUNDARY")
    return {
        "status": response.status_code,
        "headers": headers,
        "body_base64": base64.b64encode(body).decode(),
        "multipart": boundary is not None,
    }


CASES = [
    ("base", "GET", {}),
    ("single_range", "GET", {"Range": "bytes=2-5"}),
    ("suffix_range", "GET", {"Range": "bytes=-4"}),
    ("open_range", "GET", {"Range": "bytes=17-"}),
    ("malformed_unit", "GET", {"Range": "items=0-3"}),
    ("malformed_garbage", "GET", {"Range": "bytes=abc"}),
    ("unsatisfiable", "GET", {"Range": "bytes=100-200"}),
    ("if_range_etag_matches", "GET", {"Range": "bytes=2-5", "If-Range": "__ETAG__"}),
    ("if_range_stale", "GET", {"Range": "bytes=2-5", "If-Range": "\"stale-validator\""}),
    ("overlapping_merged", "GET", {"Range": "bytes=0-3,2-6"}),
    ("disjoint_multipart", "GET", {"Range": "bytes=0-1,5-6"}),
    ("head_not_allowed", "HEAD", {}),
]

base_response = client.get(URL)
etag = base_response.headers.get("etag")
cases = []
for name, method, headers in CASES:
    resolved = {key: (etag if value == "__ETAG__" else value) for key, value in headers.items()}
    response = client.request(method, URL, headers=resolved)
    cases.append({"name": name, "method": method, "request_headers": resolved,
                  **normalise(response)})

versions = {package: importlib.metadata.version(package)
            for package in ("fastapi", "starlette", "pydantic")}
revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True,
                          text=True).stdout.strip()
payload = {
    "oracle": {"interpreter": sys.version.split()[0], "revision": revision, **versions},
    "file": {"id": attachment_id, "filename": FILENAME, "mime": MIME, "size": len(CONTENT),
             "mtime": MTIME, "content_base64": base64.b64encode(CONTENT).decode()},
    "cases": cases,
}
OUT.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
print(f"wrote {OUT} with {len(cases)} cases")
