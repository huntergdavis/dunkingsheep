"""The desk's state lives under the config dir - never in the repo, and never "".

SCOPE: web/app.py, bridges/ops_metrics.py, bridges/host_watch.py, and (t1031) the tools
bridges/tools/{free_catalog,free_leaderboard,check_theme_wave}.py via tools/desk_data.py -
"ignored by git" is not the same guarantee as "never created in the repo", and the 2026-10-29
incident was a COMMIT.

The 2026-10-29 incident shape: a lane imported `web.app` from a checkout with no
VP_DATA_DIR set, the module created `data/` in the REPO, and a later `git add -A`
published three credential files. So the guarantee this file tests is behavioural, not
textual: with the variables unset, EMPTY, or pointing at the config dir, the state path
must land under `~/.config/dunkingsheep/desk` - never the repo, never the CWD.

An EMPTY value is the case the first version of this test could not see, because
`os.getenv("VP_DATA_DIR", default)` only applies the default when the variable is UNSET;
`VP_DATA_DIR=` yields `Path("")`, which resolves to the CURRENT WORKING DIRECTORY. The
code now uses `or` for both variables, and this test feeds it the empty string.
"""
import os
import re
import subprocess
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BRIDGES = os.path.join(ROOT, "bridges")

# The DATA rule is taken FROM web/app.py rather than retyped: a copy of the expression
# would pass while the file stayed broken, which is exactly the defect this test is here
# to catch. web.app itself needs fastapi (not every runner has it), so the two lines that
# define the path are extracted and evaluated in a scratch interpreter.
def app_data_source():
    path = os.path.join(ROOT, "web", "app.py")
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    wanted = [line for line in lines
              if line.startswith("DESK_DIR_DEFAULT = ") or line.startswith("DATA = ")]
    assert len(wanted) == 2, f"web/app.py must define DESK_DIR_DEFAULT and DATA: {wanted}"
    return "import os\nfrom pathlib import Path\n" + "\n".join(wanted) + "\nprint(DATA)\n"


def run_app_data(env_updates):
    """Evaluate web/app.py's DATA expression with a controlled environment."""
    env = {key: value for key, value in os.environ.items()
           if key not in ("VP_DATA_DIR", "DUNKINGSHEEP_DIR")}
    env.update(env_updates)
    result = subprocess.run([sys.executable, "-c", app_data_source()], capture_output=True,
                            text=True, env=env, cwd=ROOT)
    assert result.returncode == 0, result.stderr
    return result.stdout.strip()


TOOLS = os.path.join(BRIDGES, "tools")


def run_tool(module, attribute, env_updates, env_remove=()):
    """Import a bridges/tools module (script-style, its own dir on sys.path)."""
    env = {key: value for key, value in os.environ.items()
           if key not in ("VP_DATA_DIR", "DUNKINGSHEEP_DIR", *env_remove)}
    env.update(env_updates)
    program = (f"import sys; sys.path.insert(0, {TOOLS!r}); import {module}; "
               f"print(getattr({module}, {attribute!r}))")
    result = subprocess.run([sys.executable, "-c", program], capture_output=True,
                            text=True, env=env, cwd=ROOT)
    assert result.returncode == 0, result.stderr
    return result.stdout.strip()


def run_bridge(module, attribute, env_updates):
    """Import a bridge module in a scratch environment and print its state path."""
    env = {key: value for key, value in os.environ.items()
           if key not in ("VP_DATA_DIR", "DUNKINGSHEEP_DIR")}
    env.update(env_updates)
    program = (f"import sys; sys.path.insert(0, {BRIDGES!r}); import {module}; "
               f"print(getattr({module}, {attribute!r}))")
    result = subprocess.run([sys.executable, "-c", program], capture_output=True,
                            text=True, env=env, cwd=ROOT)
    assert result.returncode == 0, result.stderr
    return result.stdout.strip()


