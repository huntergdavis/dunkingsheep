"""Isolated source oracle: real Python send_keys handler/core, no daemon imports."""
import ast
import json
from pathlib import Path
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DunkError(Exception):
    pass


def source_functions(path, names, class_name=None):
    tree = ast.parse((ROOT / path).read_text())
    body = tree.body
    if class_name:
        body = next(node for node in body if isinstance(node, ast.ClassDef)
                    and node.name == class_name).body
    return [node for node in body if isinstance(node, ast.FunctionDef) and node.name in names]


NS = {"DunkError": DunkError, "DEFAULT_SEND_WAIT_S": 3.0,
      "threading": threading, "time": __import__("time")}
MESSAGE = next(node for node in ast.parse((ROOT / "dunk_core.py").read_text()).body
               if isinstance(node, ast.ClassDef) and node.name == "Message")
NODES = ([MESSAGE]
         + source_functions("dunk_api.py", {"_opt", "_bool", "_authorise_send", "h_send_keys"})
         + source_functions("dunk_core.py", {"_check_bool"})
         + source_functions("dunk_core.py", {"require_send_self", "send_keys_now", "_new_message"}, "Flock"))
exec(compile(ast.Module(body=NODES, type_ignores=[]), "actual-send-keys-oracle", "exec"), NS)


class Flock:
    require_send_self = NS["require_send_self"]
    send_keys_now = NS["send_keys_now"]
    _new_message = NS["_new_message"]

    def __init__(self, role="worker"):
        self.role = role
        self._lock = threading.Lock()
        self.next_message_id = 1
        self.calls, self.keys, self.messages = [], [], []

    def caller_role(self, pane):
        self.calls.append("role")
        return self.role

    def resolve_target(self, target, pane):
        self.calls.append("resolve")
        if target == "missing":
            raise DunkError("missing target")
        return {"pane_id": "other" if target == "other" else "self", "tab_label": "fixture"}

    def press_keys(self, pane, keys):
        self.keys.append(keys)
        return True, "sent"

    def _deliver(self, message):
        self.calls.append("deliver")
        message.deliver()
        message.status = "sent"
        self.messages.append(message)

    def _enqueue(self, message):
        self.calls.append("enqueue")
        self.messages.append(message)


def row(raw):
    flock = Flock()
    try:
        result = NS["h_send_keys"](flock, {"target": "self", "keys": json.loads(raw),
                                         "hold_while_typing": False, "wait_s": 0}, "self")
        # Clock samples are checked separately by the consumer; compare every other field.
        response = {key: value for key, value in result.items()
                    if key not in {"created_at", "finished_at"}}
        return {"json": raw, "keys": flock.keys[0], "text": result["text"],
                "response": response}
    except (TypeError, DunkError) as error:
        assert flock.next_message_id == 1 and not flock.messages
        return {"json": raw, "error": str(error), "error_type": type(error).__name__}


CASES = ['"ctrl+c Enter"', '"  Escape  "',
         '["ctrl+c Enter", " ", "\\u001c", " Escape "]',
         '[null, false, true, 0, 1.0, -0.0, ["a\\nb"], {"z": 1, "a": false}]',
         '{"z key": 1, "a key": 2, " ": 3, "\\u001f": 4}',
         '[184467440737095516160000000, 1e999, -1e999, 1e-999]',
         '["\\u200b", "\\u001cEscape\\u001f", "\\u0085"]',
         '[]', '{}', '""', 'null', 'false', '0', '7', '1.0', '1e999']


class RawKeyOracleTests(unittest.TestCase):
    def test_fixture_matches_actual_python(self):
        expected = json.loads((ROOT / "tests/fixtures/send_keys_raw_oracle.json").read_text())
        self.assertEqual([row(raw) for raw in CASES], expected)

    def test_failure_order_and_no_message_allocation(self):
        cases = [
            ({"target": "other", "keys": False, "wait_s": "bad"}, "worker", DunkError, "requires a superuser", ["role", "resolve"]),
            ({"target": "missing", "keys": False}, "worker", DunkError, "missing target", ["role", "resolve"]),
            ({"target": "missing", "keys": False}, "owner", DunkError, "missing target", ["role", "resolve"]),
            ({"target": "self", "keys": False, "wait_s": "bad"}, "worker", ValueError, "could not convert string to float", ["role", "resolve"]),
            ({"target": "missing", "keys": False, "wait_s": "bad"}, "owner", ValueError, "could not convert string to float", ["role"]),
            ({"target": "missing", "keys": False, "desk_token": "old"}, "worker", DunkError, "desk_token is retired", []),
        ]
        for args, role, kind, text, calls in cases:
            with self.subTest(args=args, role=role):
                flock = Flock(role)
                with self.assertRaisesRegex(kind, text):
                    NS["h_send_keys"](flock, args, "self")
                self.assertEqual(flock.calls, calls)
                self.assertEqual(flock.next_message_id, 1)
                self.assertEqual(flock.messages, [])

    def test_queued_closure_retains_key_boundaries_and_all_python_whitespace(self):
        whitespace = ''.join(chr(i) for i in range(0x110000) if chr(i).isspace())
        self.assertEqual(len(whitespace), 29)
        flock = Flock()
        result = NS["h_send_keys"](flock, {"target": "self", "keys": [whitespace, "ctrl+c Enter"], "wait_s": 0}, "self")
        self.assertEqual(result["status"], "queued")
        self.assertEqual(flock.keys, [])
        flock.messages[0].deliver()
        self.assertEqual(flock.keys, [["ctrl+c Enter"]])
        self.assertEqual(flock.next_message_id, 2)

    def test_python_nonfinite_constants_are_numbers(self):
        self.assertEqual(row('[NaN, Infinity, -Infinity]')["keys"], ["nan", "inf", "-inf"])


if __name__ == "__main__":
    import sys
    if "--emit" in sys.argv:
        (ROOT / "tests/fixtures/send_keys_raw_oracle.json").write_text(json.dumps([row(raw) for raw in CASES], ensure_ascii=True, indent=2) + "\n")
    else:
        unittest.main()
