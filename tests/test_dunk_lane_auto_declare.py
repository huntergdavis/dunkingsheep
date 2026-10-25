"""A pane TITLE must not become a lane name (t115 side effect).

VP, live: pane %28 was started with `new-tab -r` and auto-declared its lane from the
pane title while that title held an inbox nudge, giving
lanes.json {'%28': 'OC | OpenCode inbox message from %15'}. The lane was then
addressable only by that string. Fixed by hand with `lane set %28 opencode-ds41-b`;
this pins the code so it cannot recur.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

NUDGE_TITLE = "OC | OpenCode inbox message from %15"


class InferenceTests(unittest.TestCase):
    def test_the_exact_title_from_the_incident_is_not_a_name(self):
        self.assertFalse(Flock._plain_lane_name(NUDGE_TITLE))

    def test_notice_text_and_pipes_are_never_names(self):
        for title in (NUDGE_TITLE, "[inbox] new message", "a | b", "read it with: x"):
            self.assertFalse(Flock._plain_lane_name(title), title)

    def test_a_plain_lane_name_is_still_accepted(self):
        for name in ("opencode-ds41-b", "dunky_pi.2", "%28", "w8:p8"):
            self.assertTrue(Flock._plain_lane_name(name), name)

    def test_declared_names_may_contain_spaces_but_not_notice_text(self):
        for name in ("dunky pi", "PM (par-a)", "claude poolrad"):
            self.assertTrue(Flock._usable_declared_name(name), name)
        for name in (NUDGE_TITLE, "[inbox] new message"):
            self.assertFalse(Flock._usable_declared_name(name), name)


class CorruptRowTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_a_corrupt_declared_row_fails_closed(self):
        """A stored row that is not a name must not be used as one - and must say so
        rather than quietly handing the lane a mailbox called 'OC | ...'."""
        made = self.flock.create_workspace(label="lane", cwd="/tmp")["pane_id"]
        data = self.flock._lane_registry()
        data[made] = {"mailbox": NUDGE_TITLE, "declared": True, "alive": True}
        self.flock._write_lane_registry(data)
        with self.assertLogs("dunkingsheep", level="WARNING") as captured:
            name, declared = self.flock._row_identity(self.flock._lane_registry()[made])
        self.assertIsNone(name)
        self.assertFalse(declared)
        self.assertTrue(any("not a usable name" in line for line in captured.output))


if __name__ == "__main__":
    unittest.main()