class DeskDataDirTests(unittest.TestCase):
    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cfg = self.tmp.name
        self.home_config = os.path.join(os.path.expanduser("~"), ".config", "dunkingsheep")

    def test_the_default_is_under_the_config_dir_not_the_repo(self):
        # UNSET: the documented default, under the user's config dir.
        expected = os.path.join(self.home_config, "desk")
        self.assertEqual(expected, run_app_data({}))
        # DUNKINGSHEEP_DIR set: the desk is under THAT config dir.
        expected = os.path.join(self.cfg, "desk")
        self.assertEqual(expected, run_app_data({"DUNKINGSHEEP_DIR": self.cfg}))
        self.assertEqual(expected, run_bridge("ops_metrics", "DATA",
                                              {"DUNKINGSHEEP_DIR": self.cfg}))
        self.assertEqual(os.path.join(self.cfg, "desk", "host-watch-state.json"),
                         run_bridge("host_watch", "STATE",
                                    {"DUNKINGSHEEP_DIR": self.cfg}))

    def test_an_empty_variable_never_means_the_repo(self):
        # THE FIRST VERSION OF THIS TEST COULD NOT FAIL HERE: `os.getenv(name, default)`
        # treats "" as a value, so Path("") resolved to the CWD - the repo.
        for env in ({"VP_DATA_DIR": ""},
                    {"VP_DATA_DIR": "", "DUNKINGSHEEP_DIR": ""},
                    {"DUNKINGSHEEP_DIR": ""}):
            with self.subTest(env=env):
                resolved = run_app_data(env)
                self.assertNotEqual(ROOT, resolved, "the repo is never the state dir")
                self.assertFalse(resolved.startswith(ROOT),
                                 f"{resolved} is inside the repo")
                self.assertEqual(os.path.join(self.home_config, "desk"), resolved)
                self.assertFalse(run_bridge("ops_metrics", "DATA", env).startswith(ROOT))
                bridge_state = run_bridge("host_watch", "STATE", env)
                self.assertFalse(os.path.abspath(bridge_state).startswith(ROOT),
                                 f"{bridge_state} is inside the repo")

    def test_the_desk_tools_keep_their_state_out_of_the_repo(self):
        cases = (("free_catalog", "STATE"), ("free_leaderboard", "FREE_RUNS"),
                 ("free_leaderboard", "BAKEOFF"))
        home_desk = os.path.join(self.home_config, "desk")
        for module, attribute in cases:
            for env in ({}, {"VP_DATA_DIR": ""}, {"DUNKINGSHEEP_DIR": ""},
                        {"VP_DATA_DIR": "", "DUNKINGSHEEP_DIR": ""}):
                with self.subTest(module=module, attribute=attribute, env=env):
                    resolved = run_tool(module, attribute, env,
                                        env_remove=("VP_FREE_RUNS", "VP_BAKEOFF"))
                    self.assertTrue(resolved.startswith(home_desk), resolved)
                    self.assertFalse(resolved.startswith(ROOT), f"{resolved} is in the repo")
            moved = run_tool(module, attribute, {"DUNKINGSHEEP_DIR": self.cfg},
                             env_remove=("VP_FREE_RUNS", "VP_BAKEOFF"))
            self.assertTrue(moved.startswith(os.path.join(self.cfg, "desk")), moved)
            explicit = os.path.join(self.cfg, "explicit")
            self.assertTrue(run_tool(module, attribute, {"VP_DATA_DIR": explicit},
                                     env_remove=("VP_FREE_RUNS", "VP_BAKEOFF")
                                     ).startswith(explicit))

    def test_check_theme_wave_only_accepts_artifacts_under_the_desk_dir(self):
        # No tool under bridges/tools may name ROOT/'data' as a write target.
        for name in ("free_catalog.py", "free_leaderboard.py", "check_theme_wave.py"):
            with open(os.path.join(TOOLS, name), encoding="utf-8") as handle:
                source = handle.read()
            self.assertIsNone(re.search(r"""ROOT\s*/\s*["']data["']""", source),
                              f"{name} names the repo's data/")
        refused = subprocess.run(
            [sys.executable, os.path.join(TOOLS, "check_theme_wave.py"), "--theme", "x",
             "--base", "HEAD", "--artifacts", os.path.join(ROOT, "data", "out")],
            capture_output=True, text=True, cwd=ROOT,
            env={**{k: v for k, v in os.environ.items() if k != "VP_DATA_DIR"},
                 "DUNKINGSHEEP_DIR": self.cfg})
        self.assertNotEqual(0, refused.returncode)
        self.assertFalse(os.path.exists(os.path.join(ROOT, "data", "out")))

    def test_the_repo_data_dir_is_still_ignored_as_a_second_line_of_defence(self):
        # The incident was a COMMIT, so "ignored" is not the guarantee - the paths above
        # are - but a broad `git add -A` must not be able to publish it either.
        with open(os.path.join(ROOT, ".gitignore"), encoding="utf-8") as handle:
            ignored = handle.read()
        self.assertIn("data/", ignored)
