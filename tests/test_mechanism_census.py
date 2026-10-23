"""The census must match the code, or it is a document nobody updates.

Applying the "test the case it exists for" rule as a census only works while the
census is true. This test enforces four things:

1. every row states its only case in ONE line (a mechanism whose purpose needs two
   lines is doing two jobs);
2. every cited test function EXISTS in the suite (a census that cites a deleted test
   is a spelling check, not a guard);
3. every mechanism named here is a real attribute somewhere in the code;
4. every mechanism declared one-purpose in ONE_PURPOSE appears here.
"""
import ast
import os
import pathlib
import re
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tests"))
import _bootstrap  # noqa: E402,F401

CENSUS = ROOT / "docs" / "MECHANISMS.md"

# Every mechanism that exists for exactly one purpose. Adding one is a decision,
# so it goes here and in the census in the same commit.
ONE_PURPOSE = {
    "supersede", "retire", "unretire", "clear_box", "flush_box", "memory_backfill",
    "tick", "archive", "task_reap", "kill_decision", "write_heartbeat",
    "rule_resource_would_kill",
}


def rows():
    text = CENSUS.read_text()
    found = []
    for line in text.splitlines():
        if not line.startswith("| `"):
            continue
        cells = [cell.strip() for cell in line.strip("|").split("|")]
        if len(cells) != 4 or cells[0] == "mechanism":
            continue
        found.append({"mechanism": cells[0].strip("`"), "case": cells[1],
                      "test": cells[2].strip("`"), "state": cells[3]})
    return found


def test_functions():
    names = set()
    for path in (ROOT / "tests").glob("test_*.py"):
        names |= set(re.findall(r"def (test_\w+)", path.read_text()))
    return names


def code_attributes():
    """Every attribute/function name defined in the package, for the existence check."""
    names = set()
    for path in ROOT.glob("*.py"):
        try:
            tree = ast.parse(path.read_text())
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                names.add(node.name)
    return names


class CensusTests(unittest.TestCase):
    def test_every_row_states_its_only_case_in_one_line(self):
        for row in rows():
            self.assertTrue(row["case"], row["mechanism"])
            self.assertLessEqual(len(row["case"]), 90,
                                 f"{row['mechanism']}: the case needs more than one line - "
                                 f"the mechanism may be doing two jobs")
            self.assertNotIn("\n", row["case"])

    def test_every_cited_test_exists(self):
        known = test_functions()
        for row in rows():
            self.assertIn(row["test"], known,
                          f"{row['mechanism']} cites {row['test']}, which does not exist")

    def test_every_mechanism_exists_in_the_code(self):
        known = code_attributes()
        for row in rows():
            name = row["mechanism"].split(".")[-1]
            self.assertIn(name, known, f"census names {name}, which no module defines")

    def test_every_one_purpose_mechanism_has_a_row(self):
        listed = {row["mechanism"].split(".")[-1] for row in rows()}
        missing = ONE_PURPOSE - listed
        self.assertEqual(set(), missing,
                         "a one-purpose mechanism without a census row (write its one-line "
                         "case first): " + ", ".join(sorted(missing)))

    def test_no_row_claims_a_state_other_than_tested(self):
        for row in rows():
            self.assertEqual("tested", row["state"],
                             f"{row['mechanism']} is not tested; a census entry is a promise")


if __name__ == "__main__":
    unittest.main()
