"""t1165: a desk-role inbox read over the cap must print something useful, not KeyError."""
import contextlib
import io
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import _bootstrap  # noqa: F401,E402
import dunk_roles  # noqa: E402


def load_cli():
    path = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                        "dunkingsheep")
    ns = {"__file__": path, "__name__": "cli_under_test"}
    with open(path, encoding="utf-8") as handle:
        source = handle.read().split('if __name__ == "__main__":')[0]
    exec(compile(source, path, "exec"), ns)  # noqa: S102
    return ns


def inbox_result(n, size=900):
    return {"mailbox": "%133", "messages": [
        {"message_id": f"i{i}", "sender": "x", "subject": f"s{i}", "body": "b" * size}
        for i in range(n)]}


class DeskInboxCapTests(unittest.TestCase):
    def test_over_cap_keeps_newest_messages_and_the_messages_key(self):
        out = dunk_roles.clamp_desk_result(inbox_result(10))
        self.assertTrue(out["truncated"])
        ids = [m["message_id"] for m in out["messages"]]
        self.assertEqual("i9", ids[-1])
        self.assertEqual(10 - len(ids), out["omitted"])
        self.assertLessEqual(len(json.dumps(out)), dunk_roles.DESK_RESULT_BYTES)

    def test_under_cap_is_untouched(self):
        r = inbox_result(2, size=10)
        self.assertEqual(r, dunk_roles.clamp_desk_result(r))

    def test_cli_prints_a_capped_shape_without_messages(self):
        cli = load_cli()
        capped = {"truncated": True, "bytes": 9999, "cap": 4096, "head": "HEADTEXT", "note": "n"}
        for fn in ("print_inbox_read", "print_inbox_list", "print_inbox_recover"):
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                cli[fn](capped)
            self.assertIn("HEADTEXT", buf.getvalue())

    def test_cli_says_when_older_messages_were_left_out(self):
        cli = load_cli()
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            cli["print_inbox_read"](dunk_roles.clamp_desk_result(inbox_result(10)))
        self.assertIn("capped", buf.getvalue())


if __name__ == "__main__":
    unittest.main()
