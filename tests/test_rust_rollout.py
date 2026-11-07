"""t3471: the Rust cutover rollout tooling, tools/rust_rollout.sh, under a fake HOME.

Every test runs the script with HOME in a temp directory, the RUST_* paths inside it, and stub
systemctl, journalctl, cargo, sqlite3 and dunkingsheep first on PATH. The stubs append their argv to
a log, so the tests check the order and the content of each step without touching systemd, cargo, or
any real state. The real tools used are git, tar, sha256sum, cp, sed and awk.
"""
import hashlib
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
SCRIPT = os.path.join(ROOT, "tools", "rust_rollout.sh")

STUBS = {
    "systemctl": r'''#!/bin/sh
echo "systemctl $*" >> "$FAKE_LOG"
case "$*" in
  *daemon-reload*) if [ -n "$FAKE_RELOAD_FAIL" ]; then exit 1; fi ;;
  *list-unit-files*) printf 'dunkingsheep-dispatch.timer enabled enabled\ndunkingsheep-health.timer enabled enabled\n' ;;
  *is-active*) echo active ;;
esac
exit 0
''',
    "journalctl": r'''#!/bin/sh
echo "journalctl $*" >> "$FAKE_LOG"
echo "tick ok"
''',
    "cargo": r'''#!/bin/sh
echo "cargo $*" >> "$FAKE_LOG"
out="$PWD/rust/target/release"
mkdir -p "$out"
printf '#!/bin/sh\necho "dunkingsheep 0.1.0 (fake build)"\n' > "$out/dunkingsheep"
chmod 0755 "$out/dunkingsheep"
''',
    "sqlite3": r'''#!/bin/sh
echo "sqlite3 $*" >> "$FAKE_LOG"
case "$2" in
  .backup*)
    dest=$(printf '%s' "$2" | sed "s/^\.backup '//; s/'\$//")
    printf 'snapshot\n' > "$dest"
    ;;
  *integrity_check*) if [ -n "$FAKE_SQLITE_FAIL" ]; then echo corrupt; else echo ok; fi ;;
esac
exit 0
''',
    "dunkingsheep": r'''#!/bin/sh
echo "dunkingsheep $*" >> "$FAKE_LOG"
echo "fake $1 ok"
''',
}

OLD_LAUNCHER = '#!/bin/sh\nexec /usr/bin/python3 /old/release/dunkingsheep "$@"\n'
DB_FILES = ["tasks.db", "inbox.db", "context-ledger.db", "dispatch.db", "agent-state.db"]


