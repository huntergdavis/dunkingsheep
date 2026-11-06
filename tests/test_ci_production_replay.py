"""The CI job "production replay (send, task follow)" (t1605 slices 3 and 4): its static contract.
The tmux steps run only in CI, so this file checks the script and the workflow text without a tmux
server or a daemon."""

import json
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
TASK_FOLLOW_FIXTURE = os.path.join(ROOT, "rust", "fixtures", "task_follow_replay.jsonl")
JOB = "  production-replay-send:"
RUST_JOB = "  production-replay-send-rust:"


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
                      'test_send_scenario test_task_follow_scenario test_ci_production_replay; do',
                      job_text)
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


    def test_the_owner_window_is_named_bash_and_is_not_renamed(self):
        script = read(SCRIPT)
        self.assertIn("new-window -t w0 -n bash -P", script)
        self.assertIn("set-option -g automatic-rename off", script)

    # t1605 slice 4: the task-follow replay in the same job.

    def test_the_job_replays_the_four_task_follow_commands_in_the_gate_order(self):
        match = re.search(r'^TASK_FOLLOW_COMMANDS="([^"]+)"', read(SCRIPT), re.M)
        self.assertIsNotNone(match)
        self.assertEqual(",".join(gate.TASK_FOLLOW_COMMANDS), match.group(1))

    def test_the_task_follow_replay_runs_after_the_send_replay_on_the_same_socket(self):
        script = read(SCRIPT)
        send = script.index('--traffic "$ROOT/rust/fixtures/send_replay.jsonl"')
        follow = script.index('--traffic "$ROOT/rust/fixtures/task_follow_replay.jsonl"')
        self.assertLess(send, follow)
        self.assertIn('--commands "$TASK_FOLLOW_COMMANDS" --volatile "$ROOT/rust/fixtures/volatile.json"',
                      script)
        self.assertEqual(2, script.count('--against "$sock"'))

    def test_either_replay_failing_fails_the_pane_and_each_exit_code_is_reported(self):
        script = read(SCRIPT)
        self.assertIn('echo "$code" > "$temp/replay-send.status"', script)
        self.assertIn('echo "$task_follow_code" > "$temp/replay-task-follow.status"', script)
        self.assertIn('if [ "$code" -eq 0 ]; then\n    code="$task_follow_code"\n  fi', script)
        self.assertIn("for name in recover.out replay.out replay-task-follow.out daemon.log; do",
                      script)
        self.assertIn('echo "task-follow replay exit=', script)

    def test_the_task_follow_fixture_is_the_scenario_made_as_the_owner_pane(self):
        import task_follow_scenario
        entries = gate.harness.load_traffic(TASK_FOLLOW_FIXTURE)
        expected = task_follow_scenario.scenario("%2")
        self.assertEqual([cmd for cmd, _args in expected], [entry["cmd"] for entry in entries])
        self.assertEqual([args for _cmd, args in expected], [entry["args"] for entry in entries])
        for entry in entries:
            self.assertEqual("%2", entry["self_pane_id"], entry["cmd"])
            self.assertTrue(entry["response"]["ok"], entry)
        with open(TASK_FOLLOW_FIXTURE, encoding="utf-8") as handle:
            meta = json.loads(handle.readline())
        self.assertTrue(meta.get("_meta"))
        for key in ("caller", "daemon", "recorded", "scenario", "replay", "scrub", "volatile"):
            self.assertIn(key, meta)

    def test_the_task_follow_fixture_records_the_follower_round_trip(self):
        entries = gate.harness.load_traffic(TASK_FOLLOW_FIXTURE)
        results = [entry["response"]["result"] for entry in entries]
        # The recorded Python behaviour: no assignee argument files an empty assignee list, and
        # created_by names the pane. The replay holds the port to that, not to the docstring.
        self.assertEqual("%2", results[0]["created_by"])
        self.assertEqual([], results[0]["assignees"])
        self.assertEqual(("open", "low", "global", "work"),
                         tuple(results[0][key] for key in ("status", "priority", "scope", "kind")))
        self.assertEqual({"pane_id": "%2", "scope": "global", "following": True,
                          "followers": ["%2"]}, results[1])
        self.assertEqual({"followers": [{"pane_id": "%2", "scope": "global"}]}, results[2])
        self.assertEqual({"pane_id": "%2", "following": False, "was_following": True,
                          "followers": []}, results[3])
        self.assertEqual({"followers": []}, results[4])

    def test_the_replay_checkout_does_not_keep_the_workflow_token(self):
        workflow = read(WORKFLOW)
        job = workflow[workflow.index(JOB):]
        checkout = job[job.index("actions/checkout@v4"):job.index("- name: install tmux")]
        self.assertIn("persist-credentials: false", checkout)

    def test_the_script_takes_an_optional_built_binary_for_the_daemon_and_recover(self):
        script = read(SCRIPT)
        self.assertIn('bin="${3:-}"', script)
        self.assertIn('cli=("$bin")', script)
        self.assertIn('"${cli[@]}" serve', script)
        self.assertIn('"TMUX_PANE=$TMUX_PANE" "${cli[@]}" \\', script)
        self.assertIn("'${DS_BIN:-}'", script)
        self.assertNotIn('"$PY" "$ROOT/dunkingsheep"', script.split("pane_main() {", 1)[1])

    def test_the_sandbox_readiness_wait_is_120_seconds_and_below_the_owner_pane_wait(self):
        script = read(SCRIPT)
        self.assertIn("for _ in $(seq 1 600); do", script)
        readiness_s = 600 * 0.2
        self.assertEqual(120, readiness_s)
        self.assertLess(readiness_s, 200)

    def test_the_rust_job_builds_the_binary_and_replays_against_it_within_its_timeout(self):
        workflow = read(WORKFLOW)
        self.assertIn(RUST_JOB, workflow)
        job = workflow[workflow.index(RUST_JOB):]
        self.assertIn("cargo build --manifest-path rust/Cargo.toml -p ds-cli --bin dunkingsheep", job)
        self.assertIn("DS_BIN: ${{ github.workspace }}/rust/target/debug/dunkingsheep", job)
        self.assertIn("bash tools/ci_production_replay.sh", job)
        checkout = job[job.index("actions/checkout@v4"):job.index("- name: install tmux")]
        self.assertIn("persist-credentials: false", checkout)
        timeout_s = int(re.search(r"timeout-minutes: (\d+)", job).group(1)) * 60
        self.assertLessEqual(timeout_s, 10 * 60)
        self.assertIn("for test in test_production_replay test_diff_harness_sandbox "
                      "test_send_scenario test_ci_production_replay; do", job)


if __name__ == "__main__":
    unittest.main()
