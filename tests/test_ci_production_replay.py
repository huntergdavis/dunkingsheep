"""The CI job "production replay (send)" (t1605 slice 3): its static contract. The tmux steps run only in
CI, so this file checks the script and the workflow text without a tmux server or a daemon."""

import os
import re
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(ROOT, "tools"))

import _bootstrap  # noqa: F401,E402
import production_replay as gate  # noqa: E402

SCRIPT = os.path.join(ROOT, "tools", "ci_production_replay.sh")
WORKFLOW = os.path.join(ROOT, ".github", "workflows", "rust.yml")
JOB = "  production-replay-send:"


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


class CiReplayContractTests(unittest.TestCase):
    def test_the_script_is_valid_bash(self):
        result = subprocess.run(["bash", "-n", SCRIPT], capture_output=True, text=True)
        self.assertEqual(0, result.returncode, result.stderr)

    def test_the_job_replays_the_six_gated_commands_in_the_gate_order(self):
        match = re.search(r'^COMMANDS="([^"]+)"', read(SCRIPT), re.M)
        self.assertIsNotNone(match)
        self.assertEqual(",".join(gate.SEND_COMMANDS), match.group(1))

    def test_the_owner_pane_must_be_the_recorded_pane_and_is_checked(self):
        script = read(SCRIPT)
        self.assertIn('OWNER_PANE="%2"', script)
        self.assertIn('"${TMUX_PANE:-}" != "$OWNER_PANE"', script)
        self.assertIn('[ "$owner" != "$OWNER_PANE" ]', script)

    def test_the_owner_pane_title_is_pinned_to_cat_with_the_reason_written_down(self):
        script = read(SCRIPT)
        self.assertIn('PIN_TITLE="cat"', script)
        self.assertIn('select-pane -t "$owner" -T "$PIN_TITLE"', script)
        self.assertIn("The pin is", script)
        self.assertIn("temporary", script)

    def test_the_sandbox_gets_a_cleared_environment_and_its_own_state(self):
        script = read(SCRIPT)
        self.assertIn("env -i", script)
        self.assertIn('"TMUX_TMPDIR=$temp/tmux"', script)
        self.assertIn('"DUNKINGSHEEP_NO_START_DAEMON=1"', script)

    def test_no_token_secret_or_live_socket_is_referenced(self):
        workflow = read(WORKFLOW)
        job_text = workflow[workflow.index(JOB):]
        for text in (read(SCRIPT), job_text):
            self.assertNotIn("secrets.", text)
            self.assertNotIn("GITHUB_TOKEN", text)
            self.assertNotIn(".config/dunkingsheep", text)

    def test_the_workflow_runs_the_gate_tests_and_the_script_as_its_own_job(self):
        workflow = read(WORKFLOW)
        self.assertIn(JOB, workflow)
        job_text = workflow[workflow.index(JOB):]
        self.assertIn("bash tools/ci_production_replay.sh", job_text)
        self.assertIn('for test in test_production_replay test_diff_harness_sandbox '
                      'test_send_scenario test_ci_production_replay; do', job_text)
        self.assertIn('python3 -I "tests/$test.py"', job_text)


    def test_the_owner_pane_wait_is_bounded_below_the_job_timeout_and_the_message_matches(self):
        script = read(SCRIPT)
        workflow = read(WORKFLOW)
        job = workflow[workflow.index(JOB):]
        timeout_s = int(re.search(r"timeout-minutes: (\d+)", job).group(1)) * 60
        ticks = int(re.search(
            r'for _ in \$\(seq 1 (\d+)\); do\n    \[ -s "\$TEMP_DIR/replay\.status" \]', script).group(1))
        self.assertLess(ticks, timeout_s)
        self.assertIn("did not finish in %d s" % ticks, script)


if __name__ == "__main__":
    unittest.main()