def write(path, text, mode=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    if mode is not None:
        os.chmod(path, mode)


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def mode_of(path):
    return stat.S_IMODE(os.stat(path).st_mode)


class RustRolloutTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="t3471-")
        self.addCleanup(self.cleanup)
        self.home = os.path.join(self.tmp, "home")
        self.releases = os.path.join(self.home, ".local", "share", "dunkingsheep", "releases")
        self.launcher = os.path.join(self.home, ".local", "bin", "dunkingsheep")
        self.dropin = os.path.join(self.home, ".config", "systemd", "user",
                                   "dunkingsheep.service.d", "50-rust-cutover.conf")
        self.state = os.path.join(self.home, ".local", "share", "dunkingsheep", "rust-cutover.state")
        self.backups = os.path.join(self.home, ".local", "share", "dunkingsheep", "backups")
        self.log = os.path.join(self.tmp, "fake.log")
        self.worktree = os.path.join(self.tmp, "owner-release-wt")
        for name in DB_FILES:
            write(os.path.join(self.home, ".config", "dunkingsheep", name), "live sqlite stand-in\n")
        write(os.path.join(self.home, ".config", "dunkingsheep", "dunks.json"), "{}\n")
        # The daemon writes its token file 0600; the backup must keep it private.
        write(os.path.join(self.home, ".config", "dunkingsheep", "github-tokens.json"), "{}\n", mode=0o600)
        write(self.launcher, OLD_LAUNCHER, mode=0o755)
        self.old_launcher = read(self.launcher)
        self.bindir = os.path.join(self.tmp, "bin")
        for name, body in STUBS.items():
            write(os.path.join(self.bindir, name), body, mode=0o755)
        self.repo = os.path.join(self.tmp, "repo")
        os.makedirs(os.path.join(self.repo, "rust"))
        write(os.path.join(self.repo, "rust", "Cargo.toml"), "[workspace]\n")
        git_env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
                       GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid")
        subprocess.run(["git", "-C", self.repo, "init", "-q"], check=True, env=git_env)
        subprocess.run(["git", "-C", self.repo, "add", "-A"], check=True, env=git_env)
        subprocess.run(["git", "-C", self.repo, "commit", "-q", "-m", "pinned"], check=True, env=git_env)
        self.sha = subprocess.run(["git", "-C", self.repo, "rev-parse", "HEAD"], check=True,
                                  capture_output=True, text=True).stdout.strip()
        self.env = {
            "HOME": self.home,
            "PATH": self.bindir + ":/usr/bin:/bin",
            "LANG": "C.UTF-8",
            "FAKE_LOG": self.log,
            "RUST_REPO": self.repo,
            "RUST_WORKTREE": self.worktree,
            "RUST_RELEASES": self.releases,
            "RUST_CARGO": "cargo",
            "RUST_OWNER": "%1",
            "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
            "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
        }
        self.bin = os.path.join(self.releases, "rust-" + self.sha, "dunkingsheep")

    def cleanup(self):
        # Staged release directories are read-only by design; make them removable for the cleanup.
        for root, dirs, _files in os.walk(self.tmp):
            for name in dirs:
                os.chmod(os.path.join(root, name), 0o755)
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_script(self, *args, apply=False, umask=None):
        script_argv = [SCRIPT, *args]
        if apply:
            script_argv.append("--apply")
        if umask is None:
            argv = ["sh", *script_argv]
        else:
            # A permissive caller umask, to prove the script's own privacy does not depend on it.
            argv = ["sh", "-c", 'umask ' + umask + '; exec sh "$@"', "sh", *script_argv]
        return subprocess.run(argv, env=self.env, capture_output=True, text=True, cwd=self.tmp, timeout=120)

    def log_lines(self):
        if not os.path.exists(self.log):
            return []
        return read(self.log).splitlines()

    def stage(self):
        proc = self.run_script("stage", "--sha", self.sha, apply=True)
        self.assertEqual(0, proc.returncode, proc.stderr + proc.stdout)

    def activate(self):
        proc = self.run_script("activate", "--sha", self.sha, apply=True)
        self.assertEqual(0, proc.returncode, proc.stderr + proc.stdout)

    def index_of(self, line, lines):
        self.assertIn(line, lines)
        return lines.index(line)

    # -- plan and dry run ---------------------------------------------------------------------

    def test_plan_names_every_step_and_changes_nothing(self):
        proc = self.run_script("plan", "--sha", self.sha)
        self.assertEqual(0, proc.returncode, proc.stderr)
        for step in ("1. stage:", "2. activate:", "3. verify:", "4. rollback:"):
            self.assertIn(step, proc.stdout)
        self.assertFalse(os.path.exists(self.releases))
        self.assertFalse(os.path.exists(self.worktree))
        self.assertEqual([], self.log_lines())

    def test_stage_without_apply_is_a_dry_run_and_creates_nothing(self):
        proc = self.run_script("stage", "--sha", self.sha)
        self.assertEqual(0, proc.returncode, proc.stderr)
        self.assertIn("DRY-RUN: git -C", proc.stdout)
        self.assertIn("DRY-RUN: sh -c", proc.stdout)
        self.assertFalse(os.path.exists(self.releases))
        self.assertFalse(os.path.exists(self.worktree))
        self.assertEqual([], self.log_lines())

    def test_activate_without_apply_changes_nothing(self):
        proc = self.run_script("activate", "--sha", self.sha)
        self.assertEqual(0, proc.returncode, proc.stderr)
        self.assertIn("DRY-RUN: systemctl --user stop dunkingsheep-*.timer", proc.stdout)
        self.assertIn("DRY-RUN: write " + self.dropin, proc.stdout)
        self.assertEqual(OLD_LAUNCHER, read(self.launcher))
        self.assertFalse(os.path.exists(self.dropin))
        self.assertFalse(os.path.exists(self.state))
        self.assertFalse(os.path.exists(self.backups))
        self.assertEqual([], self.log_lines())

    # -- stage --------------------------------------------------------------------------------

    def test_stage_apply_builds_the_pinned_revision_and_installs_read_only(self):
        self.stage()
        wt_head = subprocess.run(["git", "-C", self.worktree, "rev-parse", "HEAD"], check=True,
                                 capture_output=True, text=True).stdout.strip()
        self.assertEqual(self.sha, wt_head)
        self.assertTrue(any("cargo build --release" in line for line in self.log_lines()))
        self.assertEqual(0o555, mode_of(self.bin))
        self.assertEqual(0o555, mode_of(os.path.dirname(self.bin)))
        with open(self.bin, "rb") as handle:
            digest = hashlib.sha256(handle.read()).hexdigest()
        self.assertIn(digest, read(os.path.join(os.path.dirname(self.bin), "SHA256SUM")))
        self.assertIn("dunkingsheep 0.1.0", read(os.path.join(os.path.dirname(self.bin), "VERSION")))

    def test_stage_twice_reports_already_staged_and_verifies_the_record(self):
        self.stage()
        proc = self.run_script("stage", "--sha", self.sha, apply=True)
        self.assertEqual(0, proc.returncode, proc.stderr)
        self.assertIn("already staged", proc.stdout)

    # -- activate -----------------------------------------------------------------------------

    def test_activate_refuses_without_a_staged_binary(self):
        proc = self.run_script("activate", "--sha", self.sha, apply=True)
        self.assertNotEqual(0, proc.returncode)
        self.assertIn("stage " + self.sha + " first", proc.stderr)
        self.assertEqual(OLD_LAUNCHER, read(self.launcher))
        self.assertFalse(os.path.exists(self.dropin))

    def test_activate_apply_stops_timers_backs_up_then_switches_and_restarts(self):
        self.stage()
        self.activate()
        lines = self.log_lines()
        stop = self.index_of("systemctl --user stop dunkingsheep-*.timer", lines)
        restart = self.index_of("systemctl --user restart dunkingsheep.service", lines)
        reload_ = self.index_of("systemctl --user daemon-reload", lines)
        sqlite = [i for i, line in enumerate(lines) if line.startswith("sqlite3 ")]
        self.assertTrue(sqlite)
        self.assertLess(stop, sqlite[0], "the timers stop before the backup")
        self.assertLess(sqlite[-1], reload_, "the backup is complete before the switch")
        self.assertLess(reload_, restart)

        drop = read(self.dropin)
        self.assertIn("ExecStart=\nExecStart=" + self.bin + " serve\n", drop)
        launcher = read(self.launcher)
        self.assertIn('exec ' + self.bin + ' "$@"', launcher)
        self.assertEqual(0o755, mode_of(self.launcher))
        python_copies = [name for name in os.listdir(os.path.dirname(self.launcher))
                         if name.startswith("dunkingsheep.python-")]
        self.assertEqual(1, len(python_copies))
        self.assertEqual(OLD_LAUNCHER, read(os.path.join(os.path.dirname(self.launcher), python_copies[0])))

        state = read(self.state)
        self.assertIn("sha=" + self.sha, state)
        self.assertIn("binary=" + self.bin, state)
        backup = [name for name in os.listdir(self.backups) if name.startswith("pre-rust-")]
        self.assertEqual(1, len(backup))
        listing = subprocess.run(["tar", "-tf", os.path.join(self.backups, backup[0])],
                                 check=True, capture_output=True, text=True).stdout.split()
        self.assertIn("./dunks.json", listing)
        self.assertIn("./tasks.db", listing)
        self.assertNotIn("./tasks.db-wal", listing)

    def test_a_failed_backup_restarts_the_timers_and_switches_nothing(self):
        self.stage()
        self.env["FAKE_SQLITE_FAIL"] = "1"
        proc = self.run_script("activate", "--sha", self.sha, apply=True)
        self.assertNotEqual(0, proc.returncode)
        self.assertIn("activate FAILED before the switch", proc.stderr)
        lines = self.log_lines()
        stop = self.index_of("systemctl --user stop dunkingsheep-*.timer", lines)
        self.assertIn("systemctl --user start dunkingsheep-*.timer", lines[stop:])
        self.assertNotIn("systemctl --user restart dunkingsheep.service", lines)
        self.assertEqual(OLD_LAUNCHER, read(self.launcher))
        self.assertFalse(os.path.exists(self.dropin))
        self.assertFalse(os.path.exists(self.state))

    def test_backup_archive_and_directory_are_private_under_a_permissive_umask(self):
        # A permissive umask (022) must not make the backup readable: github-tokens.json is in it.
        self.stage()
        proc = self.run_script("activate", "--sha", self.sha, apply=True, umask="022")
        self.assertEqual(0, proc.returncode, proc.stderr + proc.stdout)
        backups = os.path.join(self.home, ".local", "share", "dunkingsheep", "backups")
        self.assertEqual(0o700, mode_of(backups))
        archives = [name for name in os.listdir(backups) if name.startswith("pre-rust-")]
        self.assertEqual(1, len(archives))
        archive = os.path.join(backups, archives[0])
        self.assertEqual(0o600, mode_of(archive))
        listing = subprocess.run(["tar", "-tf", archive], check=True, capture_output=True,
                                 text=True).stdout.split()
        self.assertIn("./github-tokens.json", listing)

    def test_a_failure_during_the_switch_rolls_back_from_the_record(self):
        # activate dies after the switch has started (the reload fails): the record already exists,
        # so rollback restores the python launcher and removes the drop-in.
        self.stage()
        self.env["FAKE_RELOAD_FAIL"] = "1"
        proc = self.run_script("activate", "--sha", self.sha, apply=True)
        self.assertNotEqual(0, proc.returncode)
        self.assertIn("activate FAILED during the switch", proc.stderr)
        self.assertIn("status=switching", read(self.state))
        self.assertTrue(os.path.exists(self.dropin))
        self.assertIn("exec " + self.bin, read(self.launcher))
        del self.env["FAKE_RELOAD_FAIL"]
        proc = self.run_script("rollback", apply=True)
        self.assertEqual(0, proc.returncode, proc.stderr + proc.stdout)
        self.assertFalse(os.path.exists(self.dropin))
        self.assertEqual(OLD_LAUNCHER, read(self.launcher))
        self.assertFalse(os.path.exists(self.state))
        self.assertIn("systemctl --user start dunkingsheep-*.timer", self.log_lines())

    @unittest.skipIf(hasattr(os, "geteuid") and os.geteuid() == 0,
                     "root ignores the directory permissions this test relies on")
    def test_a_failure_at_the_first_switch_step_rolls_back_from_the_record(self):
        # activate dies on its first switch step (the drop-in cannot be written): the record is already
        # there, the launcher was never touched, and rollback still closes the cutover cleanly.
        self.stage()
        dropin_dir = os.path.dirname(self.dropin)
        os.makedirs(dropin_dir, exist_ok=True)
        os.chmod(dropin_dir, 0o500)
        proc = self.run_script("activate", "--sha", self.sha, apply=True)
        self.assertNotEqual(0, proc.returncode)
        self.assertIn("activate FAILED during the switch", proc.stderr)
        self.assertIn("status=switching", read(self.state))
        self.assertEqual(OLD_LAUNCHER, read(self.launcher))
        self.assertFalse(os.path.exists(self.dropin))
        os.chmod(dropin_dir, 0o755)
        proc = self.run_script("rollback", apply=True)
        self.assertEqual(0, proc.returncode, proc.stderr + proc.stdout)
        self.assertIn("no python copy", proc.stdout)
        self.assertEqual(OLD_LAUNCHER, read(self.launcher))
        self.assertFalse(os.path.exists(self.state))

    def test_activate_twice_is_refused(self):
        self.stage()
        self.activate()
        proc = self.run_script("activate", "--sha", self.sha, apply=True)
        self.assertNotEqual(0, proc.returncode)
        self.assertIn("already recorded", proc.stderr)

    # -- rollback -----------------------------------------------------------------------------

    def test_rollback_apply_restores_the_python_launcher_and_removes_the_drop_in(self):
        self.stage()
        self.activate()
        proc = self.run_script("rollback", apply=True)
        self.assertEqual(0, proc.returncode, proc.stderr + proc.stdout)
        self.assertFalse(os.path.exists(self.dropin))
        self.assertEqual(OLD_LAUNCHER, read(self.launcher))
        self.assertEqual(0o755, mode_of(self.launcher))
        self.assertFalse(os.path.exists(self.state))
        rolled = [name for name in os.listdir(os.path.dirname(self.state))
                  if name.startswith("rust-cutover.state.rolled-back-")]
        self.assertEqual(1, len(rolled))
        lines = self.log_lines()
        stop = self.index_of("systemctl --user stop dunkingsheep-*.timer dunkingsheep.service", lines)
        # Only the rollback's own steps, after its stop: activate also ran a daemon-reload and a restart.
        rollback_steps = lines[stop:]
        reload_ = rollback_steps.index("systemctl --user daemon-reload")
        restart = rollback_steps.index("systemctl --user restart dunkingsheep.service")
        start = rollback_steps.index("systemctl --user start dunkingsheep-*.timer")
        self.assertLess(reload_, restart)
        self.assertLess(restart, start)

    def test_rollback_without_a_recorded_cutover_refuses(self):
        proc = self.run_script("rollback", apply=True)
        self.assertNotEqual(0, proc.returncode)
        self.assertIn("no cutover recorded", proc.stderr)

    def test_rollback_takes_no_sha(self):
        proc = self.run_script("rollback", "--sha", self.sha)
        self.assertEqual(2, proc.returncode)
        self.assertIn("takes no --sha", proc.stderr)

    # -- verify -------------------------------------------------------------------------------

    def test_verify_without_apply_reads_and_starts_nothing(self):
        proc = self.run_script("verify")
        self.assertEqual(0, proc.returncode, proc.stderr)
        self.assertIn("## inbox count (%1)", proc.stdout)
        self.assertIn("DRY-RUN: start the timers", proc.stdout)
        lines = self.log_lines()
        self.assertFalse([line for line in lines if " start " in line], lines)
        self.assertFalse([line for line in lines if line.startswith("journalctl")], lines)
        self.assertIn("dunkingsheep status", lines)
        self.assertIn("dunkingsheep inbox count %1", lines)

    def test_verify_apply_starts_the_timers_and_ticks_each_once(self):
        proc = self.run_script("verify", apply=True)
        self.assertEqual(0, proc.returncode, proc.stderr)
        lines = self.log_lines()
        self.index_of("systemctl --user start dunkingsheep-*.timer", lines)
        for unit in ("dunkingsheep-dispatch", "dunkingsheep-health"):
            self.index_of("systemctl --user start " + unit + ".service", lines)
            self.index_of("journalctl --user -u " + unit + ".service -n 5 --no-pager", lines)

    # -- argument handling and the script itself ---------------------------------------------

    def test_unknown_subcommand_and_arguments_exit_2(self):
        self.assertEqual(2, self.run_script("bogus").returncode)
        self.assertEqual(2, self.run_script("stage", "--bogus").returncode)

    def test_a_command_without_sha_says_so(self):
        proc = self.run_script("stage")
        self.assertEqual(2, proc.returncode)
        self.assertIn("--sha <full or short commit> is required", proc.stderr)

    def test_the_script_is_valid_posix_sh(self):
        proc = subprocess.run(["sh", "-n", SCRIPT], capture_output=True, text=True)
        self.assertEqual(0, proc.returncode, proc.stderr)

    def test_the_ci_job_runs_this_file_and_the_syntax_check(self):
        workflow = read(os.path.join(ROOT, ".github", "workflows", "rust.yml"))
        job = workflow[workflow.index("  rust-rollout-tests:"):]
        self.assertIn("sh -n tools/rust_rollout.sh", job)
        self.assertIn("tests/test_rust_rollout.py", job)


if __name__ == "__main__":
    unittest.main()
