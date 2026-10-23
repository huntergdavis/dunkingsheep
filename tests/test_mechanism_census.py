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

# The census is DERIVED from the tree, not from a list somebody remembers to
# update: a public function whose name carries one of these words is a candidate,
# and must either have a row or an explicit exemption with a reason and a test.
# (A list maintained by hand is how a new mechanism would slip through in a month.)
VOCABULARY = ("supersede", "retire", "unretire", "restore", "recover", "reap", "rollback",
              "quarantine", "archive", "backfill", "resume", "purge", "migrate",
              "flush", "clear", "deprecate")

# Named by rule, not one by one: a registry shim delegates, a predicate only reads.
EXCLUDED_PREFIXES = {
    "_": "private helper, exercised through its public caller",
    "h_": "registry handler that delegates to the flock method (the mechanism is that method)",
    "is_": "read-only predicate",
    "all_": "read-only listing",
    "has_": "read-only predicate",
    "mailbox_": "read-only predicate",
    "rule_": "watchdog rule over a mechanism, not the mechanism",
}

# A mechanism-shaped function that is NOT a one-purpose mechanism, with the reason
# and the test that covers it. An exemption is as checked as a row: the test must
# exist.
EXEMPT = {
    "inbox_archive": ("thin API shim over Inbox.archive; the mechanism is `archive`",
                      "test_archive_through_the_flock_drains_unread"),
    "task_reap": ("daemon entry point that calls `reap`; the mechanism and its case are "
                  "the `reap` row",
                  "test_reap_requeues_a_dead_caller_even_with_a_live_pane"),
    "clear_caller": ("clears per-request caller state; not a recovery mechanism",
                     "test_task_claims_carry_the_socket_peer_agent_identity"),
    "recoveries": ("read-only view of recorded recoveries",
                   "test_a_reaped_event_is_reported_once_ever_not_again_after_cooldown"),
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


def mechanism_candidates():
    """Public module functions and class methods whose name carries a mechanism word.

    Nested closures are excluded: `flush()` inside the ingest loop is a local step,
    not something a caller can invoke.
    """
    found = {}
    for path in sorted(ROOT.glob("*.py")):
        try:
            tree = ast.parse(path.read_text())
        except SyntaxError:
            continue
        # Module level and class methods only: a closure inside a loop (the ingest
        # `flush`) is a local step, not something a caller can invoke.
        definitions = [node for node in tree.body
                       if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))]
        for node in tree.body:
            if isinstance(node, ast.ClassDef):
                definitions += [child for child in node.body
                                if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef))]
        for node in definitions:
            name = node.name
            if not any(word in name.lower() for word in VOCABULARY):
                continue
            if any(name.startswith(prefix) for prefix in EXCLUDED_PREFIXES):
                continue
            found.setdefault(name, path.name)
    return found


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

    def test_every_mechanism_shaped_function_is_accounted_for(self):
        """The failure mode that happens in a month: a new one-purpose function.

        Derived from the tree, so a `rollback` or a `quarantine` written tomorrow
        fails this test until it has a row (with its one-line case) or an exemption
        (with a reason and a covering test).
        """
        listed = {row["mechanism"] for row in rows()}
        known_tests = test_functions()
        missing, bad_exemptions = [], []
        for name, module in sorted(mechanism_candidates().items()):
            if name in listed:
                continue
            if name in EXEMPT:
                reason, test = EXEMPT[name]
                if not reason or test not in known_tests:
                    bad_exemptions.append(f"{name}: exemption names {test}, which does not exist")
                continue
            missing.append(f"{name} ({module})")
        self.assertEqual([], bad_exemptions, "; ".join(bad_exemptions))
        self.assertEqual([], missing,
                         "mechanism-shaped functions with no census row and no exemption: "
                         + ", ".join(missing)
                         + " - state the case in one line in docs/MECHANISMS.md first")

    def test_no_row_claims_a_state_other_than_tested(self):
        for row in rows():
            self.assertEqual("tested", row["state"],
                             f"{row['mechanism']} is not tested; a census entry is a promise")


if __name__ == "__main__":
    unittest.main()
