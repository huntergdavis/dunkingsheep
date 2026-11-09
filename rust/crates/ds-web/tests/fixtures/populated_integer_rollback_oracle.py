"""Probe Python's brief rollback with a populated attachment in an isolated namespace.

Use the pinned served interpreter, /app as the source root, /work as the only writable
scratch mount, and /pylib with the pinned TestClient dependencies. Set VP_DATA_DIR to
/work/data. No app lifespan, live daemon, credentials, or file contents are needed.
"""
import importlib.metadata
import json
import os
import sqlite3
import sys
from pathlib import Path

assert not Path('/home/hunter').exists()
assert os.environ['VP_DATA_DIR'] == '/work/data'
sys.path.insert(0, '/app')
from fastapi.testclient import TestClient  # noqa: E402
import web.app as desk  # noqa: E402

with sqlite3.connect(desk.DB) as db:
    db.execute("INSERT INTO messages(channel,role,body,created_at) VALUES('cos','owner','fixture question','2026-01-01T00:00:00Z')")
    question = db.execute('SELECT last_insert_rowid()').fetchone()[0]
    db.execute("INSERT INTO attachments(id,filename,mime,size,path,created_at) VALUES('fixture-attachment','note.txt','text/plain',1,'/scratch/note','2026-01-01T00:00:00Z')")


def snapshot():
    with sqlite3.connect(desk.DB) as db:
        return {table: db.execute('SELECT * FROM ' + table + ' ORDER BY rowid').fetchall()
                for table in ('messages', 'attachments', 'sqlite_sequence')}


desk.app.dependency_overrides[desk.require_auth] = lambda: 'api'
client = TestClient(desk.app, raise_server_exceptions=False)
before = snapshot()
response = client.post('/api/brief', json={
    'channel': 'cos', 'body': 'fixture reply', 'attachment_ids': ['fixture-attachment'],
    'answers': [question, 2**100],
})
after = snapshot()
result = {
    'versions': {p: importlib.metadata.version(p) for p in ('fastapi', 'starlette', 'pydantic')},
    'status': response.status_code, 'body': response.text, 'before': before, 'after': after,
    'unchanged': before == after,
}
Path('/work/result.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps({'versions': result['versions'], 'status': result['status'],
                  'body': result['body'], 'unchanged': result['unchanged'],
                  'attachment_rows': len(before['attachments'])}))
