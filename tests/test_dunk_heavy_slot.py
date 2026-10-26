"""host.heavy_slot_stale must only judge real slots.

Two false positives on 2026-10-25: the live wrapper is symlinked into /tmp as
'/tmp/poolrad-heavy.sh', and a prefix match alone reported it as
"legacy slot, age 1981min, holder null" on every check-in. A slot is
<prefix>.<digits> and a regular file; anything else is not a slot.
"""

import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_watchdog as wd  # noqa: E402


class HeavySlotTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.now = time.time()
        self.rule = {"directory": self.tmp.name, "prefix": "slot",
                     "max_age_minutes": 30, "severity": "warning", "class": "ops"}
        class _Inputs:
            now = self.now

            def path(self, name):
                return os.path.join(self.tmp.name, name)

            def inbox(self, *_a, **_k):
                return []

        self.inputs = _Inputs()

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, name, age_minutes=0, symlink=False, content=""):
        path = os.path.join(self.tmp.name, name)
        if symlink:
            target = os.path.join(self.tmp.name, "target-" + name)
            with open(target, "w") as handle:
                handle.write(content)
            os.symlink(target, path)
        else:
            with open(path, "w") as handle:
                handle.write(content)
        old = self.now - age_minutes * 60
        os.utime(path, (old, old))
        return path

    def findings(self):
        return [f for f in wd.rule_host_heavy_slot_stale(self.inputs, self.rule)
                if f["rule"] == "host.heavy_slot_stale"]

    def test_a_symlink_named_like_the_prefix_is_not_a_slot(self):
        self.write("slot.sh", age_minutes=1981, symlink=True)
        self.assertEqual([], self.findings())

    def test_a_prefix_name_without_a_number_is_not_a_slot(self):
        self.write("slot", age_minutes=1981)
        self.write("slot.backup", age_minutes=1981)
        self.assertEqual([], self.findings())

    def test_a_real_stale_slot_is_still_reported(self):
        self.write("slot.1", age_minutes=120)
        found = self.findings()
        self.assertEqual(1, len(found), found)
        self.assertIn("slot.1", str(found[0]))

    def test_a_fresh_slot_is_not_reported(self):
        self.write("slot.2", age_minutes=1)
        self.assertEqual([], self.findings())


if __name__ == "__main__":
    unittest.main()
