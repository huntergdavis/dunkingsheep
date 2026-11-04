"""Isolated Python oracle for the project-summary git path boundary.

Execute the actual dunk_views.git_merges AST without importing production stores.
"""
import ast
import os
from pathlib import Path
import pwd
import unittest
from unittest.mock import patch


class ProjectSummaryPathOracleTests(unittest.TestCase):
    def test_git_merges_ordinary_path_does_not_consult_home_or_passwd(self):
        source = Path(__file__).resolve().parents[1] / "dunk_views.py"
        tree = ast.parse(source.read_text())
        function = next(node for node in tree.body
                        if isinstance(node, ast.FunctionDef) and node.name == "git_merges")
        namespace = {"os": os}
        exec(compile(ast.Module(body=[function], type_ignores=[]), str(source), "exec"), namespace)

        class NoHomeRead(dict):
            def __contains__(self, key):
                if key == "HOME":
                    raise AssertionError("ordinary path consulted HOME")
                return super().__contains__(key)

            def __getitem__(self, key):
                if key == "HOME":
                    raise AssertionError("ordinary path read HOME")
                return super().__getitem__(key)

        for home in [None, "~", "~/bad", "", "\x00"]:
            for path in ["/explicit/repository", "relative/repository", ""]:
                with self.subTest(home=home, path=path):
                    environment = NoHomeRead({} if home is None else {"HOME": home})
                    with patch.object(os, "environ", environment), \
                         patch.object(pwd, "getpwuid", side_effect=AssertionError("passwd lookup")), \
                         patch.object(pwd, "getpwnam", side_effect=AssertionError("named lookup")), \
                         patch.object(os.path, "isdir", return_value=False):
                        self.assertEqual(namespace["git_merges"](path), {
                            "repo": path, "merges": [], "as_of": None,
                            "reason": f"{path} is not a git repository",
                        })

    def test_git_merges_expands_tilde_home_exactly_once(self):
        source = Path(__file__).resolve().parents[1] / "dunk_views.py"
        tree = ast.parse(source.read_text())
        function = next(node for node in tree.body
                        if isinstance(node, ast.FunctionDef) and node.name == "git_merges")
        namespace = {"os": os}
        exec(compile(ast.Module(body=[function], type_ignores=[]), str(source), "exec"), namespace)
        with patch.object(os, "environ", {"HOME": "~/a"}), \
             patch.object(os.path, "isdir", return_value=False):
            result = namespace["git_merges"]("~/workspace/dunkingsheep")
        self.assertEqual(result["repo"], "~/a/workspace/dunkingsheep")
        self.assertEqual(result["reason"], "~/a/workspace/dunkingsheep is not a git repository")


if __name__ == "__main__":
    unittest.main()
