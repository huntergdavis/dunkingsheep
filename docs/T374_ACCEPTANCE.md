# t374 acceptance: real automatic A-to-B work

This is the runbook for the only acceptance that closes t374: a **real pi agent**
and a **real Codex agent**, each on a dedicated lane, complete job A and then
receive, start, and complete job B **without a human Enter** and without a
synthetic turn event.

The tool is `tools/t374_acceptance.py`.  It prepares the jobs and launch
commands, collects the live store records, and validates the chain.  It never
presses a key, never calls `send_text`/`send_keys`, and never fabricates a turn.

## What is prepared, and what is not

Prepared now:

- two bounded jobs per lane (`A` and `B`) with private artifacts under one run
  directory;
- exact launch commands for a dedicated pi lane and a dedicated
  `codex --no-daemon` lane with the t264 producer hook;
- a `prepare` step that creates the tasks as **pane-private, assigned** tasks;
- a read-only `collect` step over `tasks.db`, `metrics.db` and the live message
  store, and a `validate` step whose checks are themselves tested to fail.

Not done by this tool: creating/staring the dedicated lanes (the CoS/VP does
that; `start-agent` is superuser), editing the dispatch allowlist, installing
into the owner's home, or merging the final wiring.  Execution is refused until
an approval file matches the running daemon version/code and the repo head.

## Preconditions (CoS2 / reviewer19 / VP15)

- The reviewed integrated release is deployed: final t372 wiring and the t130
  safe-submit fix are live, and the approval file names the exact daemon
  `version`, `code_hash`, and repo `HEAD`.
- Two dedicated lanes exist, named for this run, with no human at the keyboard:
  one pi lane and one Codex lane launched with `--no-daemon` and the t264 hook.
- Both lanes are on the dispatch allowlist, and no unrelated live lane is being
  used.  Do not add production lanes to the list for this run.
- The Codex lane has an isolated `CODEX_HOME` with the t264 hook installed and
  `--dangerously-bypass-hook-trust` only for the dedicated test lane.
- B is **pane-private** on purpose.  If the integrated loop cannot see a
  pane-private assigned task, that is a blocker to report and fix; do not weaken
  the run to global tasks to make it pass.

## Run

1. Generate the run directory:

   ```sh
   python3 tools/t374_acceptance.py plan \
       --out /home/hunter/workspace/t374-acceptance/<run-id> \
       --pi-model opencode/deepseek-v4.1-flash-free \
       --codex-model gpt-6.1-sol \
       --codex-home /tmp/t374-codex-home \
       --codex-state /tmp/t374-codex-state
   ```

   This writes `manifest.json`, `LAUNCH.md`, and the two task definitions.  It
   makes no live changes.

2. The CoS/VP creates the dedicated lanes and launches each with the exact
   command in `LAUNCH.md`.  The prompt includes `sleep 20` so the tasks can be
   created before the agent claims A.

3. Within that window, create the pane-private tasks:

   ```sh
   python3 tools/t374_acceptance.py prepare \
       --run <run-dir> --execute --approval <approval.json> \
       --pi-pane <pi-pane> --codex-pane <codex-pane>
   ```

   It creates A and B for each pane with `--scope <pane> --assign <pane>` and
   prints the ids.  It does not launch or nudge anything.

4. Do **not** send any further prompt or key.  Each agent runs
   `dunkingsheep task next "$TMUX_PANE"` once, claims A, writes
   `<run>/<lane>/A.artifact`, completes A.  The automatic handoff must claim B,
   deliver it as a `[turn-end ...]` prompt, start a new turn, and the agent must
   write `<run>/<lane>/B.artifact` and complete B.

5. Collect the live stores immediately after B completes (messages are recent):

   ```sh
   python3 tools/t374_acceptance.py collect --run <run-dir>
   python3 tools/t374_acceptance.py validate --receipt <run-dir>/receipt.json
   ```

6. Clean up with the guarded task CLI (the tool prints the commands):

   ```sh
   python3 tools/t374_acceptance.py cleanup --run <run-dir>
   python3 tools/t374_acceptance.py cleanup --run <run-dir> --execute
   ```

## What the receipt must show, per lane

| stage | store / field |
|---|---|
| A claimed by the agent itself | `tasks.db` `task_events` `claimed`, `task_leases.caller_name` = `pi` or `codex` |
| A artifact | `<run>/<lane>/A.artifact` starts with `t374 A <run> <lane>` |
| A complete | `tasks.db` `task_events` `completed` |
| handoff | `tasks.db` `task_events` event `next_job_handoff` on B, meta `{from_task: A, message_id: <id>}` |
| B claim | one `task_events` `claimed` on B, after A complete and after the handoff |
| B delivery | live message store: one message to the pane, `source=turn_end`, `status=sent`; `metrics` `message.sent` for the pane |
| B start | `metrics` `turn.start` for the pane after the B claim |
| B complete | `tasks.db` `task_events` `completed` on B; B artifact prefix |
| no human nudge | zero `send_text` messages to the lane during the run |
| no duplicate B | exactly one B claim, one handoff, one sent message |
| no work after B | a `no_assigned_work` decision after B; no C claimed or invented |

The validator also rejects B claimed before A completed, a handoff without a
`message_id`, a wrong `from_task`, a duplicate B claim/delivery, a missing B
turn start, a missing artifact, and any human send.

## Negative probes

- **Repeated completion:** after A is complete, re-run
  `dunkingsheep --self-pane <pane> task complete <A>` once.  The chain must still
  show exactly one B handoff/claim/delivery; a second is a failure.
- **No-work:** after B completes, allow the loop to make its decision and record
  `no_assigned_work`.  No C task or prompt may appear.
- **No-human-nudge:** the launch prompt is the only human input; the runbook
  forbids any `send`, `send-keys`, or Enter during A/B.  The receipt counts
  `send_text` messages to the lane and must show zero.

## Current state (2026-10-28)

Prepared only.  Main has t372 steps 1/2a and t264; the final t372 wiring and the
t130 safe-submit fix are still unmerged.  No live pi/Codex acceptance run has
been executed against an approved integrated release; no owner-home installation
or live-lane respawn has been done.  This runbook and the tool are the
preparation artifact.
