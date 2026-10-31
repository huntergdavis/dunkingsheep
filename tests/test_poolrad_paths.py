"""t1128: Poolrad paths must not depend on where the release tree is unpacked.

A release lives under releases/<sha>/, so `ROOT.parent` is releases/. These tests build
that shape (a copy of the tree under a throwaway releases/ dir, a fake HOME holding the
checkout, no POOLRAD_* variables) and load the desk's own path resolution from it.
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "bridges"))
import poolrad_paths  # noqa: E402


def clean_env(home, **extra):
    env = {key: value for key, value in os.environ.items() if not key.startswith("POOLRAD_")}
    env.update(HOME=str(home), **extra)
    env.pop("DUNKINGSHEEP_POOLRAD_CONFIG", None)
    return env


class ResolutionOrder(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.config = Path(self.tmp.name) / "poolrad.json"
        self.saved = (poolrad_paths.CONFIG, dict(os.environ))
        poolrad_paths.CONFIG = self.config
        for name in [k for k in os.environ if k.startswith("POOLRAD_")]:
            del os.environ[name]
        self.addCleanup(self.restore)

    def restore(self):
        poolrad_paths.CONFIG = self.saved[0]
        os.environ.clear()
        os.environ.update(self.saved[1])

    def test_default_is_the_workspace_checkout_not_a_sibling_of_the_tree(self):
        self.assertEqual(Path.home() / "workspace" / "poolrad-decomp", poolrad_paths.repo("POOLRAD_REPO"))

    def test_config_file_beats_the_default(self):
        self.config.write_text(json.dumps({"repo": "/srv/poolrad"}))
        self.assertEqual(Path("/srv/poolrad"), poolrad_paths.repo("POOLRAD_REPO"))

    def test_environment_beats_config_and_earlier_names_win(self):
        self.config.write_text(json.dumps({"repo": "/srv/poolrad"}))
        os.environ["POOLRAD_REPO"] = "/env/a"
        os.environ["POOLRAD_REPO_DIR"] = "/env/b"
        self.assertEqual(Path("/env/b"), poolrad_paths.repo("POOLRAD_REPO_DIR", "POOLRAD_REPO"))

    def test_a_broken_config_falls_back_to_the_default(self):
        self.config.write_text("{not json")
        self.assertEqual(poolrad_paths.DEFAULT_REPO, poolrad_paths.repo("POOLRAD_REPO"))


@unittest.skipUnless(importlib.util.find_spec("fastapi"), "web.app needs fastapi")
class ReleaseShapedTree(unittest.TestCase):
    def test_the_desk_finds_poolrad_with_no_environment_and_a_tree_under_releases(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp) / "home"
            checkout = home / "workspace" / "poolrad-decomp"
            checkout.mkdir(parents=True)
            release = Path(tmp) / "releases" / ("a" * 40)
            release.parent.mkdir()
            shutil.copytree(ROOT, release, ignore=shutil.ignore_patterns(
                ".git", ".venv", "node_modules", "dist", "__pycache__", "target"))
            code = ("import json, web.app as a; print(json.dumps({'repo': str(a.DASHBOARD_REPO),"
                    " 'game': str(a.GAME.repo_dir) if hasattr(a.GAME, 'repo_dir') else None,"
                    " 'dashboard': a._dashboard_module() is not None,"
                    " 'dir': str(a.DASHBOARD_DIR)}))")
            out = subprocess.run([sys.executable, "-c", code], cwd=release, text=True,
                                 capture_output=True, env=clean_env(
                                     home, VP_DATA_DIR=str(Path(tmp) / "data")))
            self.assertEqual(0, out.returncode, out.stderr[-800:])
            seen = json.loads(out.stdout.strip().splitlines()[-1])
            self.assertEqual(str(checkout.resolve()), seen["repo"])
            self.assertTrue(seen["dashboard"], "the vendored dashboard must load from the tree")
            self.assertTrue(seen["dir"].startswith(str(release.resolve())), seen["dir"])


if __name__ == "__main__":
    unittest.main()
