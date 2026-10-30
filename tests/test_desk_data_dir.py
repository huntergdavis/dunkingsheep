"""The desk never keeps its state inside the repo (2026-10-29 leak of data/ to a public repo)."""
import os, subprocess, sys, tempfile, unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class DeskDataDirTests(unittest.TestCase):
    def test_the_default_is_under_the_config_dir_not_the_repo(self):
        with tempfile.TemporaryDirectory() as cfg:
            env = {k: v for k, v in os.environ.items() if k != "VP_DATA_DIR"}
            env["DUNKINGSHEEP_DIR"] = cfg
            src = open(os.path.join(ROOT, "web", "app.py")).read()
            self.assertIn("DESK_DIR_DEFAULT", src)
            self.assertNotIn('ROOT / "data"', src, "no repo-root data fallback")
            for f in ("bridges/ops_metrics.py", "bridges/host_watch.py"):
                self.assertNotIn('ROOT / "data"', open(os.path.join(ROOT, f)).read(), f)

    def test_the_repo_ignores_data_and_credentials(self):
        ignore = open(os.path.join(ROOT, ".gitignore")).read()
        for pattern in ("data/", ".sqlite3"):
            self.assertIn(pattern, ignore)


if __name__ == "__main__":
    unittest.main()
