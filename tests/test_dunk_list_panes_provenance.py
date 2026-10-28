"""t299: the pane-list endpoint must not drop the status's provenance.

Measured on main e7db3963: dunk_core.list_panes() computes agent_status_source,
agent_status_reason and agent_status_inferred (DS #46), the desk projects them, and
dunk_api.h_list_panes() dropped all three because its keep tuple did not name them. A restart could
not fix it - the live daemon's core hash equals main's, so the loss was in the handler.

Two traps the reviewer named, both covered here:
  * agent_status_inferred FALSE is a real value, so a truthiness-based pass-through would keep the key
    and null the value;
  * the handler is a FILTER, so the contract is "present only when the core supplies it" - a pane with
    no provenance must gain no null keys.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_api  # noqa: E402


class FakeFlock:
    def __init__(self, panes):
        self._panes = panes

    def list_panes(self, self_pane_id=None):
        return self._panes


def row(**extra):
    base = {"pane_id": "%1", "agent": "pi", "agent_status": "idle", "cwd": "/tmp"}
    base.update(extra)
    return base


class PaneProvenanceTests(unittest.TestCase):
    def _one(self, pane):
        result = dunk_api.h_list_panes(FakeFlock([pane]), {}, None)
        return result["panes"][0]

    def test_all_three_fields_survive_with_true(self):
        got = self._one(row(agent_status_source="pi declared turns",
                            agent_status_reason="a declared turn ended",
                            agent_status_inferred=True))
        self.assertEqual("pi declared turns", got["agent_status_source"])
        self.assertEqual("a declared turn ended", got["agent_status_reason"])
        self.assertIs(True, got["agent_status_inferred"])

    def test_inferred_false_is_a_value_not_a_missing_key(self):
        got = self._one(row(agent_status_source="terminal backend",
                            agent_status_reason=None,
                            agent_status_inferred=False))
        self.assertIn("agent_status_inferred", got)
        self.assertIs(False, got["agent_status_inferred"],
                      "False must survive: a truthiness check would null it")
        self.assertIn("agent_status_reason", got)
        self.assertIsNone(got["agent_status_reason"])

    def test_a_pane_without_provenance_gains_no_null_keys(self):
        got = self._one(row())
        for key in ("agent_status_source", "agent_status_reason", "agent_status_inferred"):
            self.assertNotIn(key, got,
                             "the filter contract is 'present only when the core supplies it'")

    def test_the_key_set_is_otherwise_unchanged(self):
        got = self._one(row())
        self.assertEqual(
            {"pane_id", "agent", "agent_status", "cwd"}, set(got),
            "the fix must not add or remove any other field")


if __name__ == "__main__":
    unittest.main()
