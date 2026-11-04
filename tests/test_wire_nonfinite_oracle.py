"""Actual Python server/client/heartbeat boundaries, isolated from daemon imports."""
import ast
import json
import logging
from pathlib import Path
import socket
import sqlite3
import tempfile
import threading
import time
import types
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DunkError(Exception):
    pass


class TaskError(ValueError):
    pass


def method(file, cls, name):
    tree = ast.parse((ROOT / file).read_text())
    owner = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == cls)
    return next(node for node in owner.body if isinstance(node, ast.FunctionDef) and node.name == name)


NS = dict(json=json, socket=socket, time=time, sqlite3=sqlite3, DunkError=DunkError,
          TaskError=TaskError, DaemonUnavailable=DunkError, DEFAULT_LEASE_S=1800,
          _now=lambda: 1000.0, log=logging.getLogger("wire-oracle"),
          dispatch=lambda flock, name, args, **kw: args)
SERVER_CLASS = next(node.name for node in ast.parse((ROOT / "dunk_server.py").read_text()).body
                    if isinstance(node, ast.ClassDef) and any(isinstance(n, ast.FunctionDef)
                       and n.name == "handle_line" for n in node.body))
NODES = [method("dunk_server.py", SERVER_CLASS, "handle_line"),
         method("dunk_client.py", "FlockClient", "_call_once"),
         method("dunk_tasks.py", "Tasks", "heartbeat")]
exec(compile(ast.Module(body=NODES, type_ignores=[]), "actual-python-wire-boundaries", "exec"), NS)


class Server:
    handle_line = NS["handle_line"]

    def __init__(self):
        self.effects = []
        self.flock = types.SimpleNamespace(**{
            name: lambda *args, name=name: self.effects.append((name, args))
            for name in ["set_caller", "set_service_token", "set_socket_peer",
                         "clear_caller", "clear_socket_peer"]})

    def _bind_identity(self, pane, caller, name):
        return pane


CASES = ['[NaN, Infinity, -Infinity, 1e309, -1e309]',
         '["NaN", "Infinity", "-Infinity", "\\\"NaN", "\\u004eaN"]',
         '{"$serde_json::private::Number":"NaN","__ds_mint_number__0":"Infinity"}',
         '{"a":NaN,"b":0,"a":Infinity}',
         '[null,false,0,"",[],{},[NaN],{"NaN":Infinity}]',
         '[123456789012345678901234567890,-0,-0.0,1e-999,1e20,1e-7]']


class WireOracleTests(unittest.TestCase):
    def test_actual_server_numeric_shape_error_order_and_cleanup(self):
        for raw in CASES:
            server = Server()
            response, stop = server.handle_line('{"id":NaN,"cmd":"echo","args":{"value":' + raw + '}}')
            self.assertTrue(response["ok"])
            self.assertFalse(stop)
            wire = json.dumps(response)
            self.assertIn('"id": NaN', wire)
            self.assertEqual(json.dumps(response["result"]["value"]), json.dumps(json.loads(raw)))
            self.assertEqual([name for name, _ in server.effects], ["set_caller", "set_service_token", "set_socket_peer", "clear_caller", "clear_socket_peer"])
        for raw in ['NaNx', '+Infinity', '-NaN', '[NaN,]', '{NaN:0}']:
            server = Server()
            self.assertEqual(server.handle_line(raw)[0], {"id": None, "ok": False, "error": "invalid JSON"})
            self.assertFalse(server.effects)
        server = Server()
        self.assertEqual(server.handle_line('{"id":Infinity,"args":NaN}')[0]["error"], "args must be an object")
        self.assertFalse(server.effects)
        self.assertEqual(type(json.loads('1' + '0' * 400)), int)
        with self.assertRaisesRegex(OverflowError, "int too large to convert to float"):
            float(json.loads('1' + '0' * 400))

    def test_actual_client_socket_preserves_literals_and_collision_strings(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "peer.sock")
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(path)
            listener.listen()
            observed = []
            def peer():
                conn, _ = listener.accept()
                with conn, conn.makefile("rb") as reader:
                    raw = reader.readline()
                    observed.append(raw)
                    response, _ = Server().handle_line(raw.decode())
                    conn.sendall((json.dumps(response) + "\n").encode())
            thread = threading.Thread(target=peer)
            thread.start()
            try:
                client = types.SimpleNamespace(socket_path=path, timeout=5)
                request = {"id": 1, "cmd": "echo", "args": {"raw": float("nan"), "quoted": "NaN"}}
                result = NS["_call_once"](client, request)
                self.assertEqual(json.dumps(result), '{"raw": NaN, "quoted": "NaN"}')
                self.assertIn(b'"raw": NaN', observed[0])
                self.assertIn(b'"quoted": "NaN"', observed[0])
            finally:
                thread.join(timeout=5)
                listener.close()
            self.assertFalse(thread.is_alive())

    def test_actual_heartbeat_nan_reaches_sql_not_null_after_row_match(self):
        with sqlite3.connect(":memory:") as db:
            db.execute("CREATE TABLE task_leases(task_id TEXT,pane_id TEXT,heartbeat_at REAL,expires_at REAL NOT NULL)")
            db.execute("INSERT INTO task_leases VALUES('t1','p1',0,1)")
            db.commit()
            store = types.SimpleNamespace(db=db, _lock=threading.RLock())
            response = NS["heartbeat"](store, "t1", "p1", float("inf"))
            self.assertIn('"expires_at": Infinity', json.dumps(response))
            before = db.execute("SELECT * FROM task_leases").fetchall()
            with self.assertRaisesRegex(TaskError, "no active lease"):
                NS["heartbeat"](store, "t1", "other", float("nan"))
            with self.assertRaisesRegex(sqlite3.IntegrityError, "NOT NULL constraint failed: task_leases.expires_at"):
                NS["heartbeat"](store, "t1", "p1", float("nan"))
            self.assertEqual(db.execute("SELECT * FROM task_leases").fetchall(), before)


if __name__ == "__main__":
    import sys
    if "--emit" in sys.argv:
        rows = [{"input": raw, "output": json.dumps(json.loads(raw), separators=(",", ":"), ensure_ascii=False)} for raw in CASES]
        (ROOT / "tests/fixtures/wire_nonfinite_oracle.json").write_text(json.dumps(rows, indent=2) + "\n")
    else:
        unittest.main()
