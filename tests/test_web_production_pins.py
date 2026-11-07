"""t3331: the production web pins, the parity oracle and the docs name the same versions.

The served desk runs fastapi 0.162.0, starlette 1.10.0 and pydantic 2.14.2 (the release venv
under releases/web-9c8f919-client499-fd1d498). The requirement pins, the recorded oracle and
docs/rust/WEB_PINS.md must all say so, or the parity evidence no longer names what was served.
"""

import json
import os
import re
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REQUIREMENTS = os.path.join(ROOT, "bridges", "requirements.txt")
DOC = os.path.join(ROOT, "docs", "rust", "WEB_PINS.md")
ORACLE = os.path.join(ROOT, "rust", "crates", "ds-web", "tests", "fixtures",
                      "uploads_range_oracle.json")

SERVED = {"fastapi": "0.162.0", "starlette": "1.10.0", "pydantic": "2.14.2"}


def _requirement_pins():
    pins = {}
    with open(REQUIREMENTS, encoding="utf-8") as handle:
        for line in handle:
            match = re.fullmatch(r"([A-Za-z0-9_.-]+)==([^\s#]+)\s*", line.strip())
            if match:
                pins[match.group(1).lower()] = match.group(2)
    return pins


class ProductionPins(unittest.TestCase):
    def test_the_requirements_pin_the_served_fastapi_and_starlette(self):
        pins = _requirement_pins()
        self.assertEqual(SERVED["fastapi"], pins.get("fastapi"))
        self.assertEqual(SERVED["starlette"], pins.get("starlette"))

    def test_the_recorded_oracle_was_captured_on_the_served_versions(self):
        with open(ORACLE, encoding="utf-8") as handle:
            oracle = json.load(handle)["oracle"]
        for package, version in SERVED.items():
            self.assertEqual(version, oracle.get(package), package)

    def test_the_doc_names_the_same_versions(self):
        with open(DOC, encoding="utf-8") as handle:
            text = handle.read()
        for package, version in SERVED.items():
            self.assertIn(f"{package} {version}", text, package)


if __name__ == "__main__":
    unittest.main()
